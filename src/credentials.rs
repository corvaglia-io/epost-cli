//! Identity-bound helpers for optional credential storage.
//!
//! The ordinary profile stores no password. A Keychain entry uses an opaque
//! service derived from the physical config location, profile, API environment
//! and origin, explicit mailbox identity and account name. These helpers access
//! only the injected store; tests always inject a synthetic implementation.

use crate::{
    api::Environment,
    config::{self, LoginMethod, Profile},
    error::{Error, Result},
    keychain::CredentialStore,
    models::MailboxKind,
    secret::Secret,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const ACCOUNT: &str = "password";
const SERVICE_PREFIX: &str = "io.corvaglia.epost-cli.password.v1:";

/// Resolve an opaque identity without reading profile contents or the Keychain.
pub fn identity(
    path: &Path,
    name: &str,
    origin: &str,
    profile: &Profile,
    username: &str,
) -> Result<String> {
    validate_name(name)?;
    validate_username(username)?;
    let tenant = &profile.mailbox.tenant_id;
    if tenant.trim().is_empty()
        || tenant.len() > 4096
        || matches!(tenant.as_str(), "." | "..")
        || tenant.chars().any(char::is_control)
        || profile.mailbox.company_id < 0
    {
        return Err(Error::Config("credential mailbox identity is invalid"));
    }
    let origin = normalized_origin(origin)?;
    let path = physical_config_path(path)?;
    let company = profile.mailbox.company_id.to_be_bytes();
    let mut hash = Sha256::new();
    for field in [
        b"epost-cli-password-identity-v1".as_slice(),
        path.as_os_str().as_encoded_bytes(),
        name.as_bytes(),
        match profile.environment {
            Environment::Production => b"production".as_slice(),
            Environment::Sandbox => b"sandbox".as_slice(),
        },
        origin.as_bytes(),
        match profile.mailbox.kind {
            MailboxKind::Private => b"private".as_slice(),
            MailboxKind::Business => b"business".as_slice(),
        },
        tenant.as_bytes(),
        company.as_slice(),
        username.as_bytes(),
    ] {
        // Length prefixes prevent different field boundaries from colliding.
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field);
    }
    Ok(format!("{SERVICE_PREFIX}{}", hex::encode(hash.finalize())))
}

pub fn get(
    store: &dyn CredentialStore,
    path: &Path,
    name: &str,
    origin: &str,
    profile: &Profile,
    username: &str,
) -> Result<Secret> {
    store.get(&identity(path, name, origin, profile, username)?, ACCOUNT)
}

pub fn save(
    store: &dyn CredentialStore,
    path: &Path,
    name: &str,
    origin: &str,
    profile: &Profile,
    username: &str,
    password: &Secret,
) -> Result<()> {
    if password.expose().is_empty() || password.expose().len() > 8192 {
        return Err(Error::Config("password is empty or too long"));
    }
    store.set(
        &identity(path, name, origin, profile, username)?,
        ACCOUNT,
        password,
    )
}

pub fn forget(
    store: &dyn CredentialStore,
    path: &Path,
    name: &str,
    origin: &str,
    profile: &Profile,
    username: &str,
) -> Result<()> {
    store.delete(&identity(path, name, origin, profile, username)?, ACCOUNT)
}

