//! `settings.json` in the root: what every identity shares, read before the
//! stack starts. The file holds named settings, never environment variables, so
//! it can't reach sign-in, ports or secrets: only what's listed here goes through.

use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

const SETTINGS_FILE: &str = "settings.json";

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub memory: Memory,
    pub concurrency: Concurrency,
    pub renown: Renown,
    pub registry_url: Option<String>,
    pub log_level: Option<String>,
    pub startup_timeout_seconds: Option<u64>,
    pub automations: Option<bool>,
    pub telemetry: Telemetry,
    pub window: Window,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Memory {
    pub main_mb: Option<u64>,
    pub workflow_worker_mb: Option<u64>,
    pub pglite_worker_mb: Option<u64>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Concurrency {
    pub workflow_runs: Option<u32>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Renown {
    pub url: Option<String>,
    pub switchboard_url: Option<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Telemetry {
    pub diagnostics: Option<bool>,
    pub error_reports: Option<bool>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Window {
    pub close_quits: Option<bool>,
}

const KNOWN: [&str; 14] = [
    "version",
    "memory.mainMb",
    "memory.workflowWorkerMb",
    "memory.pgliteWorkerMb",
    "concurrency.workflowRuns",
    "renown.url",
    "renown.switchboardUrl",
    "registryUrl",
    "logLevel",
    "startupTimeoutSeconds",
    "automations",
    "telemetry.diagnostics",
    "telemetry.errorReports",
    "window.closeQuits",
];

/// The settings in `root`, or why they can't be read. No file means defaults.
pub fn read(root: &Path) -> Result<Settings, String> {
    let text = match std::fs::read_to_string(root.join(SETTINGS_FILE)) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default())
        }
        Err(error) => return Err(error.to_string()),
    };
    let parsed: Settings = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        for key in unknown_keys(&value, "") {
            crate::diag::log(format!("settings.json: ignoring {key}"));
        }
    }
    Ok(parsed)
}

fn unknown_keys(value: &Value, prefix: &str) -> Vec<String> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    object
        .iter()
        .flat_map(|(key, child)| {
            let path = format!("{prefix}{key}");
            if KNOWN.contains(&path.as_str()) {
                Vec::new()
            } else if child.is_object() && KNOWN.iter().any(|k| k.starts_with(&format!("{path}.")))
            {
                unknown_keys(child, &format!("{path}."))
            } else {
                vec![path]
            }
        })
        .collect()
}

/// Saves the Renown instance, leaving the rest of the file as it was. Blank goes
/// back to the official one.
pub fn save_renown_url(root: &Path, url: &str) -> Result<(), String> {
    let file = root.join(SETTINGS_FILE);
    let mut value: Value = match std::fs::read_to_string(&file) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| error.to_string())?,
        Err(_) => json!({}),
    };
    let object = value
        .as_object_mut()
        .ok_or("settings.json isn't an object")?;
    let renown = object.entry("renown").or_insert_with(|| json!({}));
    let renown = renown.as_object_mut().ok_or("renown isn't an object")?;
    if url.is_empty() {
        renown.remove("url");
    } else {
        renown.insert("url".into(), json!(url));
    }
    let text = serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?;
    std::fs::write(file, format!("{text}\n")).map_err(|error| error.to_string())
}

/// What a machine with `ram_mb` of memory can give the stack, with room for
/// everything else on it.
fn default_memory(ram_mb: u64) -> (u64, u64) {
    (
        (ram_mb / 4).clamp(2048, 8192),
        (ram_mb / 16).clamp(512, 2048),
    )
}

impl Settings {
    pub fn closes_to_quit(&self) -> bool {
        self.window.close_quits.unwrap_or(false)
    }

    pub fn startup_timeout(&self, default: std::time::Duration) -> std::time::Duration {
        self.startup_timeout_seconds
            .map_or(default, std::time::Duration::from_secs)
    }

