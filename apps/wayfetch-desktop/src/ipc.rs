//! The renderer's IPC commands: a thin translation to the engine's
//! transport-neutral facade, like `wayfetch-api`'s routes for HTTP
//! (architecture brief sections 4.3 and 4.7, ADR-33).
//!
//! | Command | Engine operation |
//! |---|---|
//! | `hello` | handshake: instance, boot epoch, protocol, head, limits |
//! | `query` | a [`Query`]: list, inspect, or count jobs, settings, a job's link, collected links, accounts, a sign-in |
//! | `submit` | [`SubmitRequest`] as a submit command |
//! | `job_command` | [`JobCommandRequest`]: pause, resume, cancel, retry, rename, or remove |
//! | `collect_links` | [`CollectRequest`]: collect links for review |
//! | `links_command` | [`LinksCommandRequest`]: start, remove, or move collected links |
//! | `update_settings` | [`UpdateSettingsRequest`]: change the default destination or a preference |
//! | `remove_root` | [`RemoveRootRequest`]: stop offering a folder the user added |
//! | `change_plugin` | [`ChangePluginRequest`]: turn a plugin on or off, update it, or remove its installed copy |
//! | `authorize_account` | [`AuthorizeAccountRequest`]: starts signing a provider account in |
//! | `cancel_authorization` | stops a sign-in still waiting for approval |
//! | `account_command` | [`AccountCommandRequest`]: enable, disable, or remove an account |
//! | `refresh_account` | checks an account with its provider |
//! | `choose_download_folder` | the native folder dialog; the chosen folder becomes a permitted destination |
//! | `open_file` | opens a finished job's file with this computer's default application |
//! | `reveal_file` | opens the folder holding a finished job's file |
//! | `subscribe` | an event subscription, delivered on a channel |
//! | `acknowledge` | the renderer has handled a subscription's items |
//! | `unsubscribe` | ends a subscription |
//!
//! These are the only commands the renderer can call: the build declares
//! them, and the window's capability grants them and no Tauri plugin, so
//! the renderer has no filesystem, database, shell, or network access.
//! Downloads go only to roots the engine permits: configured ones, and
//! folders the user picked in the host's native dialog. The renderer never
//! sends a path; it chooses among roots by ID, and names a job to open,
//! whose file the engine locates and checks is still the one it published.
//!
//! Arguments and results are the protocol's own JSON, the same the HTTP
//! API carries, and errors are [`ProtocolError`]s, so idempotency,
//! revision conflicts, wrong-instance refusals, and resynchronization
//! behave as they do over HTTP. The window belongs to the person signed in
//! to this computer's session, so the host grants it the local owner's
//! permissions (section 4.6), the `accounts` permission among them;
//! nothing from the renderer can widen them. No command takes or returns a
//! provider credential.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::Value;
use tauri::ipc::Channel;
use tauri::{State, Webview, WebviewWindow};
use wayfetch_engine::EngineHandle;
use wayfetch_engine::domain::id::{AccountId, JobId};
use wayfetch_engine::protocol::account::{AccountSummary, Authorization, AuthorizationId};
use wayfetch_engine::protocol::command::CommandResult;
use wayfetch_engine::protocol::query::RootSummary;
use wayfetch_engine::protocol::request::{
    AccountCommandRequest, AuthorizeAccountRequest, ChangePluginRequest, CollectRequest,
    JobCommandRequest, LinksCommandRequest, RemoveRootRequest, SubmitRequest,
    UpdateSettingsRequest,
};
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

/// Collects links for review without starting them.
#[tauri::command]
pub async fn collect_links(
    desktop: State<'_, Desktop>,
    request: Value,
) -> IpcResult<CommandResult> {
    let envelope = decode::<CollectRequest>(request)?.into_envelope()?;
    desktop.engine.execute(envelope, GRANTED).await
}

