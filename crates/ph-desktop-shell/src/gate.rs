//! Sign-in before anything runs. With a sign-in page in the app, the window opens
//! on it and the stack waits: who signs in decides the folder the stack runs from
//! and who its admin is. The commands here open onto a person's keys and files,
//! and Connect's packages share this origin, so all but `switch_identity` are
//! refused once the stack is up.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

use crate::identity::{self, Identity, LOCAL, LOCAL_KEY_FILE};
use crate::root::{self, Root, RootSource};
use crate::serve::{SignInServer, SIGN_IN_PAGE};
use crate::settings::{self, Settings};
use crate::{diag, plain_path, show_closing, stack_command, wait_for_port_while, Sidecar};

const OFFICIAL_RENOWN: &str = "https://www.renown.id";
const CHAIN_ID: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    SigningIn,
    Starting,
    Running,
}

pub struct Gate {
    app_name: String,
    config_dir: PathBuf,
    default_root: PathBuf,
    project: PathBuf,
    cli: PathBuf,
    port: u16,
    ready_timeout: Duration,
    root: Mutex<Root>,
    phase: Mutex<Phase>,
    server: Mutex<Option<SignInServer>>,
    close_quits: AtomicBool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopState {
    app_name: String,
    root: String,
    root_locked: bool,
    root_missing: bool,
    synced_folder: Option<&'static str>,
    identities: Vec<Identity>,
    has_local_key: bool,
    old_data: bool,
    settings_error: Option<String>,
    renown_url: String,
    renown_switchboard_url: Option<String>,
    chain_id: u64,
}

pub struct GateConfig {
    pub app_name: String,
    pub config_dir: PathBuf,
    pub default_root: PathBuf,
    pub project: PathBuf,
    pub cli: PathBuf,
    pub port: u16,
    pub ready_timeout: Duration,
}

impl Gate {
    pub fn new(config: GateConfig) -> Self {
        let root = root::resolve(
            std::env::var_os("PH_DESKTOP_ROOT"),
            &config.config_dir,
            &config.default_root,
        );
        if root.source == RootSource::Default {
            let _ = std::fs::create_dir_all(&root.path);
        }
        diag::log(format!("root           : {}", plain_path(&root.path)));
        Self {
            app_name: config.app_name,
            config_dir: config.config_dir,
            default_root: config.default_root,
            project: config.project,
            cli: config.cli,
            port: config.port,
            ready_timeout: config.ready_timeout,
            root: Mutex::new(root),
            phase: Mutex::new(Phase::SigningIn),
            server: Mutex::new(None),
            close_quits: AtomicBool::new(false),
        }
    }

    fn root(&self) -> Root {
        self.root.lock().expect("root lock").clone()
    }

    fn folder_of(&self, identity: &str) -> Result<PathBuf, String> {
        if identity != LOCAL && !identity::is_renown_id(identity) {
            return Err(format!("{identity} isn't an identity"));
        }
        Ok(self.root().path.join(identity))
    }

    fn while_signing_in(&self) -> Result<(), String> {
        match *self.phase.lock().expect("phase lock") {
            Phase::SigningIn => Ok(()),
            _ => Err("Sektar has already started.".to_string()),
        }
    }

    fn set_phase(&self, phase: Phase) {
        *self.phase.lock().expect("phase lock") = phase;
    }

    /// While someone is signing in there's nothing running to keep alive.
    pub fn close_ends_the_app(&self) -> bool {
        *self.phase.lock().expect("phase lock") == Phase::SigningIn
            || self.close_quits.load(Ordering::Relaxed)
    }

    fn sign_in_url(&self) -> String {
        format!("http://127.0.0.1:{}/{SIGN_IN_PAGE}", self.port)
    }

