//! Synthetic, local-only configuration tests; no host profiles or credentials.

use epost_cli::api::Environment;
use epost_cli::config::{self, Login, LoginMethod, Profile, SyncScope};
use epost_cli::error::Error;
use epost_cli::models::{Mailbox, MailboxKind};
use serde_json::{Value, json};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    base: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        // macOS temporary paths contain a system symlink; use the physical path.
        let base = temp.path().canonicalize().unwrap();
        let config = base.join("config/epost-cli/config.json");
        Self {
            _temp: temp,
            base,
            config,
        }
    }

    fn profile(&self) -> Profile {
        Profile {
            mailbox: Mailbox {
                tenant_id: "invented-private-tenant".to_owned(),
                company_id: 0,
                kind: MailboxKind::Private,
            },
            environment: Environment::Production,
            output: self.base.join("documents"),
            scope: SyncScope::Inbox,
            login: None,
        }
    }

    fn document(&self) -> Value {
        serde_json::from_slice(&fs::read(&self.config).unwrap()).unwrap()
    }

    fn modify(&self, change: impl FnOnce(&mut Value)) {
        let mut document = self.document();
        change(&mut document);
        fs::write(&self.config, serde_json::to_vec(&document).unwrap()).unwrap();
    }
}

#[test]
fn missing_profile_checks_do_not_create_files_or_parent_directories() {
    let fixture = Fixture::new();
    assert!(!config::profile_exists(&fixture.config, "personal").unwrap());
    assert!(config::load_profile(&fixture.config, "personal").is_err());
    assert_eq!(fs::read_dir(&fixture.base).unwrap().count(), 0);
}

#[test]
fn roundtrip_preserves_other_profiles_and_only_declared_settings() {
    let fixture = Fixture::new();
    let personal = fixture.profile();
    config::save_profile(&fixture.config, "personal", &personal).unwrap();
    let mut work = fixture.profile();
    work.mailbox = Mailbox {
        tenant_id: "invented-business-tenant".to_owned(),
        company_id: 987654,
        kind: MailboxKind::Business,
    };
    work.environment = Environment::Sandbox;
    work.scope = SyncScope::Both;
    work.output = fixture.base.join("work-documents");
    config::save_profile(&fixture.config, "work", &work).unwrap();
    let mut edited = personal.clone();
    edited.scope = SyncScope::Archive;
    config::save_profile(&fixture.config, "personal", &edited).unwrap();
    let loaded_work = config::load_profile(&fixture.config, "work").unwrap();
    let loaded_personal = config::load_profile(&fixture.config, "personal").unwrap();
    assert_eq!(loaded_work.mailbox, work.mailbox);
    assert_eq!(loaded_work.output, work.output);
    assert_eq!(loaded_work.environment, Environment::Sandbox);
    assert_eq!(loaded_work.scope, SyncScope::Both);
    assert_eq!(loaded_personal.mailbox, personal.mailbox);
    assert_eq!(loaded_personal.scope, SyncScope::Archive);
    assert!(config::profile_exists(&fixture.config, "work").unwrap());
    assert!(!config::profile_exists(&fixture.config, "absent").unwrap());
    assert!(config::load_profile(&fixture.config, "absent").is_err());
    let document = fixture.document();
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document.as_object().unwrap().len(), 2);
    assert_eq!(document["profiles"].as_object().unwrap().len(), 2);
    for profile in document["profiles"].as_object().unwrap().values() {
        let keys: Vec<_> = profile
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["environment", "mailbox", "output", "scope"]);
        assert_eq!(profile["mailbox"].as_object().unwrap().len(), 3);
    }
}

