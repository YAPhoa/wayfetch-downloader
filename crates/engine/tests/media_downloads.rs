//! Media downloads through the public engine API, with an in-process media
//! service, video host, and converter (ADR-27): a collected
//! media link is checked in the background and shows its title and
//! formats; the person's choice is kept and made; the engine downloads
//! every stream itself, end to end in one staging file, continues it with
//! fresh links after a pause, starts again when the formats changed, and
//! converts only through the converter. yt-dlp and ffmpeg themselves are
//! tested in `wayfetch-adapters`.

mod support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use support::{ALL, KIB, body, complete, envelope, job, shutdown, wait_for};
use url::Url;
use wayfetch_engine::media::{
    Container, Conversion, Converter, MediaError, MediaInfo, MediaPlan, MediaService, MediaStream,
    Part, PluginControl, PluginError, PluginOutcome, PluginWork, Recipe,
};
use wayfetch_engine::transfer::lease::Representation;
use wayfetch_engine::transfer::range::Framing;
use wayfetch_engine::transfer::transport::{OpenRequest, Response, Transport, TransportError};
use wayfetch_engine::transfer::writer::{BoxFuture, ChunkSource};
use wayfetch_engine::{EngineOwner, ShutdownRequest};
use wayfetch_protocol::command::{CollectMode, CollectText, LinkRef, LinkRefs};
use wayfetch_protocol::domain::failure::FailureCode;
use wayfetch_protocol::domain::id::JobId;
use wayfetch_protocol::domain::job::JobStage;
use wayfetch_protocol::domain::name::{PluginId, ProviderId};
use wayfetch_protocol::media::{
    AudioFormat, MediaChoice, MediaState, PluginChange, PluginChanged, PluginRole, PluginState,
    PluginSummary,
};
use wayfetch_protocol::query::{CollectedLink, PageSize};
use wayfetch_protocol::{Command, CommandResult, ErrorCode, Query, QueryResult};

const SERVICE: &str = "synthetic-video";
const PAGE: &str = "https://video.example/watch/abc";
const CDN: &str = "cdn.video.example";
const TITLE: &str = "A: synthetic/video";

/// A request: its path, range, and `User-Agent`.
type Request = (String, Option<(u64, Option<u64>)>, Option<String>);

/// One stream the video host serves.
#[derive(Clone)]
struct Stream {
    data: Bytes,
    /// Serving stops before this byte until cleared.
    stall_at: Option<u64>,
}

/// The video host: streams by path, and every range asked for.
#[derive(Default)]
struct Host {
    streams: Mutex<HashMap<String, Stream>>,
    requests: Mutex<Vec<Request>>,
    /// Answers every request with the whole stream, as a plain server.
    ignores_ranges: AtomicBool,
}

impl Host {
    fn put(&self, path: &str, data: Bytes) {
        self.streams.lock().unwrap().insert(
            path.into(),
            Stream {
                data,
                stall_at: None,
            },
        );
    }

