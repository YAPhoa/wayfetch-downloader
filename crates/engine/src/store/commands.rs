//! Durable, idempotent client commands.
//!
//! A command commits with its events and idempotency record, and a retry
//! with the same key replays the stored result. A failed command leaves no
//! record, so its key can be retried.

use rusqlite::{OptionalExtension, Transaction, params};
use wayfetch_domain::id::{JobId, PackageId, RootId, SourceLinkId};
use wayfetch_domain::job::{JobError, JobState, Outcome};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;
use wayfetch_protocol::command::{CommandFingerprint, IdempotencyRecord, Replay, SubmitItems};
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::query::JobRoute;
use wayfetch_protocol::{
    Command, CommandEnvelope, CommandResult, ErrorCode, InstanceId, ProtocolError,
};

use super::links::{self, Unusable};
use super::operations::{self, OperationState};
use super::{
    OrCorrupt, Store, StoreError, accounts, deletions, extractions, files, jobs, key, outbox,
    renames, roots, sql,
};

/// What the coordinator knows about destinations when a command runs.
#[derive(Debug, Clone, Default)]
pub struct CommandContext {
    /// The default root at the time. Without one, a batch belongs to the
    /// operator's download root, which was not identified yet, and is saved
    /// there once it is.
    pub destination: Option<RootId>,
    /// The operator's download root, once identified: jobs submitted
    /// without a destination belong to it.
    pub download_root: Option<RootId>,
    /// Destinations of links being started that cannot be used, and why.
    pub unusable: Unusable,
    /// The provider account links being started download through, as the
    /// coordinator found it; `None` to download them directly.
    pub route: Option<JobRoute>,
    /// Per-link automatic decisions; failures keep that link collected.
    pub routes: std::collections::BTreeMap<SourceLinkId, Result<Option<JobRoute>, ProtocolError>>,
    /// For a collect: the links the coordinator found in its text.
    pub collect: links::CollectPlan,
}

impl Store {
    /// Applies a client command durably. The host has already checked the
    /// caller's permission, and the coordinator that a settings change or
    /// a destination chosen for links names a permitted, available root.
    /// A submitted or collected batch without a chosen destination is
    /// saved in `context.destination`.
    pub fn execute(
        &mut self,
        envelope: &CommandEnvelope,
        instance: InstanceId,
        context: &CommandContext,
        now: UtcTimestamp,
    ) -> Result<CommandResult, ProtocolError> {
        let converter = self.media_converter;
        let tx = self.write()?;
        if let Some(result) = replayed(&tx, envelope, instance)? {
            return Ok(result);
        }
        let destination = context.destination;
        let result = match &envelope.command {
            Command::Submit { items, package } => submit(&tx, items, *package, destination, now)?,
            Command::CollectLinks {
                mode,
                destination: chosen,
                ..
            } => links::collect(&tx, &context.collect, *mode, chosen.or(destination), now)?,
            Command::StartLinks {
                links,
                account,
                routing,
            } => {
                let automatic = account.is_none()
                    && *routing != Some(wayfetch_protocol::command::StartRouting::Direct);
                links::start(
                    &tx,
                    links,
                    &context.unusable,
                    context.route.as_ref(),
                    &context.routes,
                    automatic,
                    now,
                )?
            }
            Command::RemoveLinks { links } => links::remove(&tx, links)?,
            Command::CheckLinks { links } => links::check(&tx, links, false, now)?,
            Command::AnalyzeLinks { links } => links::check(&tx, links, true, now)?,
            Command::MoveLinks { links, destination } => links::relocate(&tx, links, *destination)?,
            Command::ChooseLinkMedia { links, choice } => {
                links::choose_media(&tx, links, *choice, converter)?
            }
            Command::Pause {
                job,
                expected_revision,
            } => request(&tx, *job, now, |s| s.request_pause(*expected_revision))?,
            Command::Resume {
                job,
                expected_revision,
            } => request(&tx, *job, now, |s| s.request_resume(*expected_revision))?,
            Command::Cancel {
                job,
                expected_revision,
            } => request(&tx, *job, now, |s| s.request_cancel(*expected_revision))?,
            Command::Retry {
                job,
                expected_revision,
                repeat_provider_operation,
            } => retry(
                &tx,
                *job,
                *expected_revision,
                *repeat_provider_operation,
                now,
            )?,
            Command::UpdateSettings {
                expected_revision,
                default_root,
                max_active_downloads,
                download_speed_limit,
                extract_archives,
                automatic_update_checks,
            } => roots::update_settings(
                &tx,
                *expected_revision,
                *default_root,
                *max_active_downloads,
                *download_speed_limit,
                *extract_archives,
                *automatic_update_checks,
                now,
            )?,
            Command::Extract {
                job,
                expected_revision,
                expected_generation,
            } => extractions::extract(&tx, *job, *expected_revision, *expected_generation, now)?,
            Command::CancelExtraction {
                job,
                expected_revision,
                expected_generation,
            } => extractions::cancel(&tx, *job, *expected_revision, *expected_generation, now)?,
            Command::Remove {
                job,
                expected_revision,
            } => remove(&tx, *job, *expected_revision, now)?,
            Command::RemoveRoot {
                expected_revision,
                root,
            } => roots::remove_root(
                &tx,
                *expected_revision,
                *root,
                destination,
                context.download_root,
                now,
            )?,
            Command::SetAccountEnabled {
                account,
                expected_revision,
                enabled,
            } => accounts::set_enabled(&tx, *account, *expected_revision, *enabled)?,
            Command::RemoveAccount {
                account,
                expected_revision,
            } => accounts::remove(&tx, *account, *expected_revision)?,
            // File mutations run between the store’s intent and completion steps.
            Command::Rename { .. } | Command::DeleteFiles { .. } => {
                return Err(ProtocolError::new(
                    ErrorCode::Internal,
                    "file mutations run through their intent and completion methods",
                ));
            }
            // The host changes plugins outside the store.
            Command::ChangePlugin { .. } => {
                return Err(ProtocolError::new(
                    ErrorCode::Internal,
                    "a plugin change is recorded through Store::record_plugin_change",
                ));
            }
            // The account is stored once the person approved it.
            Command::AuthorizeAccount { .. } => {
                return Err(ProtocolError::new(
                    ErrorCode::Internal,
                    "an authorization is stored through Store::add_account",
                ));
            }
        };
        record(&tx, envelope, &result, now)?;
        tx.commit().map_err(StoreError::from)?;
        Ok(result)
    }
}