/// Update a profile and its optional stored password under the profile writer
/// lock. Credential snapshots are taken only after the reviewed profile matches.
/// Any mutation failure or precommit config failure restores all touched entries.
/// A committed config is never rolled back merely because later verification
/// fails; the profile and credentials then retain the user's committed choice.
pub fn save_profile_login(
    store: &dyn CredentialStore,
    path: &Path,
    name: &str,
    new_profile: &Profile,
    expected: Option<&Profile>,
    password: Option<&Secret>,
) -> Result<()> {
    let old_service = expected
        .map(|profile| login_service(path, name, profile))
        .transpose()?
        .flatten();
    let new_service = login_service(path, name, new_profile)?;
    if old_service.is_none() && new_service.is_none() {
        return config::save_profile_if_unchanged(path, name, new_profile, expected);
    }
    config::save_profile_with_effect(
        path,
        name,
        new_profile,
        expected,
        || {
            let mut snapshot = Vec::new();
            for service in [old_service.as_ref(), new_service.as_ref()]
                .into_iter()
                .flatten()
            {
                if !snapshot.iter().any(|(saved, _)| saved == service) {
                    snapshot.push((service.clone(), store.get_optional(service, ACCOUNT)?));
                }
            }
            if let Some(service) = &new_service {
                if let Some(password) = password {
                    if password.expose().is_empty() || password.expose().len() > 8192 {
                        return Err(Error::Config("password is empty or too long"));
                    }
                } else if !snapshot
                    .iter()
                    .any(|(saved, value)| saved == service && value.is_some())
                {
                    return Err(Error::Config(
                        "Keychain password is missing; sign in and save it again",
                    ));
                }
            }
            let mutation = (|| {
                if let Some(service) = &new_service
                    && let Some(password) = password
                {
                    store.set(service, ACCOUNT, password)?;
                }
                if let Some(service) = &old_service
                    && Some(service) != new_service.as_ref()
                {
                    store.delete(service, ACCOUNT)?;
                }
                Ok(())
            })();
            if let Err(error) = mutation {
                restore_snapshot(store, &snapshot)?;
                return Err(error);
            }
            Ok(snapshot)
        },
        |snapshot| restore_snapshot(store, &snapshot),
    )
}

fn login_service(path: &Path, name: &str, profile: &Profile) -> Result<Option<String>> {
    profile
        .login
        .as_ref()
        .filter(|login| login.method == LoginMethod::MacosKeychain)
        .map(|login| {
            identity(
                path,
                name,
                profile.environment.base_url(),
                profile,
                &login.username,
            )
        })
        .transpose()
}

fn restore_snapshot(
    store: &dyn CredentialStore,
    snapshot: &[(String, Option<Secret>)],
) -> Result<()> {
    let mut failed = false;
    for (service, previous) in snapshot {
        let result = match previous {
            Some(password) => store.set(service, ACCOUNT, password),
            None => store.delete(service, ACCOUNT),
        };
        failed |= result.is_err();
    }
    if failed {
        return Err(Error::Config(
            "credential changes could not be rolled back; check Keychain Access and run setup again before using this profile",
        ));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::Config("credential profile name is invalid"));
    }
    Ok(())
}

fn validate_username(username: &str) -> Result<()> {
    if username.trim().is_empty() || username.len() > 4096 || username.chars().any(char::is_control)
    {
        return Err(Error::Config("profile login username is invalid"));
    }
    Ok(())
}

fn normalized_origin(origin: &str) -> Result<String> {
    if origin.len() > 4096 || origin.chars().any(char::is_control) {
        return Err(Error::Config("credential API origin is invalid"));
    }
    let url = reqwest::Url::parse(origin)
        .map_err(|_| Error::Config("credential API origin is invalid"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Config(
            "credential API origin must be a clean HTTP origin",
        ));
    }
    Ok(url.to_string())
}