    fn stall(&self, path: &str, at: Option<u64>) {
        self.streams.lock().unwrap().get_mut(path).unwrap().stall_at = at;
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

struct Body {
    host: Arc<Host>,
    path: String,
    data: Bytes,
    offset: u64,
    position: usize,
}

impl ChunkSource for Body {
    fn next_chunk(&mut self) -> BoxFuture<'_, std::io::Result<Option<Bytes>>> {
        Box::pin(async move {
            loop {
                let stall = self.host.streams.lock().unwrap()[&self.path].stall_at;
                let at = self.offset + self.position as u64;
                if stall.is_some_and(|limit| at >= limit) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
                if self.position == self.data.len() {
                    return Ok(None);
                }
                let mut end = (self.position + KIB).min(self.data.len());
                if let Some(limit) = stall {
                    end = end.min(self.position + usize::try_from(limit - at).unwrap());
                }
                let chunk = self.data.slice(self.position..end);
                self.position = end;
                return Ok(Some(chunk));
            }
        })
    }
}

struct HostTransport(Arc<Host>);

impl Transport for HostTransport {
    fn open<'a>(
        &'a self,
        request: &'a OpenRequest,
    ) -> BoxFuture<'a, Result<Response, TransportError>> {
        Box::pin(async move {
            let host = &self.0;
            let path = request.url.path().to_owned();
            host.requests.lock().unwrap().push((
                path.clone(),
                request.range,
                request.user_agent.clone(),
            ));
            let stream = (request.url.host_str() == Some(CDN))
                .then(|| host.streams.lock().unwrap().get(&path).cloned())
                .flatten()
                .ok_or(TransportError {
                    code: FailureCode::LeaseNotFound,
                    message: "HTTP 404".into(),
                })?;
            let len = stream.data.len() as u64;
            let range = request
                .range
                .filter(|_| !host.ignores_ranges.load(Ordering::SeqCst));
            let (framing, start, end) = match range {
                Some((start, end)) => {
                    let end = end.unwrap_or(len).min(len);
                    (
                        Framing::Partial {
                            first: start,
                            last: end - 1,
                            complete: Some(len),
                        },
                        start,
                        end,
                    )
                }
                None => (Framing::Full { length: Some(len) }, 0, len),
            };
            let (start, end) = (
                usize::try_from(start).unwrap(),
                usize::try_from(end).unwrap(),
            );
            Ok(Response {
                framing,
                representation: Representation {
                    length: Some(len),
                    ..Representation::default()
                },
                filename: None,
                body: Box::new(Body {
                    host: host.clone(),
                    path,
                    data: stream.data.slice(start..end),
                    offset: start as u64,
                    position: 0,
                }),
            })
        })
    }
}

/// What the media service answers.
struct Formats {
    /// The streams of the next plan: CDN path, length, chunk size.
    streams: Vec<(&'static str, u64, Option<u64>)>,
    recipe: Recipe,
    extension: &'static str,
    /// Changes when the formats change.
    version: u32,
}

struct Service {
    id: ProviderId,
    /// Whether its plugin is turned on.
    enabled: AtomicBool,
    info: MediaInfo,
    formats: Mutex<Formats>,
    resolves: AtomicUsize,
    choices: Mutex<Vec<MediaChoice>>,
}

impl MediaService for Service {
    fn service(&self) -> &ProviderId {
        &self.id
    }

    fn recognizes(&self, link: &Url) -> bool {
        link.host_str() == Some("video.example")
    }

    fn plugins(&self) -> Vec<PluginSummary> {
        vec![PluginSummary {
            id: "synthetic-resolver".into(),
            name: "Synthetic resolver".into(),
            role: PluginRole::MediaResolver,
            services: vec![self.id.clone()],
            state: PluginState::Ready,
            version: Some("1".into()),
            problem: None,
            enabled: self.enabled.load(Ordering::SeqCst),
            updatable: true,
            installed: false,
            fallback_version: None,
        }]
    }

    fn probe<'a>(&'a self, _: &'a Url) -> BoxFuture<'a, Result<MediaInfo, MediaError>> {
        Box::pin(async move { Ok(self.info.clone()) })
    }

    fn resolve<'a>(
        &'a self,
        _: &'a Url,
        choice: MediaChoice,
        _: bool,
    ) -> BoxFuture<'a, Result<MediaPlan, MediaError>> {
        Box::pin(async move {
            // Every answer carries fresh links.
            let n = self.resolves.fetch_add(1, Ordering::SeqCst);
            self.choices.lock().unwrap().push(choice);
            let formats = self.formats.lock().unwrap();
            Ok(MediaPlan {
                identity: format!("{SERVICE};abc;v{}", formats.version),
                title: TITLE.into(),
                extension: formats.extension.into(),
                streams: formats
                    .streams
                    .iter()
                    .map(|&(path, length, chunk_bytes)| MediaStream {
                        url: Url::parse(&format!("https://{CDN}{path}?sig={n}")).unwrap(),
                        length,
                        user_agent: Some("synthetic-agent".into()),
                        chunk_bytes,
                    })
                    .collect(),
                recipe: formats.recipe,
            })
        })
    }
}

/// Writes `converted:`, the recipe, then each part's bytes.
#[derive(Default)]
struct Joiner {
    runs: Mutex<Vec<(Vec<Part>, Recipe, String)>>,
    /// Writes part of the output, then never finishes.
    hang: std::sync::atomic::AtomicBool,
    /// Writes part of the output, then fails.
    fail: std::sync::atomic::AtomicBool,
}

