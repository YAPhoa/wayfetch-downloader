//! A file host's download: a fresh URL for each attempt, continued under
//! the host's identity of the file rather than the URL's, decoded as it is
//! written when the host stores files encoded, and checked before it is
//! published.

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio_util::sync::CancellationToken;
use url::Url;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::job::JobStage;
use wayfetch_domain::name::DiagnosticText;
use wayfetch_protocol::query::FailureSummary;

use super::{Download, Stop, Target, failed, file_name, now, park_fs, target_name, why};
use crate::fs::{Root, StagingFile};
use crate::hosting::{Decoder, Encoding, FileHost, HostError, HostedFile};
use crate::transfer::lease::Representation;
use crate::transfer::range::{self, Plan};
use crate::transfer::transport::OpenRequest;
use crate::transfer::writer::{BoxFuture, ChunkSource};

/// Longest a file host may take to give a download URL.
const RESOLVE_DEADLINE: Duration = Duration::from_secs(60);

/// Marks a recorded hosted-file identity, and its version.
const IDENTITY_TAG: &str = "wfh1";

/// How much of a finished file is read at once to check it.
const VERIFY_BUFFER: usize = 1024 * 1024;

impl Download {
    /// Downloads a file host's link: asks the host for this attempt's URL,
    /// continues saved bytes only if the host still names the same file,
    /// decodes what it writes, and checks the whole file before
    /// publishing it.
    pub(super) async fn drive_hosted(
        &mut self,
        root: &Arc<Root>,
        url: &Url,
        host: Arc<dyn FileHost>,
    ) -> Result<(), Stop> {
        let continued = self.continue_partial(root, true).await?;
        let hosted = self.hosted_file(host.as_ref(), url).await?;
        let size = hosted.size;
        let resource = format!("{IDENTITY_TAG};{};{}", host.service(), hosted.identity);
        if let Some(c) = &continued {
            let stored = c.partial.representation.as_ref();
            if stored.and_then(|r| r.resource.as_deref()) != Some(resource.as_str())
                || stored.and_then(|r| r.length) != Some(size)
            {
                return Err(Stop::Restart(why::CHANGED));
            }
        }
        let durable = continued.as_ref().map_or(0, |c| c.partial.durable_end);
        if durable > size {
            return Err(Stop::Restart(why::GAPS));
        }
        let suggested = match &continued {
            Some(c) => c.partial.suggested_name.clone(),
            None => hosted
                .name
                .as_deref()
                .and_then(file_name)
                .map(|name| name.as_str().to_owned()),
        };
        let (file, resumed) = match continued {
            Some(c) if durable == size => (c.file, true),
            continued => {
                if self.job.stage == JobStage::Resolving {
                    self.advance(JobStage::Transferring).await?;
                }
                let resumed = continued.is_some();
                let file = continued.map(|c| c.file);
                let representation = Representation {
                    length: Some(size),
                    resource: Some(resource),
                    ..Representation::default()
                };
                let file = self
                    .fetch_hosted(
                        root,
                        file,
                        host.as_ref(),
                        &hosted,
                        durable,
                        representation,
                        suggested.clone(),
                    )
                    .await?;
                (file, resumed)
            }
        };
        if self.job.stage == JobStage::Resolving {
            self.advance(JobStage::Transferring).await?;
        }
        self.advance(JobStage::Verifying).await?;
        let file = match &hosted.encoding {
            Some(encoding) => {
                let (file, whole) = self.verify(file, size, encoding.clone()).await?;
                if !whole {
                    // Bytes from an earlier attempt may be what is wrong;
                    // a file downloaded whole and still wrong is reported,
                    // not downloaded again and again.
                    return Err(if resumed {
                        Stop::Restart(why::DAMAGED)
                    } else {
                        failed(
                            FailureCode::IntegrityMismatch,
                            "The downloaded file did not pass its host's integrity check, so it \
                             was not saved. Retry to download it again.",
                        )
                    });
                }
                file
            }
            None => file,
        };
        self.advance(JobStage::Publishing).await?;
        let name = target_name(self.job.job, suggested.as_deref(), url);
        self.publish(root, file, size, &name, 0).await
    }

