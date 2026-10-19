//! A media download (protocol 4.0): the media service's streams, laid end
//! to end in the job's one staging file, then joined or converted into a
//! second file when the plan needs it.
//!
//! The staging file's recorded resource is its [`Layout`]: the recipe,
//! the extension, each stream's length, and the service's identity of what
//! was chosen. A continued download asks the service for fresh links and
//! goes on only if the new plan has the same layout; streams already
//! complete need no links at all, so a conversion interrupted by a crash
//! runs again from the recorded layout alone.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use url::Url;
use wayfetch_domain::failure::FailureCode;
use wayfetch_domain::job::JobStage;

use super::{Download, MediaJob, Stop, Target, failed, park_fs, target_name, why};
use crate::fs::{RelativePath, Root, StagingFile, StagingRecord};
use crate::media::{
    Container, Conversion, Converter, MediaPlan, MediaStream, Part, Recipe, title_file_name,
};
use crate::store::{ConversionState, StoreError};
use crate::transfer::lease::Representation;
use crate::transfer::range::{self, Plan};
use crate::transfer::transport::OpenRequest;

/// Longest a media service may take to answer a plan.
const RESOLVE_DEADLINE: Duration = Duration::from_secs(90);

/// Marks a recorded layout, and its version.
const LAYOUT_TAG: &str = "wfm1";

/// What a recipe that converts needs while the converter is missing.
const NEEDS_CONVERTER: &str = "This format needs the ffmpeg plugin, which is turned off or not available. See Settings > Plugins.";

/// How a media download's staging file is laid out.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    recipe: Recipe,
    extension: String,
    lengths: Vec<u64>,
    identity: String,
}

impl Layout {
    /// The layout of a checked plan.
    fn of(plan: &MediaPlan) -> Self {
        Self {
            recipe: plan.recipe,
            extension: plan.extension.clone(),
            lengths: plan.streams.iter().map(|s| s.length).collect(),
            identity: plan.identity.clone(),
        }
    }

    /// The layout as recorded: `wfm1;recipe;extension;lengths;identity`.
    fn encode(&self) -> String {
        let recipe = match self.recipe {
            Recipe::Copy => "copy".to_owned(),
            Recipe::Mp3 => "mp3".to_owned(),
            Recipe::Join(container) => format!("join-{container}"),
        };
        let lengths: Vec<_> = self.lengths.iter().map(u64::to_string).collect();
        format!(
            "{LAYOUT_TAG};{recipe};{};{};{}",
            self.extension,
            lengths.join(","),
            self.identity
        )
    }

    /// A recorded layout, if it is one.
    fn decode(raw: &str) -> Option<Self> {
        let mut fields = raw.splitn(5, ';');
        if fields.next()? != LAYOUT_TAG {
            return None;
        }
        let recipe = match fields.next()? {
            "copy" => Recipe::Copy,
            "mp3" => Recipe::Mp3,
            other => Recipe::Join(Container::from_str(other.strip_prefix("join-")?).ok()?),
        };
        let extension = fields.next()?.to_owned();
        let lengths = fields
            .next()?
            .split(',')
            .map(|n| n.parse().ok())
            .collect::<Option<Vec<u64>>>()?;
        let identity = fields.next()?.to_owned();
        let layout = Self {
            recipe,
            extension,
            lengths,
            identity,
        };
        layout.total().map(|_| layout)
    }

    /// The staging file's whole length.
    fn total(&self) -> Option<u64> {
        self.lengths
            .iter()
            .try_fold(0u64, |total, &n| total.checked_add(n))
    }

    /// Where each stream is.
    fn parts(&self) -> Vec<Part> {
        let mut offset = 0;
        self.lengths
            .iter()
            .map(|&length| {
                let part = Part { offset, length };
                offset += length;
                part
            })
            .collect()
    }
}

