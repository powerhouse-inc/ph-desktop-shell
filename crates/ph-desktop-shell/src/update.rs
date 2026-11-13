//! Updates. An app opts in by carrying `plugins.updater` in its
//! tauri.conf.json (a release feed and the public key its releases are signed
//! with); without it nothing here runs. With it, the shell asks the feed for a
//! newer version at start and every few hours, tells the page (update-notice.js
//! draws the notice), and on the person's word downloads it, checks its
//! signature, installs it and starts the new version.
//!
//! Tauri's updater does the download, the signature and the install: the setup
//! program on Windows, the bundle on macOS, the AppImage, .deb or .rpm on Linux.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::{diag, show_closing, Sidecar};

/// The first question, once the app has had time to start.
const FIRST_CHECK: Duration = Duration::from_secs(20);
/// How often an app left open asks again.
const CHECK_EVERY: Duration = Duration::from_secs(4 * 60 * 60);
/// The least the progress moves between two words to the page.
const PROGRESS_STEP: u64 = 512 * 1024;

/// Whether the app's configuration names a release feed.
pub fn configured(config: &tauri::Config) -> bool {
    config.plugins.0.contains_key("updater")
}

/// Where an update stands, as the page sees it.
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum Status {
    /// Nothing newer is known.
    #[default]
    Idle,
    Available {
        version: String,
        notes: Option<String>,
    },
    Downloading {
        version: String,
        received: u64,
        total: Option<u64>,
    },
    /// Downloaded and verified; being installed, the app restarts next.
    Installing { version: String },
    /// The update did not go in; the app runs on as it was.
    Failed { version: String, error: String },
}

impl Status {
    /// An update under way is not replaced by a newer answer from the feed.
    fn busy(&self) -> bool {
        matches!(self, Status::Downloading { .. } | Status::Installing { .. })
    }
}

/// What the page is told: the status, and whose update it is.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct View {
    /// False in an app without a release feed: the page draws nothing.
    enabled: bool,
    product: String,
    current: String,
    #[serde(flatten)]
    status: Status,
}

#[derive(Default)]
pub struct Updates {
    enabled: AtomicBool,
    status: Mutex<Status>,
    found: Mutex<Option<Update>>,
}

impl Updates {
    fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn set(&self, app: &AppHandle, status: Status) {
        if let Ok(mut held) = self.status.lock() {
            *held = status;
        }
        tell(app);
    }
}

fn product(app: &AppHandle) -> String {
    app.config()
        .product_name
        .clone()
        .unwrap_or_else(|| app.package_info().name.clone())
}

fn view(app: &AppHandle) -> View {
    let updates = app.state::<Updates>();
    View {
        enabled: updates.enabled.load(Ordering::Relaxed),
        product: product(app),
        current: app.package_info().version.to_string(),
        status: updates.status(),
    }
}

/// Tells every window where the update stands. A page that has not loaded the
/// notice yet asks for itself once it has (`update_state`).
fn tell(app: &AppHandle) {
    let Ok(json) = serde_json::to_string(&view(app)) else {
        return;
    };
    for window in app.webview_windows().values() {
        let _ = window.eval(format!("window.__phUpdate && window.__phUpdate({json})"));
    }
}

/// Asks the feed. A newer version becomes the status; anything else — up to
/// date, offline, a feed that is not there — changes nothing and is only
/// logged: an update check must never bother anyone.
async fn check(app: &AppHandle) {
    let updates = app.state::<Updates>();
    if updates.status().busy() {
        return;
    }
    let stops = app.clone();
    let updater = app
        .updater_builder()
        // Windows: the setup program replaces files the stack holds open, and
        // the app is ended for it. The stack closes its stores first.
        .on_before_exit(move || stops.state::<Sidecar>().stop())
        .build();
    let found = match updater {
        Ok(updater) => updater.check().await,
        Err(error) => Err(error),
    };
    match found {
        Ok(Some(update)) => {
            if updates.status().busy() {
                return;
            }
            diag::log(format!(
                "update         : {} is available (running {})",
                update.version, update.current_version
            ));
            let status = Status::Available {
                version: update.version.clone(),
                notes: update.body.clone().filter(|notes| !notes.trim().is_empty()),
            };
            if let Ok(mut held) = updates.found.lock() {
                held.replace(update);
            }
            updates.set(app, status);
            // An unattended run (a kiosk, a test): installed without the asking.
            if std::env::var("PH_UPDATE_AUTO_INSTALL").is_ok_and(|v| v == "1") {
                tauri::async_runtime::spawn(install(app.clone()));
            }
        }
        Ok(None) => diag::log("update         : up to date"),
        Err(error) => diag::log(format!("update         : check failed: {error}")),
    }
}

