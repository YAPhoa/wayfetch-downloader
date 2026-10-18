//! Collected links: entries kept for review until the user starts or
//! removes them (architecture brief sections 3.2 and 7.1).
//!
//! Collecting records a batch whose links are held: the ingestion step
//! skips them, so collecting never creates a job or fetches anything.
//! Starting a link creates its job and releases the link in one
//! transaction, and a link has at most one job, so a retried, repeated, or
//! concurrent start never adds a second download; accepted links leave
//! the collection while their source records remain. Each command
//! applies to the links it can and refuses the rest with a reason; refused
//! links stay in the list as they were.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension, Transaction, params, params_from_iter};
use url::Url;
use wayfetch_domain::id::{BatchId, JobId, RootId, SourceLinkId};
use wayfetch_domain::name::{DiagnosticText, HostName};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;
use wayfetch_protocol::command::{
    AppliedLink, CollectItems, InputUrl, LinkRef, LinkRefs, MAX_COLLECTED_LINKS, RefusedLink,
};
use wayfetch_protocol::event::{EventCursor, EventKind};
use wayfetch_protocol::query::{CollectedLink, JobRoute, LinkCheck, LinkPage, PageSize};
use wayfetch_protocol::{CommandResult, ErrorCode, ProtocolError};

use super::{Store, StoreError, jobs, key, outbox, sql};

/// The most characters of an invalid entry a list shows.
const SHOWN_TEXT_CHARS: usize = 200;

/// What a destination that cannot be used for starting links says about
/// it, by root.
pub type Unusable = BTreeMap<RootId, String>;

/// One entry, checked.
enum Entry {
    Valid(InputUrl),
    Invalid { text: String, problem: String },
}

impl Entry {
    fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        match InputUrl::parse(raw) {
            Ok(url) => Self::Valid(url),
            Err(e) => Self::Invalid {
                text: without_credentials(raw),
                problem: e.message.as_str().to_owned(),
            },
        }
    }
}

/// `text` without the credentials a link in it may carry, which are never
/// stored: from a parsed URL, and from text that only looks like one.
fn without_credentials(text: &str) -> String {
    if let Ok(mut url) = Url::parse(text) {
        if !url.username().is_empty() || url.password().is_some() {
            // Only a URL with a host has credentials, and those can be
            // cleared.
            let _ = url.set_username("");
            let _ = url.set_password(None);
            return url.into();
        }
        return text.to_owned();
    }
    if let Some(start) = text.find("://").map(|i| i + 3) {
        let rest = &text[start..];
        let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        if let Some(at) = authority.rfind('@') {
            return format!("{}{}", &text[..start], &rest[at + 1..]);
        }
    }
    text.to_owned()
}

/// An invalid entry as a list shows it: without its query or fragment,
/// which can carry a token, and shortened.
fn shown_text(text: &str) -> String {
    let end = text.find(['?', '#']).unwrap_or(text.len());
    let mut shown: String = text[..end].chars().take(SHOWN_TEXT_CHARS).collect();
    if shown.len() < text.len() {
        shown.push('…');
    }
    shown
}

