//! Collected links checked over real sockets (link crawler stage 1): a
//! typed page's address is read once, the files on it are collected beside
//! it with their names and sizes, and one of them goes through the existing
//! download queue to a published file. Missing files, addresses the egress
//! policy refuses, and links to other pages are reported, never followed.

#[path = "../../engine/tests/support/mod.rs"]
mod engine_support;
mod support;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engine_support::{ALL, KIB, body, complete, envelope, jobs};
use support::{Resource, Route, Server, config_for};
use wayfetch_engine::EngineOwner;
use wayfetch_engine::protocol::query::CheckReason;
use wayfetch_protocol::command::{
    CollectMode, CollectText, Command, CommandResult, LinkRef, LinkRefs, StartRouting,
};
use wayfetch_protocol::query::{CollectedLink, LinkCheck, LinkPage, PageSize, Query, QueryResult};

type Env = engine_support::Env<Server>;

/// An HTTPS server, and engines that check links and download through the
/// same real transport.
async fn env() -> Env {
    let server = Server::https().await;
    let transport = Arc::new(wayfetch_net::HttpTransport::new(config_for(&server)).unwrap());
    let mut env = Env::new(server, transport.clone());
    env.inspector = Some(transport);
    env
}

async fn collect(owner: &EngineOwner, text: &str, mode: CollectMode) -> CommandResult {
    let command = Command::CollectLinks {
        text: CollectText::try_from(text.to_owned()).unwrap(),
        base_url: None,
        mode,
        destination: None,
    };
    owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap()
}

async fn links(owner: &EngineOwner) -> LinkPage {
    let query = Query::ListLinks {
        after: None,
        page_size: PageSize::DEFAULT,
    };
    match owner.handle().query(query, ALL).await.unwrap() {
        QueryResult::Links(page) => page,
        other => panic!("{other:?}"),
    }
}

/// The Links list once no check is waiting or running.
async fn checked(owner: &EngineOwner) -> Vec<CollectedLink> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let page = links(owner).await;
        if page.checking == 0 {
            return page.items;
        }
        assert!(tokio::time::Instant::now() < deadline, "{page:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn html(body: &str) -> Resource {
    Resource::new(Bytes::from(body.to_owned())).header("Content-Type", "text/html; charset=utf-8")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_address_leads_to_its_files_and_one_downloads() {
    let env = env().await;
    let file = body(9 * KIB);
    env.server.put(
        "/files/a.bin",
        Resource::new(file.clone()).header(
            "Content-Disposition",
            "attachment; filename=\"Release A.bin\"",
        ),
    );
    env.server.put(
        "/release",
        html(
            "<html><body><a href=\"files/a.bin\">A</a> <a href=\"/missing.zip\">B</a> \
             <a href=\"/about\">About</a> <a href=\"/release#top\">Top</a> \
             <script>var x = 'https://cdn.example/not-collected.zip';</script></body></html>",
        ),
    );
    let page = env.server.url("/release");
    let owner = env.start().await;
    let CommandResult::LinksCollected { collected, .. } =
        collect(&owner, &page, CollectMode::Analyze).await
    else {
        panic!()
    };
    assert_eq!(collected, 1);

    let items = checked(&owner).await;
    let [read, a, missing] = &items[..] else {
        panic!("{items:?}")
    };
    // The page was read once; the other page's link was counted, not
    // collected, and a script's text is not a link on the page.
    assert_eq!(
        (read.check, read.found, read.skipped),
        (LinkCheck::Page, Some(2), Some(1))
    );
    assert_eq!(env.server.hits("/about"), 0);
    assert_eq!(
        (a.check, a.name_hint.as_deref(), a.size),
        (
            LinkCheck::Online,
            Some("Release A.bin"),
            Some(9 * KIB as u64)
        )
    );
    assert_eq!(a.found_on.as_deref(), Some(page.as_str()));
    assert_eq!(
        (missing.check, missing.reason),
        (LinkCheck::Offline, Some(CheckReason::NotFound))
    );
    // Checking a file never read its body.
    assert!(
        env.server
            .log()
            .iter()
            .filter(|r| r.path == "/files/a.bin")
            .all(|r| r.method == "HEAD")
    );

    // The file found on the page goes through the existing queue.
    let refs = LinkRefs::try_from(vec![LinkRef {
        link: a.id,
        revision: a.revision,
    }])
    .unwrap();
    let start = Command::StartLinks {
        links: refs,
        account: None,
        routing: Some(StartRouting::Direct),
    };
    let CommandResult::LinksApplied { applied, refused } = owner
        .handle()
        .execute(envelope(&owner, start), ALL)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(refused.is_empty(), "{refused:?}");
    let job = applied[0].job.unwrap();
    complete(&owner, job).await;
    assert_eq!(jobs(&owner).await.len(), 1);
    assert_eq!(env.published("Release A.bin"), file);
    engine_support::shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn checks_report_what_they_cannot_reach() {
    let env = env().await;
    env.server.put("/plain", html("<a href=\"/x.zip\">x</a>"));
    env.server
        .route("/moved.bin", Route::Redirect(302, "/files/b.bin".into()));
    env.server.put("/files/b.bin", Resource::new(body(3 * KIB)));
    let owner = env.start().await;
    let text = format!(
        "{}\n{}\nhttps://10.255.255.1/blocked.zip",
        env.server.url("/plain"),
        env.server.url("/moved.bin"),
    );
    collect(&owner, &text, CollectMode::SupportedOnly).await;
    let items = checked(&owner).await;
    let [plain, moved, blocked] = &items[..] else {
        panic!("{items:?}")
    };
    // Supported links only: a page's address is not checked, let alone read.
    assert_eq!(plain.check, LinkCheck::NotChecked);
    assert_eq!(env.server.hits("/plain"), 0);
    assert_eq!(
        (moved.check, moved.size),
        (LinkCheck::Online, Some(3 * KIB as u64))
    );
    assert_eq!(
        (blocked.check, blocked.reason),
        (LinkCheck::Unknown, Some(CheckReason::Blocked))
    );
    engine_support::shutdown(owner).await;
}
