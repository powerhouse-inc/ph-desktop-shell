//! Where the app keeps everything. The root holds `settings.json`, the local key
//! and one folder per identity, so it can't record its own location: a custom
//! root is named by a small pointer in the OS config folder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const POINTER_FILE: &str = "root.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootSource {
    /// `PH_DESKTOP_ROOT`, as a test run or a managed install sets it.
    Environment,
    /// A folder the person chose.
    Pointer,
    /// The app's own data folder.
    Default,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    pub source: RootSource,
}

#[derive(Serialize, Deserialize)]
struct Pointer {
    root: PathBuf,
}

/// Environment, then the pointer, then the default.
pub fn resolve(env_root: Option<OsString>, config_dir: &Path, default_root: &Path) -> Root {
    if let Some(path) = env_root.filter(|value| !value.is_empty()) {
        return Root {
            path: PathBuf::from(path),
            source: RootSource::Environment,
        };
    }
    let pointer = std::fs::read_to_string(config_dir.join(POINTER_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Pointer>(&text).ok());
    match pointer {
        Some(Pointer { root }) => Root {
            path: root,
            source: RootSource::Pointer,
        },
        None => Root {
            path: default_root.to_path_buf(),
            source: RootSource::Default,
        },
    }
}

/// Points at `path`; the default needs no pointer.
pub fn point_at(config_dir: &Path, path: &Path, default_root: &Path) -> std::io::Result<()> {
    let file = config_dir.join(POINTER_FILE);
    if path == default_root {
        return match std::fs::remove_file(file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(config_dir)?;
    let pointer = Pointer {
        root: path.to_path_buf(),
    };
    std::fs::write(file, serde_json::to_string_pretty(&pointer)?)
}

/// A chosen root that isn't there, as on a drive that's been unplugged. It is
/// reported, never recreated: an empty folder would look like lost documents.
pub fn is_missing(root: &Root) -> bool {
    root.source != RootSource::Default && !root.path.is_dir()
}

/// The sync service a folder belongs to, when its path shows one. A live
/// database can be damaged by a service moving its files around.
pub fn synced_service(path: &Path) -> Option<&'static str> {
    const SERVICES: [(&str, &str); 5] = [
        ("onedrive", "OneDrive"),
        ("mobile documents", "iCloud Drive"),
        ("icloud", "iCloud Drive"),
        ("dropbox", "Dropbox"),
        ("google drive", "Google Drive"),
    ];
    path.components().find_map(|component| {
        let name = component.as_os_str().to_string_lossy().to_lowercase();
        SERVICES
            .iter()
            .find(|(needle, _)| name.contains(needle))
            .map(|(_, service)| *service)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ph-root-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_environment_wins() {
        let config = folder("env");
        let chosen = config.join("chosen");
        point_at(&config, &chosen, Path::new("default")).unwrap();

        let root = resolve(Some("scratch".into()), &config, Path::new("default"));

        assert_eq!(root.path, PathBuf::from("scratch"));
        assert_eq!(root.source, RootSource::Environment);
    }

    #[test]
    fn a_blank_environment_counts_as_unset() {
        let config = folder("blank");

        let root = resolve(Some("".into()), &config, Path::new("default"));

        assert_eq!(root.source, RootSource::Default);
    }

    #[test]
    fn the_pointer_wins_over_the_default() {
        let config = folder("pointer");
        let chosen = config.join("chosen");
        point_at(&config, &chosen, Path::new("default")).unwrap();

        let root = resolve(None, &config, Path::new("default"));

        assert_eq!(root.path, chosen);
        assert_eq!(root.source, RootSource::Pointer);
    }

    #[test]
    fn going_back_to_the_default_removes_the_pointer() {
        let config = folder("back");
        let default_root = config.join("default");
        point_at(&config, &config.join("chosen"), &default_root).unwrap();

        point_at(&config, &default_root, &default_root).unwrap();

        assert_eq!(
            resolve(None, &config, &default_root).source,
            RootSource::Default
        );
        assert!(!config.join(POINTER_FILE).exists());
    }

    #[test]
    fn a_broken_pointer_falls_back_to_the_default() {
        let config = folder("broken");
        std::fs::write(config.join(POINTER_FILE), "not json").unwrap();

        let root = resolve(None, &config, Path::new("default"));

        assert_eq!(root.source, RootSource::Default);
    }

    #[test]
    fn a_chosen_root_can_be_missing_and_the_default_cannot() {
        let gone = PathBuf::from("no/such/folder");
        let missing = |source| {
            is_missing(&Root {
                path: gone.clone(),
                source,
            })
        };

        assert!(missing(RootSource::Pointer));
        assert!(missing(RootSource::Environment));
        assert!(!missing(RootSource::Default));
    }

    #[test]
    fn names_the_service_a_folder_syncs_with() {
        let named = |path: &str| synced_service(Path::new(path));

        assert_eq!(named("C:/Users/m/OneDrive - Acme/Sektar"), Some("OneDrive"));
        assert_eq!(
            named("/Users/m/Library/Mobile Documents/com~apple~CloudDocs/Sektar"),
            Some("iCloud Drive")
        );
        assert_eq!(named("/home/m/Dropbox/Sektar"), Some("Dropbox"));
        assert_eq!(named("D:/data/sektar"), None);
    }
}