impl Converter for Joiner {
    fn plugin(&self) -> PluginSummary {
        PluginSummary {
            id: "synthetic-converter".into(),
            name: "Synthetic converter".into(),
            role: PluginRole::MediaConverter,
            services: Vec::new(),
            state: PluginState::Ready,
            version: None,
            problem: None,
            enabled: true,
            updatable: false,
            installed: false,
            fallback_version: None,
        }
    }

    fn convert<'a>(&'a self, conversion: Conversion<'a>) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            self.runs.lock().unwrap().push((
                conversion.parts.to_vec(),
                conversion.recipe,
                conversion.title.to_owned(),
            ));
            if self.hang.load(Ordering::SeqCst) || self.fail.load(Ordering::SeqCst) {
                std::fs::write(conversion.output, b"half").unwrap();
                if self.fail.load(Ordering::SeqCst) {
                    return Err(MediaError::new(
                        FailureCode::AdapterParseFailure,
                        "the converter could not read the streams",
                    ));
                }
                std::future::pending::<()>().await;
            }
            let input = std::fs::read(conversion.input).unwrap();
            let mut out = format!("converted:{:?}:", conversion.recipe).into_bytes();
            for part in conversion.parts {
                let start = usize::try_from(part.offset).unwrap();
                out.extend_from_slice(&input[start..start + usize::try_from(part.length).unwrap()]);
            }
            // Written in place, as ffmpeg does.
            std::fs::write(conversion.output, out).unwrap();
            Ok(())
        })
    }
}

type Env = support::Env<Arc<Host>>;

struct Fixture {
    env: Env,
    service: Arc<Service>,
    joiner: Arc<Joiner>,
}

fn fixture(converter: bool, formats: Formats) -> Fixture {
    let host = Arc::new(Host::default());
    let mut env = Env::new(host.clone(), Arc::new(HostTransport(host)));
    let service = Arc::new(Service {
        id: ProviderId::new(SERVICE).unwrap(),
        enabled: AtomicBool::new(true),
        info: MediaInfo {
            title: TITLE.into(),
            duration_seconds: Some(42),
            heights: vec![1080, 360],
            progressive: vec![360],
            audio: true,
            m4a: false,
        },
        formats: Mutex::new(formats),
        resolves: AtomicUsize::new(0),
        choices: Mutex::new(Vec::new()),
    });
    let joiner = Arc::new(Joiner::default());
    env.media.push(service.clone());
    if converter {
        env.converter = Some(joiner.clone());
    }
    Fixture {
        env,
        service,
        joiner,
    }
}

async fn listed(owner: &EngineOwner) -> Vec<CollectedLink> {
    let query = Query::ListLinks {
        after: None,
        page_size: PageSize::DEFAULT,
    };
    match owner.handle().query(query, ALL).await.unwrap() {
        QueryResult::Links(page) => page.items,
        other => panic!("{other:?}"),
    }
}

