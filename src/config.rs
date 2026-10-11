//! Secret-free mailbox profiles stored in a private local configuration file.
//!
//! Saving is atomic and guarded by an advisory writer lock. Unix opens reject
//! symlinks and hard-linked managed files, and pin the parent directory while
//! saving. These checks protect against accidental substitution and access by
//! other users, not a malicious process running under the same user ID.

use crate::api::Environment;
use crate::error::{Error, Result};
use crate::models::{Mailbox, MailboxKind};
use clap::ValueEnum;
use fs2::FileExt;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SyncScope {
    Inbox,
    Archive,
    Both,
}

/// How a runtime login obtains the password; secret values are never stored here.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginMethod {
    Prompt,
    MacosKeychain,
}

/// Optional remembered account name and password source, without a password.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    pub username: String,
    pub method: LoginMethod,
}

/// Reusable mailbox settings and optional account name; no passwords or tokens.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(deserialize_with = "strict_mailbox")]
    pub mailbox: Mailbox,
    pub environment: Environment,
    pub output: PathBuf,
    pub scope: SyncScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<Login>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    #[serde(deserialize_with = "unique_profiles")]
    profiles: BTreeMap<String, Profile>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            schema_version: 1,
            profiles: BTreeMap::new(),
        }
    }
}

/// Resolve the config location without creating files or reading host config.
pub fn path(explicit: Option<&Path>) -> Result<PathBuf> {
    let resolved = match explicit {
        Some(path) => path.to_owned(),
        None => default_path()?,
    };
    validate_absolute(&resolved)?;
    Ok(resolved)
}

fn default_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = home()?.join("Library/Application Support");
    #[cfg(not(target_os = "macos"))]
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home()?.join(".config"),
    };
    Ok(base.join("epost-cli/config.json"))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or(Error::Config(
            "HOME is unavailable; supply --config explicitly",
        ))
}

pub fn load_profile(path: &Path, name: &str) -> Result<Profile> {
    validate_name(name)?;
    let (parent, filename) = split_path(path)?;
    let directory = ConfigDirectory::open(parent, false)?
        .ok_or(Error::Config("profile does not exist; run setup first"))?;
    let raw = directory
        .read_optional(filename)?
        .ok_or(Error::Config("profile does not exist; run setup first"))?;
    let mut document = decode(&raw)?;
    document
        .profiles
        .remove(name)
        .ok_or(Error::Config("profile does not exist; run setup first"))
}

/// A missing file or parent is not an error; an unsafe or corrupt file is.
pub fn profile_exists(path: &Path, name: &str) -> Result<bool> {
    validate_name(name)?;
    let (parent, filename) = split_path(path)?;
    let Some(directory) = ConfigDirectory::open(parent, false)? else {
        return Ok(false);
    };
    let Some(raw) = directory.read_optional(filename)? else {
        return Ok(false);
    };
    Ok(decode(&raw)?.profiles.contains_key(name))
}

/// Preserve every existing profile; never overwrite corrupt configuration.
pub fn save_profile(path: &Path, name: &str, profile: &Profile) -> Result<()> {
    save_inner(path, name, profile, None, || Ok(()), |_| Ok(()))
}

/// Save only if the target still matches the profile the caller reviewed.
/// `None` requires that the target profile is absent. The comparison and save
/// share one writer lock, so a setup confirmation cannot overwrite a profile
/// changed by another cooperating writer while the user was deciding.
pub fn save_profile_if_unchanged(
    path: &Path,
    name: &str,
    profile: &Profile,
    expected: Option<&Profile>,
) -> Result<()> {
    save_profile_with_effect(path, name, profile, expected, || Ok(()), |_| Ok(()))
}

/// Compare and save a profile together with a reversible external change.
///
/// The writer lock is held while checking `expected`, preparing the private,
/// synced replacement file, applying `effect`, and committing configuration.
/// `None` requires that the target profile is absent. `effect` returns the state
/// needed to undo its change; if it fails, it must undo any partial change itself.
/// A failure before atomic replacement invokes `rollback` while still locked.
/// After replacement succeeds, later durability/readback failures do not undo
/// the effect because the new configuration is already current. Callbacks must
/// not write this configuration or recursively invoke a profile save.
pub fn save_profile_with_effect<T>(
    path: &Path,
    name: &str,
    profile: &Profile,
    expected: Option<&Profile>,
    effect: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(T) -> Result<()>,
) -> Result<()> {
    save_inner(path, name, profile, Some(expected), effect, rollback)
}