    /// Opens the sign-in page on Connect's port.
    pub fn open_sign_in(&self, app: &AppHandle) -> Result<(), String> {
        let server =
            SignInServer::start(app.clone(), self.port).map_err(|error| error.to_string())?;
        *self.server.lock().expect("server lock") = Some(server);
        if let Some(window) = app.get_webview_window("main") {
            let url = self.sign_in_url().parse().map_err(|_| "bad url")?;
            window.navigate(url).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn close_sign_in(&self) {
        if let Some(server) = self.server.lock().expect("server lock").take() {
            server.stop();
        }
    }

    /// The local key's process, or wherever it is in a development checkout.
    fn local_key_script(&self) -> PathBuf {
        let staged = self.project.join("local-key.mjs");
        if staged.is_file() {
            return staged;
        }
        self.project
            .join("node_modules/@powerhousedao/desktop/dist/local-key/local-key.mjs")
    }

    /// Runs one local key command. The password goes through stdin, so it's
    /// never on a command line, and it isn't logged.
    async fn local_key(
        &self,
        app: &AppHandle,
        command: &str,
        input: Value,
    ) -> Result<Value, String> {
        let script = plain_path(&self.local_key_script());
        let sidecar = app
            .shell()
            .sidecar("node")
            .map_err(|error| error.to_string())?;
        let (mut events, mut child) = sidecar
            .args([script, command.to_string()])
            .spawn()
            .map_err(|error| error.to_string())?;
        child
            .write(format!("{input}\n").as_bytes())
            .map_err(|error| error.to_string())?;
        let mut output = Vec::new();
        while let Some(event) = events.recv().await {
            match event {
                CommandEvent::Stdout(line) => output.extend(line),
                CommandEvent::Stderr(line) => {
                    diag::log(format!("[local key] {}", String::from_utf8_lossy(&line)));
                }
                CommandEvent::Terminated(_) => break,
                _ => {}
            }
        }
        let reply: Value = serde_json::from_slice(&output)
            .map_err(|_| "The local key didn't answer.".to_string())?;
        if reply["ok"] == json!(true) {
            Ok(reply)
        } else {
            Err(reply["error"]
                .as_str()
                .unwrap_or("That didn't work.")
                .to_string())
        }
    }

    fn start(
        self: &Arc<Self>,
        app: &AppHandle,
        folder: &Path,
        admin: &str,
        local: bool,
    ) -> Result<(), String> {
        let root = self.root();
        let settings = settings::read(&root.path)?;
        self.close_sign_in();
        self.set_phase(Phase::Starting);
        self.close_quits
            .store(settings.closes_to_quit(), Ordering::Relaxed);
        apply_env(&settings, folder, admin, local);
        std::fs::create_dir_all(folder.join("backups")).map_err(|error| error.to_string())?;
        diag::log(format!("identity folder: {}", plain_path(folder)));

        let (mut events, child) = stack_command(app, &self.cli, &self.project)
            .map_err(|error| error.to_string())?
            .spawn()
            .map_err(|error| error.to_string())?;
        let exited = app.state::<Sidecar>().hold_stack(child);

        let alive = Arc::new(AtomicBool::new(true));
        let still_alive = alive.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    CommandEvent::Stdout(line) => {
                        diag::log(format!("[sidecar] {}", String::from_utf8_lossy(&line)))
                    }
                    CommandEvent::Stderr(line) => {
                        diag::log(format!("[sidecar:err] {}", String::from_utf8_lossy(&line)))
                    }
                    CommandEvent::Terminated(payload) => {
                        diag::log(format!("sidecar exited: {:?}", payload.code));
                        still_alive.store(false, Ordering::Relaxed);
                        exited.mark();
                    }
                    _ => {}
                }
            }
        });

