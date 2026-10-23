//! Media links and the conversions of their downloads.
//!
//! A recognized link is checked in the background and the person's format
//! choice is copied to its job. A join or conversion writes a second file
//! named by its record (`aux_*`) before anything is written, so a restart
//! can redo it and cleanup can remove it by identity.

use rusqlite::{OptionalExtension, Transaction, params};
use url::Url;
use wayfetch_domain::id::{JobId, SourceLinkId};
use wayfetch_domain::job::JobStage;
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Generation;
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::filter::{CheckOutcome, LinkRoute};
use wayfetch_protocol::media::{LinkMedia, MediaChoice, MediaState};

use super::{OrCorrupt, Store, StoreError, files, key, outbox, sql};
use crate::fs::{FileIdentity, StagingRecord};
use crate::link_filter::Answer;
use crate::media::MediaInfo;

/// What an unavailable option needs while the converter is missing.
pub(crate) const NEEDS_CONVERTER: &str =
    "Some formats need the ffmpeg plugin, which is not available. See Settings > Plugins.";

/// A link's stored choice, the default if none.
pub(super) fn decode_choice(raw: Option<&str>) -> Result<MediaChoice, StoreError> {
    raw.map_or(Ok(MediaChoice::default()), |raw| {
        serde_json::from_str(raw).or_corrupt("media choice")
    })
}

/// A choice as stored.
pub(super) fn encode_choice(choice: &MediaChoice) -> String {
    serde_json::to_string(choice).expect("a choice always serializes")
}

/// A stored check result.
pub(super) fn decode_info(raw: Option<&str>) -> Result<Option<MediaInfo>, StoreError> {
    raw.map(|raw| serde_json::from_str(raw).or_corrupt("media info"))
        .transpose()
}

/// A link's media columns, as read.
pub(super) struct LinkColumns {
    pub(super) service: Option<String>,
    pub(super) state: Option<String>,
    pub(super) info: Option<String>,
    pub(super) problem: Option<String>,
    pub(super) choice: Option<String>,
}

impl LinkColumns {
    /// The columns from `row`, starting at `first`.
    pub(super) fn read(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            service: row.get(first)?,
            state: row.get(first + 1)?,
            info: row.get(first + 2)?,
            problem: row.get(first + 3)?,
            choice: row.get(first + 4)?,
        })
    }

    /// What a client sees, if the link is a media link; `converter` says
    /// whether choices needing it are available.
    pub(super) fn summary(self, converter: bool) -> Result<Option<LinkMedia>, StoreError> {
        let Some(service) = self.service else {
            return Ok(None);
        };
        let service = ProviderId::new(service).or_corrupt("media service")?;
        let state = match self.state.as_deref() {
            Some("ready") => MediaState::Ready,
            Some("failed") => MediaState::Unavailable,
            _ => MediaState::Checking,
        };
        let info = decode_info(self.info.as_deref())?;
        let options = info
            .as_ref()
            .map(|i| i.options(converter))
            .unwrap_or_default();
        let choice = decode_choice(self.choice.as_deref())?;
        let choice = info.as_ref().map_or(choice, |i| i.shown(choice, converter));
        Ok(Some(LinkMedia {
            service,
            state,
            title: info
                .as_ref()
                .map(|i| i.title.clone())
                .filter(|t| !t.is_empty()),
            duration_seconds: info.as_ref().and_then(|i| i.duration_seconds),
            choice,
            needs: options
                .iter()
                .any(|o| !o.available)
                .then(|| DiagnosticText::new(NEEDS_CONVERTER))
                .filter(|_| !converter),
            options,
            problem: self.problem.as_deref().map(DiagnosticText::new),
        }))
    }
}

/// A media download's conversion, as its staging record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversionState {
    /// No conversion started.
    None,
    /// The output `aux` is being written.
    Converting {
        /// The output.
        aux: StagingRecord,
    },
    /// The staging file is the output; `aux`, the streams, may remain.
    Converted {
        /// The streams, until removed.
        aux: Option<StagingRecord>,
    },
}

