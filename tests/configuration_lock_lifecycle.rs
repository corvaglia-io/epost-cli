//! Deterministic Unix descriptor-inheritance tests with synthetic temp profiles.
//! The forked child performs only async-signal-safe libc calls and reads no data
//! except one release byte on the test's private pipe. No credentials or network.

#![cfg(unix)]

use epost_cli::{
    api::Environment,
    config::{self, Profile, SyncScope},
    error::{Error, Result},
    models::{Mailbox, MailboxKind},
    storage::Store,
};
use std::{cell::RefCell, fs::File, io::Write, os::fd::FromRawFd, path::PathBuf};

struct Fixture {
    _temp: tempfile::TempDir,
    config: PathBuf,
    profile: Profile,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        Self {
            config: base.join("config.json"),
            profile: Profile {
                mailbox: Mailbox {
                    tenant_id: "synthetic-lock-lifecycle-tenant".into(),
                    company_id: 42,
                    kind: MailboxKind::Private,
                },
                environment: Environment::Production,
                output: base.join("documents"),
                scope: SyncScope::Inbox,
                login: None,
            },
            _temp: temp,
        }
    }
}

struct InheritedChild {
    pid: libc::pid_t,
    release: File,
}

impl InheritedChild {
    fn hold_current_descriptors() -> Result<Self> {
        let mut pipe = [-1; 2];
        // SAFETY: pipe points to two initialized descriptor slots. The parent
        // closes the read end and owns the write end, or closes both on failure.
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        // SAFETY: the child invokes only close, read and _exit before ending.
        // It never touches allocator, Rust locks, config, credentials or mail.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: these are this test's pipe ends, inherited from the parent.
            // _exit avoids running inherited Rust destructors after fork.
            unsafe {
                libc::close(pipe[1]);
                let mut byte = 0_u8;
                let count = libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                libc::_exit(if count == 1 { 0 } else { 1 });
            }
        }
        if pid < 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: neither descriptor has been transferred into a File.
            unsafe {
                libc::close(pipe[0]);
                libc::close(pipe[1]);
            }
            return Err(Error::Io(error));
        }
        // SAFETY: only the parent closes its read end; the write end is
        // transferred exactly once into an owning File.
        unsafe {
            libc::close(pipe[0]);
        }
        Ok(Self {
            pid,
            release: unsafe { File::from_raw_fd(pipe[1]) },
        })
    }

    fn is_running(&self) -> bool {
        let mut status = 0;
        // SAFETY: pid is the direct child created above; WNOHANG never blocks.
        unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) == 0 }
    }
}

impl Drop for InheritedChild {
    fn drop(&mut self) {
        // One byte releases the child even if another concurrent fork inherited
        // a copy of this pipe's write end. No sleep or child timeout is needed.
        let _ = self.release.write_all(&[1]);
        let mut status = 0;
        loop {
            // SAFETY: wait only for our direct child, which exits after the byte.
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
}

#[test]
fn completed_save_unlocks_while_a_forked_child_still_holds_inherited_descriptors() {
    let fixture = Fixture::new();
    let child = RefCell::new(None::<InheritedChild>);
    config::save_profile_with_effect(
        &fixture.config,
        "primary",
        &fixture.profile,
        None,
        || {
            *child.borrow_mut() = Some(InheritedChild::hold_current_descriptors()?);
            // Genuine overlapping writes must still fail while the save owns
            // its lock, even when an unrelated child inherited its descriptor.
            assert!(config::save_profile(&fixture.config, "overlap", &fixture.profile).is_err());
            Ok(())
        },
        |_| Ok(()),
    )
    .unwrap();
    assert!(child.borrow().as_ref().unwrap().is_running());
    config::save_profile(&fixture.config, "secondary", &fixture.profile).unwrap();
    config::set_default_profile(&fixture.config, "secondary").unwrap();
    assert!(child.borrow().as_ref().unwrap().is_running());
    assert_eq!(config::list_profiles(&fixture.config).unwrap().len(), 2);
    assert_eq!(
        config::default_profile(&fixture.config).unwrap().as_deref(),
        Some("secondary")
    );
    assert!(!config::profile_exists(&fixture.config, "overlap").unwrap());
}

#[test]
fn failed_save_unlocks_while_a_forked_child_still_holds_inherited_descriptors() {
    let fixture = Fixture::new();
    config::save_profile(&fixture.config, "primary", &fixture.profile).unwrap();
    let original = std::fs::read(&fixture.config).unwrap();
    let child = RefCell::new(None::<InheritedChild>);
    let mut replacement = fixture.profile.clone();
    replacement.scope = SyncScope::Archive;
    let result = config::save_profile_with_effect(
        &fixture.config,
        "primary",
        &replacement,
        Some(&fixture.profile),
        || {
            *child.borrow_mut() = Some(InheritedChild::hold_current_descriptors()?);
            Err::<(), Error>(Error::Config("synthetic effect failed"))
        },
        |_| panic!("an effect failure must not invoke rollback"),
    );
    assert!(result.is_err());
    assert!(child.borrow().as_ref().unwrap().is_running());
    assert_eq!(std::fs::read(&fixture.config).unwrap(), original);
    config::save_profile(&fixture.config, "secondary", &fixture.profile).unwrap();
    assert!(child.borrow().as_ref().unwrap().is_running());
    assert_eq!(
        config::load_profile(&fixture.config, "primary").unwrap(),
        fixture.profile
    );
}

#[test]
fn dropped_store_unlocks_while_a_forked_child_still_holds_inherited_descriptors() {
    let fixture = Fixture::new();
    let root = fixture.profile.output.clone();
    let namespace = "synthetic-lock-lifecycle-store";
    let store = Store::open(&root, namespace).unwrap();
    let child = InheritedChild::hold_current_descriptors().unwrap();
    assert!(child.is_running());
    // The child must not weaken exclusion while the parent still owns Store.
    assert!(Store::open(&root, namespace).is_err());
    drop(store);
    let reopened = Store::open(&root, namespace).unwrap();
    assert!(child.is_running());
    assert!(reopened.saved_documents().unwrap().is_empty());
}
