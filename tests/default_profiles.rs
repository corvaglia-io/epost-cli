//! Defaults use synthetic temporary configuration only; no host credentials.

use epost_cli::{
    api::Environment,
    config::{self, Login, LoginMethod, Profile, SyncScope},
    credentials,
    models::{Mailbox, MailboxKind},
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        Self {
            path: base.join("settings/config.json"),
            base,
            _temp: temp,
        }
    }

    fn profile(&self, tenant: &str) -> Profile {
        Profile {
            mailbox: Mailbox {
                tenant_id: tenant.to_owned(),
                company_id: 42,
                kind: MailboxKind::Private,
            },
            environment: Environment::Production,
            output: self.base.join("documents"),
            scope: SyncScope::Inbox,
            login: Some(Login {
                username: "synthetic-default@example.invalid".to_owned(),
                method: LoginMethod::MacosKeychain,
            }),
        }
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.path).unwrap()).unwrap()
    }

    fn modify(&self, modify: impl FnOnce(&mut Value)) {
        let mut document = self.document();
        modify(&mut document);
        fs::write(&self.path, serde_json::to_vec(&document).unwrap()).unwrap();
    }

    fn legacy(&self) {
        self.modify(|document| {
            document.as_object_mut().unwrap().remove("default_profile");
        });
    }

    fn assert_invalid_unchanged(&self) {
        let original = fs::read(&self.path).unwrap();
        assert!(config::list_profiles(&self.path).is_err());
        assert!(config::default_profile(&self.path).is_err());
        assert!(config::load_profile(&self.path, "personal").is_err());
        assert!(config::set_default_profile(&self.path, "personal").is_err());
        assert!(
            config::save_profile(&self.path, "other", &self.profile("synthetic-other")).is_err()
        );
        assert_eq!(fs::read(&self.path).unwrap(), original);
    }
}

#[test]
fn absent_configuration_has_no_default_or_profiles_and_reads_create_nothing() {
    let fixture = Fixture::new();
    assert!(config::list_profiles(&fixture.path).unwrap().is_empty());
    assert_eq!(config::default_profile(&fixture.path).unwrap(), None);
    assert!(config::set_default_profile(&fixture.path, "personal").is_err());
    assert_eq!(fs::read_dir(&fixture.base).unwrap().count(), 0);
}

#[test]
fn first_saved_profile_is_default_and_later_writes_preserve_selection() {
    let fixture = Fixture::new();
    let personal = fixture.profile("synthetic-personal");
    let work = fixture.profile("synthetic-work");
    config::save_profile(&fixture.path, "personal", &personal).unwrap();
    assert_eq!(fixture.document()["default_profile"], "personal");
    config::save_profile(&fixture.path, "work", &work).unwrap();
    assert_eq!(
        config::default_profile(&fixture.path).unwrap().as_deref(),
        Some("personal")
    );
    config::set_default_profile(&fixture.path, "work").unwrap();
    let mut replacement = personal.clone();
    replacement.scope = SyncScope::Both;
    config::save_profile(&fixture.path, "personal", &replacement).unwrap();
    config::save_profile(&fixture.path, "third", &fixture.profile("synthetic-third")).unwrap();
    assert_eq!(
        config::default_profile(&fixture.path).unwrap().as_deref(),
        Some("work")
    );
    assert_eq!(config::load_profile(&fixture.path, "work").unwrap(), work);
    assert_eq!(
        config::load_profile(&fixture.path, "personal").unwrap(),
        replacement
    );
    let listed = config::list_profiles(&fixture.path).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["personal", "third", "work"]
    );
    assert_eq!(listed[0].1, replacement);
    assert_eq!(listed[2].1, work);
}

#[test]
fn legacy_single_profile_resolves_without_rewriting_or_changing_credential_identity() {
    let fixture = Fixture::new();
    let personal = fixture.profile("synthetic-personal");
    config::save_profile(&fixture.path, "personal", &personal).unwrap();
    fixture.legacy();
    let original = fs::read(&fixture.path).unwrap();
    let fields = fixture.document()["profiles"].clone();
    let identity = credentials::identity(
        &fixture.path,
        "personal",
        "http://127.0.0.1:19845/",
        &personal,
        "synthetic-default@example.invalid",
    )
    .unwrap();
    assert_eq!(
        config::default_profile(&fixture.path).unwrap().as_deref(),
        Some("personal")
    );
    assert_eq!(
        config::list_profiles(&fixture.path).unwrap(),
        vec![("personal".to_owned(), personal.clone())]
    );
    assert_eq!(
        config::load_profile(&fixture.path, "personal").unwrap(),
        personal
    );
    assert_eq!(fs::read(&fixture.path).unwrap(), original);
    config::set_default_profile(&fixture.path, "personal").unwrap();
    assert_eq!(fixture.document()["schema_version"], 1);
    assert_eq!(fixture.document()["profiles"], fields);
    assert_eq!(
        credentials::identity(
            &fixture.path,
            "personal",
            "http://127.0.0.1:19845/",
            &personal,
            "synthetic-default@example.invalid"
        )
        .unwrap(),
        identity
    );
}