/// Records `items` as a new batch of held links saved in `destination`
/// (the operator's download root if `None`), with a `batch_accepted`
/// event. Refused whole if the list would hold more than
/// [`MAX_COLLECTED_LINKS`].
pub(super) fn collect(
    tx: &Transaction<'_>,
    items: &CollectItems,
    destination: Option<RootId>,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let listed: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM source_links WHERE listed = 1",
            [],
            |r| r.get(0),
        )
        .map_err(StoreError::from)?;
    let listed = usize::try_from(listed).map_err(|_| StoreError::Corrupt("link count"))?;
    if listed + items.as_slice().len() > MAX_COLLECTED_LINKS {
        return Err(ProtocolError::new(
            ErrorCode::LimitExceeded,
            &format!("the Links list holds at most {MAX_COLLECTED_LINKS} links; remove some first"),
        ));
    }
    let entries: Vec<Entry> = items.as_slice().iter().map(|i| Entry::parse(i)).collect();
    // Name the package after the first valid link's host.
    let name = entries
        .iter()
        .find_map(|e| match e {
            Entry::Valid(url) => url.as_url().host_str(),
            Entry::Invalid { .. } => None,
        })
        .unwrap_or("Links");
    let root = destination.map(sql).transpose()?;
    tx.execute(
        "INSERT INTO packages (name, created_at) VALUES (?1, ?2)",
        params![name, now.unix_millis()],
    )
    .map_err(StoreError::from)?;
    let package = tx.last_insert_rowid();
    let count = u32::try_from(entries.len()).map_err(|_| StoreError::Corrupt("input count"))?;
    tx.execute(
        "INSERT INTO input_batches (package_id, input_count, root_id, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![package, count, root, now.unix_millis()],
    )
    .map_err(StoreError::from)?;
    let batch: BatchId = key(tx.last_insert_rowid())?;
    let mut insert = tx
        .prepare(
            "INSERT INTO source_links (batch_id, ordinal, input_url, held, listed, root_id, \
             invalid, problem) VALUES (?1, ?2, ?3, 1, 1, ?4, ?5, ?6)",
        )
        .map_err(StoreError::from)?;
    let mut invalid = 0u32;
    for (ordinal, entry) in entries.iter().enumerate() {
        let ordinal = i64::try_from(ordinal).map_err(|_| StoreError::Corrupt("ordinal"))?;
        let (text, problem) = match entry {
            Entry::Valid(url) => (url.as_url().as_str(), None),
            Entry::Invalid { text, problem } => {
                invalid += 1;
                (text.as_str(), Some(DiagnosticText::new(problem)))
            }
        };
        insert
            .execute(params![
                sql(batch)?,
                ordinal,
                text,
                root,
                problem.is_some(),
                problem.as_ref().map(DiagnosticText::as_str),
            ])
            .map_err(StoreError::from)?;
    }
    outbox::append(
        tx,
        &EventKind::BatchAccepted {
            batch,
            accepted: count,
        },
        now,
    )?;
    Ok(CommandResult::LinksCollected {
        batch,
        collected: count - invalid,
        invalid,
    })
}

/// A collected link's row, as commands see it.
struct Held {
    batch: i64,
    package: i64,
    listed: bool,
    invalid: bool,
    revision: Revision,
    root: Option<RootId>,
    problem: Option<String>,
    job: Option<JobId>,
}

fn held(tx: &Transaction<'_>, link: SourceLinkId) -> Result<Option<Held>, StoreError> {
    // An ID beyond the stored range cannot exist.
    let Ok(id) = sql(link) else { return Ok(None) };
    let raw = tx
        .query_row(
            "SELECT s.batch_id, b.package_id, s.listed, s.invalid, s.revision, s.root_id, \
             s.problem, j.id FROM source_links s JOIN input_batches b ON b.id = s.batch_id \
             LEFT JOIN jobs j ON j.source_link_id = s.id WHERE s.id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, bool>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<i64>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(batch, package, listed, invalid, revision, root, problem, job)| {
            Ok(Held {
                batch,
                package,
                listed,
                invalid,
                revision: key(revision)?,
                root: root.map(key).transpose()?,
                problem,
                job: job.map(key).transpose()?,
            })
        },
    )
    .transpose()
}

/// The outcome of one link: applied, or refused with a code and message.
type Outcome = Result<AppliedLink, (ErrorCode, String)>;

/// Applies `each` to every link, collecting what was applied and refused.
fn apply_each(
    tx: &Transaction<'_>,
    links: &LinkRefs,
    mut each: impl FnMut(&LinkRef, Option<Held>) -> Result<Outcome, ProtocolError>,
) -> Result<CommandResult, ProtocolError> {
    let (mut applied, mut refused) = (Vec::new(), Vec::new());
    for link in links.as_slice() {
        match each(link, held(tx, link.link)?)? {
            Ok(done) => applied.push(done),
            Err((code, message)) => refused.push(RefusedLink {
                link: link.link,
                code,
                message: DiagnosticText::new(&message),
            }),
        }
    }
    Ok(CommandResult::LinksApplied { applied, refused })
}

