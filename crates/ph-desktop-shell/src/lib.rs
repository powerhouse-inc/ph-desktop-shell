// ph-desktop-shell: the Powerhouse desktop shell as a LIBRARY. A shell binary
// is three lines — `fn main() { ph_desktop_shell::run(tauri::generate_context!(),
// Default::default()) }` — because identity (productName, identifier, icons,
// file associations) lives entirely in the calling crate's tauri.conf.json.
// The Powerhouse-identity app (apps/powerhouse-desktop) and every generated
// builder shell (`ph generate desktop-shell`) are consumers of this one crate.
//
// What it does: supervises a Node sidecar that runs the project's stack
// (`ph run desktop` under uniform-runners, or a legacy bundled sidecar), and
// points the webview at Connect once the stack answers on its port.

pub mod diag;
pub mod external;
pub mod gate;
pub mod identity;
pub mod launch;
pub mod root;
pub mod serve;
pub mod settings;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, RunEvent, WindowEvent};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

use launch::LaunchQueue;

/// Knobs a shell binary may tune; identity does NOT live here (it is the
/// caller's tauri.conf.json). Defaults reproduce the Powerhouse app.
pub struct ShellOptions {
    /// Probe/window port when neither env nor the stack's shell.json names one.
    pub default_connect_port: u16,
    /// Legacy sidecar only: the switchboard port exported to it.
    pub default_switchboard_port: u16,
    /// How long the stack gets to answer before the loading page gives up.
    pub ready_timeout: Duration,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            default_connect_port: 3101,
            default_switchboard_port: 4101,
            ready_timeout: Duration::from_secs(180),
        }
    }
}

/// What the shell supervises. Legacy: the bundled sidecar (`sidecar/main.js`
/// from resources). Uniform-runners: `ph run desktop` in a hosts project —
/// the sidecar's behavior expressed as `hosts/desktop/host.js` executed by
/// the project's desktop runner. Activated by env:
///   PH_RUN_PROJECT  — the hosts project directory (e.g. a scaffolded app)
///   PH_RUN_CLI      — path to ph-cli's cli.mjs (dev builds; a packaged app
///                     bundles it inside resources/stack)
///   PH_WIN_CONNECT_PORT — must match the project's studio port (the probe
///                     and window URL read it)
enum Supervised {
    Legacy,
    UniformRunners { project: PathBuf, cli: PathBuf },
}

/// The packaged-app path (uniform-runners phase 6): a staged stack bundled as
/// a resource (`resources/stack`: hosts/, powerhouse.config.json, the built
/// Connect, and a VENDORED @powerhousedao/ph-cli) makes the installed app run
/// `ph run desktop` with no environment at all. Staged by
/// `@powerhousedao/desktop`'s `build` verb (bundle-stack).
fn bundled_stack(resource_dir: &Path) -> Option<Supervised> {
    let stack = resource_dir.join("stack");
    let cli = stack
        .join("node_modules")
        .join("@powerhousedao")
        .join("ph-cli")
        .join("dist")
        .join("cli.mjs");
    if stack.join("hosts").is_dir() && cli.is_file() {
        // shell.json is the stack's contract with this shell: in the no-env
        // (packaged) path nobody sets PH_WIN_CONNECT_PORT, so the probe and
        // window port come from the staged manifest. Tiny file of our own
        // making — a dependency-free scan beats pulling in serde_json.
        if let Ok(manifest) = std::fs::read_to_string(stack.join("shell.json")) {
            if let Some(port) = manifest.split("\"connectPort\"").nth(1).and_then(|rest| {
                rest.chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u16>()
                    .ok()
            }) {
                std::env::set_var("PH_WIN_CONNECT_PORT", port.to_string());
            }
        }
        return Some(Supervised::UniformRunners {
            project: stack,
            cli,
        });
    }
    None
}

fn supervised_mode() -> Supervised {
    let project = std::env::var_os("PH_RUN_PROJECT").filter(|v| !v.is_empty());
    let cli = std::env::var_os("PH_RUN_CLI").filter(|v| !v.is_empty());
    match (project, cli) {
        (Some(project), Some(cli)) => Supervised::UniformRunners {
            project: PathBuf::from(project),
            cli: PathBuf::from(cli),
        },
        (Some(_), None) => {
            diag::log(
                "PH_RUN_PROJECT is set but PH_RUN_CLI is not; staying on the bundled sidecar",
            );
            Supervised::Legacy
        }
        _ => Supervised::Legacy,
    }
}