/// The stored result of a command already accepted under the envelope's
/// key, after checking that the envelope is for this instance; `None` for
/// a new command.
pub(super) fn replayed(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    instance: InstanceId,
) -> Result<Option<CommandResult>, ProtocolError> {
    if envelope.target != instance {
        return Err(ProtocolError::new(
            ErrorCode::WrongInstance,
            "command targets another engine instance",
        ));
    }
    let key = envelope.idempotency_key.as_uuid().to_string();
    if let Some(fingerprint) = deletions::holding(tx, &key)? {
        if fingerprint != envelope.command.fingerprint().as_bytes() {
            return Err(ProtocolError::new(
                ErrorCode::IdempotencyConflict,
                "idempotency key was used for a different command",
            ));
        }
        return Err(StoreError::Busy("data deletion is unfinished; retry deletion").into());
    }
    // An unfinished rename records its result under its key when it ends.
    if let Some(fingerprint) = renames::holding(tx, &key)? {
        if fingerprint != envelope.command.fingerprint().as_bytes() {
            return Err(ProtocolError::new(
                ErrorCode::IdempotencyConflict,
                "idempotency key was used for a different command",
            ));
        }
        return Err(StoreError::Busy("the file is being renamed; try again shortly").into());
    }
    let Some(record) = stored(tx, &key)? else {
        return Ok(None);
    };
    match record.replay(&envelope.command) {
        Replay::Stored(result) => Ok(Some(result.clone())),
        Replay::Conflict => Err(ProtocolError::new(
            ErrorCode::IdempotencyConflict,
            "idempotency key was used for a different command",
        )),
    }
}

/// Records the result of the envelope's command under its key.
pub(super) fn record(
    tx: &Transaction<'_>,
    envelope: &CommandEnvelope,
    result: &CommandResult,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    record_as(
        tx,
        &envelope.idempotency_key.as_uuid().to_string(),
        envelope.command.fingerprint().as_bytes(),
        result,
        now,
    )
}

/// Records `result` under an idempotency key and command fingerprint.
pub(super) fn record_as(
    tx: &Transaction<'_>,
    idempotency_key: &str,
    fingerprint: &[u8],
    result: &CommandResult,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    let result = serde_json::to_string(result).or_corrupt("command result")?;
    tx.execute(
        "INSERT INTO commands (idempotency_key, fingerprint, result, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![idempotency_key, fingerprint, result, now.unix_millis()],
    )?;
    Ok(())
}