#[test]
fn schema_one_profiles_without_login_load_unchanged() {
    let fixture = Fixture::new();
    let expected = fixture.profile();
    config::save_profile(&fixture.config, "personal", &expected).unwrap();
    let raw = fs::read(&fixture.config).unwrap();
    let document = fixture.document();
    assert_eq!(document["schema_version"], 1);
    assert!(document["profiles"]["personal"].get("login").is_none());
    assert_eq!(
        config::load_profile(&fixture.config, "personal").unwrap(),
        expected
    );
    assert_eq!(fs::read(&fixture.config).unwrap(), raw);
}

#[test]
fn optional_login_roundtrips_only_username_and_method_for_each_supported_source() {
    for (method, encoded) in [
        (LoginMethod::Prompt, "prompt"),
        (LoginMethod::MacosKeychain, "macos_keychain"),
    ] {
        let fixture = Fixture::new();
        let mut profile = fixture.profile();
        profile.login = Some(Login {
            username: "synthetic-person@example.invalid".to_owned(),
            method,
        });
        config::save_profile(&fixture.config, "personal", &profile).unwrap();
        assert_eq!(
            config::load_profile(&fixture.config, "personal").unwrap(),
            profile
        );
        assert_eq!(
            fixture.document()["profiles"]["personal"]["login"],
            json!({"username": "synthetic-person@example.invalid", "method": encoded})
        );
        let mut without_login = profile;
        without_login.login = None;
        config::save_profile(&fixture.config, "personal", &without_login).unwrap();
        assert!(
            fixture.document()["profiles"]["personal"]
                .get("login")
                .is_none()
        );
    }
}

#[test]
fn invalid_remembered_usernames_are_rejected_before_writing_or_on_load() {
    for username in [
        "",
        " ",
        "\t",
        "control\0name",
        "control\nname",
        &"a".repeat(4097),
    ] {
        let fixture = Fixture::new();
        let mut invalid = fixture.profile();
        invalid.login = Some(Login {
            username: username.to_owned(),
            method: LoginMethod::Prompt,
        });
        let error = config::save_profile(&fixture.config, "personal", &invalid).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid configuration: profile login username is invalid"
        );
        assert!(!fixture.config.exists());
        config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
        fixture.modify(|document| {
            document["profiles"]["personal"]["login"] =
                json!({"username": username, "method": "prompt"});
        });
        let original = fs::read(&fixture.config).unwrap();
        assert!(config::load_profile(&fixture.config, "personal").is_err());
        assert!(config::save_profile(&fixture.config, "other", &fixture.profile()).is_err());
        assert_eq!(fs::read(&fixture.config).unwrap(), original);
    }
}

#[test]
fn compare_save_rejects_a_profile_created_after_an_absence_check() {
    let fixture = Fixture::new();
    assert!(!config::profile_exists(&fixture.config, "personal").unwrap());
    let mut other_writer = fixture.profile();
    other_writer.scope = SyncScope::Archive;
    config::save_profile(&fixture.config, "personal", &other_writer).unwrap();
    let original = fs::read(&fixture.config).unwrap();
    assert!(
        config::save_profile_if_unchanged(&fixture.config, "personal", &fixture.profile(), None,)
            .is_err()
    );
    assert_eq!(fs::read(&fixture.config).unwrap(), original);
    assert_eq!(
        config::load_profile(&fixture.config, "personal").unwrap(),
        other_writer
    );
}

#[test]
fn compare_save_rejects_a_profile_changed_after_confirmation_snapshot() {
    let fixture = Fixture::new();
    config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
    let reviewed = config::load_profile(&fixture.config, "personal").unwrap();
    let mut other_writer = reviewed.clone();
    other_writer.output = fixture.base.join("different-documents");
    config::save_profile(&fixture.config, "personal", &other_writer).unwrap();
    let original = fs::read(&fixture.config).unwrap();
    let mut requested = reviewed.clone();
    requested.scope = SyncScope::Both;
    assert!(
        config::save_profile_if_unchanged(
            &fixture.config,
            "personal",
            &requested,
            Some(&reviewed),
        )
        .is_err()
    );
    assert_eq!(fs::read(&fixture.config).unwrap(), original);
    assert_eq!(
        config::load_profile(&fixture.config, "personal").unwrap(),
        other_writer
    );
}