impl Download {
    /// Downloads a media link: settles a conversion an earlier run left,
    /// continues or starts its streams, converts them if the plan says so,
    /// and publishes the file.
    pub(super) async fn drive_media(&mut self, root: &Arc<Root>, url: &Url) -> Result<(), Stop> {
        let media = self.media.clone().ok_or(Stop::Abandon)?;
        let job = self.job.job;
        // A half-written output is never continued: the streams are still
        // complete, so it is made again.
        let converted = match self.call(move |s| s.conversion(job)).await?? {
            ConversionState::None => false,
            ConversionState::Converting { aux } => {
                self.discard_aux(root, aux).await?;
                false
            }
            ConversionState::Converted { aux } => {
                if let Some(aux) = aux {
                    self.discard_aux(root, aux).await?;
                }
                true
            }
        };
        let continued = self.continue_partial(root, true).await?;
        if converted && let Some(c) = continued {
            // The staging file is the finished output.
            if !c.partial.is_complete() {
                return Err(Stop::Restart(why::FILE_LOST));
            }
            let name = target_name(job, c.partial.suggested_name.as_deref(), url);
            return self.finish(root, c.file, c.partial.durable_end, name).await;
        }
        let stored = match &continued {
            Some(c) => Some(
                c.partial
                    .representation
                    .as_ref()
                    .and_then(|r| r.resource.as_deref())
                    .and_then(Layout::decode)
                    .ok_or(Stop::Restart(why::UNIDENTIFIED))?,
            ),
            None => None,
        };
        // Complete streams need no links; anything else asks for fresh ones.
        let (layout, streams, title) = match (&continued, stored) {
            (Some(c), Some(layout)) if c.partial.is_complete() => (layout, Vec::new(), None),
            (_, stored) => {
                let plan = self.media_plan(&media, url).await?;
                let layout = Layout::of(&plan);
                if stored.is_some_and(|stored| stored != layout) {
                    return Err(Stop::Restart(why::MEDIA_CHANGED));
                }
                (layout, plan.streams, Some(plan.title))
            }
        };
        let total = layout.total().ok_or(Stop::Abandon)?;
        let converter = match (layout.recipe.converts(), &media.converter) {
            (false, _) => None,
            (true, Some(converter)) => Some(converter.clone()),
            (true, None) => {
                return Err(failed(FailureCode::UnsupportedCapability, NEEDS_CONVERTER));
            }
        };
        let suggested = match &continued {
            Some(c) => c.partial.suggested_name.clone(),
            None => title
                .as_deref()
                .and_then(|t| title_file_name(t, Some(&layout.extension)))
                .map(|name| name.as_str().to_owned()),
        };
        let mut durable = continued.as_ref().map_or(0, |c| c.partial.durable_end);
        if durable > total {
            return Err(Stop::Restart(why::GAPS));
        }
        self.advance(JobStage::Transferring).await?;
        let mut file = match continued {
            Some(c) => c.file,
            None => {
                let representation = Representation {
                    length: Some(total),
                    resource: Some(layout.encode()),
                    ..Representation::default()
                };
                self.create_staging(root, representation, suggested.clone())
                    .await?
            }
        };
        self.meter(durable, Some(total));
        let mut base = 0;
        for (i, &length) in layout.lengths.iter().enumerate() {
            let end = base + length;
            if durable < end {
                let stream = streams.get(i).ok_or(Stop::Abandon)?;
                file = self
                    .fetch_stream(file, stream, base, durable - base)
                    .await?;
                durable = end;
            }
            base = end;
        }
        self.metering = None;
        let name = target_name(job, suggested.as_deref(), url);
        let Some(converter) = converter else {
            return self.finish(root, file, total, name).await;
        };
        // The tags carry the title; after a restart, the file name's stem.
        let title = title.unwrap_or_else(|| {
            let name = name.as_str();
            name.rsplit_once('.')
                .map_or(name, |(stem, _)| stem)
                .to_owned()
        });
        self.convert(root, file, &layout, &title, converter.as_ref(), name)
            .await
    }

