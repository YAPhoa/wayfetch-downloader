//! The native folder picker behind "Choose folder…" (architecture brief
//! section 8.6): the only way a path reaches the engine from the desktop
//! UI. The renderer cannot name a path; it can only ask the host to show
//! the dialog, and the host hands the chosen folder to the engine, which
//! validates and initializes it like `init-root`.
//!
//! The dialog is `rfd`'s GTK dialog, created on the main thread and awaited
//! elsewhere, as Tauri's own dialog plugin does, without registering the
//! plugin's IPC commands. One dialog is open at a time.
//!
//! With the `scripted-picker` feature, for the real-window tests only, the
//! dialog is replaced by the contents of the file named in
//! `WAYFETCH_DESKTOP_PICKER`: a folder's path, or nothing for a cancelled
//! dialog.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::WebviewWindow;
use wayfetch_engine::protocol::{ErrorCode, ProtocolError};

/// Whether a dialog is open.
#[derive(Debug, Default)]
pub struct Picker {
    open: AtomicBool,
}

/// Clears [`Picker::open`] when the dialog ends, however it ends.
struct Open<'a>(&'a AtomicBool);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Picker {
    /// Shows the folder dialog over `window`. `None` if the user cancelled.
    pub async fn choose(&self, window: WebviewWindow) -> Result<Option<PathBuf>, ProtocolError> {
        if self.open.swap(true, Ordering::AcqRel) {
            return Err(ProtocolError::new(
                ErrorCode::Busy,
                "a folder dialog is already open",
            ));
        }
        let _open = Open(&self.open);
        pick(window).await
    }
}

#[cfg(not(feature = "scripted-picker"))]
async fn pick(window: WebviewWindow) -> Result<Option<PathBuf>, ProtocolError> {
    let failed = || ProtocolError::new(ErrorCode::Internal, "the folder dialog could not open");
    let (send, dialog) = tokio::sync::oneshot::channel();
    let parent = window.clone();
    window
        .run_on_main_thread(move || {
            let dialog = rfd::AsyncFileDialog::new()
                .set_title("Choose a download folder")
                .set_can_create_directories(true)
                .set_parent(&parent)
                .pick_folder();
            let _ = send.send(dialog);
        })
        .map_err(|_| failed())?;
    let chosen = dialog.await.map_err(|_| failed())?.await;
    Ok(chosen.map(|folder| folder.path().to_owned()))
}

#[cfg(feature = "scripted-picker")]
async fn pick(_window: WebviewWindow) -> Result<Option<PathBuf>, ProtocolError> {
    let script = std::env::var_os("WAYFETCH_DESKTOP_PICKER")
        .ok_or_else(|| ProtocolError::new(ErrorCode::Internal, "no scripted answer"))?;
    let answer = std::fs::read_to_string(script)
        .map_err(|_| ProtocolError::new(ErrorCode::Internal, "no scripted answer"))?;
    let answer = answer.trim_end_matches('\n');
    Ok((!answer.is_empty()).then(|| PathBuf::from(answer)))
}