        let gate = self.clone();
        let handle = app.clone();
        let timeout = settings.startup_timeout(self.ready_timeout);
        tauri::async_runtime::spawn(async move {
            let port = gate.port;
            let up = tauri::async_runtime::spawn_blocking(move || {
                wait_for_port_while(port, timeout, &alive)
            })
            .await
            .unwrap_or(false);
            if up {
                gate.set_phase(Phase::Running);
                gate.show_connect(&handle);
            } else {
                gate.start_failed(&handle);
            }
        });
        Ok(())
    }

    fn show_connect(&self, app: &AppHandle) {
        let url = format!("http://127.0.0.1:{}/", self.port);
        diag::log(format!("stack ready, navigating to {url}"));
        if let (Some(window), Ok(url)) = (app.get_webview_window("main"), url.parse()) {
            let _ = window.navigate(url);
            diag::probe_webview(window);
        }
    }

    /// Back to the sign-in page, which is still on screen, with the reason.
    fn start_failed(&self, app: &AppHandle) {
        diag::log("the stack did not come up");
        app.state::<Sidecar>().kill();
        self.set_phase(Phase::SigningIn);
        if let Ok(server) = SignInServer::start(app.clone(), self.port) {
            *self.server.lock().expect("server lock") = Some(server);
        }
        let text = format!("{} didn't start. The log has the details.", self.app_name);
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.eval(format!("window.phDesktopStartFailed?.({})", json!(text)));
        }
    }
}

/// The stack's environment for one identity. The stack is a child of this
/// process and inherits it, and these change with each identity.
fn apply_env(settings: &Settings, folder: &Path, admin: &str, local: bool) {
    let ram_mb = {
        let system = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::nothing().with_memory(sysinfo::MemoryRefreshKind::everything()),
        );
        system.total_memory() / (1024 * 1024)
    };
    // Logged, as support will ask: nothing here is a secret.
    for (name, value) in settings.stack_env(ram_mb, &|name| std::env::var(name).ok()) {
        diag::log(format!("stack setting  : {name}={value}"));
        std::env::set_var(name, value);
    }
    std::env::set_var("FEATURE_UNIFORM_RUNNERS_ENABLED", "1");
    std::env::set_var("PH_USER_DATA_DIR", plain_path(folder));
    diag::log(format!("stack setting  : ADMINS={admin}"));
    std::env::set_var("ADMINS", admin);
    // A local key's credential is at no Renown: only what's kept here counts.
    if local {
        diag::log("stack setting  : RENOWN_SOURCE=kept");
        std::env::set_var("RENOWN_SOURCE", "kept");
    } else {
        std::env::remove_var("RENOWN_SOURCE");
    }
}

#[tauri::command]
pub fn desktop_state(gate: State<'_, Arc<Gate>>) -> Result<DesktopState, String> {
    gate.while_signing_in()?;
    let root = gate.root();
    let settings = settings::read(&root.path);
    let renown = settings.as_ref().ok().map(|settings| &settings.renown);
    let identities = identity::list(&root.path);
    Ok(DesktopState {
        app_name: gate.app_name.clone(),
        root: plain_path(&root.path),
        root_locked: root.source == RootSource::Environment,
        root_missing: root::is_missing(&root),
        synced_folder: root::synced_service(&root.path),
        has_local_key: identities.iter().any(|identity| identity.kind == "local"),
        identities,
        old_data: root.path.join(".ph").is_dir(),
        renown_url: std::env::var("RENOWN_URL")
            .ok()
            .or_else(|| renown.and_then(|renown| renown.url.clone()))
            .unwrap_or_else(|| OFFICIAL_RENOWN.to_string()),
        renown_switchboard_url: std::env::var("SWITCHBOARD_URL")
            .ok()
            .or_else(|| renown.and_then(|renown| renown.switchboard_url.clone())),
        settings_error: settings.err(),
        chain_id: CHAIN_ID,
    })
}

#[tauri::command]
pub async fn pick_root(gate: State<'_, Arc<Gate>>) -> Result<(), String> {
    gate.while_signing_in()?;
    if gate.root().source == RootSource::Environment {
        return Err("The folder is set from outside the app.".to_string());
    }
    let Some(picked) = rfd::AsyncFileDialog::new().pick_folder().await else {
        return Ok(());
    };
    let path = picked.path().to_path_buf();
    root::point_at(&gate.config_dir, &path, &gate.default_root)
        .map_err(|error| error.to_string())?;
    let source = if path == gate.default_root {
        RootSource::Default
    } else {
        RootSource::Pointer
    };
    *gate.root.lock().expect("root lock") = Root { path, source };
    Ok(())
}

