// The app's own commands are reachable from Connect, which the sidecar serves
// over http://127.0.0.1 — a *remote* origin as far as Tauri is concerned. Tauri
// refuses to dispatch a command to a remote origin unless an explicit ACL
// permission grants it (webview/mod.rs: "remote content can never reach custom
// commands unless an explicit `remote` capability has been configured"), and
// those permissions only exist if the app declares a manifest for its commands.
// Declaring them here generates `allow-<command>` / `deny-<command>`, which
// capabilities/*.json then grant.
fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new().app_manifest(
            tauri_build::AppManifest::new().commands(&[
                "take_launch_files",
                "read_launch_file",
                "log_from_webview",
            ]),
        ),
    )
    .expect("failed to run tauri-build");
}