/// Starts, removes, or moves collected links at the revisions the renderer
/// saw.
#[tauri::command]
pub async fn links_command(
    desktop: State<'_, Desktop>,
    request: Value,
) -> IpcResult<CommandResult> {
    let envelope = decode::<LinksCommandRequest>(request)?.into_envelope()?;
    desktop.engine.execute(envelope, GRANTED).await
}

/// Changes the default destination or a preference at the settings
/// revision the renderer saw.
#[tauri::command]
pub async fn update_settings(
    desktop: State<'_, Desktop>,
    request: Value,
) -> IpcResult<CommandResult> {
    let envelope = decode::<UpdateSettingsRequest>(request)?.into_envelope();
    desktop.engine.execute(envelope, GRANTED).await
}

/// Stops offering a folder the user added as a destination; its files
/// stay where they are.
#[tauri::command]
pub async fn remove_root(desktop: State<'_, Desktop>, request: Value) -> IpcResult<CommandResult> {
    let envelope = decode::<RemoveRootRequest>(request)?.into_envelope();
    desktop.engine.execute(envelope, GRANTED).await
}

/// Turns a plugin on or off, updates it, or removes the copy Wayfetch
/// installed.
#[tauri::command]
pub async fn change_plugin(
    desktop: State<'_, Desktop>,
    request: Value,
) -> IpcResult<CommandResult> {
    let envelope = decode::<ChangePluginRequest>(request)?.into_envelope();
    desktop.engine.execute(envelope, GRANTED).await
}

/// Starts signing a provider account in, or signing one in again. The
/// person approves on the provider's site; the renderer follows the
/// sign-in with [`Query::Authorization`].
#[tauri::command]
pub async fn authorize_account(
    desktop: State<'_, Desktop>,
    request: Value,
) -> IpcResult<CommandResult> {
    accounts::authorize(&desktop.engine, GRANTED, request).await
}

/// Cancels a sign-in still waiting for approval.
#[tauri::command]
pub async fn cancel_authorization(
    desktop: State<'_, Desktop>,
    authorization: Value,
) -> IpcResult<Authorization> {
    accounts::cancel(&desktop.engine, GRANTED, authorization).await
}

/// Enables, disables, or removes an account at the revision the renderer
/// saw.
#[tauri::command]
pub async fn account_command(
    desktop: State<'_, Desktop>,
    account: Value,
    request: Value,
) -> IpcResult<CommandResult> {
    accounts::command(&desktop.engine, GRANTED, account, request).await
}

/// Checks an account with its provider now, and answers with it.
#[tauri::command]
pub async fn refresh_account(
    desktop: State<'_, Desktop>,
    account: Value,
) -> IpcResult<AccountSummary> {
    accounts::refresh(&desktop.engine, GRANTED, account).await
}

/// The account commands with the permissions they run with, so they can be
/// exercised with fewer than the local owner's.
mod accounts {
    use super::*;

    pub async fn authorize(
        engine: &EngineHandle,
        granted: &[Permission],
        request: Value,
    ) -> IpcResult<CommandResult> {
        let envelope = decode::<AuthorizeAccountRequest>(request)?.into_envelope()?;
        engine.execute(envelope, granted).await
    }

    pub async fn cancel(
        engine: &EngineHandle,
        granted: &[Permission],
        authorization: Value,
    ) -> IpcResult<Authorization> {
        let id: AuthorizationId = decode(authorization)?;
        engine.cancel_authorization(id, granted).await
    }

    pub async fn command(
        engine: &EngineHandle,
        granted: &[Permission],
        account: Value,
        request: Value,
    ) -> IpcResult<CommandResult> {
        let account: AccountId = decode(account)?;
        let envelope = decode::<AccountCommandRequest>(request)?.into_envelope(account);
        engine.execute(envelope, granted).await
    }

