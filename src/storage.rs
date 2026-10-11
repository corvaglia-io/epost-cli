//! A private local retrieval store with checked repeat runs.
//!
//! Documents are named by namespace and opaque ID hashes. Unix file opens are
//! descriptor-relative and do not follow symlinks; directories are pinned and
//! checked around atomic persistence. The store protects against accidental
//! path substitution and other-user access, not a malicious process running
//! with the same user ID. PDF validation checks its header and final EOF marker,
//! not the semantic validity or safety of every PDF object.

use crate::error::{Error, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use tempfile::NamedTempFile;

const MANIFEST_NAME: &str = "manifest.json";
const MAX_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
/// Independent upper bound for content held by the local store.
pub const MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_ID_BYTES: usize = 4096;
const MAX_CONTENT_TYPE_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SaveResult {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub status: String,
    pub media_kind: String,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MediaKind {
    Pdf,
    Json,
    Binary,
}

impl MediaKind {
    fn extension(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Json => "json",
            Self::Binary => "bin",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Json => "json",
            Self::Binary => "binary",
        }
    }

    fn saved_status(self) -> &'static str {
        match self {
            Self::Pdf => "saved_pdf",
            Self::Json => "saved_json",
            Self::Binary => "saved_binary",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    letter_id: String,
    sources: BTreeSet<String>,
    path: String,
    sha256: String,
    bytes: u64,
    kind: MediaKind,
    content_type: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    namespace: String,
    namespace_hash: String,
    entries: BTreeMap<String, Record>,
}

/// Holds the namespace's exclusive advisory lock for its entire lifetime.
pub struct Store {
    root: Directory,
    namespace_dir: Directory,
    data_dir: Directory,
    _lock: File,
    manifest: Manifest,
    manifest_sha256: Option<String>,
}

impl Store {
    pub fn open(root: &Path, namespace: &str) -> Result<Self> {
        validate_identifier(namespace)?;
        let root = Directory::open_root(root)?;
        let namespace_hash = hash(namespace.as_bytes());
        let namespace_dir = root.child(&namespace_hash)?;
        let lock = namespace_dir.open_file(".lock", true)?;
        private_file(&lock)?;
        FileExt::try_lock_exclusive(&lock)
            .map_err(|_| Error::Storage("store is locked by another run"))?;
        let data_dir = namespace_dir.child("data")?;
        let raw = namespace_dir.read_optional(MANIFEST_NAME, MAX_MANIFEST_BYTES)?;
        let manifest_sha256 = raw.as_deref().map(hash);
        let manifest = match raw {
            Some(raw) => serde_json::from_slice::<Manifest>(&raw)
                .map_err(|_| Error::Storage("manifest is corrupt"))?,
            None => Manifest {
                version: 1,
                namespace: namespace.to_owned(),
                namespace_hash,
                entries: BTreeMap::new(),
            },
        };
        let store = Self {
            root,
            namespace_dir,
            data_dir,
            _lock: lock,
            manifest,
            manifest_sha256,
        };
        store.validate_manifest(namespace)?;
        Ok(store)
    }

    /// Verifies the saved bytes before adding a new inbox/archive source.
    /// An unrecorded orphan returns `None`; a later `save` may adopt it after
    /// comparing it with freshly retrieved bytes.
    pub fn verify_existing(&mut self, letter_id: &str, source: &str) -> Result<Option<SaveResult>> {
        validate_identifier(letter_id)?;
        validate_identifier(source)?;
        self.check_directories()?;
        self.check_manifest_unchanged()?;
        let key = document_key(&self.manifest.namespace_hash, letter_id);
        let Some(record) = self.manifest.entries.get(&key).cloned() else {
            return Ok(None);
        };
        self.verify_record(&record)?;
        self.record_source(&key, source)?;
        Ok(Some(self.result(&record, "already_saved")))
    }

    pub fn save(
        &mut self,
        letter_id: &str,
        source: &str,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<SaveResult> {
        validate_identifier(letter_id)?;
        validate_identifier(source)?;
        validate_content_type(content_type)?;
        let kind = classify(bytes, content_type)?;
        let digest = hash(bytes);
        self.check_directories()?;
        self.check_manifest_unchanged()?;
        let key = document_key(&self.manifest.namespace_hash, letter_id);
        if let Some(record) = self.manifest.entries.get(&key).cloned() {
            self.verify_record(&record)?;
            if record.sha256 != digest || record.bytes != bytes.len() as u64 || record.kind != kind
            {
                return Err(Error::Storage(
                    "saved document conflicts with retrieved content",
                ));
            }
            self.record_source(&key, source)?;
            return Ok(self.result(&record, "already_saved"));
        }
        let name = format!("{key}.{}", kind.extension());
        let mut orphan_exists = false;
        for candidate in [MediaKind::Pdf, MediaKind::Json, MediaKind::Binary] {
            let candidate_name = format!("{key}.{}", candidate.extension());
            if let Some(existing) = self
                .data_dir
                .read_optional(&candidate_name, MAX_CONTENT_BYTES)?
            {
                if candidate != kind
                    || hash(&existing) != digest
                    || existing.len() != bytes.len()
                    || classify(&existing, content_type)? != kind
                {
                    return Err(Error::Storage(
                        "unrecorded document conflicts with retrieved content",
                    ));
                }
                orphan_exists = true;
            }
        }
        if !orphan_exists {
            let temp = self.data_dir.write_temp(bytes)?;
            self.check_directories()?;
            match temp.persist_noclobber(self.data_dir.path.join(&name)) {
                Ok(_) => self.data_dir.sync()?,
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = self
                        .data_dir
                        .read_optional(&name, MAX_CONTENT_BYTES)?
                        .ok_or(Error::Storage("document disappeared during save"))?;
                    if hash(&existing) != digest || existing != bytes {
                        return Err(Error::Storage(
                            "document path already contains different content",
                        ));
                    }
                }
                Err(error) => return Err(Error::Io(error.error)),
            }
        }
        self.check_directories()?;
        let record = Record {
            letter_id: letter_id.to_owned(),
            sources: BTreeSet::from([source.to_owned()]),
            path: format!("data/{name}"),
            sha256: digest,
            bytes: bytes.len() as u64,
            kind,
            content_type: content_type.map(str::to_owned),
        };
        self.verify_record(&record)?;
        let mut next = self.manifest.clone();
        next.entries.insert(key, record.clone());
        self.persist_manifest(next)?;
        Ok(self.result(&record, kind.saved_status()))
    }

    fn validate_manifest(&self, namespace: &str) -> Result<()> {
        if self.manifest.version != 1
            || self.manifest.namespace != namespace
            || self.manifest.namespace_hash != hash(namespace.as_bytes())
        {
            return Err(Error::Storage("manifest namespace or version is invalid"));
        }
        for (key, record) in &self.manifest.entries {
            validate_identifier(&record.letter_id)?;
            validate_content_type(record.content_type.as_deref())?;
            if key != &document_key(&self.manifest.namespace_hash, &record.letter_id)
                || record.path != format!("data/{key}.{}", record.kind.extension())
                || !valid_hash(&record.sha256)
                || record.bytes == 0
                || record.bytes > MAX_CONTENT_BYTES as u64
                || record.sources.is_empty()
            {
                return Err(Error::Storage("manifest record is invalid"));
            }
            for source in &record.sources {
                validate_identifier(source)?;
            }
            self.verify_record(record)?;
        }
        Ok(())
    }

    fn check_directories(&self) -> Result<()> {
        self.root.validate()?;
        self.namespace_dir.validate()?;
        self.data_dir.validate()
    }

    fn verify_record(&self, record: &Record) -> Result<()> {
        self.check_directories()?;
        let name = record
            .path
            .strip_prefix("data/")
            .ok_or(Error::Storage("manifest document path is invalid"))?;
        validate_filename(name)?;
        let bytes = self
            .data_dir
            .read_optional(name, MAX_CONTENT_BYTES)?
            .ok_or(Error::Storage("saved document is missing"))?;
        if bytes.len() as u64 != record.bytes
            || hash(&bytes) != record.sha256
            || classify(&bytes, record.content_type.as_deref())? != record.kind
        {
            return Err(Error::Storage("saved document is corrupt"));
        }
        Ok(())
    }

    fn check_manifest_unchanged(&self) -> Result<()> {
        let raw = self
            .namespace_dir
            .read_optional(MANIFEST_NAME, MAX_MANIFEST_BYTES)?;
        if raw.as_deref().map(hash) != self.manifest_sha256 {
            return Err(Error::Storage("manifest changed while store was open"));
        }
        Ok(())
    }

    fn record_source(&mut self, key: &str, source: &str) -> Result<()> {
        if self.manifest.entries[key].sources.contains(source) {
            return Ok(());
        }
        let mut next = self.manifest.clone();
        next.entries
            .get_mut(key)
            .ok_or(Error::Storage("manifest record is missing"))?
            .sources
            .insert(source.to_owned());
        self.persist_manifest(next)
    }

    fn persist_manifest(&mut self, next: Manifest) -> Result<()> {
        self.check_directories()?;
        self.check_manifest_unchanged()?;
        let raw = serde_json::to_vec_pretty(&next)
            .map_err(|_| Error::Storage("manifest could not be serialized"))?;
        if raw.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Storage("manifest exceeds its size limit"));
        }
        let temp = self.namespace_dir.write_temp(&raw)?;
        self.check_directories()?;
        self.check_manifest_unchanged()?;
        let target = self.namespace_dir.path.join(MANIFEST_NAME);
        if self.manifest_sha256.is_some() {
            temp.persist(target)
                .map_err(|error| Error::Io(error.error))?;
        } else {
            temp.persist_noclobber(target)
                .map_err(|error| Error::Io(error.error))?;
        }
        self.namespace_dir.sync()?;
        self.check_directories()?;
        let persisted = self
            .namespace_dir
            .read_optional(MANIFEST_NAME, MAX_MANIFEST_BYTES)?
            .ok_or(Error::Storage("manifest disappeared during save"))?;
        if persisted != raw {
            return Err(Error::Storage("manifest changed during save"));
        }
        self.manifest_sha256 = Some(hash(&raw));
        self.manifest = next;
        Ok(())
    }

    fn result(&self, record: &Record, status: &str) -> SaveResult {
        SaveResult {
            path: self.namespace_dir.path.join(&record.path),
            sha256: record.sha256.clone(),
            bytes: record.bytes,
            status: status.to_owned(),
            media_kind: record.kind.name().to_owned(),
            content_type: record.content_type.clone(),
        }
    }
}

fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn document_key(namespace_hash: &str, id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"epost-document-v1\0");
    digest.update(namespace_hash.as_bytes());
    digest.update(b"\0");
    digest.update(id.as_bytes());
    hex::encode(digest.finalize())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_identifier(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err(Error::Storage(
            "identifier is empty or exceeds its size limit",
        ));
    }
    Ok(())
}

fn validate_content_type(value: Option<&str>) -> Result<()> {
    if value.is_some_and(|v| v.len() > MAX_CONTENT_TYPE_BYTES || v.contains(['\r', '\n', '\0'])) {
        return Err(Error::Storage("content type is invalid"));
    }
    Ok(())
}

fn classify(bytes: &[u8], content_type: Option<&str>) -> Result<MediaKind> {
    if bytes.is_empty() || bytes.len() > MAX_CONTENT_BYTES {
        return Err(Error::Storage("content is empty or exceeds its size limit"));
    }
    let mime = content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if bytes.starts_with(b"%PDF-") || mime.eq_ignore_ascii_case("application/pdf") {
        let header_valid = bytes.len() >= 9
            && bytes.starts_with(b"%PDF-")
            && bytes[5].is_ascii_digit()
            && bytes[6] == b'.'
            && bytes[7].is_ascii_digit()
            && matches!(bytes[8], b'\r' | b'\n');
        let trimmed = bytes.trim_ascii_end();
        if !header_valid || trimmed.len() <= 14 || !trimmed.ends_with(b"%%EOF") {
            return Err(Error::Storage("PDF content is invalid or truncated"));
        }
        return Ok(MediaKind::Pdf);
    }
    if serde_json::from_slice::<serde_json::Value>(bytes).is_ok() {
        return Ok(MediaKind::Json);
    }
    if mime.eq_ignore_ascii_case("application/json") || mime.to_ascii_lowercase().ends_with("+json")
    {
        return Err(Error::Storage("JSON content is invalid or truncated"));
    }
    Ok(MediaKind::Binary)
}