fn save_inner<T>(
    path: &Path,
    name: &str,
    profile: &Profile,
    expected: Option<Option<&Profile>>,
    effect: impl FnOnce() -> Result<T>,
    rollback: impl FnOnce(T) -> Result<()>,
) -> Result<()> {
    validate_name(name)?;
    validate_profile(profile)?;
    let (parent, filename) = split_path(path)?;
    let directory = ConfigDirectory::open(parent, true)?.ok_or(Error::Config(
        "configuration directory could not be created",
    ))?;
    let mut lock_name = OsString::from(".");
    lock_name.push(filename);
    lock_name.push(".lock");
    let lock = directory.open_file(&lock_name, true)?;
    FileExt::try_lock_exclusive(&lock)
        .map_err(|_| Error::Config("configuration is being saved by another run; retry shortly"))?;
    let previous = directory.read_optional(filename)?;
    let mut document = match previous.as_deref() {
        Some(raw) => decode(raw)?,
        None => Document::default(),
    };
    if expected.is_some_and(|expected| document.profiles.get(name) != expected) {
        return Err(Error::Config(
            "profile changed since it was reviewed; run setup again",
        ));
    }
    document.profiles.insert(name.to_owned(), profile.clone());
    let raw = serde_json::to_vec_pretty(&document)
        .map_err(|_| Error::Config("configuration could not be encoded"))?;
    if raw.len() > MAX_CONFIG_BYTES {
        return Err(Error::Config("configuration exceeds its size limit"));
    }
    directory.validate()?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".epost-config-")
        .tempfile_in(&directory.path)
        .map_err(config_io)?;
    private_file(temporary.as_file())?;
    temporary.write_all(&raw).map_err(config_io)?;
    temporary.as_file().sync_all().map_err(config_io)?;
    directory.validate()?;
    if directory.read_optional(filename)? != previous {
        return Err(Error::Config("configuration changed while saving; retry"));
    }
    let undo = effect()?;
    let commit = (|| {
        directory.validate()?;
        temporary
            .persist(directory.path.join(filename))
            .map_err(|_| Error::Config("configuration could not be saved atomically"))?;
        Ok(())
    })();
    if let Err(error) = commit {
        rollback(undo).map_err(|_| Error::Config("configuration could not be saved and rollback failed; repair the external credential store and run setup again before using this profile"))?;
        return Err(error);
    }
    directory.sync()?;
    directory.validate()?;
    if directory.read_optional(filename)?.as_deref() != Some(raw.as_slice()) {
        return Err(Error::Config("configuration changed during save"));
    }
    Ok(())
}

fn decode(raw: &[u8]) -> Result<Document> {
    let document: Document = serde_json::from_slice(raw)
        .map_err(|_| Error::Config("configuration is invalid; repair it before running setup"))?;
    if document.schema_version != 1 {
        return Err(Error::Config("configuration schema version is unsupported"));
    }
    for (name, profile) in &document.profiles {
        validate_name(name)?;
        validate_profile(profile)?;
    }
    Ok(document)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::Config(
            "profile name must be 1..64 letters, digits, underscores or hyphens",
        ));
    }
    Ok(())
}

fn validate_profile(profile: &Profile) -> Result<()> {
    let id = &profile.mailbox.tenant_id;
    if id.trim().is_empty()
        || id.len() > 4096
        || matches!(id.as_str(), "." | "..")
        || id.chars().any(char::is_control)
        || profile.mailbox.company_id < 0
    {
        return Err(Error::Config("profile mailbox identity is invalid"));
    }
    if profile.login.as_ref().is_some_and(|login| {
        login.username.trim().is_empty()
            || login.username.len() > 4096
            || login.username.chars().any(char::is_control)
    }) {
        return Err(Error::Config("profile login username is invalid"));
    }
    validate_absolute(&profile.output)
}

fn validate_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.components().any(|part| part == Component::ParentDir)
        || path.file_name().is_none()
        || path
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(char::is_control)
    {
        return Err(Error::Config(
            "use an absolute path without parent traversal or control characters",
        ));
    }
    Ok(())
}

fn split_path(path: &Path) -> Result<(&Path, &OsStr)> {
    validate_absolute(path)?;
    let parent = path
        .parent()
        .ok_or(Error::Config("configuration must have a parent directory"))?;
    let filename = path
        .file_name()
        .ok_or(Error::Config("configuration must have a filename"))?;
    Ok((parent, filename))
}

fn strict_mailbox<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Mailbox, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Identity {
        tenant_id: String,
        company_id: i64,
        kind: MailboxKind,
    }
    let identity = Identity::deserialize(deserializer)?;
    Ok(Mailbox {
        tenant_id: identity.tenant_id,
        company_id: identity.company_id,
        kind: identity.kind,
    })
}

fn unique_profiles<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, Profile>, D::Error> {
    struct ProfilesVisitor;
    impl<'de> Visitor<'de> for ProfilesVisitor {
        type Value = BTreeMap<String, Profile>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map of unique profile names")
        }
        fn visit_map<M: MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut profiles = BTreeMap::new();
            while let Some((name, profile)) = map.next_entry::<String, Profile>()? {
                if profiles.insert(name, profile).is_some() {
                    return Err(serde::de::Error::custom("duplicate profile name"));
                }
            }
            Ok(profiles)
        }
    }
    deserializer.deserialize_map(ProfilesVisitor)
}

