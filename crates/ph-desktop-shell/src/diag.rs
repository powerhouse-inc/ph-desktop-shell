// A release build sets `windows_subsystem = "windows"`, so it has no console and
// every `println!` goes nowhere. Anything worth knowing after the fact has to go
// to a file. This is also brief §9.6 — "the first thing support will need".
//
// The webview logs through here too (`log_from_webview`), because the alternative
// is asking someone to open devtools on a page that may already have failed.
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

struct Sink {
    file: Mutex<std::fs::File>,
    started: Instant,
}

static SINK: OnceLock<Sink> = OnceLock::new();

/// Opens (truncating) the log for this run. A fresh file per launch keeps a
/// support copy readable; the previous run is kept alongside as `.1`.
pub fn init(dir: &Path) -> PathBuf {
    let path = dir.join("ph-win.log");
    let _ = create_dir_all(dir);
    let _ = std::fs::rename(&path, dir.join("ph-win.log.1"));

    if let Ok(file) = OpenOptions::new().create(true).write(true).truncate(true).open(&path) {
        let _ = SINK.set(Sink { file: Mutex::new(file), started: Instant::now() });
    }

    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    log(format!("=== ph-win started (unix {epoch}) ==="));
    path
}

/// Appends one timestamped line. Silently does nothing before `init`, so callers
/// never have to care about ordering.
pub fn log(message: impl AsRef<str>) {
    let message = message.as_ref().trim_end();
    let Some(sink) = SINK.get() else {
        println!("[ph-win] {message}");
        return;
    };

    let line = format!("[{:>8.3}s] {message}\n", sink.started.elapsed().as_secs_f64());
    println!("[ph-win] {message}");
    if let Ok(mut file) = sink.file.lock() {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
}

/// Lets the page report what it is doing into the same timeline as the shell.
#[tauri::command]
pub fn log_from_webview(level: String, message: String) {
    log(format!("[webview:{level}] {message}"));
}

/// Reads the page's own state back into the log.
///
/// `eval_with_callback` returns a JS value straight to Rust without going
/// through the IPC ACL, so it still reports when `invoke` itself is what is
/// broken — which is exactly the case worth diagnosing. Sampled twice because
/// Connect registers its launch consumer on mount, well after page load.
pub fn probe_webview<R: tauri::Runtime>(window: tauri::WebviewWindow<R>) {
    const SNAPSHOT_JS: &str = r#"JSON.stringify({
        url: location.href,
        launchQueue: typeof window.launchQueue,
        internals: typeof window.__TAURI_INTERNALS__,
        shimLog: window.__phLog ?? null
    })"#;

    for (index, delay) in [(1u8, 4u64), (2, 20)] {
        let window = window.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(delay));
            let result = window.eval_with_callback(SNAPSHOT_JS, move |value| {
                log(format!("webview probe {index}: {value}"));
            });
            if let Err(error) = result {
                log(format!("webview probe {index} failed: {error}"));
            }
        });
    }
}
