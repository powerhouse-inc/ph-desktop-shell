// The OS hands `.phd` files to the app as command-line arguments — on
// double-click, "Open with", or a second launch while we are already running.
//
// Connect already knows how to import them: it consumes the PWA File Handling
// API (`apps/connect/src/utils/launch-queue.ts`) and its web manifest declares
// `file_handlers` for `.phd`/`.phdm`. WebView2 does not implement that API, so
// the shell's whole job is to hand Connect the launches Chromium would have.
// `launch-queue.js` is the other half; this side owns the queue and the bytes.
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde::Serialize;
use tauri::ipc::Response;
use tauri::State;

use crate::diag;

/// Mirrors the extensions Connect declares in its manifest's `file_handlers`.
const ASSOCIATED_EXTENSIONS: [&str; 2] = ["phd", "phdm"];

/// Asks the shim to drain the queue. Safe to call at any time: it is an
/// initialization script, so it is installed before any page script runs, and
/// it no-ops until Connect has registered a launchQueue consumer.
pub const PULL_JS: &str = "window.__phTakeLaunchFiles && window.__phTakeLaunchFiles()";

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LaunchFileMeta {
    id: u64,
    name: String,
    size: u64,
    /// Milliseconds since the epoch. Connect dedupes pending imports on
    /// `name|size|lastModified`, so this has to be the file's real mtime — the
    /// `Date.now()` default of `new File()` would queue a fresh copy every time
    /// the same document was opened.
    last_modified: u64,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    /// Handed to us by the OS, not yet claimed by the webview.
    pending: Vec<(u64, PathBuf)>,
    /// Claimed and still readable. Dropped on read, so an id is readable exactly
    /// once and the webview never holds a durable arbitrary-file-read primitive.
    claimed: HashMap<u64, PathBuf>,
}

#[derive(Default)]
pub struct LaunchQueue(Mutex<Inner>);

impl LaunchQueue {
    pub fn push(&self, paths: impl IntoIterator<Item = PathBuf>) -> usize {
        let mut inner = self.0.lock().expect("launch queue lock");
        let mut added = 0;
        for path in paths {
            let id = inner.next_id;
            inner.next_id += 1;
            inner.pending.push((id, path));
            added += 1;
        }
        added
    }
}

/// Drains the queue into metadata the shim can build `File` objects from. A file
/// that vanished between launch and pull is skipped rather than failing the batch.
#[tauri::command]
pub fn take_launch_files(queue: State<'_, LaunchQueue>) -> Vec<LaunchFileMeta> {
    let mut inner = queue.0.lock().expect("launch queue lock");
    let pending: Vec<(u64, PathBuf)> = std::mem::take(&mut inner.pending);
    diag::log(format!("take_launch_files: {} queued", pending.len()));

    let mut metas = Vec::with_capacity(pending.len());
    for (id, path) in pending {
        let Ok(meta) = std::fs::metadata(&path) else {
            diag::log(format!("launch file gone: {}", path.display()));
            continue;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let last_modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as u64)
            .unwrap_or(0);

        diag::log(format!(
            "  -> id {id} {name} ({} bytes, mtime {last_modified})",
            meta.len()
        ));
        metas.push(LaunchFileMeta {
            id,
            name,
            size: meta.len(),
            last_modified,
        });
        inner.claimed.insert(id, path);
    }
    metas
}

/// Returns the file's bytes. Keyed by id rather than path so this cannot be used
/// as a general "read any file" command by page code.
#[tauri::command]
pub fn read_launch_file(id: u64, queue: State<'_, LaunchQueue>) -> Result<Response, String> {
    let path = queue
        .0
        .lock()
        .expect("launch queue lock")
        .claimed
        .remove(&id)
        .ok_or_else(|| format!("no launch file with id {id}"))?;

    match std::fs::read(&path) {
        Ok(bytes) => {
            diag::log(format!(
                "read_launch_file: id {id} -> {} bytes",
                bytes.len()
            ));
            Ok(Response::new(bytes))
        }
        Err(error) => {
            let message = format!("{}: {error}", path.display());
            diag::log(format!("read_launch_file: id {id} FAILED {message}"));
            Err(message)
        }
    }
}

/// Extracts openable paths from a process argv. `argv[0]` is the executable;
/// anything leading with `-` is a flag (both Tauri and WebView2 pass some
/// through); a relative path resolves against the cwd of *that* invocation,
/// which for a second instance is not our own.
pub fn paths_from_args<I, S>(args: I, cwd: &Path) -> Vec<PathBuf>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .skip(1)
        .filter(|arg| !arg.as_ref().to_string_lossy().starts_with('-'))
        .map(|arg| {
            let path = PathBuf::from(arg.as_ref());
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .filter(|path| is_associated(path))
        .collect()
}

fn is_associated(path: &Path) -> bool {
    let matches_ext = path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
        ASSOCIATED_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(ext))
    });
    matches_ext && path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `paths_from_args` filters on the filesystem, so the fixtures have to be real.
    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ph-win-launch-{name}"));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        dir
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fixture").expect("fixture file");
        path
    }

    #[test]
    fn takes_associated_files_and_skips_the_executable() {
        let dir = fixture_dir("basic");
        let doc = touch(&dir, "budget.phd");
        // argv[0] is the exe even when it is not a path we would otherwise keep.
        let args = vec![
            PathBuf::from("ph-win.exe").into_os_string(),
            doc.clone().into_os_string(),
        ];

        assert_eq!(paths_from_args(args, &dir), vec![doc]);
    }

    #[test]
    fn ignores_flags_and_unassociated_files() {
        let dir = fixture_dir("filtering");
        let doc = touch(&dir, "model.phdm");
        let other = touch(&dir, "notes.txt");
        let args = vec![
            PathBuf::from("ph-win.exe").into_os_string(),
            std::ffi::OsString::from("--some-webview2-flag"),
            other.into_os_string(),
            doc.clone().into_os_string(),
        ];

        assert_eq!(paths_from_args(args, &dir), vec![doc]);
    }

    #[test]
    fn resolves_a_relative_path_against_the_given_cwd() {
        // A second instance's argv is relative to *its* cwd, not ours.
        let dir = fixture_dir("relative");
        let doc = touch(&dir, "report.phd");
        let args = vec![
            std::ffi::OsString::from("ph-win.exe"),
            std::ffi::OsString::from("report.phd"),
        ];

        assert_eq!(paths_from_args(args, &dir), vec![doc]);
    }

    #[test]
    fn matches_the_extension_case_insensitively() {
        let dir = fixture_dir("case");
        let doc = touch(&dir, "SHOUTING.PHD");
        let args = vec![
            std::ffi::OsString::from("ph-win.exe"),
            doc.clone().into_os_string(),
        ];

        assert_eq!(paths_from_args(args, &dir), vec![doc]);
    }

    #[test]
    fn drops_a_path_that_does_not_exist() {
        let dir = fixture_dir("missing");
        let args = vec![
            std::ffi::OsString::from("ph-win.exe"),
            dir.join("never-written.phd").into_os_string(),
        ];

        assert!(paths_from_args(args, &dir).is_empty());
    }

    #[test]
    fn read_launch_file_is_one_shot_per_id() {
        let queue = LaunchQueue::default();
        assert_eq!(queue.push([PathBuf::from("a.phd")]), 1);

        let mut inner = queue.0.lock().unwrap();
        let (id, path) = inner.pending.remove(0);
        inner.claimed.insert(id, path);
        // The webview may read a launched file once; the id is spent afterwards.
        assert!(inner.claimed.remove(&id).is_some());
        assert!(inner.claimed.remove(&id).is_none());
    }
}
