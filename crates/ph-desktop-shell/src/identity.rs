//! Who has used this computer: `local` when there's a local key, and a folder
//! per Renown identity, named by its lowercase address. Each folder holds that
//! identity's `.ph` tree, its `backups` and a copy of its public profile. The
//! profile is for display only: the folder's name is the identity.

use std::path::{Path, PathBuf};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const LOCAL: &str = "local";
pub const LOCAL_KEY_FILE: &str = "local.key";
const PROFILE_FILE: &str = "profile-cached.json";
const AVATAR_STEM: &str = "profile-cached";

/// In order of preference: the first one found is the avatar.
const AVATARS: [(&str, &str); 3] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("webp", "image/webp"),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub kind: &'static str,
    pub id: String,
    pub address: String,
    pub name: Option<String>,
    pub image: Option<String>,
}

#[derive(Deserialize)]
struct Keystore {
    address: String,
}

#[derive(Deserialize)]
struct Profile {
    name: Option<String>,
}

/// `0x` and 40 lowercase hex digits: a folder that can only be a Renown identity.
pub fn is_renown_id(name: &str) -> bool {
    name.len() == 42
        && name.starts_with("0x")
        && name[2..]
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// The local key's address, which a keystore names without the password.
pub fn local_address(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(LOCAL_KEY_FILE)).ok()?;
    let keystore: Keystore = serde_json::from_str(&text).ok()?;
    let address = keystore.address.to_lowercase();
    Some(if address.starts_with("0x") {
        address
    } else {
        format!("0x{address}")
    })
}

fn listed(folder: &Path, kind: &'static str, id: String, address: String) -> Identity {
    let name = std::fs::read_to_string(folder.join(PROFILE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Profile>(&text).ok())
        .and_then(|profile| profile.name);
    Identity {
        kind,
        id,
        address,
        name,
        image: avatar(folder),
    }
}

/// Everyone in `root`, the local identity first.
pub fn list(root: &Path) -> Vec<Identity> {
    let mut renown: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|name| is_renown_id(name))
                .collect()
        })
        .unwrap_or_default();
    renown.sort();

    let local = local_address(root)
        .map(|address| listed(&root.join(LOCAL), "local", LOCAL.to_string(), address));
    local
        .into_iter()
        .chain(
            renown
                .into_iter()
                .map(|id| listed(&root.join(&id), "renown", id.clone(), id)),
        )
        .collect()
}

fn avatar(folder: &Path) -> Option<String> {
    AVATARS.iter().find_map(|(ext, mime)| {
        let bytes = std::fs::read(folder.join(format!("{AVATAR_STEM}.{ext}"))).ok()?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Some(format!("data:{mime};base64,{encoded}"))
    })
}

