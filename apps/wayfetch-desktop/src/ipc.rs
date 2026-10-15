//! The renderer's IPC commands: a thin translation to the engine's
//! transport-neutral facade, like `wayfetch-api`'s routes for HTTP
//! (architecture brief sections 4.3 and 4.7, ADR-33).
//!
//! | Command | Engine operation |
//! |---|---|
//! | `hello` | handshake: instance, boot epoch, protocol, head, limits |
//! | `query` | a [`Query`]: list, inspect, or count jobs, settings, a job's link |
//! | `submit` | [`SubmitRequest`] as a submit command |
//! | `job_command` | [`JobCommandRequest`]: pause, resume, cancel, rename, or remove |
//! | `update_settings` | [`UpdateSettingsRequest`]: choose the default destination |
//! | `choose_download_folder` | the native folder dialog; the chosen folder becomes a permitted destination |
//! | `subscribe` | an event subscription, delivered on a channel |
//! | `acknowledge` | the renderer has handled a subscription's items |
//! | `unsubscribe` | ends a subscription |
//!
//! These are the only commands the renderer can call: the build declares
//! them, and the window's capability grants them and no Tauri plugin, so
//! the renderer has no filesystem, database, shell, or network access.
//! Downloads go only to roots the engine permits: configured ones, and
//! folders the user picked in the host's native dialog. The renderer never
//! sends a path; it chooses among roots by ID.
//!
//! Arguments and results are the protocol's own JSON, the same the HTTP
//! API carries, and errors are [`ProtocolError`]s, so idempotency,
//! revision conflicts, wrong-instance refusals, and resynchronization
//! behave as they do over HTTP. The window belongs to the person signed in
//! to this computer's session, so the host grants it the local owner's
//! permissions (section 4.6); nothing from the renderer can widen them.

use serde::de::DeserializeOwned;
use serde_json::Value;
use tauri::ipc::Channel;
use tauri::{State, Webview, WebviewWindow};
use wayfetch_engine::EngineHandle;
use wayfetch_engine::domain::id::JobId;
use wayfetch_engine::protocol::command::CommandResult;
use wayfetch_engine::protocol::query::RootSummary;
use wayfetch_engine::protocol::request::{JobCommandRequest, SubmitRequest, UpdateSettingsRequest};
use wayfetch_engine::protocol::{
    ErrorCode, EventCursor, Hello, Permission, ProtocolError, Query, QueryResult, StreamItem,
    Subscribe,
};

use crate::picker::Picker;
use crate::subscriptions::{SubscriptionId, Subscriptions};

/// What the local owner may do.
const GRANTED: &[Permission] = Permission::ALL;

/// What every command shares.
pub struct Desktop {
    /// The embedded engine.
    pub engine: EngineHandle,
    /// Open event subscriptions.
    pub subscriptions: Subscriptions,
    /// The native folder dialog.
    pub picker: Picker,
}

type IpcResult<T> = Result<T, ProtocolError>;

/// The handshake.
#[tauri::command]
pub async fn hello(desktop: State<'_, Desktop>) -> IpcResult<Hello> {
    desktop.engine.hello().await
}

/// A read query.
#[tauri::command]
pub async fn query(desktop: State<'_, Desktop>, query: Value) -> IpcResult<QueryResult> {
    let query: Query = decode(query)?;
    desktop.engine.query(query, GRANTED).await
}

/// Submits a batch of URLs.
#[tauri::command]
pub async fn submit(desktop: State<'_, Desktop>, request: Value) -> IpcResult<CommandResult> {
    let envelope = decode::<SubmitRequest>(request)?.into_envelope()?;
    desktop.engine.execute(envelope, GRANTED).await
}

/// Pauses, resumes, cancels, renames, or removes a job at the revision the
/// renderer saw.
#[tauri::command]
pub async fn job_command(
    desktop: State<'_, Desktop>,
    job: Value,
    request: Value,
) -> IpcResult<CommandResult> {
    let job: JobId = decode(job)?;
    let envelope = decode::<JobCommandRequest>(request)?.into_envelope(job)?;
    desktop.engine.execute(envelope, GRANTED).await
}