/// A staging record's media phase and `aux_*` columns.
type AuxRow = (
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

/// `record`'s `aux_*` file, if any.
fn aux_of(
    tx: &Transaction<'_>,
    record: &StagingRecord,
) -> Result<(Option<String>, Option<StagingRecord>), StoreError> {
    let row: Option<AuxRow> = tx
        .query_row(
            "SELECT media_phase, aux_name, aux_device, aux_inode, aux_birth_ns FROM staging_files \
             WHERE job_id = ?1 AND generation = ?2 AND name = ?3",
            params![sql(record.job)?, sql(record.generation)?, record.name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((phase, name, device, inode, birth_ns)) = row else {
        return Err(StoreError::NotFound("staging file"));
    };
    let aux = match (name, device, inode) {
        (Some(name), Some(device), Some(inode)) => Some(StagingRecord {
            name,
            identity: FileIdentity {
                device: device.cast_unsigned(),
                inode: inode.cast_unsigned(),
                birth_ns,
            },
            ..record.clone()
        }),
        (None, None, None) => None,
        _ => return Err(StoreError::Corrupt("media output record")),
    };
    Ok((phase, aux))
}

impl Store {
    /// Whether choices needing the converter can be made. The coordinator
    /// sets it once at start.
    pub(crate) fn set_media_converter(&mut self, ready: bool) {
        self.media_converter = ready;
    }

    /// Records what check `task` of `link` found, if it is still the
    /// link's check, with a `links_changed` event. A choice the link no
    /// longer offers goes back to the default.
    pub(crate) fn record_media(
        &mut self,
        task: i64,
        link: SourceLinkId,
        found: Result<MediaInfo, String>,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let tx = self.write()?;
        if !super::crawl::finish(&tx, task)? {
            return Ok(());
        }
        let (state, info, problem) = match found {
            Ok(info) => {
                let info = serde_json::to_string(&info.normalized()).or_corrupt("media info")?;
                // A pathological answer is a failed check, never a huge row.
                if info.len() > 8192 {
                    (
                        "failed",
                        None,
                        Some("the video's details are too long".to_owned()),
                    )
                } else {
                    ("ready", Some(info), None)
                }
            }
            Err(problem) => ("failed", None, Some(problem)),
        };
        let changed = tx.execute(
            "UPDATE source_links SET media_state = ?1, media_info = ?2, media_problem = ?3 \
             WHERE id = ?4 AND media_state = 'pending' AND listed = 1",
            params![
                state,
                info,
                problem
                    .as_deref()
                    .map(|p| DiagnosticText::new(p).as_str().to_owned()),
                sql(link)?
            ],
        )?;
        if changed == 1 {
            let answer = Answer {
                name: None,
                size: None,
                check: if state == "ready" {
                    CheckOutcome::Online
                } else {
                    CheckOutcome::Unknown
                },
                route: LinkRoute::Media,
            };
            super::links::settle(&tx, sql(link)?, &answer)?;
            outbox::append(&tx, &EventKind::LinksChanged, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Checks every listed media link whose check failed again, as after
    /// a plugin was installed. Returns how many.
    pub(crate) fn retry_failed_media(&mut self, now: UtcTimestamp) -> Result<usize, StoreError> {
        let tx = self.write()?;
        let n = retry_failed(&tx, now)?;
        tx.commit()?;
        Ok(n)
    }

    /// What the conversion of `job`'s newest staging file stands at.
    pub(crate) fn conversion(&mut self, job: JobId) -> Result<ConversionState, StoreError> {
        let tx = self.read()?;
        let Some(partial) = files::newest(&tx, job)? else {
            return Ok(ConversionState::None);
        };
        Ok(match aux_of(&tx, &partial)? {
            (None, None) => ConversionState::None,
            (Some(p), Some(aux)) if p == "converting" => ConversionState::Converting { aux },
            (Some(p), aux) if p == "converted" => ConversionState::Converted { aux },
            _ => return Err(StoreError::Corrupt("media phase")),
        })
    }

    /// Records the output file `output` of the generation's conversion
    /// before anything is written to it. The job must be verifying, and its
    /// staging file not converting already.
    pub(crate) fn begin_conversion(&mut self, output: &StagingRecord) -> Result<(), StoreError> {
        let tx = self.write()?;
        files::job_at(
            &tx,
            output.job,
            output.generation,
            &[JobStage::Verifying],
            "a conversion runs while the job is verifying",
        )?;
        let changed = tx.execute(
            "UPDATE staging_files SET media_phase = 'converting', aux_name = ?1, \
             aux_device = ?2, aux_inode = ?3, aux_birth_ns = ?4 \
             WHERE job_id = ?5 AND generation = ?6 AND root_id = ?7 AND cleanup IS NULL \
             AND media_phase IS NULL AND aux_name IS NULL",
            params![
                output.name,
                output.identity.device.cast_signed(),
                output.identity.inode.cast_signed(),
                output.identity.birth_ns,
                sql(output.job)?,
                sql(output.generation)?,
                sql(output.root)?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Staging(
                "the staging file cannot start a conversion",
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Makes the finished output of `length` bytes the generation's staging
    /// file, keeping the streams as `aux_*` until they are removed, and
    /// makes the output's whole length its durable coverage. The caller has
    /// synchronized the output. Returns the output's record.
    pub(crate) fn finish_conversion(
        &mut self,
        job: JobId,
        generation: Generation,
        length: u64,
    ) -> Result<StagingRecord, StoreError> {
        let tx = self.write()?;
        files::job_at(
            &tx,
            job,
            generation,
            &[JobStage::Verifying],
            "a conversion finishes while the job is verifying",
        )?;
        let length_sql =
            i64::try_from(length).map_err(|_| StoreError::Staging("output too long"))?;
        let changed = tx.execute(
            "UPDATE staging_files SET media_phase = 'converted', \
             name = aux_name, aux_name = name, \
             file_device = aux_device, aux_device = file_device, \
             file_inode = aux_inode, aux_inode = file_inode, \
             file_birth_ns = aux_birth_ns, aux_birth_ns = file_birth_ns, \
             representation_known = 1, representation_length = ?3, \
             representation_etag = NULL, representation_modified = NULL \
             WHERE job_id = ?1 AND generation = ?2 AND media_phase = 'converting' \
             AND cleanup IS NULL",
            params![sql(job)?, sql(generation)?, length_sql],
        )?;
        if changed != 1 {
            return Err(StoreError::Staging("no conversion to finish"));
        }
        let partial: i64 = tx.query_row(
            "SELECT id FROM partials WHERE job_id = ?1 AND generation = ?2",
            params![sql(job)?, sql(generation)?],
            |r| r.get(0),
        )?;
        tx.execute(
            "UPDATE partials SET checkpoint_seq = checkpoint_seq + 1, durable_bytes = ?1 \
             WHERE id = ?2",
            params![length_sql, partial],
        )?;
        tx.execute(
            "DELETE FROM durable_ranges WHERE partial_id = ?1",
            [partial],
        )?;
        if length > 0 {
            tx.execute(
                "INSERT INTO durable_ranges (partial_id, start_inclusive, end_exclusive) \
                 VALUES (?1, 0, ?2)",
                params![partial, length_sql],
            )?;
        }
        tx.execute(
            "UPDATE jobs SET bytes_total = ?1 WHERE id = ?2",
            params![length_sql, sql(job)?],
        )?;
        let record =
            files::staging(&tx, job, generation)?.ok_or(StoreError::NotFound("staging file"))?;
        crash_point!("convert-before-commit");
        tx.commit()?;
        Ok(record)
    }

    /// Forgets the `aux_*` file `aux` of a staging file, once it is
    /// durably removed or gone: a converting record goes back to no
    /// conversion, a converted one stays converted.
    pub(crate) fn forget_conversion_aux(&mut self, aux: &StagingRecord) -> Result<(), StoreError> {
        let tx = self.write()?;
        let changed = tx.execute(
            "UPDATE staging_files SET aux_name = NULL, aux_device = NULL, aux_inode = NULL, \
             aux_birth_ns = NULL, \
             media_phase = CASE media_phase WHEN 'converting' THEN NULL ELSE media_phase END \
             WHERE job_id = ?1 AND generation = ?2 AND root_id = ?3 AND aux_name = ?4 \
             AND aux_device = ?5 AND aux_inode = ?6",
            params![
                sql(aux.job)?,
                sql(aux.generation)?,
                sql(aux.root)?,
                aux.name,
                aux.identity.device.cast_signed(),
                aux.identity.inode.cast_signed(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Staging("no such media file to forget"));
        }
        tx.commit()?;
        Ok(())
    }
}

/// The `aux_*` files of `job`'s staging records, for explicit deletion.
pub(super) fn aux_files(
    tx: &Transaction<'_>,
    job: JobId,
) -> Result<Vec<StagingRecord>, StoreError> {
    let records = tx
        .prepare(
            "SELECT job_id, generation, root_id, aux_name, aux_device, aux_inode, aux_birth_ns \
             FROM staging_files WHERE job_id = ?1 AND aux_name IS NOT NULL",
        )?
        .query_map([sql(job)?], |row| {
            Ok((
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    records
        .into_iter()
        .map(|(generation, root, name, device, inode, birth_ns)| {
            Ok(StagingRecord {
                root: key(root)?,
                job,
                generation: key(generation)?,
                name,
                identity: FileIdentity {
                    device: device.cast_unsigned(),
                    inode: inode.cast_unsigned(),
                    birth_ns,
                },
            })
        })
        .collect()
}

/// Checks listed media links whose check failed again.
pub(super) fn retry_failed(tx: &Transaction<'_>, now: UtcTimestamp) -> Result<usize, StoreError> {
    let failed: Vec<(i64, String, String)> = tx
        .prepare(
            "SELECT id, input_url, media_service FROM source_links \
             WHERE media_state = 'failed' AND listed = 1",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (id, url, service) in &failed {
        let url = Url::parse(url).or_corrupt("source URL")?;
        super::crawl::enqueue(tx, *id, &url, Some(service), now)?;
    }
    Ok(failed.len())
}