    pub async fn refresh(
        engine: &EngineHandle,
        granted: &[Permission],
        account: Value,
    ) -> IpcResult<AccountSummary> {
        let account: AccountId = decode(account)?;
        engine.refresh_account(account, granted).await
    }
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

/// Opens a finished job's file with this computer's default application.
#[tauri::command]
pub async fn open_file(desktop: State<'_, Desktop>, job: Value) -> IpcResult<()> {
    let file = desktop.engine.published_path(decode(job)?, GRANTED).await?;
    launch(file).await
}

/// Opens the folder holding a finished job's file in the file manager.
#[tauri::command]
pub async fn reveal_file(desktop: State<'_, Desktop>, job: Value) -> IpcResult<()> {
    let file = desktop.engine.published_path(decode(job)?, GRANTED).await?;
    let folder = file.parent().map(Path::to_owned).ok_or_else(not_opened)?;
    launch(folder).await
}

/// Hands `path`, which the engine located, to the desktop's handler for
/// it. `opener` runs `xdg-open` with the path as one argument, never
/// through a shell command line, and the path is absolute, so it is never
/// read as an option or a URL. The handler runs detached: closing what it
/// opens is the user's business, and the engine is not involved.
async fn launch(path: PathBuf) -> IpcResult<()> {
    debug_assert!(path.is_absolute());
    tokio::task::spawn_blocking(move || opener::open(&path))
        .await
        .map_err(|_| not_opened())?
        .map_err(|_| not_opened())
}

fn not_opened() -> ProtocolError {
    ProtocolError::new(
        ErrorCode::NotApplicable,
        "this computer has no application to open it",
    )
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
        let signing_in_with_a_password = decode::<AuthorizeAccountRequest>(json!({
            "idempotency_key": "6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11",
            "target": "6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a12",
            "service": "real-debrid",
            "password": "synthetic",
        }));
        assert_eq!(
            signing_in_with_a_password.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    /// The account commands pass on only the permissions granted: without
    /// `accounts`, nothing reaches the engine's account work.
    #[tokio::test(flavor = "multi_thread")]
    async fn account_commands_need_the_accounts_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let config = wayfetch_engine::EngineConfig::new(tmp.path().join("state"));
        let owner = wayfetch_engine::EngineBuilder::new(config, tokio::runtime::Handle::current())
            .start()
            .await
            .unwrap();
        let engine = owner.handle();
        let instance = owner.identity().instance;
        let others = [
            Permission::Read,
            Permission::Submit,
            Permission::Control,
            Permission::Sources,
        ];
        let request = |fields: Value| {
            let mut body = json!({
                "idempotency_key": "6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a11",
                "target": instance,
            });
            body.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            body
        };
        let denied = ErrorCode::PermissionDenied;
        let sign_in = request(json!({"service": "real-debrid"}));
        let remove = request(json!({"action": "remove", "expected_revision": 1}));
        let sign_in_id = json!("6f1c2c5e-6c1e-4b8a-9f57-3a8f9f0f6a13");
        assert_eq!(
            accounts::authorize(&engine, &others, sign_in.clone())
                .await
                .unwrap_err()
                .code,
            denied
        );
        assert_eq!(
            accounts::command(&engine, &others, json!("acct-1"), remove)
                .await
                .unwrap_err()
                .code,
            denied
        );
        assert_eq!(
            accounts::refresh(&engine, &others, json!("acct-1"))
                .await
                .unwrap_err()
                .code,
            denied
        );
        assert_eq!(
            accounts::cancel(&engine, &others, sign_in_id.clone())
                .await
                .unwrap_err()
                .code,
            denied
        );
        // The local owner's permissions reach the engine, which answers on
        // its merits: no account integration was configured here.
        let err = accounts::authorize(&engine, GRANTED, sign_in)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotApplicable);
        let err = accounts::cancel(&engine, GRANTED, sign_in_id)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        owner
            .shutdown(wayfetch_engine::ShutdownRequest::default())
            .await;
    }
}