fn validate_filename(name: &str) -> Result<()> {
    if name.is_empty()
        || Path::new(name).components().count() != 1
        || !matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
        || name.contains(['/', '\\', '\0'])
    {
        return Err(Error::Storage("unsafe storage filename"));
    }
    Ok(())
}

struct Directory {
    path: PathBuf,
    #[cfg(unix)]
    handle: File,
}

impl Directory {
    fn open_root(path: &Path) -> Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(Error::Storage("storage root is empty"));
        }
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir().map_err(Error::Io)?.join(path)
        };
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::ParentDir => {
                    return Err(Error::Storage("storage root contains parent traversal"));
                }
                Component::CurDir => {}
                _ => normalized.push(component.as_os_str()),
            }
        }
        if normalized.parent().is_none() {
            return Err(Error::Storage("filesystem root cannot be a document store"));
        }
        #[cfg(unix)]
        {
            let mut handle = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open("/")
                .map_err(Error::Io)?;
            let mut current = PathBuf::from("/");
            for component in normalized.components() {
                if let Component::Normal(name) = component {
                    current.push(name);
                    mkdir_at(&handle, name)?;
                    handle = open_at(&handle, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                }
            }
            private_directory(&handle)?;
            let directory = Self {
                path: normalized,
                handle,
            };
            directory.validate()?;
            Ok(directory)
        }
        #[cfg(not(unix))]
        {
            create_checked_directories(&normalized)?;
            let directory = Self { path: normalized };
            directory.validate()?;
            Ok(directory)
        }
    }

    fn child(&self, name: &str) -> Result<Self> {
        validate_filename(name)?;
        self.validate()?;
        #[cfg(unix)]
        {
            mkdir_at(&self.handle, std::ffi::OsStr::new(name))?;
            let handle = open_at(
                &self.handle,
                std::ffi::OsStr::new(name),
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )?;
            private_directory(&handle)?;
            let child = Self {
                path: self.path.join(name),
                handle,
            };
            child.validate()?;
            Ok(child)
        }
        #[cfg(not(unix))]
        {
            let path = self.path.join(name);
            if let Err(error) = fs::create_dir(&path) {
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(Error::Io(error));
                }
            }
            let child = Self { path };
            child.validate()?;
            Ok(child)
        }
    }

    fn validate(&self) -> Result<()> {
        let mut current = PathBuf::new();
        for component in self.path.components() {
            current.push(component.as_os_str());
            let metadata = fs::symlink_metadata(&current).map_err(Error::Io)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(Error::Storage(
                    "storage directory is a symlink or is not a directory",
                ));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current = fs::symlink_metadata(&self.path).map_err(Error::Io)?;
            let pinned = self.handle.metadata().map_err(Error::Io)?;
            if current.dev() != pinned.dev() || current.ino() != pinned.ino() {
                return Err(Error::Storage(
                    "storage directory changed while store was open",
                ));
            }
        }
        Ok(())
    }

    fn open_file(&self, name: &str, create: bool) -> Result<File> {
        validate_filename(name)?;
        self.validate()?;
        #[cfg(unix)]
        let file = open_at(
            &self.handle,
            std::ffi::OsStr::new(name),
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
            reject_symlink_if_present(&path)?;
            let file = OpenOptions::new()
                .read(true)
                .write(create)
                .create(create)
                .open(&path)
                .map_err(Error::Io)?;
            reject_symlink_if_present(&path)?;
            file
        };
        check_regular_file(&file)?;
        self.validate()?;
        Ok(file)
    }

    fn read_optional(&self, name: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        let file = match self.open_file(name, false) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let size = file.metadata().map_err(Error::Io)?.len();
        if size > max_bytes as u64 {
            return Err(Error::Storage("stored file exceeds its size limit"));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(Error::Io)?;
        if bytes.len() > max_bytes {
            return Err(Error::Storage("stored file exceeds its size limit"));
        }
        self.validate()?;
        Ok(Some(bytes))
    }

    fn write_temp(&self, bytes: &[u8]) -> Result<NamedTempFile> {
        self.validate()?;
        let mut temp = tempfile::Builder::new()
            .prefix(".epost-tmp-")
            .tempfile_in(&self.path)
            .map_err(Error::Io)?;
        private_file(temp.as_file())?;
        temp.write_all(bytes).map_err(Error::Io)?;
        temp.as_file().sync_all().map_err(Error::Io)?;
        self.validate()?;
        Ok(temp)
    }

    fn sync(&self) -> Result<()> {
        #[cfg(unix)]
        self.handle.sync_all().map_err(Error::Io)?;
        Ok(())
    }
}

