//! Embeds the shared web UI and declares the IPC commands, so the renderer
//! can call only the commands its capability grants
//! (`capabilities/main.json`).

/// Every command the renderer may call; see `src/ipc.rs`.
const COMMANDS: &[&str] = &[
    "hello",
    "query",
    "submit",
    "job_command",
    "subscribe",
    "acknowledge",
    "unsubscribe",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("the desktop shell's Tauri configuration is valid");
}
