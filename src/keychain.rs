//! Optional user-approved credential storage through the native macOS Keychain.
//!
//! Callers supply an identity-bound service and account and decide when access is
//! permitted. Constructing the store or asking whether it is supported performs
//! no Keychain operation. Tests exercise helpers and fake stores only.

use crate::{
    error::{Error, Result},
    secret::Secret,
};

/// A recognized absent-entry result, distinct from denial or unavailable access.
pub const MISSING_ENTRY_MESSAGE: &str = "saved Keychain sign-in was not found; run login --remember for this profile or supply runtime credentials";

pub fn is_missing(error: &Error) -> bool {
    matches!(error, Error::Config(message) if *message == MISSING_ENTRY_MESSAGE)
}

/// A small boundary for injecting synthetic credential stores in tests.
pub trait CredentialStore {
    fn get(&self, service: &str, account: &str) -> Result<Secret>;
    /// Read without displaying OS authentication UI. Synthetic stores may use
    /// their ordinary read; native backends must suppress interaction explicitly.
    fn get_noninteractive(&self, service: &str, account: &str) -> Result<Secret> {
        self.get(service, account)
    }
    fn get_optional(&self, service: &str, account: &str) -> Result<Option<Secret>> {
        match self.get(service, account) {
            Ok(secret) => Ok(Some(secret)),
            Err(error) if is_missing(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn set(&self, service: &str, account: &str, password: &Secret) -> Result<()>;
    /// Removing a missing entry succeeds, so forgetting credentials is repeatable.
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

/// Native OS backend. No subprocess or credential command-line argument is used.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeStore;

pub const fn supported() -> bool {
    cfg!(target_os = "macos")
}

// The crate's macOS interaction guard changes process-wide Keychain UI state.
// Serialize our native calls so a guarded read cannot overlap another native
// operation. No asynchronous suspension occurs while this lock is held.
#[cfg(target_os = "macos")]
static NATIVE_ACCESS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
fn native_access() -> Result<std::sync::MutexGuard<'static, ()>> {
    NATIVE_ACCESS.lock().map_err(|_| {
        Error::Config("Keychain access is unavailable; retry or supply runtime credentials")
    })
}

#[cfg(target_os = "macos")]
impl CredentialStore for NativeStore {
    fn get(&self, service: &str, account: &str) -> Result<Secret> {
        let options = native_options(service, account)?;
        let _access = native_access()?;
        let bytes = security_framework::passwords::generic_password(options)
            .map_err(|error| status_error(error.code()))?;
        secret_from_bytes(bytes)
    }

    fn get_noninteractive(&self, service: &str, account: &str) -> Result<Secret> {
        use security_framework::os::macos::keychain::SecKeychain;

        let options = native_options(service, account)?;
        let _access = native_access()?;
        // PasswordOptions in security-framework 3.7 has no authentication-UI
        // setter. Use its supported macOS guard around this synchronous read.
        // The guard reenables interaction on drop, so preserve an already
        // disabled state by creating it only when interaction was allowed.
        let interaction_allowed = SecKeychain::user_interaction_allowed()
            .map_err(|_| Error::Config("Keychain authentication UI could not be disabled; supply runtime credentials for this non-interactive run"))?;
        let _interaction = if interaction_allowed {
            Some(SecKeychain::disable_user_interaction()
                .map_err(|_| Error::Config("Keychain authentication UI could not be disabled; supply runtime credentials for this non-interactive run"))?)
        } else {
            None
        };
        let bytes = security_framework::passwords::generic_password(options)
            .map_err(|error| noninteractive_status_error(error.code()))?;
        secret_from_bytes(bytes)
    }

    fn set(&self, service: &str, account: &str, password: &Secret) -> Result<()> {
        let options = native_options(service, account)?;
        validate_password(password.expose())?;
        let _access = native_access()?;
        security_framework::passwords::set_generic_password_options(
            password.expose().as_bytes(),
            options,
        )
        .map_err(|error| status_error(error.code()))
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let options = native_options(service, account)?;
        let _access = native_access()?;
        match security_framework::passwords::delete_generic_password_options(options) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ITEM_NOT_FOUND => Ok(()),
            Err(error) => Err(status_error(error.code())),
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl CredentialStore for NativeStore {
    fn get(&self, _service: &str, _account: &str) -> Result<Secret> {
        Err(unsupported())
    }

    fn set(&self, _service: &str, _account: &str, _password: &Secret) -> Result<()> {
        Err(unsupported())
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported() -> Error {
    Error::Config(
        "Keychain credentials are available only on macOS; use prompts or your scheduler's secret mechanism",
    )
}

#[cfg(target_os = "macos")]
fn native_options(
    service: &str,
    account: &str,
) -> Result<security_framework::passwords::PasswordOptions> {
    validate_identity(service, account)?;
    let mut options =
        security_framework::passwords::PasswordOptions::new_generic_password(service, account);
    // Keep this optional storage local to this Mac, including get and delete.
    options.set_access_synchronized(Some(false));
    Ok(options)
}

#[cfg(any(target_os = "macos", test))]
fn validate_identity(service: &str, account: &str) -> Result<()> {
    if [service, account]
        .iter()
        .any(|value| value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control))
    {
        return Err(Error::Config("Keychain credential identity is invalid"));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn validate_password(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 8192 {
        return Err(Error::Config(
            "saved Keychain password is empty or too long; remove the invalid epost-cli entry in Keychain Access before running login --remember, or supply runtime credentials",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn secret_from_bytes(bytes: Vec<u8>) -> Result<Secret> {
    // Ensure the native return buffer is cleared on every path, including a
    // malformed or oversized stored value. Secret zeroizes its owned string.
    let bytes = zeroize::Zeroizing::new(bytes);
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| Error::Config("saved Keychain password is not valid text; remove the invalid epost-cli entry in Keychain Access before running login --remember, or supply runtime credentials"))?;
    validate_password(value)?;
    Ok(Secret::new(value.to_owned()))
}

// OSStatus values from Apple's Security.framework SecBase.h. Never format the
// native error, because diagnostics must not contain account identifiers.
#[cfg(any(target_os = "macos", test))]
const ITEM_NOT_FOUND: i32 = -25300;

#[cfg(any(target_os = "macos", test))]
fn status_error(code: i32) -> Error {
    let message = match code {
        ITEM_NOT_FOUND => MISSING_ENTRY_MESSAGE,
        -25308 => {
            "Keychain access requires an unlocked login session; unlock it and retry or supply runtime credentials"
        }
        -25293 | -34018 => {
            "Keychain access was denied; allow this app in Keychain Access or supply runtime credentials"
        }
        -128 => "Keychain access was cancelled; retry or supply runtime credentials",
        -25291 | -25294 | -25295 => {
            "login Keychain is unavailable; check Keychain Access or supply runtime credentials"
        }
        -26275 => {
            "saved Keychain entry is invalid; remove the invalid epost-cli entry in Keychain Access before running login --remember, or supply runtime credentials"
        }
        _ => "Keychain operation failed; check Keychain Access or supply runtime credentials",
    };
    Error::Config(message)
}

#[cfg(any(target_os = "macos", test))]
fn noninteractive_status_error(code: i32) -> Error {
    match code {
        -25308 | -25293 | -34018 | -128 => Error::Config(
            "saved Keychain sign-in requires interactive authorization; unlock the login Keychain and authorize epost-cli in an interactive session, or supply runtime credentials for this non-interactive run",
        ),
        _ => status_error(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap};

    #[derive(Default)]
    struct FakeStore(RefCell<HashMap<(String, String), Secret>>);

    impl CredentialStore for FakeStore {
        fn get(&self, service: &str, account: &str) -> Result<Secret> {
            self.0
                .borrow()
                .get(&(service.to_owned(), account.to_owned()))
                .cloned()
                .ok_or_else(|| status_error(ITEM_NOT_FOUND))
        }

        fn set(&self, service: &str, account: &str, password: &Secret) -> Result<()> {
            self.0
                .borrow_mut()
                .insert((service.to_owned(), account.to_owned()), password.clone());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.0
                .borrow_mut()
                .remove(&(service.to_owned(), account.to_owned()));
            Ok(())
        }
    }

    #[test]
    fn fake_store_keeps_services_and_accounts_separate_and_forgets_repeatably() {
        let store: &dyn CredentialStore = &FakeStore::default();
        let password = Secret::new("synthetic-password".to_owned());
        store
            .set("synthetic-service-a", "user@example.invalid", &password)
            .unwrap();
        assert_eq!(
            store
                .get("synthetic-service-a", "user@example.invalid")
                .unwrap()
                .expose(),
            password.expose()
        );
        assert_eq!(
            store
                .get_noninteractive("synthetic-service-a", "user@example.invalid")
                .unwrap()
                .expose(),
            password.expose()
        );
        assert!(
            store
                .get("synthetic-service-b", "user@example.invalid")
                .is_err()
        );
        assert!(
            store
                .get_optional("synthetic-service-b", "user@example.invalid")
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get("synthetic-service-a", "other@example.invalid")
                .is_err()
        );
        store
            .delete("synthetic-service-a", "user@example.invalid")
            .unwrap();
        store
            .delete("synthetic-service-a", "user@example.invalid")
            .unwrap();
        assert!(
            store
                .get("synthetic-service-a", "user@example.invalid")
                .is_err()
        );
    }

    #[test]
    fn returned_secret_is_bounded_valid_text_and_redacted() {
        let password = secret_from_bytes(b"synthetic-secret".to_vec()).unwrap();
        assert_eq!(password.expose(), "synthetic-secret");
        assert_eq!(format!("{password:?}"), "Secret([REDACTED])");
        assert!(secret_from_bytes(vec![]).is_err());
        assert!(secret_from_bytes(vec![b'x'; 8193]).is_err());
        assert!(secret_from_bytes(vec![0xff]).is_err());
        assert!(secret_from_bytes(vec![b'x'; 8192]).is_ok());
    }

    #[test]
    fn native_error_codes_have_static_actionable_redacted_messages() {
        for (code, expected) in [
            (ITEM_NOT_FOUND, "not found"),
            (-25308, "unlocked"),
            (-25293, "denied"),
            (-34018, "denied"),
            (-128, "cancelled"),
            (-25291, "unavailable"),
            (-25294, "unavailable"),
            (-25295, "unavailable"),
            (-26275, "invalid"),
            (12345, "failed"),
        ] {
            let error = status_error(code);
            assert!(error.to_string().contains(expected));
            assert!(!error.to_string().contains(&code.to_string()));
            assert_eq!(error.exit_code(), 1);
            assert_eq!(is_missing(&error), code == ITEM_NOT_FOUND);
        }
    }

    #[test]
    fn noninteractive_status_errors_never_suggest_prompting_during_the_run() {
        for code in [-25308, -25293, -34018, -128] {
            let error = noninteractive_status_error(code);
            assert!(
                error
                    .to_string()
                    .contains("requires interactive authorization")
            );
            assert!(error.to_string().contains("supply runtime credentials"));
            assert!(!error.to_string().contains(&code.to_string()));
            assert!(!is_missing(&error));
        }
        assert!(is_missing(&noninteractive_status_error(ITEM_NOT_FOUND)));
        assert!(
            noninteractive_status_error(-25291)
                .to_string()
                .contains("unavailable")
        );
    }

    #[test]
    fn identities_reject_unbounded_or_control_character_values() {
        assert!(validate_identity("epost-cli:synthetic-hash", "user@example.invalid").is_ok());
        for invalid in ["", "bad\nname", "bad\0name"] {
            assert!(validate_identity(invalid, "synthetic-account").is_err());
            assert!(validate_identity("synthetic-service", invalid).is_err());
        }
        assert!(validate_identity(&"s".repeat(1025), "synthetic-account").is_err());
        assert!(validate_identity("synthetic-service", &"a".repeat(1025)).is_err());
    }
}