/// Saves what's public about a person. Each image replaces the last, in
/// whatever format it comes: left behind, an older file could win the first
/// match and show a stale avatar.
pub fn save_profile(
    folder: &Path,
    address: &str,
    name: Option<&str>,
    image: Option<(&str, &[u8])>,
) -> Result<(), String> {
    std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    if let Some(name) = name {
        let profile = json!({ "address": address, "name": name });
        let text = serde_json::to_string_pretty(&profile).map_err(|error| error.to_string())?;
        std::fs::write(folder.join(PROFILE_FILE), text).map_err(|error| error.to_string())?;
    }
    if let Some((ext, bytes)) = image {
        if !AVATARS.iter().any(|(known, _)| *known == ext) {
            return Err(format!("{ext} isn't an image format this app keeps"));
        }
        for (old, _) in AVATARS {
            let _ = std::fs::remove_file(folder.join(format!("{AVATAR_STEM}.{old}")));
        }
        std::fs::write(folder.join(format!("{AVATAR_STEM}.{ext}")), bytes)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub fn credentials_file(folder: &Path) -> PathBuf {
    folder.join(".ph").join(".credentials.json")
}

/// The credential this identity's switchboard kept for a session key, by the
/// keys `FileCredentialKeep` uses.
pub fn kept_credential(
    folder: &Path,
    address: &str,
    chain_id: u64,
    app_did: &str,
) -> Option<Value> {
    let text = std::fs::read_to_string(credentials_file(folder)).ok()?;
    let mut entries: Value = serde_json::from_str(&text).ok()?;
    let key = format!("{}|{chain_id}|{app_did}", address.to_lowercase());
    entries.get_mut(&key).map(Value::take)
}

/// Moves a store from before sign-in into this identity's folder, and only when
/// that folder has none of its own.
pub fn adopt_old_data(root: &Path, folder: &Path) -> Result<(), String> {
    let source = root.join(".ph");
    let target = folder.join(".ph");
    if target.exists() {
        return Err("This identity already has documents here.".to_string());
    }
    std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    std::fs::rename(source, target).map_err(|error| error.to_string())?;
    // The shell made this for a store that no longer lives there.
    let _ = std::fs::remove_dir(root.join("pglite"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: &str = "0x1111111111111111111111111111111111111111";
    const SECOND: &str = "0x2222222222222222222222222222222222222222";

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ph-identity-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ids(root: &Path) -> Vec<String> {
        list(root).into_iter().map(|identity| identity.id).collect()
    }

    #[test]
    fn knows_a_renown_folder_by_its_name() {
        assert!(is_renown_id(FIRST));
        assert!(!is_renown_id("local"));
        assert!(!is_renown_id(".ph"));
        assert!(!is_renown_id("0x1234"));
        assert!(!is_renown_id("0xABCDEF1111111111111111111111111111111111"));
    }

    #[test]
    fn lists_identities_and_nothing_else() {
        let root = folder("list");
        std::fs::create_dir_all(root.join(SECOND)).unwrap();
        std::fs::create_dir_all(root.join(FIRST)).unwrap();
        std::fs::create_dir_all(root.join(".ph")).unwrap();
        std::fs::create_dir_all(root.join("backups")).unwrap();
        std::fs::write(root.join("settings.json"), "{}").unwrap();

        assert_eq!(ids(&root), vec![FIRST, SECOND]);
    }

    #[test]
    fn the_local_identity_needs_its_key_and_comes_first() {
        let root = folder("local");
        std::fs::create_dir_all(root.join(FIRST)).unwrap();
        std::fs::create_dir_all(root.join(LOCAL)).unwrap();

        assert_eq!(ids(&root), vec![FIRST]);

        std::fs::write(
            root.join(LOCAL_KEY_FILE),
            format!(r#"{{"version":3,"address":"{}"}}"#, &SECOND[2..]),
        )
        .unwrap();

        assert_eq!(ids(&root), vec![LOCAL, FIRST]);
        assert_eq!(list(&root)[0].address, SECOND);
    }

    #[test]
    fn a_broken_key_file_lists_no_local_identity() {
        let root = folder("broken-key");
        std::fs::write(root.join(LOCAL_KEY_FILE), "not json").unwrap();

        assert!(list(&root).is_empty());
    }

    #[test]
    fn names_people_from_their_saved_profile() {
        let root = folder("named");
        let person = root.join(FIRST);

        save_profile(&person, FIRST, Some("marit"), None).unwrap();

        assert_eq!(list(&root)[0].name.as_deref(), Some("marit"));
    }

    #[test]
    fn a_new_avatar_replaces_the_old_in_any_format() {
        let root = folder("avatar");
        let person = root.join(FIRST);
        save_profile(&person, FIRST, None, Some(("png", b"old"))).unwrap();

        save_profile(&person, FIRST, None, Some(("jpg", b"new"))).unwrap();

        assert!(!person.join("profile-cached.png").exists());
        let image = list(&root)[0].image.clone().unwrap();
        assert!(image.starts_with("data:image/jpeg;base64,"), "{image}");
    }

    #[test]
    fn the_first_match_wins_among_avatars() {
        let person = folder("first-match");
        std::fs::write(person.join("profile-cached.webp"), b"w").unwrap();
        std::fs::write(person.join("profile-cached.jpg"), b"j").unwrap();

        assert!(avatar(&person).unwrap().starts_with("data:image/jpeg"));
    }

    #[test]
    fn refuses_an_image_it_could_not_show() {
        let person = folder("svg");

        assert!(save_profile(&person, FIRST, None, Some(("svg", b"<svg/>"))).is_err());
    }

    #[test]
    fn saving_no_name_keeps_the_one_there() {
        let root = folder("keep-name");
        let person = root.join(FIRST);
        save_profile(&person, FIRST, Some("marit"), None).unwrap();

        save_profile(&person, FIRST, None, None).unwrap();

        assert_eq!(list(&root)[0].name.as_deref(), Some("marit"));
    }

    #[test]
    fn finds_a_kept_credential_by_address_chain_and_key() {
        let person = folder("kept");
        std::fs::create_dir_all(person.join(".ph")).unwrap();
        std::fs::write(
            credentials_file(&person),
            format!(r#"{{"{FIRST}|1|did:key:app":{{"id":"urn:x"}}}}"#),
        )
        .unwrap();

        assert_eq!(
            kept_credential(
                &person,
                &FIRST.to_uppercase().replace("0X", "0x"),
                1,
                "did:key:app"
            ),
            Some(json!({ "id": "urn:x" }))
        );
        assert_eq!(kept_credential(&person, FIRST, 1, "did:key:other"), None);
    }

    #[test]
    fn moves_old_data_into_an_identity() {
        let root = folder("adopt");
        std::fs::create_dir_all(root.join(".ph").join("reactor-storage")).unwrap();
        std::fs::create_dir_all(root.join("pglite")).unwrap();
        let person = root.join(FIRST);

        adopt_old_data(&root, &person).unwrap();

        assert!(person.join(".ph").join("reactor-storage").is_dir());
        assert!(!root.join(".ph").exists());
        assert!(!root.join("pglite").exists());
    }

    #[test]
    fn never_moves_old_data_over_an_identitys_own() {
        let root = folder("adopt-refused");
        std::fs::create_dir_all(root.join(".ph")).unwrap();
        let person = root.join(FIRST);
        std::fs::create_dir_all(person.join(".ph")).unwrap();

        assert!(adopt_old_data(&root, &person).is_err());
        assert!(root.join(".ph").is_dir());
    }
}