/// Chooses the default destination at the settings revision the renderer
/// saw.
#[tauri::command]
pub async fn update_settings(
    desktop: State<'_, Desktop>,
    request: Value,
) -> IpcResult<CommandResult> {
    let envelope = decode::<UpdateSettingsRequest>(request)?.into_envelope();
    desktop.engine.execute(envelope, GRANTED).await
}

/// Shows the native folder dialog and permits the folder the user chose as
/// a destination, initializing it as a root. `null` if the dialog was
/// cancelled, which changes nothing. Making the folder the default is a
/// separate [`update_settings`].
#[tauri::command]
pub async fn choose_download_folder(
    window: WebviewWindow,
    desktop: State<'_, Desktop>,
) -> IpcResult<Option<RootSummary>> {
    let Some(folder) = desktop.picker.choose(window).await? else {
        return Ok(None);
    };
    desktop
        .engine
        .register_root(folder, GRANTED)
        .await
        .map(Some)
}

/// Follows durable events after `after`, or from now. Items arrive on
/// `channel`, then `null` if the stream ends for any reason but a resync
/// or [`unsubscribe`]. Returns the subscription's ID, for acknowledging
/// and ending it.
#[tauri::command]
pub async fn subscribe(
    webview: Webview,
    desktop: State<'_, Desktop>,
    after: Value,
    channel: Channel<Option<StreamItem>>,
) -> IpcResult<SubscriptionId> {
    let after: Option<EventCursor> = decode(after)?;
    // A slot first, so a renderer at its limit costs the engine nothing.
    let slot = desktop.subscriptions.reserve(webview.label())?;
    let subscription = desktop
        .engine
        .subscribe(Subscribe { after }, GRANTED)
        .await?;
    Ok(slot.start(subscription, move |item| channel.send(item).is_ok()))
}

/// Records that the renderer has handled the first `received` items of a
/// subscription, which lets more be sent.
#[tauri::command]
pub fn acknowledge(
    webview: Webview,
    desktop: State<'_, Desktop>,
    subscription: SubscriptionId,
    received: u64,
) {
    desktop
        .subscriptions
        .acknowledge(webview.label(), subscription, received);
}

/// Ends a subscription. Jobs are unaffected.
#[tauri::command]
pub fn unsubscribe(webview: Webview, desktop: State<'_, Desktop>, subscription: SubscriptionId) {
    desktop.subscriptions.close(webview.label(), subscription);
}

/// Decodes an argument with the protocol's validation. Limits the
/// protocol enforces while decoding keep their code, as over HTTP; any
/// other malformed argument is an invalid request.
fn decode<T: DeserializeOwned>(value: Value) -> IpcResult<T> {
    serde_json::from_value(value).map_err(|e| {
        let message = e.to_string();
        // A protocol type's own refusal is written `<code>: <message>`.
        let code = if message.starts_with(&format!("{}:", ErrorCode::LimitExceeded)) {
            ErrorCode::LimitExceeded
        } else {
            ErrorCode::InvalidRequest
        };
        ProtocolError::new(code, &message)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arguments_are_validated_like_http_requests() {
        let too_big = decode::<Query>(json!({"type": "list_jobs", "page_size": 501}));
        assert_eq!(too_big.unwrap_err().code, ErrorCode::LimitExceeded);
        let unknown = decode::<Query>(json!({"type": "list_jobs", "sql": "1=1"}));
        assert_eq!(unknown.unwrap_err().code, ErrorCode::InvalidRequest);
        let privileged = decode::<SubmitRequest>(json!({
            "idempotency_key": "6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11",
            "target": "6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a12",
            "items": ["https://a.example/"],
            "is_admin": true,
        }));
        assert_eq!(privileged.unwrap_err().code, ErrorCode::InvalidRequest);
        assert!(decode::<JobId>(json!("job-0")).is_err());
        assert_eq!(decode::<Option<EventCursor>>(json!(null)).unwrap(), None);
    }
}
