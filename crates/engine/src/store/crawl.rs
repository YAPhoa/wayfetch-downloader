//! The queue of link checks and what they found.
//!
//! A check is one `crawl_tasks` row per link. Checking again, or removing or
//! starting the link, drops the row, so a late answer changes nothing.
//! Results commit with the row's deletion and a `links_changed` event in one
//! transaction.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, Transaction, params};
use url::Url;
use wayfetch_domain::id::SourceLinkId;
use wayfetch_domain::name::{DiagnosticText, ProviderId};
use wayfetch_domain::time::UtcTimestamp;
use wayfetch_protocol::command::{InputUrl, MAX_COLLECTED_LINKS};
use wayfetch_protocol::event::EventKind;
use wayfetch_protocol::filter::{CheckOutcome, LinkOrigin, LinkRoute};
use wayfetch_protocol::query::CheckReason;

use super::{OrCorrupt, Store, StoreError, key, links, outbox, roots, sql};
use crate::crawl::{Checked, Checker};
use crate::link_filter::{Answer, Link, Verdict};
use crate::transfer::transport::Reach;

/// What requests for a link may reach, from its `public_only` column: a
/// page on the public internet cannot point checks or downloads at the
/// local networks the operator allowed for links the person adds.
pub(super) fn reach(public_only: bool) -> Reach {
    if public_only {
        Reach::Public
    } else {
        Reach::Allowed
    }
}

/// A check the coordinator took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Task {
    /// The check's identity.
    pub(crate) id: i64,
    /// The link.
    pub(crate) link: SourceLinkId,
    /// The link as collected.
    pub(crate) url: Url,
    /// What it is limited by, one check at a time.
    pub(crate) queue: String,
    /// Whether it may be read as a page.
    pub(crate) analyze: bool,
    /// What its requests may reach.
    pub(crate) reach: Reach,
    /// The video service that checks it, for a media link.
    pub(crate) media: Option<ProviderId>,
}

/// A link found on a page, to collect beside it.
#[derive(Debug, Clone)]
pub(crate) struct Child {
    /// The link.
    pub(crate) url: InputUrl,
    /// The video service that recognizes it, if any.
    pub(crate) media: Option<ProviderId>,
}

/// What a check of a page found to collect: the links admitted, and how
/// many others on it were left alone.
#[derive(Debug, Clone, Default)]
pub(crate) struct Found {
    pub(crate) children: Vec<Child>,
    pub(crate) skipped: u32,
}

/// The queue a check of `url` waits in: its video service's, or its host's.
fn queue(url: &Url, media: Option<&str>) -> String {
    match media {
        Some(service) => format!("media:{service}"),
        None => url.host_str().unwrap_or("-").to_owned(),
    }
}

/// Queues a check of `link`, replacing one waiting or running, and
/// forgets what the last check found. A media link's check goes to its
/// video service.
pub(super) fn enqueue(
    tx: &Transaction<'_>,
    link: i64,
    url: &Url,
    media: Option<&str>,
    now: UtcTimestamp,
) -> Result<(), StoreError> {
    tx.execute("DELETE FROM crawl_tasks WHERE source_link_id = ?1", [link])?;
    tx.execute(
        "INSERT INTO crawl_tasks (source_link_id, queue, state, created_at) \
         VALUES (?1, ?2, 'pending', ?3)",
        params![link, queue(url, media), now.unix_millis()],
    )?;
    tx.execute(
        "UPDATE source_links SET check_state = NULL, check_reason = NULL, check_problem = NULL, \
         checked_via = NULL, origin_page = 0, size = NULL, observed_name = NULL, found = NULL, skipped = NULL, filtered = 0, \
         media_state = CASE WHEN media_service IS NULL THEN NULL ELSE 'pending' END, \
         media_problem = NULL WHERE id = ?1",
        [link],
    )?;
    Ok(())
}

/// Drops the check of `link`, if any: it left the list.
pub(super) fn drop_check(tx: &Transaction<'_>, link: i64) -> Result<(), StoreError> {
    tx.execute("DELETE FROM crawl_tasks WHERE source_link_id = ?1", [link])?;
    Ok(())
}

/// Ends running check `task`: true if it was still the link's current
/// check, so its result may be recorded.
pub(super) fn finish(tx: &Transaction<'_>, task: i64) -> Result<bool, StoreError> {
    Ok(tx.execute(
        "DELETE FROM crawl_tasks WHERE id = ?1 AND state = 'running'",
        [task],
    )? == 1)
}