    /// The stack's environment. What's already in the environment wins, so a
    /// person can always override this file from outside it.
    pub fn stack_env(
        &self,
        ram_mb: u64,
        inherited: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<(String, String)> {
        let (default_main, default_worker) = default_memory(ram_mb);
        let mut env = Vec::new();
        let mut set = |name: &str, value: Option<String>| {
            if let (None, Some(value)) = (inherited(name), value) {
                env.push((name.to_string(), value));
            }
        };

        set(
            "PH_WORKFLOWS_WORKER_MEMORY_MB",
            Some(
                self.memory
                    .workflow_worker_mb
                    .unwrap_or(default_worker)
                    .to_string(),
            ),
        );
        set(
            "PH_PGLITE_WORKER_MEMORY_MB",
            self.memory.pglite_worker_mb.map(|mb| mb.to_string()),
        );
        set(
            "PH_WORKFLOWS_RUN_CONCURRENCY",
            self.concurrency.workflow_runs.map(|n| n.to_string()),
        );
        set("RENOWN_URL", self.renown.url.clone());
        set("SWITCHBOARD_URL", self.renown.switchboard_url.clone());
        set("PH_REGISTRY_URL", self.registry_url.clone());
        set("LOG_LEVEL", self.log_level.clone());
        set(
            "PH_WORKFLOWS_ENABLED",
            self.automations
                .filter(|on| !on)
                .map(|_| "false".to_string()),
        );
        // PH_TELEMETRY can only stop what's on. It's PH_MEMORY_TELEMETRY that writes.
        let diagnostics = self.telemetry.diagnostics.unwrap_or(true);
        set(
            "PH_TELEMETRY",
            Some(if diagnostics { "on" } else { "off" }.to_string()),
        );
        set(
            "PH_MEMORY_TELEMETRY",
            diagnostics.then(|| "on".to_string()),
        );
        if !self.telemetry.error_reports.unwrap_or(false) {
            set("PH_NO_TELEMETRY", Some("1".to_string()));
            set("DO_NOT_TRACK", Some("1".to_string()));
        }

        // NODE_OPTIONS holds other flags too, so this adds to it.
        let options = inherited("NODE_OPTIONS").unwrap_or_default();
        if !options.contains("--max-old-space-size") {
            let main = self.memory.main_mb.unwrap_or(default_main);
            let flag = format!("--max-old-space-size={main}");
            let joined = format!("{options} {flag}");
            env.push(("NODE_OPTIONS".to_string(), joined.trim().to_string()));
        }
        env
    }
}

/// What the stack runs as. A build is one thing throughout: a release build
/// runs production, and a debug build is left to the dev flow that started it.
/// The environment still wins.
pub fn node_env(release: bool, inherited: Option<&str>) -> Option<&'static str> {
    (release && inherited.is_none()).then_some("production")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_build_runs_production() {
        assert_eq!(node_env(true, None), Some("production"));
    }

    #[test]
    fn a_debug_build_stays_dev() {
        assert_eq!(node_env(false, None), None);
    }

    #[test]
    fn the_environment_decides_when_it_says() {
        assert_eq!(node_env(true, Some("development")), None);
    }

    const GB: u64 = 1024;