fn connect_port(default: u16) -> u16 {
    std::env::var("PH_WIN_CONNECT_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Holds the sidecar so it can be killed on exit; Tauri does not reap it for us.
#[derive(Default)]
pub(crate) struct Sidecar(Mutex<Option<CommandChild>>);

impl Sidecar {
    pub(crate) fn hold(&self, child: CommandChild) {
        if let Ok(mut guard) = self.0.lock() {
            guard.replace(child);
        }
    }

    pub(crate) fn kill(&self) {
        if let Ok(mut guard) = self.0.lock() {
            if let Some(child) = guard.take() {
                kill_children(child.pid());
                let _ = child.kill();
            }
        }
    }
}

/// The stack runs workers of its own, and on Windows killing a process leaves
/// its children running: they'd keep the ports and the store's locks.
#[cfg(windows)]
fn kill_children(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(not(windows))]
fn kill_children(_pid: u32) {}

/// Tauri hands back extended-length paths (`\\?\D:\...`) on Windows. Node cannot
/// resolve a main module from one: it splits on the prefix and lstats "D:",
/// failing with EISDIR. Strip it before handing a path to the sidecar.
pub(crate) fn plain_path(path: &std::path::Path) -> String {
    let s = path.to_string_lossy().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// The sidecar binds Connect only after Switchboard has booted, so a successful
/// TCP connect is a sufficient readiness signal.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    wait_for_port_while(port, timeout, &AtomicBool::new(true))
}

/// As `wait_for_port`, giving up as soon as `alive` goes false.
pub(crate) fn wait_for_port_while(port: u16, timeout: Duration, alive: &AtomicBool) -> bool {
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline && alive.load(Ordering::Relaxed) {
        if std::net::TcpStream::connect_timeout(
            &addr.parse().expect("valid loopback addr"),
            Duration::from_millis(500),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    false
}

/// Tray: closing the window leaves the service running. Labels carry the
/// caller's product name — a builder's identity with no Rust.
fn add_tray(app: &tauri::App, product_name: &str) -> tauri::Result<()> {
    let open = MenuItem::with_id(
        app,
        "open",
        format!("Open {product_name}"),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    TrayIconBuilder::new()
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip(product_name)
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "quit" => {
                app.state::<Sidecar>().kill();
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Build and run the shell around the caller's identity. `context` must come
/// from the BINARY crate's `tauri::generate_context!()` — that is where
/// tauri.conf.json (productName, identifier, icons, windows, resources) bakes.
pub fn run(context: tauri::Context, options: ShellOptions) {
    // Identity-derived labels: a builder's product name lands in the tray
    // tooltip and menu with no code on their side.
    let product_name = context
        .config()
        .product_name
        .clone()
        .unwrap_or_else(|| "Powerhouse".to_string());

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            // Opening a second `.phd` while we are running: the OS starts
            // another process, single-instance forwards us its argv, and that
            // process exits. This is the `focus-existing` launch handler
            // Connect declares in its web manifest.
            let launched = launch::paths_from_args(argv, Path::new(&cwd));
            diag::log(format!("second instance: {} launch arg(s)", launched.len()));
            app.state::<LaunchQueue>().push(launched);
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
                let _ = w.eval(launch::PULL_JS);
            }
        }))
        .manage(Sidecar::default())
        .manage(LaunchQueue::default())
        .invoke_handler(tauri::generate_handler![
            launch::take_launch_files,
            launch::read_launch_file,
            diag::log_from_webview,
            external::open_external,
            gate::desktop_state,
            gate::pick_root,
            gate::save_renown_url,
            gate::local_create,
            gate::local_unlock,
            gate::saved_credential,
            gate::save_profile,
            gate::adopt_old_data,
            gate::start_stack,
            gate::switch_identity
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // A release build has no console, so the run has to leave a trace
            // on disk or it cannot be diagnosed at all.
            let log_dir = app
                .path()
                .app_log_dir()
                .unwrap_or_else(|_| app.path().app_data_dir().expect("app data dir").join("logs"));
            let log_path = diag::init(&log_dir);

            let inherited = std::env::var("NODE_ENV").ok();
            if let Some(mode) = settings::node_env(!cfg!(debug_assertions), inherited.as_deref()) {
                diag::log(format!("stack setting  : NODE_ENV={mode}"));
                std::env::set_var("NODE_ENV", mode);
            }
            if std::env::var_os(settings::URL_SIGNING_SECRET).is_none() {
                // Its value never goes in the log.
                match settings::new_url_signing_secret() {
                    Ok(secret) => {
                        diag::log("attachment links: signed with a secret for this run");
                        std::env::set_var(settings::URL_SIGNING_SECRET, secret);
                    }
                    Err(error) => diag::log(format!("attachment links: no secret: {error}")),
                }
            }

            // Built here rather than from tauri.conf.json (`"create": false`)
            // so it can carry an initialization script. `eval` after navigating
            // would race Connect's mount: if `window.launchQueue` is missing at
            // that moment, Connect registers its consumer against nothing and
            // the launch is lost with no error.
            let window_config = app.config().app.windows[0].clone();
            let window = tauri::WebviewWindowBuilder::from_config(&handle, &window_config)?
                .initialization_script(include_str!("launch-queue.js"))
                .initialization_script(include_str!("desktop-bridge.js"))
                .build()?;

            // Devtools are compiled in (the `devtools` feature; a release build
            // has debug_assertions off) but only opened on request, so a normal
            // launch is unaffected. Opening here rather than after navigation
            // means the console catches the loading page and the shim's own
            // startup, which is usually where the interesting failure is.
            let devtools = std::env::var("PH_WIN_DEVTOOLS")
                .map(|value| !value.is_empty() && value != "0")
                .unwrap_or(false);
            if devtools {
                window.open_devtools();
            }

            // Cold start: a `.phd` double-click launches us with the path in
            // argv. The shim pulls it once Connect has registered its consumer.
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let launched = launch::paths_from_args(std::env::args_os(), &cwd);
            for path in &launched {
                diag::log(format!("launch arg     : {}", path.display()));
            }
            app.state::<LaunchQueue>().push(launched);

            // Env wins (dev flows wire it); then the bundled stack resource
            // (the packaged-app path); then the legacy sidecar.
            let mut mode = supervised_mode();
            if matches!(mode, Supervised::Legacy) {
                if let Ok(resource_dir) = app.path().resource_dir() {
                    if let Some(stack_mode) = bundled_stack(&resource_dir) {
                        std::env::set_var("FEATURE_UNIFORM_RUNNERS_ENABLED", "1");
                        mode = stack_mode;
                    }
                }
            }
            let port = connect_port(options.default_connect_port);

            // A sign-in page in the app means nothing starts until someone has
            // used it: who signs in decides the folder and the admin.
            if let Supervised::UniformRunners { project, cli } = &mode {
                if app.asset_resolver().get(serve::SIGN_IN_PAGE.to_string()).is_some() {
                    diag::log(format!("log file       : {}", log_path.display()));
                    diag::log(format!("connect port   : {port}"));
                    let gate = Arc::new(gate::Gate::new(gate::GateConfig {
                        app_name: product_name.clone(),
                        config_dir: app.path().app_config_dir().expect("app config dir"),
                        default_root: app.path().app_data_dir().expect("app data dir"),
                        project: project.clone(),
                        cli: cli.clone(),
                        port,
                        ready_timeout: options.ready_timeout,
                    }));
                    app.manage(gate.clone());
                    gate.open_sign_in(&handle).map_err(std::io::Error::other)?;
                    add_tray(app, &product_name)?;
                    return Ok(());
                }
            }

            // PGlite must write to a per-user location; the resource dir is
            // read-only once installed under Program Files. An inherited
            // PH_WIN_DATA_DIR wins, so a test run can be pointed at a scratch
            // store instead of the real one. app_data_dir derives from the
            // caller's identifier, so builder apps never share stores.
            let data_dir = match std::env::var_os("PH_WIN_DATA_DIR") {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir),
                _ => app
                    .path()
                    .app_data_dir()
                    .expect("app data dir")
                    .join("pglite"),
            };
            std::fs::create_dir_all(&data_dir)?;

            diag::log(format!("log file       : {}", log_path.display()));
            diag::log(format!("devtools       : {}", if devtools { "open" } else { "off (PH_WIN_DEVTOOLS=1)" }));
            diag::log(format!("data dir       : {}", plain_path(&data_dir)));
            diag::log(format!("connect port   : {port}"));

            let (mut rx, child) = match &mode {
                Supervised::Legacy => {
                    let entry = app
                        .path()
                        .resource_dir()
                        .expect("resource dir")
                        .join("sidecar")
                        .join("main.js");
                    if !entry.is_file() {
                        // A shell built without legacy assets (every builder
                        // shell, and the packaged app before its stack is
                        // staged) must say WHY it has nothing to run.
                        diag::log(
                            "nothing to supervise: no PH_RUN_* env, no resources/stack, no legacy sidecar",
                        );
                        return Ok(());
                    }

                    // The sidecar inherits this process's environment.
                    std::env::set_var("PH_WIN_DATA_DIR", plain_path(&data_dir));
                    std::env::set_var("PH_WIN_CONNECT_PORT", port.to_string());
                    std::env::set_var(
                        "PH_SWITCHBOARD_PORT",
                        options.default_switchboard_port.to_string(),
                    );
                    diag::log(format!("sidecar entry  : {}", plain_path(&entry)));

                    app.shell()
                        .sidecar("node")
                        .expect("sidecar 'node' is declared in externalBin")
                        .args([plain_path(&entry)])
                        .spawn()
                        .expect("failed to spawn node sidecar")
                }
                Supervised::UniformRunners { project, cli } => {
                    // `ph run desktop`: the compound reads ports from the
                    // project's own host files; the shell only needs
                    // PH_WIN_CONNECT_PORT to agree for its probe/URL. The
                    // host's `${userData}` resolves to this app's data dir.
                    std::env::set_var("FEATURE_UNIFORM_RUNNERS_ENABLED", "1");
                    std::env::set_var(
                        "PH_USER_DATA_DIR",
                        plain_path(data_dir.parent().unwrap_or(&data_dir)),
                    );
                    diag::log(format!("run project    : {}", plain_path(project)));
                    diag::log(format!("ph-cli         : {}", plain_path(cli)));

                    app.shell()
                        .sidecar("node")
                        .expect("sidecar 'node' is declared in externalBin")
                        .args([plain_path(cli), "run".into(), "desktop".into()])
                        // plain_path on the cwd too: ph-cli takes projectRoot
                        // from cwd, and a `\\?\` prefix leaks into storage
                        // paths where PGlite's fsync fails with EPERM.
                        .current_dir(PathBuf::from(plain_path(project)))
                        .spawn()
                        .expect("failed to spawn ph run desktop")
                }
            };

            app.state::<Sidecar>().0.lock().unwrap().replace(child);

            // Surface sidecar output in the console; this is the first thing
            // support will ask for.
            tauri::async_runtime::spawn(async move {
                while let Some(event) = rx.recv().await {
                    match event {
                        CommandEvent::Stdout(line) => {
                            diag::log(format!("[sidecar] {}", String::from_utf8_lossy(&line)))
                        }
                        CommandEvent::Stderr(line) => {
                            diag::log(format!("[sidecar:err] {}", String::from_utf8_lossy(&line)))
                        }
                        CommandEvent::Terminated(payload) => {
                            diag::log(format!("sidecar exited: {:?}", payload.code));
                        }
                        _ => {}
                    }
                }
            });

            add_tray(app, &product_name)?;

            // Navigate off the loading page once the supervised stack answers.
            let ready_timeout = options.ready_timeout;
            tauri::async_runtime::spawn(async move {
                let ready = tauri::async_runtime::spawn_blocking(move || {
                    wait_for_port(port, ready_timeout)
                })
                .await
                .unwrap_or(false);

                if let Some(window) = handle.get_webview_window("main") {
                    if ready {
                        let url = format!("http://127.0.0.1:{port}/");
                        diag::log(format!("sidecar ready, navigating to {url}"));
                        let _ = window.navigate(url.parse().expect("valid url"));
                        diag::probe_webview(window);
                    } else {
                        diag::log("sidecar did not become ready in time");
                    }
                }
            });

            Ok(())
        })
        .on_window_event(|window, event| {
            // Close hides to tray rather than quitting, so the service survives,
            // unless there's nothing to keep running or the person asked.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                let ends = app
                    .try_state::<Arc<gate::Gate>>()
                    .is_some_and(|gate| gate.close_ends_the_app());
                if ends {
                    app.state::<Sidecar>().kill();
                    app.exit(0);
                    return;
                }
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(context)
        .expect("error building the desktop shell")
        .run(|app, event| {
            if let RunEvent::ExitRequested { .. } | RunEvent::Exit = event {
                app.state::<Sidecar>().kill();
            }
        });
}