/// Collects the page and waits for its check.
async fn collect(owner: &EngineOwner) -> CollectedLink {
    let command = Command::CollectLinks {
        text: CollectText::try_from(PAGE.to_owned()).unwrap(),
        base_url: None,
        mode: CollectMode::Analyze,
        destination: None,
    };
    owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let link = listed(owner).await.pop().unwrap();
        let media = link.media.as_ref().expect("a media link");
        if media.state != MediaState::Checking {
            return link;
        }
        assert!(tokio::time::Instant::now() < deadline, "{link:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn refs(link: &CollectedLink) -> LinkRefs {
    LinkRefs::try_from(vec![LinkRef {
        link: link.id,
        revision: link.revision,
    }])
    .unwrap()
}

async fn choose(owner: &EngineOwner, link: &CollectedLink, choice: MediaChoice) -> CommandResult {
    let command = Command::ChooseLinkMedia {
        links: refs(link),
        choice,
    };
    owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap()
}

/// Starts the link with automatic routing, as the UI does by default.
async fn start(owner: &EngineOwner, link: &CollectedLink) -> JobId {
    let command = Command::StartLinks {
        routing: None,
        links: refs(link),
        account: None,
    };
    match owner
        .handle()
        .execute(envelope(owner, command), ALL)
        .await
        .unwrap()
    {
        CommandResult::LinksApplied { applied, refused } => {
            assert!(refused.is_empty(), "{refused:?}");
            applied[0].job.unwrap()
        }
        other => panic!("{other:?}"),
    }
}

fn audio_only() -> Formats {
    Formats {
        streams: vec![("/audio", 10 * KIB as u64, None)],
        recipe: Recipe::Mp3,
        extension: "mp3",
        version: 1,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_media_link_shows_its_title_and_downloads_as_mp3() {
    let f = fixture(true, audio_only());
    let audio = body(10 * KIB);
    f.env.server.put("/audio", audio.clone());
    let owner = f.env.start().await;
    let link = collect(&owner).await;
    let media = link.media.clone().unwrap();
    assert_eq!(media.state, MediaState::Ready);
    assert_eq!(media.title.as_deref(), Some(TITLE));
    assert_eq!(media.duration_seconds, Some(42));
    assert!(media.options.iter().all(|o| o.available), "{media:?}");
    let mp3 = MediaChoice::Audio {
        format: AudioFormat::Mp3,
    };
    choose(&owner, &link, mp3).await;
    let link = listed(&owner).await.pop().unwrap();
    assert_eq!(link.media.as_ref().unwrap().choice, mp3);

    let id = start(&owner, &link).await;
    let done = complete(&owner, id).await;
    assert_eq!(done.media.as_ref().unwrap().choice, mp3);
    assert_eq!(*f.service.choices.lock().unwrap(), [mp3]);
    let mut expected = b"converted:Mp3:".to_vec();
    expected.extend_from_slice(&audio);
    // Characters no platform allows in a name are replaced.
    assert_eq!(f.env.published("A_ synthetic_video.mp3"), expected);
    assert_eq!(done.bytes_total, Some(expected.len() as u64));
    let runs = f.joiner.runs.lock().unwrap().clone();
    assert_eq!(
        runs,
        [(
            vec![Part {
                offset: 0,
                length: 10 * KIB as u64
            }],
            Recipe::Mp3,
            TITLE.to_owned()
        )]
    );
    // Streams are fetched with the agent their links were made for.
    let requests = f.env.server.requests();
    assert!(
        requests
            .iter()
            .all(|(_, _, agent)| agent.as_deref() == Some("synthetic-agent"))
    );
    // Neither the streams nor the output stay behind.
    assert_eq!(f.env.staging_files(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn video_and_sound_are_joined_and_continued_with_fresh_links_after_a_pause() {
    let f = fixture(
        true,
        Formats {
            // The picture is asked for in chunks, as the host requires.
            streams: vec![
                ("/video", 24 * KIB as u64, Some(8 * KIB as u64)),
                ("/sound", 6 * KIB as u64, None),
            ],
            recipe: Recipe::Join(Container::Mp4),
            extension: "mp4",
            version: 1,
        },
    );
    let (video, sound) = (body(24 * KIB), body(6 * KIB));
    f.env.server.put("/video", video.clone());
    f.env.server.put("/sound", sound.clone());
    f.env.server.stall("/video", Some(12 * KIB as u64));
    let owner = f.env.start().await;
    let link = collect(&owner).await;
    let id = start(&owner, &link).await;
    wait_for(&owner, id, |j| j.bytes_done == 12 * KIB as u64).await;
    let summary = job(&owner, id).await;
    assert_eq!(summary.bytes_total, Some(30 * KIB as u64));
    let pause = Command::Pause {
        job: id,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let summary = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;

    f.env.server.stall("/video", None);
    let resume = Command::Resume {
        job: id,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    let done = complete(&owner, id).await;
    assert_eq!(
        done.generation, summary.generation,
        "continued, not restarted"
    );
    assert_eq!(f.service.resolves.load(Ordering::SeqCst), 2);
    let mut expected = b"converted:Join(Mp4):".to_vec();
    expected.extend_from_slice(&video);
    expected.extend_from_slice(&sound);
    assert_eq!(f.env.published("A_ synthetic_video.mp4"), expected);
    let ranges: Vec<_> = f
        .env
        .server
        .requests()
        .into_iter()
        .map(|(path, range, _)| (path, range))
        .collect();
    let k = |n: usize| (n * KIB) as u64;
    assert_eq!(
        ranges,
        [
            ("/video".into(), Some((0, Some(k(8))))),
            ("/video".into(), Some((k(8), Some(k(16))))),
            // Continued after the pause from the durable bytes.
            ("/video".into(), Some((k(12), Some(k(20))))),
            ("/video".into(), Some((k(20), Some(k(24))))),
            ("/sound".into(), None),
        ]
    );
    let runs = f.joiner.runs.lock().unwrap().clone();
    assert_eq!(
        runs[0].0,
        [
            Part {
                offset: 0,
                length: k(24)
            },
            Part {
                offset: k(24),
                length: k(6)
            }
        ]
    );
    assert_eq!(f.env.staging_files(), 0);
    shutdown(owner).await;
}

/// Downloads one 6 KiB stream the service says to fetch in 10 MiB chunks,
/// returning the ranges asked for.
async fn one_hinted_chunk(ignores_ranges: bool) -> Vec<Option<(u64, Option<u64>)>> {
    let length = 6 * KIB as u64;
    let f = fixture(
        true,
        Formats {
            streams: vec![("/audio", length, Some(10 << 20))],
            ..audio_only()
        },
    );
    let audio = body(6 * KIB);
    f.env.server.put("/audio", audio.clone());
    f.env
        .server
        .ignores_ranges
        .store(ignores_ranges, Ordering::SeqCst);
    let owner = f.env.start().await;
    let link = collect(&owner).await;
    let id = start(&owner, &link).await;
    complete(&owner, id).await;
    let mut expected = b"converted:Mp3:".to_vec();
    expected.extend_from_slice(&audio);
    assert_eq!(f.env.published("A_ synthetic_video.mp3"), expected);
    shutdown(owner).await;
    f.env
        .server
        .requests()
        .into_iter()
        .map(|(_, range, _)| range)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chunk_hint_ranges_even_a_stream_that_fits_in_one_chunk() {
    // Hosts that hint a chunk size (YouTube) throttle unranged requests.
    assert_eq!(
        one_hinted_chunk(false).await,
        [Some((0, Some(6 * KIB as u64)))]
    );
    // A host that sends the whole stream for that range is not asked again.
    assert_eq!(
        one_hinted_chunk(true).await,
        [Some((0, Some(6 * KIB as u64)))]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_formats_start_again_from_zero() {
    let f = fixture(
        true,
        Formats {
            streams: vec![
                ("/video", 16 * KIB as u64, None),
                ("/sound", 4 * KIB as u64, None),
            ],
            recipe: Recipe::Join(Container::Webm),
            extension: "webm",
            version: 1,
        },
    );
    f.env.server.put("/video", body(16 * KIB));
    f.env.server.put("/sound", body(4 * KIB));
    f.env.server.stall("/video", Some(8 * KIB as u64));
    let owner = f.env.start().await;
    let link = collect(&owner).await;
    let id = start(&owner, &link).await;
    wait_for(&owner, id, |j| j.bytes_done == 8 * KIB as u64).await;
    let summary = job(&owner, id).await;
    let pause = Command::Pause {
        job: id,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, pause), ALL)
        .await
        .unwrap();
    let summary = wait_for(&owner, id, |j| j.stage == JobStage::Paused).await;

    // The service now answers other formats for the same video.
    f.env.server.put("/video2", body(18 * KIB));
    {
        let mut formats = f.service.formats.lock().unwrap();
        formats.streams[0] = ("/video2", 18 * KIB as u64, None);
        formats.version = 2;
    }
    f.env.server.stall("/video", None);
    let resume = Command::Resume {
        job: id,
        expected_revision: summary.revision,
    };
    owner
        .handle()
        .execute(envelope(&owner, resume), ALL)
        .await
        .unwrap();
    let done = complete(&owner, id).await;
    assert!(done.generation > summary.generation);
    assert_eq!(
        done.restart_reason.as_ref().map(|r| r.as_str()),
        Some("the video's formats changed since the saved data was downloaded")
    );
    let published = f.env.published("A_ synthetic_video.webm");
    assert_eq!(published.len(), "converted:Join(Webm):".len() + 22 * KIB);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_converter_only_single_streams_are_offered_and_kept_as_they_are() {
    let f = fixture(
        false,
        Formats {
            streams: vec![("/progressive", 9 * KIB as u64, None)],
            recipe: Recipe::Copy,
            extension: "mp4",
            version: 1,
        },
    );
    let progressive = body(9 * KIB);
    f.env.server.put("/progressive", progressive.clone());
    let owner = f.env.start().await;
    let link = collect(&owner).await;
    let media = link.media.clone().unwrap();
    let available: Vec<_> = media
        .options
        .iter()
        .filter(|o| o.available)
        .map(|o| o.choice)
        .collect();
    assert_eq!(
        available,
        [MediaChoice::Video {
            max_height: Some(360)
        }]
    );
    // The best video, kept until chosen otherwise, shows as that height.
    assert_eq!(
        media.choice,
        MediaChoice::Video {
            max_height: Some(360)
        }
    );
    assert!(media.needs.is_some(), "{media:?}");
    // A choice that needs the converter is refused.
    let refused = choose(
        &owner,
        &link,
        MediaChoice::Audio {
            format: AudioFormat::Mp3,
        },
    )
    .await;
    match refused {
        CommandResult::LinksApplied { applied, refused } => {
            assert!(applied.is_empty());
            assert_eq!(refused.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    // Only the media service's own plugin is listed.
    let QueryResult::Settings(settings) =
        owner.handle().query(Query::Settings {}, ALL).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(settings.plugins.len(), 1);
    assert_eq!(settings.plugins[0].id, "synthetic-resolver");

    let choice = MediaChoice::Video {
        max_height: Some(360),
    };
    choose(&owner, &link, choice).await;
    let link = listed(&owner).await.pop().unwrap();
    let id = start(&owner, &link).await;
    complete(&owner, id).await;
    assert_eq!(f.env.published("A_ synthetic_video.mp4"), progressive);
    assert!(f.joiner.runs.lock().unwrap().is_empty());
    assert_eq!(f.env.staging_files(), 0);
    shutdown(owner).await;
}

/// The conversion outputs in the staging directory.
fn outputs(env: &Env) -> Vec<String> {
    std::fs::read_dir(env.root.join(".wayfetch/staging"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".media"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_conversion_runs_again_from_the_saved_streams_after_a_restart() {
    let f = fixture(true, audio_only());
    let audio = body(10 * KIB);
    f.env.server.put("/audio", audio.clone());
    f.joiner.hang.store(true, Ordering::SeqCst);
    let owner = f.env.start().await;
    let link = collect(&owner).await;
    let id = start(&owner, &link).await;
    wait_for(&owner, id, |j| j.stage == JobStage::Verifying).await;
    // The engine creates the output before polling the converter. Wait for
    // the converter itself, or shutdown may precede the first conversion.
    tokio::time::timeout(Duration::from_secs(10), async {
        while f.joiner.runs.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the first conversion started");
    assert_eq!(outputs(&f.env).len(), 1);
    shutdown(owner).await;
    assert_eq!(f.env.staging_files(), 2);

    // The streams are complete, so no new links are needed.
    f.joiner.hang.store(false, Ordering::SeqCst);
    let owner = f.env.start().await;
    complete(&owner, id).await;
    assert_eq!(f.service.resolves.load(Ordering::SeqCst), 1);
    let mut expected = b"converted:Mp3:".to_vec();
    expected.extend_from_slice(&audio);
    assert_eq!(f.env.published("A_ synthetic_video.mp3"), expected);
    assert_eq!(f.joiner.runs.lock().unwrap().len(), 2);
    assert_eq!(f.env.staging_files(), 0);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_conversion_removes_its_output_and_keeps_the_streams() {
    let f = fixture(true, audio_only());
    f.env.server.put("/audio", body(10 * KIB));
    f.joiner.fail.store(true, Ordering::SeqCst);
    let owner = f.env.start().await;
    let link = collect(&owner).await;
    let id = start(&owner, &link).await;
    let failed = wait_for(&owner, id, |j| j.stage == JobStage::Failed).await;
    assert_eq!(
        failed.failure.unwrap().message.unwrap().as_str(),
        "the converter could not read the streams"
    );
    assert!(outputs(&f.env).is_empty());
    assert_eq!(f.env.staging_files(), 1);
    shutdown(owner).await;
}

/// Turns the synthetic resolver on and off; its updates never reach the
/// release. With `slow`, an update first writes a file that finishes only
/// once `slow` is cleared.
struct Control {
    service: Arc<Service>,
    changes: AtomicUsize,
    slow: Arc<AtomicBool>,
    written: Arc<AtomicBool>,
}

impl Control {
    fn new(service: Arc<Service>) -> Self {
        Self {
            service,
            changes: AtomicUsize::new(0),
            slow: Arc::new(AtomicBool::new(false)),
            written: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl PluginControl for Control {
    fn change<'a>(
        &'a self,
        plugin: &'a PluginId,
        change: PluginChange,
        work: PluginWork,
    ) -> BoxFuture<'a, Result<PluginOutcome, PluginError>> {
        Box::pin(async move {
            self.changes.fetch_add(1, Ordering::SeqCst);
            if plugin.as_str() != "synthetic-resolver" {
                return Err(PluginError::Unknown(plugin.clone()));
            }
            let enabled = match change {
                PluginChange::Enable => true,
                PluginChange::Disable => false,
                PluginChange::Update => {
                    if self.slow.load(Ordering::SeqCst) {
                        let (slow, written) = (self.slow.clone(), self.written.clone());
                        work.blocking(move || {
                            while slow.load(Ordering::SeqCst) {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            written.store(true, Ordering::SeqCst);
                        })
                        .await?;
                    }
                    return Err(PluginError::Unreachable(
                        "the release could not be fetched".into(),
                    ));
                }
                PluginChange::Remove => {
                    return Err(PluginError::NotApplicable("nothing was installed".into()));
                }
            };
            let was = self.service.enabled.swap(enabled, Ordering::SeqCst);
            Ok(PluginOutcome {
                changed: was != enabled,
                version: Some("1".into()),
            })
        })
    }
}

fn change_plugin(plugin: &str, change: PluginChange) -> Command {
    Command::ChangePlugin {
        plugin: PluginId::new(plugin).unwrap(),
        change,
    }
}

async fn settings_plugins(owner: &EngineOwner) -> (u64, Vec<PluginSummary>) {
    match owner.handle().query(Query::Settings {}, ALL).await.unwrap() {
        QueryResult::Settings(settings) => (settings.revision.get(), settings.plugins),
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_turned_off_stops_its_links_until_it_is_turned_on_again() {
    let mut f = fixture(true, audio_only());
    let control = Arc::new(Control::new(f.service.clone()));
    f.env.plugin_control = Some(control.clone());
    let owner = f.env.start().await;
    let (before, _) = settings_plugins(&owner).await;

    let off = envelope(
        &owner,
        change_plugin("synthetic-resolver", PluginChange::Disable),
    );
    let result = owner.handle().execute(off.clone(), ALL).await.unwrap();
    let expected = CommandResult::PluginChanged(PluginChanged {
        plugin: PluginId::new("synthetic-resolver").unwrap(),
        changed: true,
        version: Some("1".into()),
    });
    assert_eq!(result, expected);
    // A retry replays the recorded result without changing it again.
    assert_eq!(owner.handle().execute(off, ALL).await.unwrap(), expected);
    assert_eq!(control.changes.load(Ordering::SeqCst), 1);
    let (after, plugins) = settings_plugins(&owner).await;
    assert!(
        after > before,
        "the change is announced as a settings change"
    );
    let resolver = plugins
        .iter()
        .find(|p| p.id == "synthetic-resolver")
        .unwrap();
    assert!(!resolver.enabled);

    // Its links are refused clearly, not downloaded some other way.
    let link = collect(&owner).await;
    let media = link.media.clone().unwrap();
    assert_eq!(media.state, MediaState::Unavailable);
    assert_eq!(
        media.problem.as_ref().map(|p| p.as_str()),
        Some(
            "This needs the Synthetic resolver plugin, which is turned off. See Settings > Plugins."
        )
    );

    // Turned on again, the link's failed check runs again.
    let on = envelope(
        &owner,
        change_plugin("synthetic-resolver", PluginChange::Enable),
    );
    owner.handle().execute(on, ALL).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while listed(&owner).await[0].media.as_ref().unwrap().state != MediaState::Ready {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Failures say what happened, and change nothing.
    let steady = settings_plugins(&owner).await.0;
    let refused = |command| async {
        owner
            .handle()
            .execute(envelope(&owner, command), ALL)
            .await
            .unwrap_err()
            .code
    };
    assert_eq!(
        refused(change_plugin("synthetic-resolver", PluginChange::Update)).await,
        ErrorCode::Busy
    );
    assert_eq!(
        refused(change_plugin("synthetic-resolver", PluginChange::Remove)).await,
        ErrorCode::NotApplicable
    );
    assert_eq!(
        refused(change_plugin("other", PluginChange::Enable)).await,
        ErrorCode::NotFound
    );
    assert_eq!(settings_plugins(&owner).await.0, steady);
    shutdown(owner).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_cannot_be_changed_without_a_host_that_can() {
    let f = fixture(true, audio_only());
    let owner = f.env.start().await;
    let command = change_plugin("synthetic-resolver", PluginChange::Disable);
    let err = owner
        .handle()
        .execute(envelope(&owner, command), ALL)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotApplicable);
    shutdown(owner).await;
}

/// Starts a slow update of the synthetic resolver, answering once its
/// file write has begun.
async fn slow_update(
    f: &mut Fixture,
) -> (
    EngineOwner,
    Arc<Control>,
    wayfetch_protocol::CommandEnvelope,
    tokio::task::JoinHandle<Result<CommandResult, wayfetch_protocol::ProtocolError>>,
) {
    let control = Arc::new(Control::new(f.service.clone()));
    control.slow.store(true, Ordering::SeqCst);
    f.env.plugin_control = Some(control.clone());
    let owner = f.env.start().await;
    let update = envelope(
        &owner,
        change_plugin("synthetic-resolver", PluginChange::Update),
    );
    let (handle, sent) = (owner.handle(), update.clone());
    let running = tokio::spawn(async move { handle.execute(sent, ALL).await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while control.changes.load(Ordering::SeqCst) == 0 {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (owner, control, update, running)
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_keeps_the_state_directory_until_a_plugin_write_ends() {
    let mut f = fixture(true, audio_only());
    let (owner, control, _, _running) = slow_update(&mut f).await;
    let slow = control.slow.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        slow.store(false, Ordering::SeqCst);
    });
    let report = owner
        .shutdown(ShutdownRequest {
            timeout: Some(Duration::from_secs(10)),
        })
        .await;
    assert!(report.clean && report.state_dir_released, "{report:?}");
    assert!(
        control.written.load(Ordering::SeqCst),
        "the lock was released before the write ended"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_held_by_a_running_plugin_change_starts_nothing_else() {
    let mut f = fixture(true, audio_only());
    let (owner, control, update, running) = slow_update(&mut f).await;
    let reuse = |command| {
        let mut envelope = update.clone();
        envelope.command = command;
        let handle = owner.handle();
        async move { handle.execute(envelope, ALL).await.unwrap_err().code }
    };
    assert_eq!(
        reuse(change_plugin("other", PluginChange::Enable)).await,
        ErrorCode::IdempotencyConflict
    );
    assert_eq!(
        reuse(change_plugin("synthetic-resolver", PluginChange::Disable)).await,
        ErrorCode::IdempotencyConflict
    );
    // A retry waits for the change to end.
    assert_eq!(reuse(update.command.clone()).await, ErrorCode::Busy);
    assert_eq!(control.changes.load(Ordering::SeqCst), 1);
    control.slow.store(false, Ordering::SeqCst);
    assert_eq!(running.await.unwrap().unwrap_err().code, ErrorCode::Busy);
    assert!(f.service.enabled.load(Ordering::SeqCst), "nothing else ran");
    assert_eq!(control.changes.load(Ordering::SeqCst), 1);
    shutdown(owner).await;
}