/// Starts the checks, in an app that opted in.
pub fn watch(app: &AppHandle) {
    let updates = app.state::<Updates>();
    updates.enabled.store(true, Ordering::Relaxed);
    let first = seconds_from("PH_UPDATE_FIRST_CHECK_SECS").unwrap_or(FIRST_CHECK);
    let every = seconds_from("PH_UPDATE_CHECK_EVERY_SECS").unwrap_or(CHECK_EVERY);
    diag::log(format!(
        "update         : on, running {}, first check in {} s, then every {} s",
        app.package_info().version,
        first.as_secs(),
        every.as_secs()
    ));
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(first);
        loop {
            tauri::async_runtime::block_on(check(&app));
            std::thread::sleep(every);
        }
    });
}

/// A test run asks sooner than a person would wait for.
fn seconds_from(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()?
        .parse::<u64>()
        .ok()
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
}

/// Downloads, verifies and installs what the last check found, then starts the
/// new version. A failure leaves the running app as it was.
async fn install(app: AppHandle) {
    let updates = app.state::<Updates>();
    let Some(update) = updates.found.lock().ok().and_then(|found| found.clone()) else {
        return;
    };
    let version = update.version.clone();
    updates.set(
        &app,
        Status::Downloading {
            version: version.clone(),
            received: 0,
            total: None,
        },
    );

    let (mut received, mut told) = (0u64, 0u64);
    let (progress_app, progress_version) = (app.clone(), version.clone());
    let downloaded = update
        .download(
            move |chunk, total| {
                received += chunk as u64;
                if received - told < PROGRESS_STEP && Some(received) != total {
                    return;
                }
                told = received;
                progress_app.state::<Updates>().set(
                    &progress_app,
                    Status::Downloading {
                        version: progress_version.clone(),
                        received,
                        total,
                    },
                );
            },
            || {},
        )
        .await;
    let bytes = match downloaded {
        Ok(bytes) => bytes,
        Err(error) => return failed(&app, version, format!("{error}")),
    };

    diag::log(format!(
        "update         : {version} downloaded and verified, installing"
    ));
    updates.set(
        &app,
        Status::Installing {
            version: version.clone(),
        },
    );
    // Off the async runtime: a package install waits for a password prompt.
    // On Windows this does not return: the setup program takes over and the
    // app is ended (on_before_exit stopped the stack first).
    let installing = update.clone();
    let installed = tauri::async_runtime::spawn_blocking(move || installing.install(bytes)).await;
    match installed {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return failed(&app, version, format!("{error}")),
        Err(error) => return failed(&app, version, format!("{error}")),
    }

    diag::log(format!("update         : {version} installed, restarting"));
    show_closing(&app, &product(&app));
    let stops = app.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || stops.state::<Sidecar>().stop()).await;
    app.restart();
}

fn failed(app: &AppHandle, version: String, error: String) {
    diag::log(format!("update         : {version} not installed: {error}"));
    app.state::<Updates>()
        .set(app, Status::Failed { version, error });
}

/// Where the update stands; what a page asks when it loads.
#[tauri::command]
pub fn update_state(app: AppHandle) -> View {
    view(&app)
}

/// Asks the feed now, as from a "Check for updates" button.
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<View, String> {
    if app.state::<Updates>().enabled.load(Ordering::Relaxed) {
        check(&app).await;
    }
    Ok(view(&app))
}

/// Installs the update the notice offers. Answers at once; the page follows
/// the progress through the status.
#[tauri::command]
pub fn update_install(app: AppHandle) -> Result<(), String> {
    let updates = app.state::<Updates>();
    match updates.status() {
        Status::Available { .. } | Status::Failed { .. } => {}
        Status::Idle => return Err("No update is waiting.".into()),
        _ => return Ok(()),
    }
    tauri::async_runtime::spawn(install(app.clone()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_reads_the_status_beside_the_app() {
        let view = View {
            enabled: true,
            product: "Sektar".into(),
            current: "1.0.9".into(),
            status: Status::Downloading {
                version: "1.1.0".into(),
                received: 10,
                total: Some(40),
            },
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            serde_json::json!({
                "enabled": true, "product": "Sektar", "current": "1.0.9",
                "state": "downloading", "version": "1.1.0", "received": 10, "total": 40
            })
        );
    }

    #[test]
    fn an_update_under_way_is_left_alone() {
        assert!(!Status::Idle.busy());
        assert!(!Status::Available {
            version: "1".into(),
            notes: None
        }
        .busy());
        assert!(!Status::Failed {
            version: "1".into(),
            error: "x".into()
        }
        .busy());
        assert!(Status::Downloading {
            version: "1".into(),
            received: 0,
            total: None
        }
        .busy());
        assert!(Status::Installing {
            version: "1".into()
        }
        .busy());
    }

    #[test]
    fn an_app_without_a_feed_has_no_updates() {
        let mut config: tauri::Config = serde_json::from_value(serde_json::json!({
            "identifier": "inc.powerhouse.test"
        }))
        .unwrap();
        assert!(!configured(&config));
        config.plugins.0.insert(
            "updater".into(),
            serde_json::json!({ "pubkey": "k", "endpoints": [] }),
        );
        assert!(configured(&config));
    }
}
