//! Opens a page in the person's own browser. The app has one window and no
//! popups, so a sign-in that navigates that window away leaves no way back.

use tauri::AppHandle;
use tauri_plugin_shell::ShellExt;

/// A web page: `https`, or this computer's own, as a Renown instance run locally
/// is. Page code must not be able to launch a file or a custom scheme.
fn may_open(url: &str) -> bool {
    let local = |prefix: &str| {
        url.strip_prefix(prefix)
            .is_some_and(|rest| matches!(rest.chars().next(), None | Some(':' | '/')))
    };
    url.starts_with("https://") || local("http://127.0.0.1") || local("http://localhost")
}

// `Shell::open` is deprecated for tauri-plugin-opener, which would be a new
// plugin in every generated shell for this one call.
#[allow(deprecated)]
#[tauri::command]
pub fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    if !may_open(&url) {
        return Err("only web pages can be opened".to_string());
    }
    app.shell()
        .open(url, None)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::may_open;

    #[test]
    fn opens_web_pages() {
        assert!(may_open("https://www.renown.id/console"));
        assert!(may_open("http://127.0.0.1:4800/console"));
        assert!(may_open("http://localhost:4800/console"));
    }

    #[test]
    fn refuses_anything_else() {
        assert!(!may_open("http://renown.test/console"));
        assert!(!may_open("http://127.0.0.1.evil.test/console"));
        assert!(!may_open("http://localhost.evil.test/console"));
        assert!(!may_open("file:///C:/Windows/notepad.exe"));
        assert!(!may_open("ms-settings:"));
    }
}