/// Reject aliases that could redirect an approved credential to another config.
/// Missing components are appended to the canonical existing prefix; creating
/// those directories or the config file later does not change the binding.
fn physical_config_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path.components().any(|part| part == Component::ParentDir)
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
    {
        return Err(Error::Config(
            "credential config path must be absolute without parent traversal or control characters",
        ));
    }
    let parts: Vec<_> = path.components().collect();
    let mut resolved = PathBuf::new();
    let mut missing = false;
    for (index, part) in parts.iter().enumerate() {
        resolved.push(part.as_os_str());
        if missing {
            continue;
        }
        match fs::symlink_metadata(&resolved) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || (index + 1 < parts.len() && !metadata.is_dir())
                    || (index + 1 == parts.len() && !metadata.is_file())
                {
                    return Err(Error::Config(
                        "credential config path contains a symlink or unsafe entry",
                    ));
                }
                #[cfg(unix)]
                if metadata.is_file() {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() != 1 {
                        return Err(Error::Config(
                            "credential config file must have no hard links",
                        ));
                    }
                }
                resolved = resolved
                    .canonicalize()
                    .map_err(|_| Error::Config("credential config path could not be resolved"))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(_) => {
                return Err(Error::Config(
                    "credential config path could not be inspected",
                ));
            }
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Login, SyncScope},
        models::Mailbox,
    };
    use std::{
        cell::{Cell, RefCell},
        collections::HashMap,
    };

    const USERNAME: &str = "synthetic-user@example.invalid";
    const ORIGIN: &str = "http://127.0.0.1:18375/";

    struct Fixture {
        _temp: tempfile::TempDir,
        base: PathBuf,
        path: PathBuf,
        profile: Profile,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let path = base.join("settings/new/config.json");
            let profile = Profile {
                mailbox: Mailbox {
                    tenant_id: "invented-private-tenant".to_owned(),
                    company_id: 17,
                    kind: MailboxKind::Private,
                },
                environment: Environment::Production,
                output: base.join("documents"),
                scope: SyncScope::Inbox,
                login: None,
            };
            Self {
                _temp: temp,
                base,
                path,
                profile,
            }
        }

        fn service(&self) -> String {
            identity(&self.path, "personal", ORIGIN, &self.profile, USERNAME).unwrap()
        }
    }

    #[derive(Default)]
    struct FakeStore {
        entries: RefCell<HashMap<(String, String), Secret>>,
        calls: Cell<usize>,
        deny_get: Cell<bool>,
        fail_next_delete: Cell<bool>,
        fail_set_for: RefCell<Option<String>>,
        after_set: RefCell<Option<Box<dyn FnOnce()>>>,
    }

    impl CredentialStore for FakeStore {
        fn get(&self, service: &str, account: &str) -> Result<Secret> {
            self.calls.set(self.calls.get() + 1);
            if self.deny_get.get() {
                return Err(Error::Config("synthetic Keychain access denied"));
            }
            self.entries
                .borrow()
                .get(&(service.to_owned(), account.to_owned()))
                .cloned()
                .ok_or(Error::Config(crate::keychain::MISSING_ENTRY_MESSAGE))
        }
        fn set(&self, service: &str, account: &str, password: &Secret) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.fail_set_for.borrow().as_deref() == Some(service) {
                return Err(Error::Config("synthetic Keychain write failed"));
            }
            self.entries
                .borrow_mut()
                .insert((service.to_owned(), account.to_owned()), password.clone());
            let hook = self.after_set.borrow_mut().take();
            if let Some(hook) = hook {
                hook();
            }
            Ok(())
        }
        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.fail_next_delete.replace(false) {
                return Err(Error::Config("synthetic Keychain delete failed"));
            }
            self.entries
                .borrow_mut()
                .remove(&(service.to_owned(), account.to_owned()));
            Ok(())
        }
    }

    #[test]
    fn identity_is_opaque_and_isolates_every_login_component() {
        let fixture = Fixture::new();
        let service = fixture.service();
        assert_eq!(service.len(), SERVICE_PREFIX.len() + 64);
        assert!(service.starts_with(SERVICE_PREFIX));
        assert!(!service.contains(USERNAME));
        assert!(!service.contains(&fixture.profile.mailbox.tenant_id));
        assert!(!service.contains("personal"));
        assert!(!service.contains(ORIGIN));
        let changes = [
            identity(
                &fixture.base.join("other/config.json"),
                "personal",
                ORIGIN,
                &fixture.profile,
                USERNAME,
            )
            .unwrap(),
            identity(&fixture.path, "work", ORIGIN, &fixture.profile, USERNAME).unwrap(),
            identity(
                &fixture.path,
                "personal",
                "http://127.0.0.1:18376/",
                &fixture.profile,
                USERNAME,
            )
            .unwrap(),
            identity(
                &fixture.path,
                "personal",
                ORIGIN,
                &fixture.profile,
                "other-user@example.invalid",
            )
            .unwrap(),
        ];
        for changed in changes {
            assert_ne!(changed, service);
        }
        let mut changed = fixture.profile.clone();
        changed.environment = Environment::Sandbox;
        assert_ne!(
            identity(&fixture.path, "personal", ORIGIN, &changed, USERNAME).unwrap(),
            service
        );
        changed = fixture.profile.clone();
        changed.mailbox.kind = MailboxKind::Business;
        assert_ne!(
            identity(&fixture.path, "personal", ORIGIN, &changed, USERNAME).unwrap(),
            service
        );
        changed = fixture.profile.clone();
        changed.mailbox.tenant_id = "invented-other-tenant".to_owned();
        assert_ne!(
            identity(&fixture.path, "personal", ORIGIN, &changed, USERNAME).unwrap(),
            service
        );
        changed = fixture.profile.clone();
        changed.mailbox.company_id += 1;
        assert_ne!(
            identity(&fixture.path, "personal", ORIGIN, &changed, USERNAME).unwrap(),
            service
        );
    }

    #[test]
    fn missing_config_creation_and_nonlogin_settings_keep_the_same_binding() {
        let fixture = Fixture::new();
        let before = fixture.service();
        assert!(!fixture.path.exists());
        fs::create_dir_all(fixture.path.parent().unwrap()).unwrap();
        fs::write(&fixture.path, b"synthetic config; contents are never read").unwrap();
        assert_eq!(fixture.service(), before);
        let mut changed = fixture.profile.clone();
        changed.output = fixture.base.join("other-documents");
        changed.scope = SyncScope::Both;
        assert_eq!(
            identity(&fixture.path, "personal", ORIGIN, &changed, USERNAME).unwrap(),
            before
        );
    }

    #[test]
    fn helpers_use_opaque_service_and_constant_account_with_fake_store_only() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        let secret = Secret::new("synthetic-password".to_owned());
        save(
            &store,
            &fixture.path,
            "personal",
            ORIGIN,
            &fixture.profile,
            USERNAME,
            &secret,
        )
        .unwrap();
        assert_eq!(
            store.entries.borrow().keys().next().unwrap(),
            &(fixture.service(), ACCOUNT.to_owned())
        );
        assert_eq!(
            get(
                &store,
                &fixture.path,
                "personal",
                ORIGIN,
                &fixture.profile,
                USERNAME
            )
            .unwrap()
            .expose(),
            secret.expose()
        );
        assert!(
            get(
                &store,
                &fixture.path,
                "work",
                ORIGIN,
                &fixture.profile,
                USERNAME
            )
            .is_err()
        );
        forget(
            &store,
            &fixture.path,
            "personal",
            ORIGIN,
            &fixture.profile,
            USERNAME,
        )
        .unwrap();
        forget(
            &store,
            &fixture.path,
            "personal",
            ORIGIN,
            &fixture.profile,
            USERNAME,
        )
        .unwrap();
        assert!(
            get(
                &store,
                &fixture.path,
                "personal",
                ORIGIN,
                &fixture.profile,
                USERNAME
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_values_fail_before_any_store_access() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        for username in ["", "  ", "fake\nuser", "fake\0user"] {
            assert!(
                get(
                    &store,
                    &fixture.path,
                    "personal",
                    ORIGIN,
                    &fixture.profile,
                    username
                )
                .is_err()
            );
            assert!(
                forget(
                    &store,
                    &fixture.path,
                    "personal",
                    ORIGIN,
                    &fixture.profile,
                    username
                )
                .is_err()
            );
        }
        assert!(
            identity(
                &fixture.path,
                "bad/name",
                ORIGIN,
                &fixture.profile,
                USERNAME
            )
            .is_err()
        );
        for origin in [
            "file:///tmp/",
            "http://user:fake@127.0.0.1/",
            "http://127.0.0.1/api",
            "http://127.0.0.1/?query=fake",
            "http://127.0.0.1/#fake",
        ] {
            assert!(
                identity(
                    &fixture.path,
                    "personal",
                    origin,
                    &fixture.profile,
                    USERNAME
                )
                .is_err()
            );
        }
        assert!(
            save(
                &store,
                &fixture.path,
                "personal",
                ORIGIN,
                &fixture.profile,
                USERNAME,
                &Secret::new(String::new())
            )
            .is_err()
        );
        assert_eq!(store.calls.get(), 0);
    }

    #[test]
    fn config_paths_reject_relative_parent_traversal_and_unsafe_entries() {
        let fixture = Fixture::new();
        for path in [
            PathBuf::from("config.json"),
            fixture.base.join("../config.json"),
            fixture.base.join("bad\nname.json"),
            fixture.base.clone(),
        ] {
            assert!(identity(&path, "personal", ORIGIN, &fixture.profile, USERNAME).is_err());
        }
        let parent_file = fixture.base.join("parent-file");
        fs::write(&parent_file, b"synthetic").unwrap();
        assert!(
            identity(
                &parent_file.join("config.json"),
                "personal",
                ORIGIN,
                &fixture.profile,
                USERNAME
            )
            .is_err()
        );
        let directory_config = fixture.base.join("directory-config.json");
        fs::create_dir(&directory_config).unwrap();
        assert!(
            identity(
                &directory_config,
                "personal",
                ORIGIN,
                &fixture.profile,
                USERNAME
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_ancestors_final_links_and_hardlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let actual_parent = fixture.base.join("actual");
        fs::create_dir(&actual_parent).unwrap();
        let alias_parent = fixture.base.join("alias");
        symlink(&actual_parent, &alias_parent).unwrap();
        assert!(
            identity(
                &alias_parent.join("config.json"),
                "personal",
                ORIGIN,
                &fixture.profile,
                USERNAME
            )
            .is_err()
        );
        let actual_file = actual_parent.join("config.json");
        fs::write(&actual_file, b"synthetic").unwrap();
        let final_alias = fixture.base.join("config-alias.json");
        symlink(&actual_file, &final_alias).unwrap();
        assert!(identity(&final_alias, "personal", ORIGIN, &fixture.profile, USERNAME).is_err());
        let hard_link = fixture.base.join("config-hard-link.json");
        fs::hard_link(&actual_file, &hard_link).unwrap();
        assert!(identity(&actual_file, "personal", ORIGIN, &fixture.profile, USERNAME).is_err());
        assert!(identity(&hard_link, "personal", ORIGIN, &fixture.profile, USERNAME).is_err());
    }

    #[test]
    fn equivalent_clean_origin_spelling_has_one_identity() {
        let fixture = Fixture::new();
        assert_eq!(
            identity(
                &fixture.path,
                "personal",
                "https://EXAMPLE.INVALID:443",
                &fixture.profile,
                USERNAME
            )
            .unwrap(),
            identity(
                &fixture.path,
                "personal",
                "https://example.invalid/",
                &fixture.profile,
                USERNAME
            )
            .unwrap(),
        );
    }

    fn keychain_profile(fixture: &Fixture) -> Profile {
        let mut profile = fixture.profile.clone();
        profile.login = Some(Login {
            username: USERNAME.to_owned(),
            method: LoginMethod::MacosKeychain,
        });
        profile
    }

    fn profile_service(fixture: &Fixture, profile: &Profile) -> String {
        login_service(&fixture.path, "personal", profile)
            .unwrap()
            .unwrap()
    }

    fn seed_profile(fixture: &Fixture, store: &FakeStore, profile: &Profile) {
        config::save_profile(&fixture.path, "personal", profile).unwrap();
        store
            .set(
                &profile_service(fixture, profile),
                ACCOUNT,
                &Secret::new("synthetic-old-password".to_owned()),
            )
            .unwrap();
        store.calls.set(0);
    }

    #[test]
    fn failed_profile_compare_and_swap_does_not_access_credentials() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        let expected = keychain_profile(&fixture);
        seed_profile(&fixture, &store, &expected);
        let mut concurrent = expected.clone();
        concurrent.scope = SyncScope::Both;
        config::save_profile(&fixture.path, "personal", &concurrent).unwrap();
        let password = Secret::new("synthetic-replacement-password".to_owned());
        assert!(
            save_profile_login(
                &store,
                &fixture.path,
                "personal",
                &expected,
                Some(&expected),
                Some(&password),
            )
            .is_err()
        );
        assert_eq!(store.calls.get(), 0);
        assert_eq!(
            config::load_profile(&fixture.path, "personal").unwrap(),
            concurrent
        );
    }

    #[cfg(unix)]
    #[test]
    fn config_persist_failure_restores_all_touched_credentials() {
        for changed_identity in [false, true] {
            let fixture = Fixture::new();
            let store = FakeStore::default();
            let expected = keychain_profile(&fixture);
            seed_profile(&fixture, &store, &expected);
            let parent = fixture.path.parent().unwrap().to_owned();
            // The config helper has prepared and fsynced its temporary file before
            // this fake-store hook. Unlink only that synthetic file to fail rename.
            *store.after_set.borrow_mut() = Some(Box::new(move || {
                let mut removed = 0;
                for entry in fs::read_dir(&parent).unwrap() {
                    let entry = entry.unwrap();
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".epost-config-")
                    {
                        fs::remove_file(entry.path()).unwrap();
                        removed += 1;
                    }
                }
                assert_eq!(removed, 1);
            }));
            let mut replacement = expected.clone();
            replacement.scope = SyncScope::Both;
            if changed_identity {
                replacement.mailbox.company_id += 1;
            }
            let password = Secret::new("synthetic-replacement-password".to_owned());
            assert!(
                save_profile_login(
                    &store,
                    &fixture.path,
                    "personal",
                    &replacement,
                    Some(&expected),
                    Some(&password),
                )
                .is_err()
            );
            assert_eq!(
                config::load_profile(&fixture.path, "personal").unwrap(),
                expected
            );
            assert_eq!(
                store
                    .get(&profile_service(&fixture, &expected), ACCOUNT)
                    .unwrap()
                    .expose(),
                "synthetic-old-password"
            );
            if changed_identity {
                assert!(
                    store
                        .get_optional(&profile_service(&fixture, &replacement), ACCOUNT)
                        .unwrap()
                        .is_none()
                );
            }
        }
    }

    #[test]
    fn changed_identity_commit_replaces_old_credential_and_profile() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        let expected = keychain_profile(&fixture);
        seed_profile(&fixture, &store, &expected);
        let mut replacement = expected.clone();
        replacement.mailbox.tenant_id = "invented-replacement-tenant".to_owned();
        replacement.login.as_mut().unwrap().username = "replacement@example.invalid".to_owned();
        let password = Secret::new("synthetic-replacement-password".to_owned());
        save_profile_login(
            &store,
            &fixture.path,
            "personal",
            &replacement,
            Some(&expected),
            Some(&password),
        )
        .unwrap();
        assert_eq!(
            config::load_profile(&fixture.path, "personal").unwrap(),
            replacement
        );
        assert!(
            store
                .get_optional(&profile_service(&fixture, &expected), ACCOUNT)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .get(&profile_service(&fixture, &replacement), ACCOUNT)
                .unwrap()
                .expose(),
            password.expose()
        );
    }

    #[test]
    fn failed_delete_restores_all_touched_entries_and_keeps_old_profile() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        let expected = keychain_profile(&fixture);
        seed_profile(&fixture, &store, &expected);
        let mut replacement = expected.clone();
        replacement.mailbox.company_id += 1;
        store.fail_next_delete.set(true);
        let password = Secret::new("synthetic-replacement-password".to_owned());
        assert!(
            save_profile_login(
                &store,
                &fixture.path,
                "personal",
                &replacement,
                Some(&expected),
                Some(&password),
            )
            .is_err()
        );
        assert_eq!(
            config::load_profile(&fixture.path, "personal").unwrap(),
            expected
        );
        assert_eq!(
            store
                .get(&profile_service(&fixture, &expected), ACCOUNT)
                .unwrap()
                .expose(),
            "synthetic-old-password"
        );
        assert!(
            store
                .get_optional(&profile_service(&fixture, &replacement), ACCOUNT)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn forgetting_keychain_changes_profile_to_prompt_and_retains_username() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        let expected = keychain_profile(&fixture);
        seed_profile(&fixture, &store, &expected);
        let mut replacement = expected.clone();
        replacement.login.as_mut().unwrap().method = LoginMethod::Prompt;
        save_profile_login(
            &store,
            &fixture.path,
            "personal",
            &replacement,
            Some(&expected),
            None,
        )
        .unwrap();
        let saved = config::load_profile(&fixture.path, "personal").unwrap();
        assert_eq!(
            saved.login.unwrap(),
            Login {
                username: USERNAME.to_owned(),
                method: LoginMethod::Prompt
            }
        );
        assert!(
            store
                .get_optional(&profile_service(&fixture, &expected), ACCOUNT)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn prompt_only_profile_saves_without_store_access() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        store.deny_get.set(true);
        let mut profile = fixture.profile.clone();
        profile.login = Some(Login {
            username: USERNAME.to_owned(),
            method: LoginMethod::Prompt,
        });
        save_profile_login(&store, &fixture.path, "personal", &profile, None, None).unwrap();
        assert_eq!(store.calls.get(), 0);
        assert_eq!(
            config::load_profile(&fixture.path, "personal").unwrap(),
            profile
        );
    }

    #[test]
    fn snapshot_denial_is_not_treated_as_missing_and_never_mutates_credentials() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        let expected = keychain_profile(&fixture);
        seed_profile(&fixture, &store, &expected);
        store.deny_get.set(true);
        let password = Secret::new("synthetic-replacement-password".to_owned());
        assert!(
            save_profile_login(
                &store,
                &fixture.path,
                "personal",
                &expected,
                Some(&expected),
                Some(&password),
            )
            .is_err()
        );
        assert_eq!(store.calls.get(), 1);
        assert_eq!(
            config::load_profile(&fixture.path, "personal").unwrap(),
            expected
        );
        let entries = store.entries.borrow();
        assert_eq!(
            entries.values().next().unwrap().expose(),
            "synthetic-old-password"
        );
    }

    #[test]
    fn partial_mutation_rollback_failure_is_actionable_and_redacted() {
        let fixture = Fixture::new();
        let store = FakeStore::default();
        let expected = keychain_profile(&fixture);
        seed_profile(&fixture, &store, &expected);
        let mut replacement = expected.clone();
        replacement.mailbox.company_id += 1;
        store.fail_next_delete.set(true);
        *store.fail_set_for.borrow_mut() = Some(profile_service(&fixture, &expected));
        let password = Secret::new("synthetic-replacement-password".to_owned());
        let error = save_profile_login(
            &store,
            &fixture.path,
            "personal",
            &replacement,
            Some(&expected),
            Some(&password),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("rolled back"));
        assert!(error.contains("run setup again"));
        assert!(!error.contains(USERNAME));
        assert!(!error.contains(password.expose()));
        assert_eq!(
            config::load_profile(&fixture.path, "personal").unwrap(),
            expected
        );
    }
}