    fn folder(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ph-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn parsed(text: &str) -> Settings {
        serde_json::from_str(text).unwrap()
    }

    fn env_of(settings: &Settings, ram_mb: u64) -> Vec<(String, String)> {
        settings.stack_env(ram_mb, &|_| None)
    }

    fn value<'a>(env: &'a [(String, String)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn no_file_means_defaults() {
        assert_eq!(read(&folder("none")).unwrap(), Settings::default());
    }

    #[test]
    fn a_broken_file_is_an_error_not_defaults() {
        let dir = folder("broken");
        std::fs::write(dir.join(SETTINGS_FILE), "{ nope").unwrap();

        assert!(read(&dir).is_err());
    }

    #[test]
    fn a_wrong_type_is_an_error() {
        let dir = folder("type");
        std::fs::write(dir.join(SETTINGS_FILE), r#"{"memory":{"mainMb":"lots"}}"#).unwrap();

        assert!(read(&dir).is_err());
    }

    #[test]
    fn memory_follows_the_machine() {
        let small = env_of(&Settings::default(), 8 * GB);
        let large = env_of(&Settings::default(), 64 * GB);

        assert_eq!(
            value(&small, "NODE_OPTIONS"),
            Some("--max-old-space-size=2048")
        );
        assert_eq!(value(&small, "PH_WORKFLOWS_WORKER_MEMORY_MB"), Some("512"));
        assert_eq!(
            value(&large, "NODE_OPTIONS"),
            Some("--max-old-space-size=8192")
        );
        assert_eq!(value(&large, "PH_WORKFLOWS_WORKER_MEMORY_MB"), Some("2048"));
    }

    #[test]
    fn named_settings_become_their_variables() {
        let settings = parsed(
            r#"{
              "memory": { "mainMb": 3000, "workflowWorkerMb": 1500, "pgliteWorkerMb": 700 },
              "concurrency": { "workflowRuns": 2 },
              "renown": { "url": "https://r.test", "switchboardUrl": "https://s.test/graphql" },
              "registryUrl": "https://registry.test",
              "logLevel": "debug"
            }"#,
        );
        let env = env_of(&settings, 16 * GB);

        assert_eq!(
            value(&env, "NODE_OPTIONS"),
            Some("--max-old-space-size=3000")
        );
        assert_eq!(value(&env, "PH_WORKFLOWS_WORKER_MEMORY_MB"), Some("1500"));
        assert_eq!(value(&env, "PH_PGLITE_WORKER_MEMORY_MB"), Some("700"));
        assert_eq!(value(&env, "PH_WORKFLOWS_RUN_CONCURRENCY"), Some("2"));
        assert_eq!(value(&env, "RENOWN_URL"), Some("https://r.test"));
        assert_eq!(
            value(&env, "SWITCHBOARD_URL"),
            Some("https://s.test/graphql")
        );
        assert_eq!(
            value(&env, "PH_REGISTRY_URL"),
            Some("https://registry.test")
        );
        assert_eq!(value(&env, "LOG_LEVEL"), Some("debug"));
    }

    /// They need Postgres, so on this embedded database the stack wouldn't start.
    #[test]
    fn worker_pools_cant_be_asked_for() {
        let env = env_of(&parsed(r#"{"concurrency":{"reactorWorkers":3}}"#), 16 * GB);

        assert_eq!(value(&env, "REACTOR_WORKERS"), None);
    }

    #[test]
    fn unset_settings_stay_out_of_the_environment() {
        let env = env_of(&Settings::default(), 16 * GB);

        for name in [
            "RENOWN_URL",
            "PH_REGISTRY_URL",
            "REACTOR_WORKERS",
            "PH_WORKFLOWS_ENABLED",
        ] {
            assert_eq!(value(&env, name), None, "{name}");
        }
    }

    #[test]
    fn telemetry_stays_on_this_computer_by_default() {
        let env = env_of(&Settings::default(), 16 * GB);

        assert_eq!(value(&env, "PH_TELEMETRY"), Some("on"));
        assert_eq!(value(&env, "PH_MEMORY_TELEMETRY"), Some("on"));
        assert_eq!(value(&env, "PH_NO_TELEMETRY"), Some("1"));
        assert_eq!(value(&env, "DO_NOT_TRACK"), Some("1"));
    }

    #[test]
    fn diagnostics_can_be_turned_off() {
        let env = env_of(&parsed(r#"{"telemetry":{"diagnostics":false}}"#), 16 * GB);

        assert_eq!(value(&env, "PH_TELEMETRY"), Some("off"));
        assert_eq!(value(&env, "PH_MEMORY_TELEMETRY"), None);
    }

    #[test]
    fn error_reports_are_a_choice() {
        let env = env_of(&parsed(r#"{"telemetry":{"errorReports":true}}"#), 16 * GB);

        assert_eq!(value(&env, "PH_NO_TELEMETRY"), None);
        assert_eq!(value(&env, "DO_NOT_TRACK"), None);
    }

    #[test]
    fn automations_can_be_left_off_for_recovery() {
        let off = env_of(&parsed(r#"{"automations":false}"#), 16 * GB);
        let on = env_of(&parsed(r#"{"automations":true}"#), 16 * GB);

        assert_eq!(value(&off, "PH_WORKFLOWS_ENABLED"), Some("false"));
        assert_eq!(value(&on, "PH_WORKFLOWS_ENABLED"), None);
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        let settings = parsed(r#"{"logLevel":"debug","memory":{"mainMb":3000}}"#);
        let inherited = |name: &str| match name {
            "LOG_LEVEL" => Some("warn".to_string()),
            "NODE_OPTIONS" => Some("--max-old-space-size=9000".to_string()),
            _ => None,
        };

        let env = settings.stack_env(16 * GB, &inherited);

        assert_eq!(value(&env, "LOG_LEVEL"), None);
        assert_eq!(value(&env, "NODE_OPTIONS"), None);
    }

    #[test]
    fn other_node_options_are_kept() {
        let inherited =
            |name: &str| (name == "NODE_OPTIONS").then(|| "--trace-warnings".to_string());

        let env = Settings::default().stack_env(16 * GB, &inherited);

        assert_eq!(
            value(&env, "NODE_OPTIONS"),
            Some("--trace-warnings --max-old-space-size=4096")
        );
    }

    #[test]
    fn nothing_in_the_file_can_reach_sign_in_or_secrets() {
        let settings = parsed(
            r#"{
              "AUTH_ENABLED": "false",
              "ADMINS": "0xdead",
              "env": { "REQUIRE_AUTHENTICATED_CALLER": "false" },
              "memory": { "AUTH_ENABLED": "false" }
            }"#,
        );
        let env = env_of(&settings, 16 * GB);

        for name in [
            "AUTH_ENABLED",
            "ADMINS",
            "REQUIRE_AUTHENTICATED_CALLER",
            "RENOWN_SOURCE",
        ] {
            assert_eq!(value(&env, name), None, "{name}");
        }
    }

    #[test]
    fn names_what_it_ignores() {
        let value: Value = serde_json::from_str(
            r#"{"logLevel":"info","ADMINS":"x","memory":{"mainMb":1,"AUTH_ENABLED":"x"}}"#,
        )
        .unwrap();

        let mut keys = unknown_keys(&value, "");
        keys.sort();

        assert_eq!(keys, vec!["ADMINS", "memory.AUTH_ENABLED"]);
    }

    #[test]
    fn saving_the_renown_url_keeps_the_rest() {
        let dir = folder("save");
        std::fs::write(dir.join(SETTINGS_FILE), r#"{"logLevel":"debug"}"#).unwrap();

        save_renown_url(&dir, "https://r.test").unwrap();
        let saved = read(&dir).unwrap();

        assert_eq!(saved.renown.url.as_deref(), Some("https://r.test"));
        assert_eq!(saved.log_level.as_deref(), Some("debug"));

        save_renown_url(&dir, "").unwrap();

        assert_eq!(read(&dir).unwrap().renown.url, None);
    }

    #[test]
    fn saving_creates_the_file() {
        let dir = folder("create");

        save_renown_url(&dir, "https://r.test").unwrap();

        assert_eq!(
            read(&dir).unwrap().renown.url.as_deref(),
            Some("https://r.test")
        );
    }

    #[test]
    fn the_startup_timeout_can_be_longer() {
        let default = std::time::Duration::from_secs(180);

        assert_eq!(Settings::default().startup_timeout(default), default);
        assert_eq!(
            parsed(r#"{"startupTimeoutSeconds":600}"#).startup_timeout(default),
            std::time::Duration::from_secs(600)
        );
    }
}