#[test]
fn compare_save_matches_absence_or_old_profile_and_preserves_unrelated_updates() {
    let fixture = Fixture::new();
    config::save_profile_if_unchanged(&fixture.config, "personal", &fixture.profile(), None)
        .unwrap();
    let reviewed = config::load_profile(&fixture.config, "personal").unwrap();
    let mut work = fixture.profile();
    work.mailbox.kind = MailboxKind::Business;
    work.mailbox.company_id = 876;
    work.mailbox.tenant_id = "invented-other-tenant".to_owned();
    config::save_profile(&fixture.config, "work", &work).unwrap();
    let mut requested = reviewed.clone();
    requested.scope = SyncScope::Both;
    config::save_profile_if_unchanged(&fixture.config, "personal", &requested, Some(&reviewed))
        .unwrap();
    assert_eq!(
        config::load_profile(&fixture.config, "personal").unwrap(),
        requested
    );
    assert_eq!(config::load_profile(&fixture.config, "work").unwrap(), work);
}

#[test]
fn associated_effect_does_not_run_for_stale_invalid_or_corrupt_configuration() {
    for failure in ["stale", "absent", "invalid", "corrupt"] {
        let fixture = Fixture::new();
        let reviewed = fixture.profile();
        config::save_profile(&fixture.config, "personal", &reviewed).unwrap();
        let mut requested = reviewed.clone();
        requested.scope = SyncScope::Archive;
        match failure {
            "stale" => {
                let mut current = reviewed.clone();
                current.scope = SyncScope::Both;
                config::save_profile(&fixture.config, "personal", &current).unwrap();
            }
            "absent" => {}
            "invalid" => requested.output = PathBuf::from("relative/documents"),
            "corrupt" => fs::write(&fixture.config, b"{broken synthetic JSON").unwrap(),
            _ => unreachable!(),
        }
        let original = fs::read(&fixture.config).unwrap();
        let applied = Cell::new(false);
        let rolled_back = Cell::new(false);
        assert!(
            config::save_profile_with_effect(
                &fixture.config,
                "personal",
                &requested,
                (failure != "absent").then_some(&reviewed),
                || {
                    applied.set(true);
                    Ok(())
                },
                |_| {
                    rolled_back.set(true);
                    Ok(())
                },
            )
            .is_err(),
            "{failure}"
        );
        assert!(!applied.get(), "{failure}");
        assert!(!rolled_back.get(), "{failure}");
        assert_eq!(fs::read(&fixture.config).unwrap(), original, "{failure}");
    }
}

fn assert_config_writer_locked(fixture: &Fixture) {
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.config.with_file_name(".config.json.lock"))
        .unwrap();
    assert!(fs2::FileExt::try_lock_exclusive(&lock).is_err());
}

fn prepared_config_files(fixture: &Fixture) -> Vec<PathBuf> {
    fs::read_dir(fixture.config.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".epost-config-")
        })
        .collect()
}