impl Store {
    /// Takes up to `limit` waiting checks, the oldest of each queue not in
    /// `busy` that can be sent, oldest first, and marks them running. A
    /// video service's check can always be sent, a link's when `sendable`
    /// says. A huge page's links share one queue, so they never hold back
    /// another host's.
    pub(crate) fn take_checks(
        &mut self,
        limit: usize,
        busy: &[String],
        sendable: impl Fn(&Url) -> bool,
    ) -> Result<Vec<Task>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        /// A waiting check: its id, queue, link, URL, whether it may be
        /// read as a page, video service, and whether it is public only.
        type Waiting = (i64, String, i64, String, bool, Option<String>, bool);
        let tx = self.write()?;
        let waiting: Vec<Waiting> = tx
            .prepare(
                "SELECT t.id, t.queue, s.id, s.input_url, s.may_analyze, s.media_service, \
                 s.public_only FROM crawl_tasks t \
                 JOIN source_links s ON s.id = t.source_link_id \
                 WHERE t.state = 'pending' ORDER BY t.id",
            )?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        let mut taken = BTreeSet::new();
        let mut tasks = Vec::new();
        for (id, queue, link, url, analyze, media, public_only) in waiting {
            if tasks.len() == limit {
                break;
            }
            if busy.contains(&queue) || taken.contains(&queue) {
                continue;
            }
            let url = Url::parse(&url).or_corrupt("source URL")?;
            // One that cannot be sent waits without holding back the
            // rest of its queue.
            if media.is_none() && !sendable(&url) {
                continue;
            }
            tx.execute(
                "UPDATE crawl_tasks SET state = 'running' WHERE id = ?1",
                [id],
            )?;
            taken.insert(queue.clone());
            tasks.push(Task {
                id,
                link: key(link)?,
                url,
                queue,
                analyze,
                reach: reach(public_only),
                media: media
                    .map(ProviderId::new)
                    .transpose()
                    .or_corrupt("media service")?,
            });
        }
        tx.commit()?;
        Ok(tasks)
    }

    /// Puts checks that were running when the engine stopped back to wait.
    /// A check sends only `HEAD` and `GET`, so repeating one is safe.
    pub(crate) fn requeue_checks(&mut self) -> Result<usize, StoreError> {
        let tx = self.write()?;
        let n = tx.execute(
            "UPDATE crawl_tasks SET state = 'pending' WHERE state = 'running'",
            [],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Records what check `task` of `link` found, if it is still the
    /// link's check: the link's new state as `shown`, with the provider that
    /// answered when its own server's answer is `origin` instead, and, for a page that was read,
    /// the links found on it, collected beside it with checks of their
    /// own. Links already in its batch are not collected again; links a
    /// link filter matches, and those once the Links list is full, are
    /// left out and the page says so. A link held back for this check is
    /// then listed or dropped, as its filters' conditions say.
    pub(crate) fn record_link_check(
        &mut self,
        task: i64,
        link: SourceLinkId,
        (checked, via): (&Checked, Option<&Checker>),
        origin: Option<&Checked>,
        found: &Found,
        now: UtcTimestamp,
    ) -> Result<(), StoreError> {
        let origin = origin.unwrap_or(checked);
        let tx = self.write()?;
        if !finish(&tx, task)? {
            return Ok(());
        }
        let id = sql(link)?;
        let (outcome, reason, problem, size, name): (
            CheckOutcome,
            Option<CheckReason>,
            Option<String>,
            Option<u64>,
            Option<&str>,
        ) = match checked {
            Checked::File { name, size } => (
                CheckOutcome::Online,
                None,
                None,
                *size,
                // Only a name that could be a file's is kept.
                name.as_deref()
                    .filter(|n| crate::download::file_name(n).is_some()),
            ),
            Checked::Page { .. } => (CheckOutcome::Page, None, None, None, None),
            Checked::Offline(reason, message) => (
                CheckOutcome::Offline,
                Some(*reason),
                Some(message.clone()),
                None,
                None,
            ),
            Checked::Unknown(reason, message) => (
                CheckOutcome::Unknown,
                Some(*reason),
                Some(message.clone()),
                None,
                None,
            ),
        };
        let state = match outcome {
            CheckOutcome::Online => "online",
            CheckOutcome::Offline => "offline",
            CheckOutcome::Unknown => "unknown",
            CheckOutcome::Page => "page",
        };
        let (mut collected, mut left_out) = (None, 0u32);
        if let Checked::Page {
            links: Some(_),
            local,
        } = origin
        {
            let children = collect_children(&tx, id, &found.children, !local, now)?;
            collected = Some(children.found);
            left_out = children.left_out;
        }
        let mut notes: Vec<String> = problem.filter(|p| !p.is_empty()).into_iter().collect();
        if left_out > 0 {
            notes = vec![format!(
                "{left_out} more links on this page were not collected: the Links list holds \
                 at most {MAX_COLLECTED_LINKS}."
            )];
        }
        let problem = (!notes.is_empty()).then(|| notes.join(" "));
        tx.execute(
            "UPDATE source_links SET check_state = ?1, check_reason = ?2, check_problem = ?3, \
             size = ?4, observed_name = ?5, found = ?6, skipped = ?7, checked_via = ?9, \
             origin_page = ?10 WHERE id = ?8",
            params![
                state,
                reason.map(<&str>::from),
                problem
                    .as_deref()
                    .map(|p| DiagnosticText::new(p).as_str().to_owned()),
                size.map(i64::try_from)
                    .transpose()
                    .or_corrupt("file size")?,
                name,
                collected,
                collected.map(|_| found.skipped),
                id,
                via.map(|via| via.service().as_str()),
                matches!(origin, Checked::Page { .. }),
            ],
        )?;
        let answer = Answer {
            name,
            size,
            check: outcome,
            // A file host downloads without an account.
            route: match via {
                Some(Checker::Account(_)) => LinkRoute::Account,
                Some(Checker::Host(_)) | None => LinkRoute::Direct,
            },
        };
        links::settle(&tx, id, &answer)?;
        outbox::append(&tx, &EventKind::LinksChanged, now)?;
        tx.commit()?;
        Ok(())
    }
}

/// How the links found on a page were collected.
struct Collected {
    /// Listed beside it, or held back for their checks.
    found: u32,
    /// Left out because the Links list was full.
    left_out: u32,
}

/// Collects `children` of page `parent` into its batch, each with a check,
/// unless a link filter leaves it out, which the page counts. A link
/// already in the batch counts as found, not again. With `public_only`,
/// their requests reach public addresses only.
fn collect_children(
    tx: &Transaction<'_>,
    parent: i64,
    children: &[Child],
    public_only: bool,
    now: UtcTimestamp,
) -> Result<Collected, StoreError> {
    let (batch, root, page): (i64, Option<i64>, String) = tx.query_row(
        "SELECT batch_id, root_id, input_url FROM source_links WHERE id = ?1",
        [parent],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let page = Url::parse(&page).or_corrupt("source URL")?;
    let filters = roots::active_filters(tx)?;
    let listed: i64 = tx.query_row(
        "SELECT COUNT(*) FROM source_links WHERE listed = 1",
        [],
        |r| r.get(0),
    )?;
    let mut room = usize::try_from(listed)
        .map(|n| MAX_COLLECTED_LINKS.saturating_sub(n))
        .unwrap_or(0);
    let mut ordinal: i64 = tx.query_row(
        "SELECT COALESCE(MAX(ordinal), -1) FROM source_links WHERE batch_id = ?1",
        [batch],
        |r| r.get(0),
    )?;
    let (mut found, mut left_out, mut filtered) = (0u32, 0u32, 0u32);
    for child in children {
        let url = child.url.as_url();
        let known: Option<i64> = tx
            .query_row(
                "SELECT id FROM source_links WHERE batch_id = ?1 AND input_url = ?2 LIMIT 1",
                params![batch, url.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        if known.is_some() {
            found += 1;
            continue;
        }
        let link = Link {
            url,
            origin: LinkOrigin::AnalyzedPage,
            found_on: Some(&page),
            media: child.media.is_some(),
            checked: true,
        };
        let verdict = filters.judge(&link);
        if verdict == Verdict::Drop {
            filtered += 1;
            continue;
        }
        if room == 0 {
            left_out += 1;
            continue;
        }
        room -= 1;
        ordinal += 1;
        let media = child.media.as_ref().map(ProviderId::as_str);
        tx.execute(
            "INSERT INTO source_links (batch_id, ordinal, input_url, held, listed, root_id, \
             parent_id, public_only, media_service, media_state, screen) \
             VALUES (?1, ?2, ?3, 1, 1, ?4, ?5, ?6, ?7, CASE WHEN ?7 IS NULL THEN NULL \
             ELSE 'pending' END, ?8)",
            params![
                batch,
                ordinal,
                url.as_str(),
                root,
                parent,
                public_only,
                media,
                links::screen(verdict, &link)?,
            ],
        )?;
        enqueue(tx, tx.last_insert_rowid(), url, media, now)?;
        found += 1;
    }
    tx.execute(
        "UPDATE source_links SET filtered = ?1 WHERE id = ?2",
        params![filtered, parent],
    )?;
    Ok(Collected { found, left_out })
}

/// The page a link was found on, as a list shows it: without its query or
/// fragment, which can carry a token.
pub(super) fn shown_page(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut url) => {
            url.set_query(None);
            url.set_fragment(None);
            url.into()
        }
        Err(_) => super::links::shown_text(url),
    }
}

#[cfg(test)]
mod tests {
    use wayfetch_protocol::command::{CollectMode, CollectText, LinkRef, LinkRefs, StartRouting};
    use wayfetch_protocol::query::{CollectedLink, LinkCheck, PageSize};
    use wayfetch_protocol::{
        Command, CommandEnvelope, CommandResult, IdempotencyKey, InstanceId, Query, QueryResult,
    };

    use super::*;
    use crate::crawl::extract::Entry;
    use crate::store::{CollectPlan, Collecting, CommandContext};

    const NOW: UtcTimestamp = UtcTimestamp::from_unix_millis(1_700_000_000_000);

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        instance: InstanceId,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path()).unwrap();
            Self {
                _dir: dir,
                store,
                instance: InstanceId::generate(),
            }
        }

        fn execute(&mut self, command: Command, context: &CommandContext) -> CommandResult {
            let envelope = CommandEnvelope {
                idempotency_key: IdempotencyKey::generate(),
                target: self.instance,
                command,
            };
            self.store
                .execute(&envelope, self.instance, context, NOW)
                .unwrap()
        }

        /// Collects `links` (with a video service for `media:` ones) as text,
        /// or as HTML.
        fn collect(&mut self, links: &[&str], mode: CollectMode, html: bool) {
            let plan = CollectPlan {
                links: links
                    .iter()
                    .map(|l| match l.strip_prefix("media:") {
                        Some(l) => Collecting {
                            entry: Entry::parse(l),
                            media: Some(ProviderId::new("youtube".to_owned()).unwrap()),
                            hosted: false,
                        },
                        None => Collecting {
                            entry: Entry::parse(l),
                            media: None,
                            hosted: false,
                        },
                    })
                    .collect(),
                html,
                ..CollectPlan::default()
            };
            let context = CommandContext {
                collect: plan,
                ..CommandContext::default()
            };
            let command = Command::CollectLinks {
                text: CollectText::try_from("(planned)".to_owned()).unwrap(),
                base_url: None,
                mode,
                destination: None,
            };
            assert!(matches!(
                self.execute(command, &context),
                CommandResult::LinksCollected { .. }
            ));
        }

        fn links(&mut self) -> (Vec<CollectedLink>, u64) {
            let query = Query::ListLinks {
                after: None,
                page_size: PageSize::DEFAULT,
            };
            match self.store.query(&query, self.instance).unwrap() {
                QueryResult::Links(page) => (page.items, page.checking),
                other => panic!("{other:?}"),
            }
        }

        fn may_analyze(&mut self) -> Vec<bool> {
            let tx = self.store.read().unwrap();
            tx.prepare("SELECT may_analyze FROM source_links ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }

        fn links_command(&mut self, links: &[&CollectedLink], make: fn(LinkRefs) -> Command) {
            let refs = links
                .iter()
                .map(|l| LinkRef {
                    link: l.id,
                    revision: l.revision,
                })
                .collect::<Vec<_>>();
            let command = make(LinkRefs::try_from(refs).unwrap());
            match self.execute(command, &CommandContext::default()) {
                CommandResult::LinksApplied { refused, .. } => assert!(refused.is_empty()),
                other => panic!("{other:?}"),
            }
        }
    }

    fn checks(links: &[CollectedLink]) -> Vec<LinkCheck> {
        links.iter().map(|l| l.check).collect()
    }

    const FILE: &str = "https://a.example/f.zip";
    const PAGE: &str = "https://a.example/release";
    const VIDEO: &str = "media:https://www.youtube.com/watch?v=x";

    #[test]
    fn collecting_queues_the_checks_its_mode_asks_for() {
        let mut f = Fixture::new();
        f.collect(
            &[FILE, PAGE, VIDEO, "ftp://a.example/x"],
            CollectMode::Analyze,
            false,
        );
        let (links, checking) = f.links();
        assert_eq!(checking, 3);
        assert_eq!(
            checks(&links),
            [
                LinkCheck::Checking,
                LinkCheck::Checking,
                LinkCheck::Checking,
                LinkCheck::Invalid
            ]
        );
        // Typed links may be read as pages; a video and an invalid entry not.
        assert_eq!(f.may_analyze(), [true, true, false, false]);

        let mut f = Fixture::new();
        f.collect(&[FILE, PAGE, VIDEO], CollectMode::SupportedOnly, false);
        let (links, _) = f.links();
        assert_eq!(
            checks(&links),
            [
                LinkCheck::Checking,
                LinkCheck::NotChecked,
                LinkCheck::Checking
            ]
        );
        assert_eq!(f.may_analyze(), [false, false, false]);

        // Links in pasted HTML are a page's content: never read as pages.
        let mut f = Fixture::new();
        f.collect(&[FILE], CollectMode::Analyze, true);
        assert_eq!(f.may_analyze(), [false]);
    }

    #[test]
    fn a_check_that_cannot_be_sent_does_not_hold_back_its_queue() {
        let mut f = Fixture::new();
        f.collect(
            &["https://a.example/1.zip", "https://a.example/2.zip"],
            CollectMode::Analyze,
            false,
        );
        let taken = f
            .store
            .take_checks(4, &[], |url| url.path() == "/2.zip")
            .unwrap();
        let urls: Vec<_> = taken.iter().map(|t| t.url.as_str()).collect();
        assert_eq!(urls, ["https://a.example/2.zip"]);
        let busy = [taken[0].queue.clone()];
        assert!(f.store.take_checks(4, &busy, |_| true).unwrap().is_empty());
    }

    #[test]
    fn checks_are_taken_one_per_queue_oldest_first() {
        let mut f = Fixture::new();
        f.collect(
            &[
                "https://a.example/1.zip",
                "https://a.example/2.zip",
                VIDEO,
                "https://b.example/3.zip",
            ],
            CollectMode::Analyze,
            false,
        );
        // Without link checks only the video's is taken.
        let only_media = f.store.take_checks(4, &[], |_| false).unwrap();
        assert_eq!(only_media.len(), 1);
        assert_eq!(only_media[0].queue, "media:youtube");
        let taken = f
            .store
            .take_checks(4, &["media:youtube".to_owned()], |_| true)
            .unwrap();
        let urls: Vec<_> = taken.iter().map(|t| t.url.as_str()).collect();
        assert_eq!(urls, ["https://a.example/1.zip", "https://b.example/3.zip"]);
        assert!(taken.iter().all(|t| t.analyze && t.media.is_none()));
        let busy: Vec<String> = taken.iter().map(|t| t.queue.clone()).collect();
        assert!(f.store.take_checks(4, &busy, |_| true).unwrap().is_empty());
        // A restart sends the running checks again.
        assert_eq!(f.store.requeue_checks().unwrap(), 3);
        assert_eq!(
            f.store.take_checks(1, &[], |_| true).unwrap()[0].id,
            taken[0].id
        );
    }

    #[test]
    fn a_file_check_records_its_name_and_size() {
        let mut f = Fixture::new();
        f.collect(
            &[FILE, "https://a.example/gone.zip"],
            CollectMode::Analyze,
            false,
        );
        let tasks = f.store.take_checks(2, &[], |_| true).unwrap();
        // The second is in the same queue and waits.
        assert_eq!(tasks.len(), 1);
        let file = Checked::File {
            name: Some("Real.zip".into()),
            size: Some(0),
        };
        f.store
            .record_link_check(
                tasks[0].id,
                tasks[0].link,
                (&file, None),
                None,
                &Found::default(),
                NOW,
            )
            .unwrap();
        let task = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
        let gone = Checked::Offline(CheckReason::NotFound, "HTTP 404".into());
        f.store
            .record_link_check(
                task.id,
                task.link,
                (&gone, None),
                None,
                &Found::default(),
                NOW,
            )
            .unwrap();
        let (links, checking) = f.links();
        assert_eq!(checking, 0);
        let [file, gone] = &links[..] else { panic!() };
        assert_eq!(
            (file.check, file.name_hint.as_deref(), file.size),
            (LinkCheck::Online, Some("Real.zip"), Some(0))
        );
        assert_eq!(
            (gone.check, gone.reason, gone.size),
            (LinkCheck::Offline, Some(CheckReason::NotFound), None)
        );
        assert_eq!(gone.check_detail.as_ref().unwrap().as_str(), "HTTP 404");
    }

    #[test]
    fn a_page_collects_its_files_once_beside_it() {
        let mut f = Fixture::new();
        f.collect(
            &[PAGE, "https://a.example/known.zip"],
            CollectMode::Analyze,
            false,
        );
        let task = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
        let child = |url: &str| Child {
            url: InputUrl::parse(url).unwrap(),
            media: None,
        };
        let found = Found {
            children: vec![
                child("https://cdn.example/new.zip"),
                child("https://a.example/known.zip"),
                child("https://cdn.example/new.zip"),
            ],
            skipped: 7,
        };
        let page = Checked::Page {
            links: Some(Vec::new()),
            local: false,
        };
        f.store
            .record_link_check(task.id, task.link, (&page, None), None, &found, NOW)
            .unwrap();
        let (links, _) = f.links();
        let [page, known, new] = &links[..] else {
            panic!("{links:?}")
        };
        assert_eq!(
            (page.check, page.found, page.skipped),
            (LinkCheck::Page, Some(3), Some(7))
        );
        assert_eq!((known.found_on.as_deref(), new.batch), (None, page.batch));
        assert_eq!(new.found_on.as_deref(), Some(PAGE));
        assert_eq!(new.check, LinkCheck::Checking);
        // Found links are only checked, never read as pages.
        assert_eq!(f.may_analyze(), [true, true, false]);

        // The page cannot be started; its files can.
        let refs = LinkRefs::try_from(vec![LinkRef {
            link: page.id,
            revision: page.revision,
        }])
        .unwrap();
        let start = Command::StartLinks {
            links: refs,
            account: None,
            routing: Some(StartRouting::Direct),
        };
        let CommandResult::LinksApplied { refused, .. } =
            f.execute(start, &CommandContext::default())
        else {
            panic!()
        };
        assert!(refused[0].message.as_str().contains("web page"));

        // Removing the page keeps it for the link found on it, until that
        // goes too.
        let page = page.clone();
        let new = new.clone();
        f.links_command(&[&page], |links| Command::RemoveLinks { links });
        assert_eq!(f.links().0.len(), 2);
        assert_eq!(f.links().0[1].found_on.as_deref(), Some(PAGE));
        f.links_command(&[&new], |links| Command::RemoveLinks { links });
        let rows: i64 = f
            .store
            .read()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM source_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the page and its last link are gone");
    }

    impl Fixture {
        /// Saves `filters` as the link filters.
        fn filter(&mut self, filters: serde_json::Value) {
            let revision = self.store.settings().unwrap().revision;
            let command = Command::UpdateSettings {
                expected_revision: revision,
                default_root: None,
                max_active_downloads: None,
                download_speed_limit: None,
                extract_archives: None,
                automatic_update_checks: None,
                link_filters: Some(
                    serde_json::from_value(serde_json::json!({
                        "enabled": true, "filters": filters,
                    }))
                    .unwrap(),
                ),
            };
            self.execute(command, &CommandContext::default());
        }

        /// The rows of links, listed or not.
        fn rows(&mut self) -> Vec<(String, bool)> {
            let tx = self.store.read().unwrap();
            tx.prepare("SELECT input_url, screen IS NULL FROM source_links ORDER BY id")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }

        /// Records that check `task` found a file of `size` bytes.
        fn found_file(&mut self, task: &Task, size: u64) {
            self.found_named(task, None, size);
        }

        /// Records that check `task` found a file of `size` bytes, which its
        /// server called `name`.
        fn found_named(&mut self, task: &Task, name: Option<&str>, size: u64) {
            let file = Checked::File {
                name: name.map(str::to_owned),
                size: Some(size),
            };
            self.store
                .record_link_check(
                    task.id,
                    task.link,
                    (&file, None),
                    None,
                    &Found::default(),
                    NOW,
                )
                .unwrap();
        }
    }

    #[test]
    fn link_filters_leave_links_out_as_they_are_collected_or_once_checked() {
        let mut f = Fixture::new();
        f.filter(serde_json::json!([
            { "name": "Ads", "enabled": true,
              "link": { "op": "contains", "pattern": "ads.example" } },
            { "name": "Small archives", "enabled": true,
              "file_type": { "op": "is", "kinds": ["archive"] },
              "size": { "op": "is", "max": 1000 } },
        ]));
        let plan = CollectPlan {
            links: [
                "https://ads.example/x.zip",
                "https://a.example/small.zip",
                "https://b.example/big.zip",
                PAGE,
            ]
            .iter()
            .map(|l| Collecting {
                entry: Entry::parse(l),
                media: None,
                hosted: false,
            })
            .collect(),
            ..CollectPlan::default()
        };
        let context = CommandContext {
            collect: plan,
            ..CommandContext::default()
        };
        let collect = |page: &str| Command::CollectLinks {
            text: CollectText::try_from(page.to_owned()).unwrap(),
            base_url: None,
            mode: CollectMode::Analyze,
            destination: None,
        };
        let CommandResult::LinksCollected {
            collected,
            filtered,
            ..
        } = f.execute(collect("(planned)"), &context)
        else {
            panic!()
        };
        assert_eq!((collected, filtered), (3, 1));
        // The archives wait off the list for their size, and the page for
        // the name its server gives.
        let page = match f.store.query(
            &Query::ListLinks {
                after: None,
                page_size: PageSize::DEFAULT,
            },
            f.instance,
        ) {
            Ok(QueryResult::Links(page)) => page,
            other => panic!("{other:?}"),
        };
        assert_eq!((page.items.len(), page.total, page.screening), (0, 0, 3));
        let tasks = f.store.take_checks(3, &[], |_| true).unwrap();
        let [small, big] = &tasks[..] else {
            panic!("{tasks:?}")
        };
        f.found_file(small, 10);
        f.found_file(big, 5000);
        let page_task = &f.store.take_checks(1, &[], |_| true).unwrap()[0];
        // A page's links pass the same filters.
        let child = |url: &str| Child {
            url: InputUrl::parse(url).unwrap(),
            media: None,
        };
        let found = Found {
            children: vec![
                child("https://ads.example/y.zip"),
                child("https://c.example/z.mkv"),
            ],
            skipped: 0,
        };
        let read = Checked::Page {
            links: Some(Vec::new()),
            local: false,
        };
        f.store
            .record_link_check(
                page_task.id,
                page_task.link,
                (&read, None),
                None,
                &found,
                NOW,
            )
            .unwrap();
        assert_eq!(
            f.rows(),
            [
                ("https://b.example/big.zip".to_owned(), true),
                (PAGE.to_owned(), true),
                ("https://c.example/z.mkv".to_owned(), false),
            ]
        );
        let (links, _) = f.links();
        assert_eq!(links[1].found, Some(1));
        assert_eq!(
            links[1].check_detail.as_ref().unwrap().as_str(),
            "Your link filters left out 1 link on this page."
        );
        // Its server says the video is a small archive, so it is left out
        // too, and the page counts it.
        let video = &f.store.take_checks(1, &[], |_| true).unwrap()[0];
        f.found_named(video, Some("z.zip"), 10);
        assert_eq!(f.rows().len(), 2);
        assert_eq!(
            f.links().0[1].check_detail.as_ref().unwrap().as_str(),
            "Your link filters left out 2 links on this page."
        );

        // Text whose every link a filter matches is refused.
        let only_ads = CommandContext {
            collect: CollectPlan {
                links: vec![Collecting {
                    entry: Entry::parse("https://ads.example/z.zip"),
                    media: None,
                    hosted: false,
                }],
                ..CollectPlan::default()
            },
            ..CommandContext::default()
        };
        let envelope = CommandEnvelope {
            idempotency_key: IdempotencyKey::generate(),
            target: f.instance,
            command: collect("(planned again)"),
        };
        let error = f
            .store
            .execute(&envelope, f.instance, &only_ads, NOW)
            .unwrap_err();
        assert!(error.message.as_str().contains("left out all 1 links"));
    }

    #[test]
    fn links_held_for_their_checks_follow_the_filters_as_they_change() {
        let mut f = Fixture::new();
        let small = serde_json::json!([
            { "name": "Small", "enabled": true, "size": { "op": "is", "max": 1000 } },
        ]);
        f.filter(small.clone());
        f.collect(&[FILE], CollectMode::SupportedOnly, false);
        assert_eq!(f.links().0.len(), 0);
        // Turned off, the filter no longer holds the link back.
        let revision = f.store.settings().unwrap().revision;
        f.execute(
            Command::UpdateSettings {
                expected_revision: revision,
                default_root: None,
                max_active_downloads: None,
                download_speed_limit: None,
                extract_archives: None,
                automatic_update_checks: None,
                link_filters: Some(wayfetch_protocol::filter::LinkFilters {
                    enabled: false,
                    ..wayfetch_protocol::filter::LinkFilters::default()
                }),
            },
            &CommandContext::default(),
        );
        assert_eq!(f.links().0.len(), 1);
        // A link still waiting when its filter is removed is judged by the
        // filters as they are when its check answers.
        f.filter(small);
        let other = "https://b.example/g.zip";
        f.collect(&[other], CollectMode::SupportedOnly, false);
        assert_eq!(f.links().0.len(), 1);
        let task = f
            .store
            .take_checks(2, &[], |_| true)
            .unwrap()
            .into_iter()
            .find(|t| t.url.as_str() == other)
            .unwrap();
        f.filter(serde_json::json!([
            { "name": "Offline", "enabled": true, "check": { "op": "is", "values": ["offline"] } },
        ]));
        f.found_file(&task, 10);
        assert_eq!(f.links().0.len(), 2);
    }

    #[test]
    fn a_link_held_for_its_check_is_listed_when_no_condition_holds() {
        let mut f = Fixture::new();
        f.filter(serde_json::json!([
            { "name": "Offline", "enabled": true,
              "check": { "op": "is", "values": ["offline"] } },
        ]));
        // Not checked, so no check can match: listed at once.
        f.collect(&[PAGE, FILE], CollectMode::SupportedOnly, false);
        assert_eq!(f.links().0.len(), 1);
        let task = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
        f.found_file(&task, 1);
        assert_eq!(f.links().0.len(), 2);
    }

    #[test]
    fn a_new_check_drops_the_old_ones_answer() {
        let mut f = Fixture::new();
        f.collect(&[FILE], CollectMode::Analyze, false);
        let old = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
        let (links, _) = f.links();
        f.links_command(&[&links[0]], |links| Command::CheckLinks { links });
        let late = Checked::File {
            name: None,
            size: Some(1),
        };
        f.store
            .record_link_check(
                old.id,
                old.link,
                (&late, None),
                None,
                &Found::default(),
                NOW,
            )
            .unwrap();
        let (links, checking) = f.links();
        assert_eq!(
            (links[0].check, links[0].size, checking),
            (LinkCheck::Checking, None, 1)
        );
        let new = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
        assert_ne!(new.id, old.id);
    }

    #[test]
    fn analyzing_lets_a_link_be_read_as_a_page() {
        let mut f = Fixture::new();
        f.collect(&[PAGE], CollectMode::SupportedOnly, false);
        let (links, checking) = f.links();
        assert_eq!((links[0].check, checking), (LinkCheck::NotChecked, 0));
        f.links_command(&[&links[0]], |links| Command::AnalyzeLinks { links });
        let task = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
        assert!(task.analyze);
    }

    #[test]
    fn links_found_on_a_public_page_reach_public_addresses_only() {
        for (local, reach) in [(false, Reach::Public), (true, Reach::Allowed)] {
            let mut f = Fixture::new();
            f.collect(&[PAGE], CollectMode::Analyze, false);
            let task = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
            // The person typed the page: it gets what the operator allows.
            assert_eq!(task.reach, Reach::Allowed);
            let found = Found {
                children: vec![Child {
                    url: InputUrl::parse("http://192.168.1.10/f.zip").unwrap(),
                    media: None,
                }],
                skipped: 0,
            };
            let page = Checked::Page {
                links: Some(Vec::new()),
                local,
            };
            f.store
                .record_link_check(task.id, task.link, (&page, None), None, &found, NOW)
                .unwrap();
            let child = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
            assert_eq!(child.reach, reach, "page served locally: {local}");
            // Its download reaches no more than its checks.
            let (links, _) = f.links();
            f.links_command(&[&links[1]], |links| Command::StartLinks {
                links,
                account: None,
                routing: Some(StartRouting::Direct),
            });
            let admitted = f.store.admissible(1, &[], true).unwrap();
            assert_eq!(admitted[0].reach, reach);
        }
    }

    #[test]
    fn links_in_pasted_html_reach_public_addresses_only() {
        for (html, reach) in [(true, Reach::Public), (false, Reach::Allowed)] {
            let mut f = Fixture::new();
            f.collect(&["http://192.168.1.10/f.zip"], CollectMode::Analyze, html);
            let task = f.store.take_checks(1, &[], |_| true).unwrap().remove(0);
            assert_eq!(task.reach, reach, "pasted as HTML: {html}");
            let (links, _) = f.links();
            f.links_command(&[&links[0]], |links| Command::StartLinks {
                links,
                account: None,
                routing: Some(StartRouting::Direct),
            });
            assert_eq!(f.store.admissible(1, &[], true).unwrap()[0].reach, reach);
        }
    }
}
