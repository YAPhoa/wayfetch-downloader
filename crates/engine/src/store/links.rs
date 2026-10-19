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
use wayfetch_domain::name::{DiagnosticText, HostName, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_domain::version::Revision;
use wayfetch_protocol::command::{
    AppliedLink, CollectMode, LinkRef, LinkRefs, MAX_COLLECTED_LINKS, RefusedLink,
};
use wayfetch_protocol::event::{EventCursor, EventKind};
use wayfetch_protocol::media::MediaChoice;
use wayfetch_protocol::query::{CollectedLink, JobRoute, LinkCheck, LinkPage, PageSize};
use wayfetch_protocol::{CommandResult, ErrorCode, ProtocolError};

use super::{Store, StoreError, crawl, jobs, key, media, outbox, sql};
use crate::crawl::extract::Entry;
use crate::crawl::looks_like_file;

/// The most characters of an invalid entry a list shows.
const SHOWN_TEXT_CHARS: usize = 200;

/// What a destination that cannot be used for starting links says about
/// it, by root.
pub type Unusable = BTreeMap<RootId, String>;

/// An invalid entry as a list shows it: without its query or fragment,
/// which can carry a token, and shortened.
pub(super) fn shown_text(text: &str) -> String {
    let end = text.find(['?', '#']).unwrap_or(text.len());
    let mut shown: String = text[..end].chars().take(SHOWN_TEXT_CHARS).collect();
    if shown.len() < text.len() {
        shown.push('…');
    }
    shown
}

/// A link of a collect, as the coordinator found it in the text.
#[derive(Debug, Clone)]
pub struct Collecting {
    /// The link, or the entry that is not one.
    pub entry: Entry,
    /// The video service that recognizes it, if any.
    pub media: Option<ProviderId>,
}

/// What the coordinator found in a collect's text.
#[derive(Debug, Clone, Default)]
pub struct CollectPlan {
    /// The links to collect, in order, each once.
    pub links: Vec<Collecting>,
    /// Whether the text was HTML: its links are a page's content, which
    /// are never read as pages themselves.
    pub html: bool,
    /// Links found more than once.
    pub repeated: u32,
    /// Links in HTML that lead to other pages, left alone.
    pub skipped: u32,
}

/// Records `plan`'s links as a new batch of held links saved in
/// `destination` (the operator's download root if `None`), with a
/// `batch_accepted` event, and queues the checks `mode` asks for: in
/// [`CollectMode::Analyze`] every link's, each link typed as text allowed
/// to be read as a page; otherwise only those of links that look like
/// files or that a video service recognizes. Refused whole if the list
/// would hold more than [`MAX_COLLECTED_LINKS`].
pub(super) fn collect(
    tx: &Transaction<'_>,
    plan: &CollectPlan,
    mode: CollectMode,
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
    if listed + plan.links.len() > MAX_COLLECTED_LINKS {
        return Err(ProtocolError::new(
            ErrorCode::LimitExceeded,
            &format!("the Links list holds at most {MAX_COLLECTED_LINKS} links; remove some first"),
        ));
    }
    // Name the package after the first valid link's host.
    let name = plan
        .links
        .iter()
        .find_map(|l| match &l.entry {
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
    let count = u32::try_from(plan.links.len()).map_err(|_| StoreError::Corrupt("input count"))?;
    let analyze = mode == CollectMode::Analyze;
    tx.execute(
        "INSERT INTO input_batches (package_id, input_count, root_id, created_at, mode) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            package,
            count,
            root,
            now.unix_millis(),
            if analyze { "analyze" } else { "supported_only" }
        ],
    )
    .map_err(StoreError::from)?;
    let batch: BatchId = key(tx.last_insert_rowid())?;
    let mut insert = tx
        .prepare(
            "INSERT INTO source_links (batch_id, ordinal, input_url, held, listed, root_id, \
             invalid, problem, media_service, media_state, may_analyze) \
             VALUES (?1, ?2, ?3, 1, 1, ?4, ?5, ?6, ?7, CASE WHEN ?7 IS NULL THEN NULL \
             ELSE 'pending' END, ?8)",
        )
        .map_err(StoreError::from)?;
    let mut invalid = 0u32;
    for (position, link) in plan.links.iter().enumerate() {
        let ordinal = i64::try_from(position).map_err(|_| StoreError::Corrupt("ordinal"))?;
        let service = match link.entry {
            Entry::Valid(_) => link.media.as_ref().map(ProviderId::as_str),
            Entry::Invalid { .. } => None,
        };
        let (text, problem) = match &link.entry {
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
                service,
                analyze && !plan.html && problem.is_none() && service.is_none(),
            ])
            .map_err(StoreError::from)?;
        if let Entry::Valid(url) = &link.entry {
            let url = url.as_url();
            if analyze || service.is_some() || looks_like_file(url) {
                crawl::enqueue(tx, tx.last_insert_rowid(), url, service, now)?;
            }
        }
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
        repeated: plan.repeated,
        skipped: plan.skipped,
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
    media: media::LinkColumns,
    /// The link as collected.
    url: String,
    /// Whether links were found on it, which keeps its row.
    parent: bool,
}

fn held(tx: &Transaction<'_>, link: SourceLinkId) -> Result<Option<Held>, StoreError> {
    // An ID beyond the stored range cannot exist.
    let Ok(id) = sql(link) else { return Ok(None) };
    let raw = tx
        .query_row(
            &format!(
                "SELECT s.batch_id, b.package_id, s.listed, s.invalid, s.revision, s.root_id, \
                 s.problem, j.id, s.input_url, \
                 EXISTS(SELECT 1 FROM source_links c WHERE c.parent_id = s.id), \
                 {MEDIA_COLUMNS} FROM source_links s \
                 JOIN input_batches b ON b.id = s.batch_id \
                 LEFT JOIN jobs j ON j.source_link_id = s.id WHERE s.id = ?1"
            ),
            [id],
            |r| {
                Ok((
                    (
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, bool>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<i64>>(5)?,
                    ),
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, bool>(9)?,
                    media::LinkColumns::read(r, 10)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |((batch, package, listed, invalid, revision, root), problem, job, url, parent, media)| {
            Ok(Held {
                batch,
                package,
                listed,
                invalid,
                revision: key(revision)?,
                root: root.map(key).transpose()?,
                problem,
                job: job.map(key).transpose()?,
                media,
                url,
                parent,
            })
        },
    )
    .transpose()
}

/// A collected link's media columns, in [`media::LinkColumns::read`]'s
/// order.
const MEDIA_COLUMNS: &str =
    "s.media_service, s.media_state, s.media_info, s.media_problem, s.media_choice";

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
const PAGE: &str =
    "This link is a web page; the files found on it are listed beside it. Start those instead.";

/// Takes `link` off the list, keeping its record, and drops its check. A
/// page never started is no longer held, and marked staged so ingestion
/// never makes it a job; one started already is both.
fn unlist(tx: &Transaction<'_>, link: i64) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE source_links SET listed = 0, held = 0, staged = 1 WHERE id = ?1",
        [link],
    )?;
    crawl::drop_check(tx, link)
}

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
    routes: &BTreeMap<SourceLinkId, Result<Option<JobRoute>, ProtocolError>>,
    automatic: bool,
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
        if row.parent {
            return Ok(Err((ErrorCode::NotApplicable, PAGE.to_owned())));
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
        let route = match routes.get(&link.link) {
            Some(Ok(route)) => route.as_ref(),
            Some(Err(error)) => return Ok(Err((error.code, error.message.as_str().to_owned()))),
            None if automatic => {
                return Ok(Err((
                    ErrorCode::Busy,
                    "The automatic route is not ready; start again or choose Download via Direct."
                        .into(),
                )));
            }
            None => route,
        };
        // Automatic routing is revalidated in the same transaction as the
        // job. The coordinator also opened its current sealed credential.
        if automatic && let Some(route) = route {
            let usable: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1 AND service = ?2 \
                     AND enabled = 1 AND status = 'authenticated' \
                     AND (expires_at IS NULL OR expires_at > ?3))",
                    params![
                        sql(route.account)?,
                        route.service.as_str(),
                        now.unix_millis()
                    ],
                    |r| r.get(0),
                )
                .map_err(StoreError::from)?;
            if !usable {
                return Ok(Err((ErrorCode::NotApplicable,
                        "The automatically chosen account is no longer usable; check Accounts and start again.".into())));
            }
        }
        // A media link starts as a media download only when it was routed
        // automatically and no account took it.
        let media = match (&row.media.service, automatic, route) {
            (Some(service), true, None) => Some(jobs::NewMedia {
                service: ProviderId::new(service.clone())
                    .map_err(|_| StoreError::Corrupt("media service"))?,
                choice: media::decode_choice(row.media.choice.as_deref())?,
                title: media::decode_info(row.media.info.as_deref())?.map(|i| i.title),
            }),
            _ => None,
        };
        let job = jobs::create(
            tx,
            sql(link.link)?,
            row.package,
            row.root.map(sql).transpose()?,
            route,
            media.as_ref(),
            now,
        )?;
        unlist(tx, sql(link.link)?)?;
        Ok(Ok(AppliedLink {
            link: link.link,
            job: Some(job),
            revision: None,
        }))
    })
}