fn config_io(_: std::io::Error) -> Error {
    Error::Config("local configuration filesystem operation failed")
}

struct ConfigDirectory {
    path: PathBuf,
    #[cfg(unix)]
    handle: File,
}

impl ConfigDirectory {
    fn open(path: &Path, create: bool) -> Result<Option<Self>> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut handle = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")
                .map_err(config_io)?;
            for part in path.components() {
                if let Component::Normal(name) = part {
                    handle = match open_at(&handle, name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
                        Ok(file) => file,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            if !create {
                                return Ok(None);
                            }
                            mkdir_at(&handle, name).map_err(config_io)?;
                            open_at(&handle, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                                .map_err(config_io)?
                        }
                        Err(error) => return Err(config_io(error)),
                    };
                }
            }
            let directory = Self {
                path: path.to_owned(),
                handle,
            };
            directory.validate()?;
            Ok(Some(directory))
        }
        #[cfg(not(unix))]
        {
            let mut current = PathBuf::new();
            for part in path.components() {
                current.push(part.as_os_str());
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Ok(_) => return Err(Error::Config("configuration ancestor is unsafe")),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if !create {
                            return Ok(None);
                        }
                        fs::create_dir(&current).map_err(config_io)?;
                    }
                    Err(error) => return Err(config_io(error)),
                }
            }
            Ok(Some(Self {
                path: path.to_owned(),
            }))
        }
    }

    fn validate(&self) -> Result<()> {
        let mut current = PathBuf::new();
        for part in self.path.components() {
            current.push(part.as_os_str());
            let metadata = fs::symlink_metadata(&current).map_err(config_io)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(Error::Config("configuration ancestor is unsafe"));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let actual = fs::symlink_metadata(&self.path).map_err(config_io)?;
            let pinned = self.handle.metadata().map_err(config_io)?;
            if actual.dev() != pinned.dev() || actual.ino() != pinned.ino() {
                return Err(Error::Config("configuration directory changed during use"));
            }
        }
        Ok(())
    }

    fn open_file(&self, name: &OsStr, create: bool) -> Result<File> {
        self.open_file_io(name, create).map_err(config_io)
    }

    fn open_file_io(&self, name: &OsStr, create: bool) -> std::io::Result<File> {
        self.validate().map_err(std::io::Error::other)?;
        #[cfg(unix)]
        let file = open_at(
            &self.handle,
            name,
            if create {
                libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK
            } else {
                libc::O_RDONLY | libc::O_NONBLOCK
            },
            0o600,
        )?;
        #[cfg(not(unix))]
        let file = {
            let path = self.path.join(name);
            if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(std::io::Error::other("unsafe configuration file"));
            }
            OpenOptions::new()
                .read(true)
                .write(create)
                .create(create)
                .open(&path)?
        };
        private_file(&file).map_err(std::io::Error::other)?;
        self.validate().map_err(std::io::Error::other)?;
        Ok(file)
    }

    fn read_optional(&self, name: &OsStr) -> Result<Option<Vec<u8>>> {
        let file = match self.open_file_io(name, false) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(config_io(error)),
        };
        let size = file.metadata().map_err(config_io)?.len();
        if size > MAX_CONFIG_BYTES as u64 {
            return Err(Error::Config("configuration exceeds its size limit"));
        }
        let mut raw = Vec::with_capacity(size as usize);
        file.take(MAX_CONFIG_BYTES as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(config_io)?;
        if raw.len() > MAX_CONFIG_BYTES {
            return Err(Error::Config("configuration exceeds its size limit"));
        }
        self.validate()?;
        Ok(Some(raw))
    }

    fn sync(&self) -> Result<()> {
        #[cfg(unix)]
        self.handle.sync_all().map_err(config_io)?;
        Ok(())
    }
}

fn private_file(file: &File) -> Result<()> {
    let metadata = file.metadata().map_err(config_io)?;
    if !metadata.is_file() {
        return Err(Error::Config("configuration path is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
            return Err(Error::Config(
                "configuration files must be private and have no hard links",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_at(parent: &File, name: &OsStr, flags: i32, mode: libc::mode_t) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::other("invalid configuration filename"))?;
    // SAFETY: parent is a live descriptor; name is terminated; a successful
    // descriptor is transferred exactly once into an owning File.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn mkdir_at(parent: &File, name: &OsStr) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::other("invalid configuration directory"))?;
    // SAFETY: parent is a live descriptor and name is a terminated component.
    let status = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if status < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Ok(())
}
