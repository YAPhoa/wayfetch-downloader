//! What the daemon's tests share, whether they run `wayfetchd` as a
//! subprocess or in its container: an API client, stalling fixture files,
//! and the sizes and deadlines they assume. Each including test file also
//! includes the net and engine test support as `support` and
//! `engine_support`.

#![allow(dead_code, reason = "each test file uses part of the support")]

use std::time::Duration;

use bytes::Bytes;
use eventsource_stream::{Event, Eventsource};
use futures_util::{Stream, StreamExt};
use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::engine_support::body;
use crate::support::{Resource, Server};

/// How long any one wait may take.
pub const DEADLINE: Duration = Duration::from_secs(30);
pub const MIB: usize = 1024 * 1024;
/// The engine's default checkpoint interval.
pub const CHECKPOINT: u64 = 8 * MIB as u64;
/// Size of stalling files, and where they stall: past two checkpoints.
pub const SIZE: usize = 20 * MIB + 11;
pub const STALL: usize = 17 * MIB + 5;
/// Durable bytes once a transfer stalled at [`STALL`] has settled.
pub const SETTLED: u64 = 2 * CHECKPOINT;
/// A token for every permission.
pub const OWNER_TOKEN: &str = "owner-0123456789abcdef0123456789abcdef";

/// An HTTP client of one daemon.
#[derive(Clone)]
pub struct Client {
    pub http: reqwest::Client,
    pub base: String,
    pub token: Option<String>,
    pub instance: String,
}

impl Client {
    /// A client of the daemon at `base` (`http://ADDR`) serving
    /// `instance`, sending `token` if any.
    pub fn new(base: &str, instance: &str, token: Option<&str>) -> Self {
        // The workspace builds reqwest without a default TLS provider; a
        // plain-HTTP client still needs one installed.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        Self {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base: base.to_owned(),
            token: token.map(str::to_owned),
            instance: instance.to_owned(),
        }
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self.http.request(method, format!("{}{path}", self.base));
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    pub async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    pub async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(self.request(reqwest::Method::GET, path)).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> (StatusCode, Value) {
        self.send(self.request(reqwest::Method::POST, path).json(body))
            .await
    }

    /// A command body for this daemon's instance with a fresh key.
    pub fn command(&self, fields: Value) -> Value {
        let mut body = json!({
            "idempotency_key": fresh_key(),
            "target": self.instance,
        });
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        body
    }

    /// Submits `url` and returns the new job's ID.
    pub async fn submit(&self, url: &str) -> String {
        let known = self.jobs().await.len();
        let (status, result) = self
            .post("/v1/inputs", &self.command(json!({"items": [url]})))
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["type"], "submitted", "{result}");
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let jobs = self.jobs().await;
            if jobs.len() > known {
                return jobs[known]["id"].as_str().unwrap().to_owned();
            }
            assert!(tokio::time::Instant::now() < deadline, "no job added");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Every job, a page at a time.
    pub async fn jobs(&self) -> Vec<Value> {
        let mut jobs = Vec::new();
        let mut path = "/v1/jobs?page_size=2".to_owned();
        loop {
            let (status, page) = self.get(&path).await;
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(page["type"], "jobs", "{page}");
            jobs.extend(page["items"].as_array().unwrap().iter().cloned());
            match page["next_after"].as_str() {
                Some(after) => path = format!("/v1/jobs?page_size=2&after={after}"),
                None => return jobs,
            }
        }
    }

    pub async fn job(&self, id: &str) -> Value {
        let (status, result) = self.get(&format!("/v1/jobs/{id}")).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        result["job"].clone()
    }

    /// Polls job `id` until `done` holds.
    pub async fn wait(&self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + DEADLINE;
        loop {
            let job = self.job(id).await;
            if done(&job) {
                return job;
            }
            let parked = matches!(job["stage"].as_str(), Some("failed" | "blocked"));
            assert!(!parked, "{job}");
            assert!(tokio::time::Instant::now() < deadline, "{job}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn stage(&self, id: &str, stage: &str) -> Value {
        self.wait(id, |job| job["stage"] == stage).await
    }

    /// Waits until a stalled transfer's durable bytes settle.
    pub async fn settled(&self, id: &str) -> u64 {
        let job = self
            .wait(id, |job| job["bytes_done"].as_u64().unwrap() >= SETTLED)
            .await;
        let done = job["bytes_done"].as_u64().unwrap();
        assert!(done <= STALL as u64, "{done}");
        done
    }

    /// Sends `action` for job `id` at its current revision.
    pub async fn control(&self, id: &str, action: &str) -> (StatusCode, Value) {
        let revision = self.job(id).await["revision"].clone();
        let body = self.command(json!({"action": action, "expected_revision": revision}));
        self.post(&format!("/v1/jobs/{id}/commands"), &body).await
    }

    /// Opens the event stream, resuming after `last_event_id` if given.
    pub async fn events(&self, last_event_id: Option<&str>) -> Events {
        let mut request = self.request(reqwest::Method::GET, "/v1/events");
        if let Some(id) = last_event_id {
            request = request.header("last-event-id", id);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/event-stream",
            "{response:?}"
        );
        Events(Box::pin(response.bytes_stream().eventsource()))
    }
}

pub type EventResult = Result<Event, eventsource_stream::EventStreamError<reqwest::Error>>;

/// An open event stream.
pub struct Events(pub std::pin::Pin<Box<dyn Stream<Item = EventResult> + Send>>);

impl Events {
    /// The next event, or `None` once the stream ended.
    pub async fn next(&mut self) -> Option<Event> {
        tokio::time::timeout(DEADLINE, self.0.next())
            .await
            .expect("an event in time")
            .map(|e| e.unwrap())
    }

    /// Reads until an event satisfies `done`, and returns it with its data.
    pub async fn until(&mut self, done: impl Fn(&Event, &Value) -> bool) -> (Event, Value) {
        loop {
            let event = self.next().await.expect("the stream continues");
            let data: Value = serde_json::from_str(&event.data).unwrap();
            if done(&event, &data) {
                return (event, data);
            }
        }
    }

    /// Reads until job `id` reaches `stage`; returns the event.
    pub async fn stage(&mut self, id: &str, stage: &str) -> Event {
        self.until(|e, data| {
            e.event == "event"
                && data.pointer("/kind/job/id") == Some(&id.into())
                && data.pointer("/kind/job/stage") == Some(&stage.into())
        })
        .await
        .0
    }
}

pub fn fresh_key() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A file served at `path`, stalled at [`STALL`] until released.
pub fn stalled(server: &Server, path: &str) -> Bytes {
    let data = body(SIZE);
    server.put(
        path,
        Resource {
            stall_at: Some(STALL),
            ..Resource::new(data.clone())
        },
    );
    data
}

pub fn release(server: &Server, path: &str) {
    server.update(path, |r| r.stall_at = None);
}

pub fn write_private(path: &std::path::Path, text: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}