/// Removes links from the list. A link not started goes, with batches and
/// packages left with nothing; a started one, or a page with links found
/// on it, only leaves the list, since its download or those links keep it
/// as their source. A page kept only for that goes once its last found
/// link does. A link already gone counts as removed.
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
        if row.job.is_some() || row.parent {
            unlist(tx, sql(link.link)?)?;
        } else {
            delete_unused(tx, sql(link.link)?)?;
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

/// Deletes link `id`, which has no download and no found links, and then
/// the pages it was found on that are kept only for links found on them
/// and have none left.
fn delete_unused(tx: &Transaction<'_>, id: i64) -> Result<(), StoreError> {
    let mut next = Some(id);
    while let Some(id) = next {
        let parent: Option<i64> = tx.query_row(
            "DELETE FROM source_links WHERE id = ?1 RETURNING parent_id",
            [id],
            |r| r.get(0),
        )?;
        next = match parent {
            Some(parent) => tx
                .query_row(
                    "SELECT id FROM source_links s WHERE id = ?1 AND listed = 0 \
                     AND NOT EXISTS (SELECT 1 FROM jobs WHERE source_link_id = s.id) \
                     AND NOT EXISTS (SELECT 1 FROM source_links c WHERE c.parent_id = s.id)",
                    [parent],
                    |r| r.get(0),
                )
                .optional()?,
            None => None,
        };
    }
    Ok(())
}

/// Checks links again, from scratch; with `analyze`, a link that is a web
/// page is also read, and the files and videos on it collected beside it.
/// A link already started, or not a valid link, is refused.
pub(super) fn check(
    tx: &Transaction<'_>,
    links: &LinkRefs,
    analyze: bool,
    now: UtcTimestamp,
) -> Result<CommandResult, ProtocolError> {
    let result = apply_each(tx, links, |link, row| {
        let Some(row) = row else {
            return Ok(Err((ErrorCode::NotFound, REMOVED.to_owned())));
        };
        if let Some(refused) = refusal(link, &row, false) {
            return Ok(Err(refused));
        }
        if row.invalid {
            return Ok(Err((
                ErrorCode::NotApplicable,
                "This is not a link Wayfetch can check.".to_owned(),
            )));
        }
        let id = sql(link.link)?;
        if analyze && row.media.service.is_none() {
            tx.execute(
                "UPDATE source_links SET may_analyze = 1 WHERE id = ?1",
                [id],
            )
            .map_err(StoreError::from)?;
        }
        let url = Url::parse(&row.url).map_err(|_| StoreError::Corrupt("source URL"))?;
        crawl::enqueue(tx, id, &url, row.media.service.as_deref(), now)?;
        Ok(Ok(AppliedLink {
            link: link.link,
            job: None,
            revision: Some(row.revision),
        }))
    })?;
    if let CommandResult::LinksApplied { applied, .. } = &result
        && !applied.is_empty()
    {
        outbox::append(tx, &EventKind::LinksChanged, now)?;
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

/// Chooses what media links not started yet are downloaded as. A link
/// that is not a media link, was not checked yet, or does not offer the
/// choice now is refused.
pub(super) fn choose_media(
    tx: &Transaction<'_>,
    links: &LinkRefs,
    choice: MediaChoice,
    converter: bool,
) -> Result<CommandResult, ProtocolError> {
    apply_each(tx, links, |link, row| {
        let Some(row) = row else {
            return Ok(Err((ErrorCode::NotFound, REMOVED.to_owned())));
        };
        if let Some(refused) = refusal(link, &row, false) {
            return Ok(Err(refused));
        }
        if row.media.service.is_none() {
            return Ok(Err((
                ErrorCode::NotApplicable,
                "This is not a video link, so it has no formats to choose from.".to_owned(),
            )));
        }
        let Some(info) = media::decode_info(row.media.info.as_deref())? else {
            return Ok(Err((
                ErrorCode::NotApplicable,
                "This video's formats are not known yet. Wait for its check to finish.".to_owned(),
            )));
        };
        if !info.can_make(choice, converter) {
            let why = if info.can_make(choice, true) {
                media::NEEDS_CONVERTER
            } else {
                "This video does not offer that format."
            };
            return Ok(Err((ErrorCode::NotApplicable, why.to_owned())));
        }
        let mut revision = row.revision;
        if media::decode_choice(row.media.choice.as_deref())? != choice {
            revision = revision
                .next()
                .map_err(|_| StoreError::Corrupt("link revision"))?;
            tx.execute(
                "UPDATE source_links SET media_choice = ?1, revision = ?2 WHERE id = ?3",
                params![
                    media::encode_choice(&choice),
                    sql(revision)?,
                    sql(link.link)?
                ],
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

/// A listed link's columns, as [`list`] reads them.
struct Listed {
    id: i64,
    batch: i64,
    created: i64,
    revision: i64,
    invalid: bool,
    text: String,
    root: Option<i64>,
    problem: Option<String>,
    path: Option<String>,
    job: Option<i64>,
    checking: bool,
    state: Option<String>,
    reason: Option<String>,
    detail: Option<String>,
    size: Option<i64>,
    name: Option<String>,
    found: Option<u32>,
    skipped: Option<u32>,
    page: Option<String>,
    media: media::LinkColumns,
}

impl Listed {
    const COLUMNS: &str = "s.id, s.batch_id, b.created_at, s.revision, s.invalid, s.input_url, \
         s.root_id, s.problem, r.path, j.id, t.id IS NOT NULL, s.check_state, s.check_reason, \
         s.check_problem, s.size, s.observed_name, s.found, s.skipped, p.input_url";

    fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            batch: r.get(1)?,
            created: r.get(2)?,
            revision: r.get(3)?,
            invalid: r.get(4)?,
            text: r.get(5)?,
            root: r.get(6)?,
            problem: r.get(7)?,
            path: r.get(8)?,
            job: r.get(9)?,
            checking: r.get(10)?,
            state: r.get(11)?,
            reason: r.get(12)?,
            detail: r.get(13)?,
            size: r.get(14)?,
            name: r.get(15)?,
            found: r.get(16)?,
            skipped: r.get(17)?,
            page: r.get(18)?,
            media: media::LinkColumns::read(r, 19)?,
        })
    }

    /// What is known about the link: a media link by its service's check,
    /// any other by its own.
    fn check(&self) -> Result<LinkCheck, StoreError> {
        if self.invalid {
            return Ok(LinkCheck::Invalid);
        }
        if self.media.service.is_some() {
            return Ok(match self.media.state.as_deref() {
                Some("ready") => LinkCheck::Online,
                Some("failed") => LinkCheck::Unknown,
                _ if self.checking => LinkCheck::Checking,
                _ => LinkCheck::NotChecked,
            });
        }
        if self.checking {
            return Ok(LinkCheck::Checking);
        }
        Ok(match self.state.as_deref() {
            None => LinkCheck::NotChecked,
            Some("online") => LinkCheck::Online,
            Some("offline") => LinkCheck::Offline,
            Some("unknown") => LinkCheck::Unknown,
            Some("page") => LinkCheck::Page,
            Some(_) => return Err(StoreError::Corrupt("check state")),
        })
    }

    fn into_link(self, converter: bool) -> Result<CollectedLink, StoreError> {
        let check = self.check()?;
        let url = (!self.invalid)
            .then(|| Url::parse(&self.text).ok())
            .flatten();
        if !self.invalid && url.is_none() {
            return Err(StoreError::Corrupt("collected link"));
        }
        // What a new check has not answered yet is not shown as known.
        let checked = check != LinkCheck::Checking;
        Ok(CollectedLink {
            id: key(self.id)?,
            batch: key(self.batch)?,
            collected_at: UtcTimestamp::from_unix_millis(self.created),
            revision: key(self.revision)?,
            check,
            host: url.as_ref().and_then(|u| HostName::new(u.host_str()?).ok()),
            name_hint: self.name.filter(|_| checked).or_else(|| {
                url.as_ref()
                    .and_then(crate::download::url_file_name)
                    .map(|name| name.as_str().to_owned())
            }),
            text: self.invalid.then(|| shown_text(&self.text)),
            destination: self.root.map(key).transpose()?,
            destination_path: self.path,
            job: self.job.map(key).transpose()?,
            problem: self.problem.as_deref().map(DiagnosticText::new),
            reason: self
                .reason
                .filter(|_| checked)
                .map(|r| r.parse().map_err(|_| StoreError::Corrupt("check reason")))
                .transpose()?,
            check_detail: self
                .detail
                .filter(|_| checked)
                .as_deref()
                .map(DiagnosticText::new),
            size: self
                .size
                .filter(|_| checked)
                .map(u64::try_from)
                .transpose()
                .map_err(|_| StoreError::Corrupt("file size"))?,
            found_on: self.page.as_deref().map(crawl::shown_page),
            found: self.found.filter(|_| checked),
            skipped: self.skipped.filter(|_| checked),
            media: self.media.summary(converter)?.map(Box::new),
        })
    }
}

/// A page of listed links after `after`, oldest first.
pub(super) fn list(
    tx: &Transaction<'_>,
    after: Option<SourceLinkId>,
    page_size: PageSize,
    as_of: EventCursor,
    converter: bool,
) -> Result<LinkPage, StoreError> {
    let after = match after {
        // Past the stored range: nothing follows.
        Some(id) => sql(id).unwrap_or(i64::MAX),
        None => 0,
    };
    let size = usize::from(page_size.get());
    let mut items = tx
        .prepare(&format!(
            "SELECT {}, {MEDIA_COLUMNS} FROM source_links s \
             JOIN input_batches b ON b.id = s.batch_id \
             LEFT JOIN roots r ON r.id = s.root_id \
             LEFT JOIN jobs j ON j.source_link_id = s.id \
             LEFT JOIN crawl_tasks t ON t.source_link_id = s.id \
             LEFT JOIN source_links p ON p.id = s.parent_id \
             WHERE s.listed = 1 AND s.id > ?1 ORDER BY s.id LIMIT ?2",
            Listed::COLUMNS
        ))?
        .query_map(
            params_from_iter([after, i64::from(page_size.get()) + 1]),
            Listed::read,
        )?
        .map(|row| row?.into_link(converter))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let next_after = if items.len() > size {
        items.truncate(size);
        items.last().map(|l| l.id)
    } else {
        None
    };
    let (total, checking, before): (i64, i64, i64) = tx.query_row(
        "SELECT COUNT(*), COUNT(t.id), COALESCE(SUM(s.id <= ?1), 0) FROM source_links s \
         LEFT JOIN crawl_tasks t ON t.source_link_id = s.id WHERE s.listed = 1",
        [after],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let count = |n: i64| u64::try_from(n).map_err(|_| StoreError::Corrupt("link count"));
    Ok(LinkPage {
        items,
        next_after,
        total: count(total)?,
        checking: count(checking)?,
        before: Some(count(before)?),
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
    /// Original valid URLs still held for a start, by link ID.
    pub(crate) fn start_urls(
        &mut self,
        links: &[SourceLinkId],
    ) -> Result<Vec<(SourceLinkId, Url)>, StoreError> {
        let tx = self.read()?;
        let mut urls = Vec::new();
        for id in links {
            let Ok(sql_id) = sql(*id) else {
                continue;
            };
            let raw: Option<String> = tx.query_row(
                "SELECT input_url FROM source_links WHERE id = ?1 AND held = 1 AND listed = 1 AND invalid = 0 \
                 AND NOT EXISTS(SELECT 1 FROM jobs WHERE source_link_id = source_links.id)",
                [sql_id], |r| r.get(0)).optional()?;
            if let Some(raw) = raw {
                urls.push((
                    *id,
                    Url::parse(&raw).map_err(|_| StoreError::Corrupt("source URL"))?,
                ));
            }
        }
        Ok(urls)
    }

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