#[tauri::command]
pub fn save_renown_url(gate: State<'_, Arc<Gate>>, url: String) -> Result<(), String> {
    gate.while_signing_in()?;
    if !(url.is_empty() || url.starts_with("https://") || url.starts_with("http://")) {
        return Err("That doesn't look like an address.".to_string());
    }
    settings::save_renown_url(&gate.root().path, &url)
}

#[tauri::command]
pub async fn local_create(
    app: AppHandle,
    gate: State<'_, Arc<Gate>>,
    password: String,
    replace: bool,
) -> Result<(), String> {
    gate.while_signing_in()?;
    let key = gate.root().path.join(LOCAL_KEY_FILE);
    if key.exists() && !replace {
        return Err("There's already a local key here.".to_string());
    }
    let input = json!({ "keyPath": plain_path(&key), "password": password });
    gate.local_key(&app, "create", input).await.map(|_| ())
}

#[derive(Serialize)]
pub struct Unlocked {
    credential: Value,
}

/// Unlocks the local key and delegates to the window's session key. The key
/// itself never reaches this process: it stays in the local key's own.
#[tauri::command]
pub async fn local_unlock(
    app: AppHandle,
    gate: State<'_, Arc<Gate>>,
    password: String,
    app_did: String,
    app_name: String,
) -> Result<Unlocked, String> {
    gate.while_signing_in()?;
    let root = gate.root().path;
    let input = json!({
        "keyPath": plain_path(&root.join(LOCAL_KEY_FILE)),
        "password": password,
        "credentialsPath": plain_path(&identity::credentials_file(&root.join(LOCAL))),
        "chainId": CHAIN_ID,
        "appName": app_name,
        "appDid": app_did,
    });
    let mut reply = gate.local_key(&app, "sign", input).await?;
    Ok(Unlocked {
        credential: reply["credential"].take(),
    })
}

#[tauri::command]
pub fn saved_credential(
    gate: State<'_, Arc<Gate>>,
    address: String,
    app_did: String,
) -> Result<Option<Value>, String> {
    gate.while_signing_in()?;
    let folder = gate.folder_of(&address.to_lowercase())?;
    Ok(identity::kept_credential(
        &folder, &address, CHAIN_ID, &app_did,
    ))
}

#[derive(Deserialize)]
pub struct ProfileImage {
    ext: String,
    bytes: Vec<u8>,
}

#[tauri::command]
pub fn save_profile(
    gate: State<'_, Arc<Gate>>,
    address: String,
    name: Option<String>,
    image: Option<ProfileImage>,
) -> Result<(), String> {
    gate.while_signing_in()?;
    let folder = gate.folder_of(&address)?;
    let image = image
        .as_ref()
        .map(|image| (image.ext.as_str(), image.bytes.as_slice()));
    identity::save_profile(&folder, &address, name.as_deref(), image)
}

#[tauri::command]
pub fn adopt_old_data(gate: State<'_, Arc<Gate>>, identity: String) -> Result<(), String> {
    gate.while_signing_in()?;
    let folder = gate.folder_of(&identity)?;
    identity::adopt_old_data(&gate.root().path, &folder)
}

#[tauri::command]
pub fn start_stack(
    app: AppHandle,
    gate: State<'_, Arc<Gate>>,
    identity: String,
) -> Result<(), String> {
    gate.while_signing_in()?;
    let folder = gate.folder_of(&identity)?;
    let local = identity == LOCAL;
    let admin = if local {
        identity::local_address(&gate.root().path).ok_or("There's no local key here.")?
    } else {
        identity
    };
    gate.start(&app, &folder, &admin, local)
}

/// Stops the stack and goes back to the sign-in page. Off the main thread, as
/// the stack can take seconds and the window has to say so meanwhile.
#[tauri::command(async)]
pub fn switch_identity(app: AppHandle, gate: State<'_, Arc<Gate>>) -> Result<(), String> {
    show_closing(&app, &gate.app_name);
    app.state::<Sidecar>().stop();
    let addr = format!("127.0.0.1:{}", gate.port);
    let addr: std::net::SocketAddr = addr.parse().map_err(|_| "bad address")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(100));
    }
    gate.set_phase(Phase::SigningIn);
    gate.open_sign_in(&app)
}