    /// The service's plan for this job's choice, with fresh stream links.
    async fn media_plan(&self, media: &MediaJob, url: &Url) -> Result<MediaPlan, Stop> {
        let Some(service) = &media.service else {
            return Err(failed(
                FailureCode::UnsupportedCapability,
                "Wayfetch has no integration for this video site any more.",
            ));
        };
        if let Some(problem) = crate::media::unusable(&service.plugins()) {
            return Err(failed(FailureCode::UnsupportedCapability, problem));
        }
        let resolving = tokio::time::timeout(
            RESOLVE_DEADLINE,
            service.resolve(url, media.choice, media.converter.is_some()),
        );
        let plan = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return Err(Stop::Interrupted),
            plan = resolving => plan,
        };
        let plan = plan
            .map_err(|_| {
                failed(
                    FailureCode::RequestTimeout,
                    "Reading the video's formats took too long.",
                )
            })?
            .map_err(|e| failed(e.code, e.message))?;
        plan.checked_length()
            .map_err(|e| failed(e.code, e.message))?;
        Ok(plan)
    }

    /// Downloads `stream`, laid at `base` in the staging file, from its
    /// byte `from`, in requests of at most its chunk size.
    async fn fetch_stream(
        &self,
        mut file: StagingFile,
        stream: &MediaStream,
        base: u64,
        from: u64,
    ) -> Result<StagingFile, Stop> {
        let mut at = from;
        while at < stream.length {
            let end = stream.chunk_bytes.map_or(stream.length, |chunk| {
                at.saturating_add(chunk).min(stream.length)
            });
            let requested = (at > 0 || end < stream.length).then_some((at, Some(end)));
            let request = OpenRequest {
                url: stream.url.clone(),
                range: requested,
                // The layout identifies the stream; its links do not.
                validators: None,
                connect_timeout: self.timeouts.connect,
                read_buffer: self.limits.read_buffer_bytes.get(),
                user_agent: stream.user_agent.clone(),
                reach: self.job.reach,
            };
            let response = self.send(&request).await?;
            let plan = range::plan(requested, response.framing, Some(stream.length))
                .map_err(|e| failed(FailureCode::IntegrityMismatch, e))?;
            let stop = match plan {
                Plan::Write {
                    start,
                    end: Some(stop),
                    ..
                } if start == at && stop > at => stop,
                Plan::RestartFromZero { .. } => return Err(Stop::Restart(why::RANGE_IGNORED)),
                Plan::Reconcile { .. } => return Err(Stop::Restart(why::NOTHING_LEFT)),
                Plan::Write { .. } => {
                    return Err(failed(
                        FailureCode::IntegrityMismatch,
                        "the video host did not send the bytes asked for",
                    ));
                }
            };
            let target = Target {
                start: base + at,
                end: Some(base + stop),
            };
            file = self.stream(file, response, &target).await?.1;
            at = stop;
        }
        Ok(file)
    }

    /// Converts the complete streams in `file` into a second staging file
    /// recorded before it is written, makes that the job's file, and
    /// publishes it.
    async fn convert(
        &mut self,
        root: &Arc<Root>,
        file: StagingFile,
        layout: &Layout,
        title: &str,
        converter: &dyn Converter,
        name: RelativePath,
    ) -> Result<(), Stop> {
        let (job, generation) = (self.job.job, self.job.generation);
        self.advance(JobStage::Verifying).await?;
        let r = root.clone();
        let output = self
            .blocking(move || r.create_media_output(job, generation))
            .await?
            .map_err(|e| park_fs(&e))?;
        let record = output.record().clone();
        let begun = record.clone();
        if let Err(e) = self.call(move |s| s.begin_conversion(&begun)).await? {
            // As for a refused staging registration: only a file no
            // committed record can name is removed here.
            if !matches!(e, StoreError::Sqlite(_)) {
                drop(output);
                let (r, record) = (root.clone(), record.clone());
                let _ = self.blocking(move || r.remove_staging(&record)).await;
            }
            return Err(e.into());
        }
        let input = root.staging_file_path(&file.record().name);
        let written = root.staging_file_path(&record.name);
        let parts = layout.parts();
        let conversion = Conversion {
            input: &input,
            parts: &parts,
            output: &written,
            recipe: layout.recipe,
            title,
        };
        let converted = {
            // The converter writes in the staging directory.
            let _guard = self.guard.clone();
            tokio::select! {
                biased;
                // The output stays recorded; the next run removes it.
                () = self.cancel.cancelled() => return Err(Stop::Interrupted),
                converted = converter.convert(conversion) => converted,
            }
        };
        if let Err(e) = converted {
            drop(output);
            self.discard_aux(root, record).await?;
            return Err(failed(e.code, e.message));
        }
        drop(output);
        // The converter opened the output by path: it must still be the
        // file the engine created.
        let (r, reopening) = (root.clone(), record.clone());
        let (output, length) = self
            .blocking(move || {
                let output = r.reopen_staging(&reopening, 0).map_err(|e| park_fs(&e))?;
                let unavailable = |e: std::io::Error| {
                    failed(
                        FailureCode::DestinationUnavailable,
                        format!("could not finish the converted file: {e}"),
                    )
                };
                let length = output.file_len().map_err(unavailable)?;
                output.sync_data().map_err(unavailable)?;
                Ok::<_, Stop>((output, length))
            })
            .await??;
        if length == 0 {
            return Err(failed(
                FailureCode::IntegrityMismatch,
                "the converter wrote an empty file",
            ));
        }
        let finished = self
            .call(move |s| s.finish_conversion(job, generation, length))
            .await??;
        if finished != *output.record() {
            return Err(Stop::Abandon);
        }
        // The streams are no longer needed; one that cannot be removed now
        // stays recorded for the next run or deletion.
        let streams = file.record().clone();
        drop(file);
        let _ = self.discard_aux(root, streams).await;
        self.advance(JobStage::Publishing).await?;
        self.publish(root, output, length, &name, 0).await
    }

    /// Removes a conversion's other file `aux`, then forgets it. A name
    /// that holds another file is left alone and forgotten.
    async fn discard_aux(&self, root: &Arc<Root>, aux: StagingRecord) -> Result<(), Stop> {
        let (r, removing) = (root.clone(), aux.clone());
        // Removed, already gone, or another file: either way the record
        // no longer names a file of the engine's.
        self.blocking(move || r.remove_staging(&removing))
            .await?
            .map_err(|e| park_fs(&e))?;
        self.call(move |s| s.forget_conversion_aux(&aux)).await??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_round_trip_and_reject_anything_else() {
        let layout = Layout {
            recipe: Recipe::Join(Container::Mp4),
            extension: "mp4".into(),
            lengths: vec![100, 20],
            identity: "youtube;abc;137;140".into(),
        };
        let encoded = layout.encode();
        assert_eq!(encoded, "wfm1;join-mp4;mp4;100,20;youtube;abc;137;140");
        assert_eq!(Layout::decode(&encoded), Some(layout.clone()));
        assert_eq!(
            layout.parts(),
            [
                Part {
                    offset: 0,
                    length: 100
                },
                Part {
                    offset: 100,
                    length: 20
                }
            ]
        );
        for raw in [
            "sha256:abc",
            "wfm2;copy;mp4;1;x",
            "wfm1;join-avi;avi;1,2;x",
            "wfm1;copy;mp4;1,x;x",
            "wfm1;copy;mp4;18446744073709551615,1;x",
            "wfm1;copy;mp4",
        ] {
            assert_eq!(Layout::decode(raw), None, "{raw}");
        }
    }
}