fn check_regular_file(file: &File) -> Result<()> {
    let metadata = file.metadata().map_err(Error::Io)?;
    if !metadata.is_file() {
        return Err(Error::Storage("stored path is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Error::Storage("stored file has unexpected hard links"));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(Error::Storage("stored file permissions are not private"));
        }
    }
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(unix)]
fn open_at(
    directory: &File,
    name: &std::ffi::OsStr,
    flags: i32,
    mode: libc::c_uint,
) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| Error::Storage("storage path contains a NUL byte"))?;
    // SAFETY: the directory descriptor remains owned and open; `name` is a
    // valid terminated string. A successful descriptor is transferred once.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
            return Err(Error::Storage(
                "storage path is a symlink or has an invalid component",
            ));
        }
        return Err(Error::Io(error));
    }
    // SAFETY: openat returned a new descriptor whose ownership is ours.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn mkdir_at(directory: &File, name: &std::ffi::OsStr) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| Error::Storage("storage path contains a NUL byte"))?;
    // SAFETY: the directory descriptor is live and `name` is terminated.
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(Error::Io(error));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_mode(file: &File, mode: libc::mode_t) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor is live; fchmod operates on that exact object.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } < 0 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

fn private_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    set_private_mode(file, 0o600)?;
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

#[cfg(unix)]
fn private_directory(file: &File) -> Result<()> {
    set_private_mode(file, 0o700)
}

#[cfg(not(unix))]
fn reject_symlink_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(Error::Storage("stored path is a symlink"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

#[cfg(not(unix))]
fn create_checked_directories(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        reject_symlink_if_present(&current)?;
        if let Err(error) = fs::create_dir(&current) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(Error::Io(error));
            }
        }
    }
    Ok(())
}