fn stored(tx: &Transaction<'_>, key: &str) -> Result<Option<IdempotencyRecord>, StoreError> {
    let Some((fingerprint, result)) = tx
        .query_row(
            "SELECT fingerprint, result FROM commands WHERE idempotency_key = ?1",
            [key],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let fingerprint: [u8; 32] = fingerprint.try_into().or_corrupt("command fingerprint")?;
    Ok(Some(IdempotencyRecord {
        fingerprint: CommandFingerprint::from_bytes(fingerprint),
        result: serde_json::from_str(&result).or_corrupt("command result")?,
    }))
}

/// Records a batch and its links for the ingestion workflow. Jobs are
/// created by [`Store::stage_pending`], not here: the result is the durable
/// batch.
fn submit(
    tx: &Transaction<'_>,
    items: &SubmitItems,
    package: Option<PackageId>,
    destination: Option<RootId>,
    now: UtcTimestamp,
) -> Result<CommandResult, StoreError> {
    let package = match package {
        Some(id) => {
            let id = sql(id).map_err(|_| StoreError::NotFound("package"))?;
            tx.query_row("SELECT id FROM packages WHERE id = ?1", [id], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .ok_or(StoreError::NotFound("package"))?
        }
        None => {
            // Name a new package after the first input's host.
            let name = items.as_slice()[0].as_url().host_str().unwrap_or("Package");
            tx.execute(
                "INSERT INTO packages (name, created_at) VALUES (?1, ?2)",
                params![name, now.unix_millis()],
            )?;
            tx.last_insert_rowid()
        }
    };
    let accepted = u32::try_from(items.as_slice().len()).or_corrupt("input count")?;
    tx.execute(
        "INSERT INTO input_batches (package_id, input_count, root_id, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![
            package,
            accepted,
            destination.map(sql).transpose()?,
            now.unix_millis()
        ],
    )?;
    let batch = key(tx.last_insert_rowid())?;
    let mut insert =
        tx.prepare("INSERT INTO source_links (batch_id, ordinal, input_url) VALUES (?1, ?2, ?3)")?;
    for (ordinal, item) in items.as_slice().iter().enumerate() {
        insert.execute(params![
            sql(batch)?,
            i64::try_from(ordinal).or_corrupt("ordinal")?,
            item.as_url().as_str()
        ])?;
    }
    outbox::append(tx, &EventKind::BatchAccepted { batch, accepted }, now)?;
    Ok(CommandResult::Submitted { batch, accepted })
}

/// Applies a revision-checked user request to one job.
fn request(
    tx: &Transaction<'_>,
    id: JobId,
    now: UtcTimestamp,
    apply: impl FnOnce(&mut JobState) -> Result<Outcome, JobError>,
) -> Result<CommandResult, StoreError> {
    deletions::check_idle(tx, id)?;
    let mut job = jobs::load_listed(tx, id)?;
    let previous = job.state;
    let outcome = apply(&mut job.state)?;
    if outcome == Outcome::Changed {
        let failure = job.failure.take();
        jobs::save(tx, &mut job, &previous, failure, now)?;
    }
    Ok(CommandResult::JobUpdated {
        job: id,
        revision: job.state.revision(),
        changed: outcome == Outcome::Changed,
    })
}

/// Starts a failed or stopped cancelled job again under a new generation,
/// reserving its newest staging file so the attempt continues its saved
/// data. A retry deadline still ahead is refused, and so is a provider
/// operation with an unknown outcome unless the user confirmed repeating
/// it (`repeat`).
fn retry(
    tx: &Transaction<'_>,
    id: JobId,
    expected: Revision,
    repeat: bool,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    deletions::check_idle(tx, id)?;
    let mut job = jobs::load_listed(tx, id)?;
    let previous = job.state;
    // Revision and stage first: a stale or inapplicable request says so.
    let mut next = job.state;
    next.request_retry(expected).map_err(StoreError::from)?;
    if let Some(at) = job.failure.as_ref().and_then(|f| f.retry_at)
        && at > now
    {
        return Err(ProtocolError {
            retry_at: Some(at),
            ..ProtocolError::new(
                ErrorCode::NotApplicable,
                "the source asked to wait before trying again; retry after its deadline",
            )
        });
    }
    let unknown = operations::open(tx, id)?.is_some_and(|op| {
        matches!(
            op.state,
            OperationState::Intended | OperationState::Ambiguous
        )
    });
    if unknown && !repeat {
        return Err(ProtocolError::new(
            ErrorCode::NotApplicable,
            "the provider may already have generated this link, but its answer was lost; \
             retrying asks it again, which may repeat that work at the provider, so confirm \
             the retry to go on",
        ));
    }
    if unknown {
        operations::confirm(tx, id)?;
    }
    job.state = next;
    files::reserve_for_retry(tx, id, job.state.generation())?;
    jobs::set_restart_reason(tx, &mut job, None)?;
    jobs::save(tx, &mut job, &previous, None, now)?;
    Ok(CommandResult::JobUpdated {
        job: id,
        revision: job.state.revision(),
        changed: true,
    })
}

/// Removes a finished, failed, or cancelled job from lists and counts, with
/// a `job_removed` event. Its files and records stay.
pub(super) fn remove(
    tx: &Transaction<'_>,
    id: JobId,
    expected: Revision,
    now: UtcTimestamp,
) -> Result<CommandResult, StoreError> {
    let job = jobs::load_listed(tx, id)?;
    job.state.check_remove(expected)?;
    if renames::pending(tx, id)? {
        return Err(StoreError::Busy(
            "the job's file is being renamed; try again once that finishes",
        ));
    }
    // List-only removal explicitly abandons an unfinished destructive intent.
    tx.execute("DELETE FROM deletions WHERE job_id = ?1", [sql(id)?])?;
    tx.execute(
        "UPDATE jobs SET removed_at = ?1, updated_at = ?1 WHERE id = ?2",
        params![now.unix_millis(), sql(id)?],
    )?;
    outbox::append(tx, &EventKind::JobRemoved { job: id }, now)?;
    Ok(CommandResult::JobUpdated {
        job: id,
        revision: job.state.revision(),
        changed: true,
    })
}