    /// The host's file for this attempt.
    async fn hosted_file(&self, host: &dyn FileHost, url: &Url) -> Result<HostedFile, Stop> {
        let resolving = tokio::time::timeout(RESOLVE_DEADLINE, host.resolve(url));
        let resolved = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(Stop::Interrupted),
            resolved = resolving => resolved,
        };
        match resolved {
            Err(_) => Err(failed(
                FailureCode::RequestTimeout,
                "The file host took too long to answer.",
            )),
            Ok(Err(e)) => Err(host_failure(e)),
            Ok(Ok(hosted)) => Ok(hosted),
        }
    }

    /// Downloads the hosted file from byte `from` into `file`, or into a
    /// new staging file described by `representation`.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_hosted(
        &mut self,
        root: &Arc<Root>,
        file: Option<StagingFile>,
        host: &dyn FileHost,
        hosted: &HostedFile,
        from: u64,
        representation: Representation,
        suggested: Option<String>,
    ) -> Result<StagingFile, Stop> {
        let size = hosted.size;
        let requested = (from > 0).then_some((from, None));
        let request = OpenRequest {
            url: hosted.url.clone(),
            range: requested,
            // The host's identity names the file; its URLs do not.
            validators: None,
            connect_timeout: self.timeouts.connect,
            read_buffer: self.limits.read_buffer_bytes.get(),
            user_agent: None,
            reach: self.job.reach,
        };
        let mut response = match self.send_or_refused(&request).await? {
            Ok(response) => response,
            Err(e) => {
                return Err(match e.status.and_then(|status| host.refused(status)) {
                    Some(explained) => host_failure(explained),
                    None => failed(e.code, e.message),
                });
            }
        };
        if response
            .representation
            .length
            .is_some_and(|length| length != size)
        {
            return Err(failed(
                FailureCode::IntegrityMismatch,
                "the file host's download is not the size it reported",
            ));
        }
        let start = match range::plan(requested, response.framing, Some(size))
            .map_err(|e| failed(FailureCode::IntegrityMismatch, e))?
        {
            Plan::Write {
                start,
                end: Some(end),
                ..
            } if start == from && end == size => start,
            Plan::RestartFromZero { .. } if from == 0 => 0,
            Plan::RestartFromZero { .. } => return Err(Stop::Restart(why::RANGE_IGNORED)),
            Plan::Reconcile { .. } => return Err(Stop::Restart(why::NOTHING_LEFT)),
            Plan::Write { .. } => {
                return Err(failed(
                    FailureCode::IntegrityMismatch,
                    "the file host did not send the bytes asked for",
                ));
            }
        };
        let file = match file {
            Some(file) => file,
            None => self.create_staging(root, representation, suggested).await?,
        };
        if let Some(encoding) = &hosted.encoding {
            response.body = Box::new(Decoding {
                body: response.body,
                decoder: encoding.decoder(start),
            });
        }
        self.meter(start, Some(size));
        let target = Target {
            start,
            end: Some(size),
        };
        let (_, file) = self.stream(file, response, &target).await?;
        self.metering = None;
        Ok(file)
    }

    /// Reads the finished file once, through `encoding`'s check. Returns
    /// the file and whether it passed.
    async fn verify(
        &self,
        file: StagingFile,
        size: u64,
        encoding: Arc<dyn Encoding>,
    ) -> Result<(StagingFile, bool), Stop> {
        let cancel = self.cancel.clone();
        let read = self
            .blocking(move || {
                check_file(&file, size, encoding.as_ref(), &cancel).map(|ok| (file, ok))
            })
            .await?;
        match read {
            Ok((file, Some(whole))) => Ok((file, whole)),
            Ok((_, None)) => Err(Stop::Interrupted),
            Err(source) => Err(park_fs(&crate::fs::FsError::Io {
                op: "read staging file",
                source,
            })),
        }
    }
}

/// Feeds `size` bytes of `file` to the check. `None` if cancelled first;
/// the file is read between cancellation checks, never abandoned mid-read.
fn check_file(
    file: &StagingFile,
    size: u64,
    encoding: &dyn Encoding,
    cancel: &CancellationToken,
) -> std::io::Result<Option<bool>> {
    let mut verifier = encoding.verifier();
    let mut buffer = vec![0; VERIFY_BUFFER];
    let mut at = 0;
    while at < size {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let want = usize::try_from(size - at).map_or(VERIFY_BUFFER, |left| left.min(VERIFY_BUFFER));
        let read = file.read_at(&mut buffer[..want], at)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        verifier.update(&buffer[..read]);
        at += read as u64;
    }
    Ok(Some(verifier.verify()))
}

/// The failure a host's refusal records.
fn host_failure(e: HostError) -> Stop {
    Stop::Fail(FailureSummary {
        code: e.code,
        retry_at: e.retry_after.and_then(|after| now().checked_add(after)),
        message: Some(DiagnosticText::new(&e.message)),
    })
}

/// A body decoded as it is read.
struct Decoding {
    body: Box<dyn ChunkSource>,
    decoder: Box<dyn Decoder>,
}

impl ChunkSource for Decoding {
    fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<bytes::Bytes>>> {
        Box::pin(async move {
            let Some(chunk) = self.body.next_chunk().await? else {
                return Ok(None);
            };
            let mut data = BytesMut::from(&chunk[..]);
            self.decoder.decode(&mut data);
            Ok(Some(data.freeze()))
        })
    }
}