const REMOVED: &str = "This link is no longer in the Links list.";
const CHANGED: &str = "This link changed since the list was shown. Check it and try again.";
const STARTED: &str = "This link was already started; it is in Downloads.";

/// Refusals that apply to every command: a link no longer in the list (or
/// never in it, submitted to start at once), one already started unless
/// `started` may be acted on, or one that changed since the client saw it.
/// `None` if it may go on.
fn refusal(link: &LinkRef, row: &Held, started: bool) -> Option<(ErrorCode, String)> {
    if !row.listed {
        return Some((ErrorCode::NotFound, REMOVED.to_owned()));
    }
    if row.job.is_some() && !started {
        return Some((ErrorCode::NotApplicable, STARTED.to_owned()));
    }
    (row.revision != link.revision).then(|| (ErrorCode::RevisionConflict, CHANGED.to_owned()))
}

/// Starts each held link whose destination is usable as a queued job
/// downloaded through `route` (directly if `None`), in one transaction. A
/// link started before reports its job again, on the route it was started
/// on. The whole command is refused, and every link stays, if the route's
/// account is gone, disabled, or of another service.
pub(super) fn start(
    tx: &Transaction<'_>,
    links: &LinkRefs,
    unusable: &Unusable,
    route: Option<&JobRoute>,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    if let Some(route) = route {
        let account: Option<(String, bool)> = tx
            .query_row(
                "SELECT service, enabled FROM accounts WHERE id = ?1",
                [sql(route.account)?],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        match account {
            None => {
                return Err(ProtocolError::new(
                    ErrorCode::NotFound,
                    "the chosen account was removed; choose another way to download",
                ));
            }
            Some((service, _)) if service != route.service.as_str() => {
                return Err(ProtocolError::new(
                    ErrorCode::InvalidRequest,
                    "the chosen account belongs to another service",
                ));
            }
            Some((_, false)) => {
                return Err(ProtocolError::new(
                    ErrorCode::NotApplicable,
                    "the chosen account is disabled; enable it or choose another way to download",
                ));
            }
            Some(_) => {}
        }
    }
    apply_each(tx, links, |link, row| {
        let Some(row) = row else {
            return Ok(Err((ErrorCode::NotFound, REMOVED.to_owned())));
        };
        if let Some(job) = row.job {
            return Ok(Ok(AppliedLink {
                link: link.link,
                job: Some(job),
                revision: None,
            }));
        }
        if let Some(refused) = refusal(link, &row, false) {
            return Ok(Err(refused));
        }
        if row.invalid {
            let why = row.problem.as_deref().unwrap_or("not a valid link");
            return Ok(Err((
                ErrorCode::NotApplicable,
                format!(
                    "Wayfetch cannot download this: {why}. Remove it, or collect a corrected link."
                ),
            )));
        }
        if let Some(why) = row.root.and_then(|r| unusable.get(&r)) {
            let problem = format!("Its folder cannot be used: {why}. Choose another destination.");
            tx.execute(
                "UPDATE source_links SET problem = ?1 WHERE id = ?2",
                params![DiagnosticText::new(&problem).as_str(), sql(link.link)?],
            )
            .map_err(StoreError::from)?;
            return Ok(Err((ErrorCode::NotApplicable, problem)));
        }
        let job = jobs::create(
            tx,
            sql(link.link)?,
            row.package,
            row.root.map(sql).transpose()?,
            route,
            now,
        )?;
        tx.execute(
            "UPDATE source_links SET listed = 0 WHERE id = ?1",
            [sql(link.link)?],
        )
        .map_err(StoreError::from)?;
        Ok(Ok(AppliedLink {
            link: link.link,
            job: Some(job),
            revision: None,
        }))
    })
}

/// Removes links from the list. A link not started goes, with batches and
/// packages left with nothing; a started one only leaves the list, since
/// its download keeps its source. A link already gone counts as removed.
pub(super) fn remove(
    tx: &Transaction<'_>,
    links: &LinkRefs,
) -> Result<CommandResult, ProtocolError> {
    let mut batches = BTreeSet::new();
    let result = apply_each(tx, links, |link, row| {
        let applied = AppliedLink {
            link: link.link,
            job: None,
            revision: None,
        };
        let Some(row) = row else {
            return Ok(Ok(applied));
        };
        if !row.listed {
            return Ok(Ok(applied));
        }
        if let Some(refused) = refusal(link, &row, true) {
            return Ok(Err(refused));
        }
        if row.job.is_some() {
            tx.execute(
                "UPDATE source_links SET listed = 0 WHERE id = ?1",
                [sql(link.link)?],
            )
            .map_err(StoreError::from)?;
        } else {
            tx.execute("DELETE FROM source_links WHERE id = ?1", [sql(link.link)?])
                .map_err(StoreError::from)?;
            batches.insert(row.batch);
        }
        Ok(Ok(applied))
    })?;
    for batch in batches {
        let package: Option<i64> = tx
            .query_row(
                "DELETE FROM input_batches WHERE id = ?1 \
                 AND NOT EXISTS (SELECT 1 FROM source_links WHERE batch_id = ?1) \
                 RETURNING package_id",
                [batch],
                |r| r.get(0),
            )
            .optional()
            .map_err(StoreError::from)?;
        if let Some(package) = package {
            tx.execute(
                "DELETE FROM packages WHERE id = ?1 \
                 AND NOT EXISTS (SELECT 1 FROM input_batches WHERE package_id = ?1) \
                 AND NOT EXISTS (SELECT 1 FROM jobs WHERE package_id = ?1)",
                [package],
            )
            .map_err(StoreError::from)?;
        }
    }
    Ok(result)
}

/// Saves links not started yet in `destination` from now on. Other links, jobs, and
/// the default destination keep theirs.
pub(super) fn relocate(
    tx: &Transaction<'_>,
    links: &LinkRefs,
    destination: RootId,
) -> Result<CommandResult, ProtocolError> {
    apply_each(tx, links, |link, row| {
        let Some(row) = row else {
            return Ok(Err((ErrorCode::NotFound, REMOVED.to_owned())));
        };
        if let Some(refused) = refusal(link, &row, false) {
            return Ok(Err(refused));
        }
        let mut revision = row.revision;
        if row.root != Some(destination) {
            revision = revision
                .next()
                .map_err(|_| StoreError::Corrupt("link revision"))?;
            tx.execute(
                "UPDATE source_links SET root_id = ?1, revision = ?2, problem = \
                 CASE WHEN invalid = 1 THEN problem END WHERE id = ?3",
                params![sql(destination)?, sql(revision)?, sql(link.link)?],
            )
            .map_err(StoreError::from)?;
        }
        Ok(Ok(AppliedLink {
            link: link.link,
            job: None,
            revision: Some(revision),
        }))
    })
}

/// A page of listed links after `after`, oldest first.
pub(super) fn list(
    tx: &Transaction<'_>,
    after: Option<SourceLinkId>,
    page_size: PageSize,
    as_of: EventCursor,
) -> Result<LinkPage, StoreError> {
    let after = match after {
        // Past the stored range: nothing follows.
        Some(id) => sql(id).unwrap_or(i64::MAX),
        None => 0,
    };
    let size = usize::from(page_size.get());
    let mut items = tx
        .prepare(
            "SELECT s.id, s.batch_id, b.created_at, s.revision, s.invalid, s.input_url, \
             s.root_id, s.problem, r.path, j.id FROM source_links s \
             JOIN input_batches b ON b.id = s.batch_id \
             LEFT JOIN roots r ON r.id = s.root_id \
             LEFT JOIN jobs j ON j.source_link_id = s.id \
             WHERE s.listed = 1 AND s.id > ?1 ORDER BY s.id LIMIT ?2",
        )?
        .query_map(
            params_from_iter([after, i64::from(page_size.get()) + 1]),
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<i64>>(9)?,
                ))
            },
        )?
        .map(|row| {
            let (id, batch, created, revision, invalid, text, root, problem, path, job) = row?;
            let url = (!invalid).then(|| Url::parse(&text).ok()).flatten();
            if !invalid && url.is_none() {
                return Err(StoreError::Corrupt("collected link"));
            }
            Ok(CollectedLink {
                id: key(id)?,
                batch: key(batch)?,
                collected_at: UtcTimestamp::from_unix_millis(created),
                revision: key(revision)?,
                check: if invalid {
                    LinkCheck::Invalid
                } else {
                    LinkCheck::NotChecked
                },
                host: url.as_ref().and_then(|u| HostName::new(u.host_str()?).ok()),
                name_hint: url
                    .as_ref()
                    .and_then(crate::download::url_file_name)
                    .map(|name| name.as_str().to_owned()),
                text: invalid.then(|| shown_text(&text)),
                destination: root.map(key).transpose()?,
                destination_path: path,
                job: job.map(key).transpose()?,
                problem: problem.as_deref().map(DiagnosticText::new),
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let next_after = if items.len() > size {
        items.truncate(size);
        items.last().map(|l| l.id)
    } else {
        None
    };
    let total: i64 = tx.query_row(
        "SELECT COUNT(*) FROM source_links WHERE listed = 1",
        [],
        |r| r.get(0),
    )?;
    Ok(LinkPage {
        items,
        next_after,
        total: u64::try_from(total).map_err(|_| StoreError::Corrupt("link count"))?,
        as_of,
    })
}

/// A listed link exactly as it was collected.
pub(super) fn source(tx: &Transaction<'_>, link: SourceLinkId) -> Result<String, StoreError> {
    let Ok(id) = sql(link) else {
        return Err(StoreError::NotFound("link"));
    };
    tx.query_row(
        "SELECT input_url FROM source_links WHERE id = ?1 AND listed = 1",
        [id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(StoreError::NotFound("link"))
}

impl Store {
    /// The destinations of those of `links` that a start could create a
    /// download for, so the coordinator can check them first.
    pub fn link_roots(&mut self, links: &[SourceLinkId]) -> Result<Vec<RootId>, StoreError> {
        let tx = self.read()?;
        let ids = links
            .iter()
            .filter_map(|l| sql(*l).ok())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let roots = tx
            .prepare(&format!(
                "SELECT DISTINCT root_id FROM source_links WHERE held = 1 AND invalid = 0 \
                 AND root_id IS NOT NULL AND id IN ({})",
                vec!["?"; ids.len()].join(", ")
            ))?
            .query_map(params_from_iter(ids), |r| r.get::<_, i64>(0))?
            .map(|r| key(r?))
            .collect::<Result<_, _>>()?;
        Ok(roots)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_never_kept() {
        assert_eq!(
            without_credentials("https://user:pw@files.example/a?t=1"),
            "https://files.example/a?t=1"
        );
        assert_eq!(
            without_credentials("ftp://user:pw@files.example/a"),
            "ftp://files.example/a"
        );
        assert_eq!(without_credentials("htps//x y@z"), "htps//x y@z");
        assert_eq!(
            without_credentials("https://us er:p w@files .example/a"),
            "https://files .example/a"
        );
        assert_eq!(without_credentials("not a link"), "not a link");
    }

    #[test]
    fn shown_text_hides_queries_and_is_bounded() {
        assert_eq!(
            shown_text("htp://a.example/x?token=1"),
            "htp://a.example/x…"
        );
        assert_eq!(shown_text("plain"), "plain");
        let long = "x".repeat(SHOWN_TEXT_CHARS + 5);
        assert_eq!(shown_text(&long).chars().count(), SHOWN_TEXT_CHARS + 1);
    }
}
