//! Opens a page in the person's own browser. The app has one window and no
//! popups, so a sign-in that navigates that window away leaves no way back.

use tauri::AppHandle;
use tauri_plugin_shell::ShellExt;

/// `https` only: page code must not be able to launch a file or a custom scheme.
// `Shell::open` is deprecated for tauri-plugin-opener, which would be a new
// plugin in every generated shell for this one call.
#[allow(deprecated)]
#[tauri::command]
pub fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("only https pages can be opened".to_string());
    }
    app.shell()
        .open(url, None)
        .map_err(|error| error.to_string())
}