#[test]
fn legacy_multiple_profiles_require_selection_and_explicit_save_can_choose_default() {
    let fixture = Fixture::new();
    config::save_profile(
        &fixture.path,
        "personal",
        &fixture.profile("synthetic-personal"),
    )
    .unwrap();
    config::save_profile(&fixture.path, "work", &fixture.profile("synthetic-work")).unwrap();
    fixture.legacy();
    let original = fs::read(&fixture.path).unwrap();
    assert_eq!(config::default_profile(&fixture.path).unwrap(), None);
    assert_eq!(config::list_profiles(&fixture.path).unwrap().len(), 2);
    assert_eq!(fs::read(&fixture.path).unwrap(), original);
    config::save_profile(&fixture.path, "work", &fixture.profile("synthetic-work")).unwrap();
    assert_eq!(
        config::default_profile(&fixture.path).unwrap().as_deref(),
        Some("work")
    );
    fixture.legacy();
    config::set_default_profile(&fixture.path, "personal").unwrap();
    assert_eq!(
        config::default_profile(&fixture.path).unwrap().as_deref(),
        Some("personal")
    );
}

#[test]
fn invalid_or_missing_default_selection_leaves_saved_configuration_unchanged() {
    let fixture = Fixture::new();
    config::save_profile(
        &fixture.path,
        "personal",
        &fixture.profile("synthetic-personal"),
    )
    .unwrap();
    let original = fs::read(&fixture.path).unwrap();
    for name in [
        "missing",
        "",
        "../personal",
        "bad\nname",
        "bad\0name",
        &"x".repeat(65),
    ] {
        assert!(config::set_default_profile(&fixture.path, name).is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), original);
    }
    for relative in ["config.json", "../config.json"] {
        assert!(config::list_profiles(relative.as_ref()).is_err());
        assert!(config::default_profile(relative.as_ref()).is_err());
        assert!(config::set_default_profile(relative.as_ref(), "personal").is_err());
    }
}

#[test]
fn corrupt_or_unknown_defaults_fail_every_reader_and_writer_without_repairing_data() {
    for invalid in [
        json!("missing"),
        json!("../personal"),
        json!(""),
        json!("bad\nname"),
        json!(5),
        json!([]),
        json!({}),
        json!("x".repeat(65)),
    ] {
        let fixture = Fixture::new();
        config::save_profile(
            &fixture.path,
            "personal",
            &fixture.profile("synthetic-personal"),
        )
        .unwrap();
        fixture.modify(|document| document["default_profile"] = invalid);
        fixture.assert_invalid_unchanged();
    }
    for raw in [b"{not valid synthetic JSON}".as_slice(), b"{\"schema_version\":1,\"default_profile\":\"personal\",\"default_profile\":\"personal\",\"profiles\":{}}"] {
        let fixture = Fixture::new();
        config::save_profile(&fixture.path, "personal", &fixture.profile("synthetic-personal")).unwrap();
        fs::write(&fixture.path, raw).unwrap();
        fixture.assert_invalid_unchanged();
    }
}

#[cfg(unix)]
#[test]
fn default_updates_observe_existing_writer_lock_and_preserve_all_profiles() {
    use fs2::FileExt;
    let fixture = Fixture::new();
    config::save_profile(
        &fixture.path,
        "personal",
        &fixture.profile("synthetic-personal"),
    )
    .unwrap();
    config::save_profile(&fixture.path, "work", &fixture.profile("synthetic-work")).unwrap();
    let original = fs::read(&fixture.path).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.path.with_file_name(".config.json.lock"))
        .unwrap();
    FileExt::try_lock_exclusive(&lock).unwrap();
    assert!(config::set_default_profile(&fixture.path, "work").is_err());
    assert_eq!(fs::read(&fixture.path).unwrap(), original);
    drop(lock);
    config::set_default_profile(&fixture.path, "work").unwrap();
    assert_eq!(
        config::default_profile(&fixture.path).unwrap().as_deref(),
        Some("work")
    );
    assert_eq!(config::list_profiles(&fixture.path).unwrap().len(), 2);
}

#[cfg(unix)]
#[test]
fn defaults_reject_symlinks_hard_links_and_public_config_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for unsafe_kind in ["symlink", "hardlink", "permissions", "lock-symlink"] {
        let fixture = Fixture::new();
        config::save_profile(
            &fixture.path,
            "personal",
            &fixture.profile("synthetic-personal"),
        )
        .unwrap();
        let alias = fixture.base.join("alias.json");
        match unsafe_kind {
            "symlink" => {
                fs::rename(&fixture.path, &alias).unwrap();
                symlink(&alias, &fixture.path).unwrap();
            }
            "hardlink" => fs::hard_link(&fixture.path, &alias).unwrap(),
            "permissions" => {
                fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o644)).unwrap()
            }
            "lock-symlink" => {
                let lock = fixture.path.with_file_name(".config.json.lock");
                fs::rename(&lock, &alias).unwrap();
                symlink(&alias, &lock).unwrap();
            }
            _ => unreachable!(),
        }
        let original = fs::read(&fixture.path).unwrap();
        if unsafe_kind != "lock-symlink" {
            assert!(config::list_profiles(&fixture.path).is_err());
            assert!(config::default_profile(&fixture.path).is_err());
        }
        assert!(config::set_default_profile(&fixture.path, "personal").is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), original);
    }
}