#[test]
fn associated_effect_failure_preserves_config_and_removes_prepared_file() {
    let fixture = Fixture::new();
    let reviewed = fixture.profile();
    config::save_profile(&fixture.config, "personal", &reviewed).unwrap();
    let original = fs::read(&fixture.config).unwrap();
    let mut requested = reviewed.clone();
    requested.scope = SyncScope::Archive;
    let applied = Cell::new(false);
    let rolled_back = Cell::new(false);
    let error = config::save_profile_with_effect(
        &fixture.config,
        "personal",
        &requested,
        Some(&reviewed),
        || {
            assert_config_writer_locked(&fixture);
            let prepared = prepared_config_files(&fixture);
            assert_eq!(prepared.len(), 1);
            let replacement: Value =
                serde_json::from_slice(&fs::read(&prepared[0]).unwrap()).unwrap();
            assert_eq!(replacement["profiles"]["personal"]["scope"], "archive");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(&prepared[0]).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            applied.set(true);
            Err::<(), _>(Error::Config("synthetic effect failed"))
        },
        |_| {
            rolled_back.set(true);
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid configuration: synthetic effect failed"
    );
    assert!(applied.get());
    assert!(!rolled_back.get());
    assert_eq!(fs::read(&fixture.config).unwrap(), original);
    assert!(prepared_config_files(&fixture).is_empty());
}

#[test]
fn associated_effect_commits_with_config_and_does_not_roll_back() {
    let fixture = Fixture::new();
    let reviewed = fixture.profile();
    config::save_profile(&fixture.config, "personal", &reviewed).unwrap();
    let mut requested = reviewed.clone();
    requested.scope = SyncScope::Archive;
    let applied = Cell::new(0);
    let rolled_back = Cell::new(false);
    config::save_profile_with_effect(
        &fixture.config,
        "personal",
        &requested,
        Some(&reviewed),
        || {
            assert_config_writer_locked(&fixture);
            assert_eq!(
                config::load_profile(&fixture.config, "personal").unwrap(),
                reviewed
            );
            let previous = applied.replace(1);
            Ok(previous)
        },
        |previous| {
            rolled_back.set(true);
            applied.set(previous);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(applied.get(), 1);
    assert!(!rolled_back.get());
    assert_eq!(
        config::load_profile(&fixture.config, "personal").unwrap(),
        requested
    );
    assert!(prepared_config_files(&fixture).is_empty());
}

#[test]
fn failed_atomic_save_rolls_back_associated_effect_before_unlocking() {
    let fixture = Fixture::new();
    let reviewed = fixture.profile();
    config::save_profile(&fixture.config, "personal", &reviewed).unwrap();
    let original = fs::read(&fixture.config).unwrap();
    let backup = fixture.base.join("previous-config.json");
    let mut requested = reviewed.clone();
    requested.scope = SyncScope::Archive;
    let applied = Cell::new(false);
    let rolled_back = Cell::new(false);
    let error = config::save_profile_with_effect(
        &fixture.config,
        "personal",
        &requested,
        Some(&reviewed),
        || {
            applied.set(true);
            // Synthetic fault injection: normal callbacks must not modify config.
            fs::rename(&fixture.config, &backup).unwrap();
            fs::create_dir(&fixture.config).unwrap();
            Ok(backup.clone())
        },
        |previous| {
            assert_config_writer_locked(&fixture);
            rolled_back.set(true);
            applied.set(false);
            fs::remove_dir(&fixture.config).unwrap();
            fs::rename(previous, &fixture.config).unwrap();
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid configuration: configuration could not be saved atomically"
    );
    assert!(!applied.get());
    assert!(rolled_back.get());
    assert_eq!(fs::read(&fixture.config).unwrap(), original);
    assert_eq!(
        config::load_profile(&fixture.config, "personal").unwrap(),
        reviewed
    );
    assert!(prepared_config_files(&fixture).is_empty());
}

#[test]
fn associated_effect_rollback_failure_is_reported_without_inner_details() {
    let fixture = Fixture::new();
    let reviewed = fixture.profile();
    config::save_profile(&fixture.config, "personal", &reviewed).unwrap();
    let backup = fixture.base.join("previous-config.json");
    let rolled_back = Cell::new(false);
    let error = config::save_profile_with_effect(
        &fixture.config,
        "personal",
        &reviewed,
        Some(&reviewed),
        || {
            // Synthetic fault injection: force rename failure, retaining old data.
            fs::rename(&fixture.config, &backup).unwrap();
            fs::create_dir(&fixture.config).unwrap();
            Ok(())
        },
        |_| {
            assert_config_writer_locked(&fixture);
            rolled_back.set(true);
            Err(Error::Config("fake-secret-in-rollback-error"))
        },
    )
    .unwrap_err();
    assert!(rolled_back.get());
    assert_eq!(
        error.to_string(),
        "invalid configuration: configuration could not be saved and rollback failed; repair the external credential store and run setup again before using this profile"
    );
    assert!(!error.to_string().contains("fake-secret"));
    assert!(prepared_config_files(&fixture).is_empty());
    // Restore the fault-injected path after the transaction has released its lock.
    fs::remove_dir(&fixture.config).unwrap();
    fs::rename(backup, &fixture.config).unwrap();
    assert_eq!(
        config::load_profile(&fixture.config, "personal").unwrap(),
        reviewed
    );
}

#[test]
fn rejects_unsafe_names_before_creating_any_configuration() {
    let fixture = Fixture::new();
    for name in [
        "",
        "../escape",
        "a/b",
        "a\\b",
        "a b",
        "a\n",
        "ä",
        &"a".repeat(65),
    ] {
        assert!(config::save_profile(&fixture.config, name, &fixture.profile()).is_err());
        assert!(config::load_profile(&fixture.config, name).is_err());
        assert!(config::profile_exists(&fixture.config, name).is_err());
    }
    assert!(!fixture.config.exists());
    config::save_profile(&fixture.config, "Valid-name_09", &fixture.profile()).unwrap();
    assert!(config::profile_exists(&fixture.config, "Valid-name_09").unwrap());
}

#[test]
fn rejects_relative_root_and_parent_traversal_paths() {
    let fixture = Fixture::new();
    for bad in [
        PathBuf::from("relative/config.json"),
        PathBuf::from("/"),
        fixture.base.join("missing/../config.json"),
        fixture.base.join("bad\nname/config.json"),
    ] {
        assert!(config::path(Some(&bad)).is_err());
        assert!(config::save_profile(&bad, "personal", &fixture.profile()).is_err());
        assert!(config::profile_exists(&bad, "personal").is_err());
    }
    for bad in [
        PathBuf::from("relative/documents"),
        PathBuf::from("/"),
        fixture.base.join("missing/../documents"),
    ] {
        let mut profile = fixture.profile();
        profile.output = bad;
        assert!(config::save_profile(&fixture.config, "personal", &profile).is_err());
    }
    assert!(!fixture.config.exists());
    assert_eq!(config::path(Some(&fixture.config)).unwrap(), fixture.config);
}

#[test]
fn rejects_invalid_mailbox_identities_without_writing() {
    let fixture = Fixture::new();
    for id in ["", " ", ".", "..", "control\0id", &"a".repeat(4097)] {
        let mut profile = fixture.profile();
        profile.mailbox.tenant_id = id.to_owned();
        assert!(config::save_profile(&fixture.config, "personal", &profile).is_err());
    }
    let mut profile = fixture.profile();
    profile.mailbox.company_id = -1;
    assert!(config::save_profile(&fixture.config, "personal", &profile).is_err());
    assert!(!fixture.config.exists());
}

#[test]
fn corrupt_or_unsupported_config_is_never_overwritten() {
    for corruption in [
        "syntax",
        "schema",
        "unknown",
        "scope",
        "mailbox-kind",
        "login-method",
        "identity",
        "output",
        "name",
    ] {
        let fixture = Fixture::new();
        config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
        if corruption == "syntax" {
            fs::write(&fixture.config, b"{broken synthetic JSON").unwrap();
        } else {
            fixture.modify(|document| match corruption {
                "schema" => document["schema_version"] = json!(2),
                "unknown" => document["unrecognized"] = json!(true),
                "scope" => document["profiles"]["personal"]["scope"] = json!("unsupported"),
                "mailbox-kind" => {
                    document["profiles"]["personal"]["mailbox"]["kind"] = json!("unknown")
                }
                "login-method" => {
                    document["profiles"]["personal"]["login"] = json!({
                        "username": "synthetic-person@example.invalid",
                        "method": "unsupported",
                    });
                }
                "identity" => document["profiles"]["personal"]["mailbox"]["company_id"] = json!(-1),
                "output" => document["profiles"]["personal"]["output"] = json!("relative/path"),
                "name" => {
                    let profile = document["profiles"]["personal"].take();
                    document["profiles"]["../escape"] = profile;
                }
                _ => unreachable!(),
            });
        }
        let original = fs::read(&fixture.config).unwrap();
        assert!(
            config::profile_exists(&fixture.config, "personal").is_err(),
            "{corruption}"
        );
        assert!(
            config::load_profile(&fixture.config, "personal").is_err(),
            "{corruption}"
        );
        assert!(
            config::save_profile(&fixture.config, "other", &fixture.profile()).is_err(),
            "{corruption}"
        );
        assert_eq!(fs::read(&fixture.config).unwrap(), original);
    }
}

#[test]
fn unknown_credential_fields_are_rejected_at_every_config_level() {
    for level in ["document", "profile", "mailbox", "login"] {
        for field in [
            "password",
            "access_token",
            "refresh_token",
            "token",
            "api_key",
        ] {
            let fixture = Fixture::new();
            let mut profile = fixture.profile();
            profile.login = Some(Login {
                username: "synthetic-person@example.invalid".to_owned(),
                method: LoginMethod::Prompt,
            });
            config::save_profile(&fixture.config, "personal", &profile).unwrap();
            fixture.modify(|document| {
                let object = match level {
                    "document" => document,
                    "profile" => &mut document["profiles"]["personal"],
                    "mailbox" => &mut document["profiles"]["personal"]["mailbox"],
                    "login" => &mut document["profiles"]["personal"]["login"],
                    _ => unreachable!(),
                };
                object[field] = json!("fake-secret-never-log-this");
            });
            let original = fs::read(&fixture.config).unwrap();
            let error = config::load_profile(&fixture.config, "personal").unwrap_err();
            assert!(!error.to_string().contains("fake-secret"));
            assert!(!error.to_string().contains("invented-private-tenant"));
            assert!(!error.to_string().contains(fixture.base.to_str().unwrap()));
            assert!(config::save_profile(&fixture.config, "other", &fixture.profile()).is_err());
            assert_eq!(fs::read(&fixture.config).unwrap(), original);
        }
    }
}

#[test]
fn duplicate_profile_names_are_not_silently_discarded() {
    let fixture = Fixture::new();
    config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
    let profile = serde_json::to_string(&fixture.profile()).unwrap();
    let duplicates = format!(
        "{{\"schema_version\":1,\"profiles\":{{\"personal\":{profile},\"personal\":{profile}}}}}"
    );
    fs::write(&fixture.config, &duplicates).unwrap();
    assert!(config::load_profile(&fixture.config, "personal").is_err());
    assert!(config::save_profile(&fixture.config, "other", &fixture.profile()).is_err());
    assert_eq!(fs::read_to_string(&fixture.config).unwrap(), duplicates);
}

#[test]
fn oversized_config_is_rejected_before_parsing_or_overwriting() {
    let fixture = Fixture::new();
    config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&fixture.config)
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    assert!(config::profile_exists(&fixture.config, "personal").is_err());
    assert!(config::save_profile(&fixture.config, "other", &fixture.profile()).is_err());
    assert_eq!(
        fs::metadata(&fixture.config).unwrap().len(),
        1024 * 1024 + 1
    );
}

#[cfg(unix)]
fn symlink(source: &Path, target: &Path) {
    std::os::unix::fs::symlink(source, target).unwrap();
}

#[cfg(unix)]
#[test]
fn app_created_dirs_files_and_locks_are_private_without_chmodding_ancestors() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.base, fs::Permissions::from_mode(0o755)).unwrap();
    config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
    assert_eq!(
        fs::metadata(&fixture.base).unwrap().permissions().mode() & 0o777,
        0o755
    );
    for path in [
        fixture.base.join("config"),
        fixture.config.parent().unwrap().to_owned(),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [
        &fixture.config,
        &fixture.config.with_file_name(".config.json.lock"),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn rejects_symlink_config_lock_and_ancestors_without_touching_targets() {
    for target in ["config", "lock", "ancestor"] {
        let fixture = Fixture::new();
        config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
        let outside = fixture.base.join("outside");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"untouched synthetic sentinel").unwrap();
        let path = if target == "ancestor" {
            let link = fixture.base.join("link");
            symlink(&outside, &link);
            link.join("config.json")
        } else {
            let replace = if target == "config" {
                fixture.config.clone()
            } else {
                fixture.config.with_file_name(".config.json.lock")
            };
            fs::remove_file(&replace).unwrap();
            symlink(&sentinel, &replace);
            fixture.config.clone()
        };
        if target != "lock" {
            assert!(
                config::profile_exists(&path, "personal").is_err(),
                "{target}"
            );
            assert!(config::load_profile(&path, "personal").is_err(), "{target}");
        }
        assert!(
            config::save_profile(&path, "other", &fixture.profile()).is_err(),
            "{target}"
        );
        assert_eq!(
            fs::read(&sentinel).unwrap(),
            b"untouched synthetic sentinel"
        );
        assert_eq!(fs::read_dir(outside).unwrap().count(), 1);
    }
}

#[cfg(unix)]
#[test]
fn rejects_hard_linked_configs_and_locks_without_modifying_original() {
    for target in ["config", "lock"] {
        let fixture = Fixture::new();
        config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
        let original_path = if target == "config" {
            fixture.config.clone()
        } else {
            fixture.config.with_file_name(".config.json.lock")
        };
        let alias = fixture.base.join("alias");
        fs::hard_link(&original_path, &alias).unwrap();
        let original = fs::read(&alias).unwrap();
        if target == "config" {
            assert!(config::load_profile(&fixture.config, "personal").is_err());
            assert!(config::profile_exists(&fixture.config, "personal").is_err());
        }
        assert!(config::save_profile(&fixture.config, "other", &fixture.profile()).is_err());
        assert_eq!(fs::read(&alias).unwrap(), original);
    }
}

#[cfg(unix)]
#[test]
fn concurrent_writer_lock_prevents_lost_profiles_and_releases_after_drop() {
    use fs2::FileExt;
    let fixture = Fixture::new();
    config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.config.with_file_name(".config.json.lock"))
        .unwrap();
    FileExt::try_lock_exclusive(&lock).unwrap();
    assert!(config::save_profile(&fixture.config, "work", &fixture.profile()).is_err());
    assert!(config::profile_exists(&fixture.config, "personal").unwrap());
    assert!(!config::profile_exists(&fixture.config, "work").unwrap());
    drop(lock);
    config::save_profile(&fixture.config, "work", &fixture.profile()).unwrap();
    assert!(config::profile_exists(&fixture.config, "personal").unwrap());
    assert!(config::profile_exists(&fixture.config, "work").unwrap());
}

#[cfg(unix)]
#[test]
fn nonregular_config_and_broad_file_permissions_fail_closed() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    config::save_profile(&fixture.config, "personal", &fixture.profile()).unwrap();
    fs::set_permissions(&fixture.config, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(config::load_profile(&fixture.config, "personal").is_err());
    assert!(config::save_profile(&fixture.config, "other", &fixture.profile()).is_err());
    fs::remove_file(&fixture.config).unwrap();
    let name = std::ffi::CString::new(fixture.config.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a terminated synthetic temporary test path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(config::profile_exists(&fixture.config, "personal").is_err());
    assert!(config::save_profile(&fixture.config, "other", &fixture.profile()).is_err());
}
