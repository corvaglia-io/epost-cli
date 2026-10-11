//! Entirely synthetic local filesystem tests; no provider captures or secrets.

use epost_cli::storage::Store;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Fixture {
    _temp: TempDir,
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        // macOS exposes its temporary directory through a system symlink.
        // Tests deliberately use the real path; user-controlled symlinks are
        // tested separately and must be rejected by the store.
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("documents");
        Self {
            _temp: temp,
            base,
            root,
        }
    }

    fn namespace(&self, scope: &str) -> PathBuf {
        self.root
            .join(hex::encode(Sha256::digest(scope.as_bytes())))
    }

    fn manifest(&self, scope: &str) -> PathBuf {
        self.namespace(scope).join("manifest.json")
    }

    fn read_manifest(&self, scope: &str) -> Value {
        serde_json::from_slice(&fs::read(self.manifest(scope)).unwrap()).unwrap()
    }
}

fn pdf() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>",
        "<< /Length 0 >>\nstream\n\nendstream",
    ];
    let mut document = String::from("%PDF-1.4\n");
    let mut offsets = vec![0];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(document.len());
        document.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    let xref = document.len();
    document.push_str("xref\n0 5\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        document.push_str(&format!("{offset:010} 00000 n \n"));
    }
    document.push_str(&format!(
        "trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    document.into_bytes()
}

#[test]
fn opaque_malicious_ids_and_sources_never_become_paths() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "invented tenant / ../../scope").unwrap();
    let bytes = pdf();
    for id in [
        "../../escape",
        "/absolute/path",
        "C:\\evil\\file",
        "id\0with-nul",
        "name.pdf?x=../a",
    ] {
        let saved = store
            .save(id, "archive:../../fictional-folder", &bytes, None)
            .unwrap();
        assert!(saved.path.starts_with(&fixture.root));
        assert_eq!(saved.content_type, None);
        assert_eq!(saved.path.extension().unwrap(), "pdf");
        let name = saved.path.file_stem().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 64);
        assert!(name.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(fs::read(saved.path).unwrap(), bytes);
    }
    assert!(!fixture.base.join("escape").exists());
}

#[test]
fn repeat_runs_verify_and_record_all_inbox_archive_sources() {
    let fixture = Fixture::new();
    let bytes = pdf();
    let first = {
        let mut store = Store::open(&fixture.root, "synthetic-private").unwrap();
        store
            .save("invented-letter", "inbox", &bytes, Some("application/pdf"))
            .unwrap()
    };
    let mut store = Store::open(&fixture.root, "synthetic-private").unwrap();
    let second = store
        .verify_existing("invented-letter", "archive:root")
        .unwrap()
        .unwrap();
    let third = store
        .save("invented-letter", "archive:folder:invented", &bytes, None)
        .unwrap();
    assert_eq!(first.path, second.path);
    assert_eq!(second.path, third.path);
    assert_eq!(second.status, "already_saved");
    assert_eq!(third.status, "already_saved");
    for result in [&first, &second, &third] {
        assert_eq!(result.content_type.as_deref(), Some("application/pdf"));
    }
    let manifest = fixture.read_manifest("synthetic-private");
    let entries = manifest["entries"].as_object().unwrap();
    assert_eq!(entries.len(), 1);
    let record = entries.values().next().unwrap();
    assert_eq!(record["content_type"], "application/pdf");
    assert_eq!(
        record["sources"],
        serde_json::json!(["archive:folder:invented", "archive:root", "inbox"])
    );
    assert_eq!(fs::read(first.path).unwrap(), bytes);
}

#[test]
fn namespace_isolation_prevents_cross_tenant_collisions() {
    let fixture = Fixture::new();
    let bytes = pdf();
    let mut private = Store::open(&fixture.root, "origin-A|private|invented-tenant|0").unwrap();
    let mut business = Store::open(&fixture.root, "origin-A|business|invented-tenant|0").unwrap();
    let mut other_origin =
        Store::open(&fixture.root, "origin-B|private|invented-tenant|0").unwrap();
    let first = private
        .save("same-invented-id", "inbox", &bytes, None)
        .unwrap();
    let second = business
        .save("same-invented-id", "inbox", &bytes, None)
        .unwrap();
    let third = other_origin
        .save("same-invented-id", "inbox", &bytes, None)
        .unwrap();
    assert_ne!(first.path, second.path);
    assert_ne!(first.path, third.path);
}

#[test]
fn known_good_file_is_never_overwritten_by_changed_content() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    let original = store
        .save("id", "inbox", b"first synthetic binary\0", None)
        .unwrap();
    assert!(
        store
            .save("id", "archive:root", b"changed synthetic binary\0", None)
            .is_err()
    );
    assert_eq!(
        fs::read(original.path).unwrap(),
        b"first synthetic binary\0"
    );
    let manifest = fixture.read_manifest("scope");
    let entry = manifest["entries"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(entry["sources"], serde_json::json!(["inbox"]));
}

#[test]
fn detects_corrupt_and_missing_saved_files() {
    for remove in [false, true] {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.root, "scope").unwrap();
        let saved = store.save("id", "inbox", &pdf(), None).unwrap();
        if remove {
            fs::remove_file(&saved.path).unwrap();
        } else {
            let mut corrupt = fs::read(&saved.path).unwrap();
            corrupt[20] ^= 1;
            fs::write(&saved.path, corrupt).unwrap();
        }
        assert!(store.verify_existing("id", "inbox").is_err());
        drop(store);
        assert!(Store::open(&fixture.root, "scope").is_err());
    }
}

#[test]
fn rejects_malformed_manifest_and_detects_changes_while_open() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    store.save("id", "inbox", &pdf(), None).unwrap();
    fs::write(fixture.manifest("scope"), b"{broken synthetic JSON").unwrap();
    assert!(store.verify_existing("id", "inbox").is_err());
    drop(store);
    assert!(Store::open(&fixture.root, "scope").is_err());
}

#[test]
fn rejects_manifest_path_traversal_namespace_and_record_tampering() {
    for field in ["path", "namespace", "sha256", "bytes", "sources"] {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.root, "scope").unwrap();
        store.save("id", "inbox", &pdf(), None).unwrap();
        drop(store);
        let mut manifest = fixture.read_manifest("scope");
        if field == "namespace" {
            manifest["namespace"] = Value::String("another invented scope".to_owned());
        } else {
            let record = manifest["entries"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap();
            record[field] = match field {
                "path" => Value::String("../../escape.pdf".to_owned()),
                "sha256" => Value::String("0".repeat(64)),
                "bytes" => serde_json::json!(1),
                "sources" => serde_json::json!([]),
                _ => unreachable!(),
            };
        }
        fs::write(
            fixture.manifest("scope"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(Store::open(&fixture.root, "scope").is_err(), "{field}");
    }
}

#[test]
fn rejects_an_oversized_manifest_before_reading_it() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    store.save("id", "inbox", &pdf(), None).unwrap();
    drop(store);
    fs::OpenOptions::new()
        .write(true)
        .open(fixture.manifest("scope"))
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    assert!(Store::open(&fixture.root, "scope").is_err());
}

#[test]
fn interrupted_manifest_write_recovers_only_identical_complete_document() {
    for corrupt in [false, true] {
        let fixture = Fixture::new();
        let bytes = pdf();
        let saved = {
            let mut store = Store::open(&fixture.root, "scope").unwrap();
            store.save("id", "inbox", &bytes, None).unwrap()
        };
        fs::remove_file(fixture.manifest("scope")).unwrap();
        if corrupt {
            fs::write(&saved.path, b"synthetic interrupted bytes").unwrap();
        }
        let mut store = Store::open(&fixture.root, "scope").unwrap();
        assert!(store.verify_existing("id", "inbox").unwrap().is_none());
        let result = store.save("id", "archive:root", &bytes, None);
        if corrupt {
            assert!(result.is_err());
            assert_eq!(
                fs::read(saved.path).unwrap(),
                b"synthetic interrupted bytes"
            );
            assert!(!fixture.manifest("scope").exists());
        } else {
            assert_eq!(result.unwrap().path, saved.path);
            assert_eq!(fs::read(saved.path).unwrap(), bytes);
            assert_eq!(
                fixture.read_manifest("scope")["entries"]
                    .as_object()
                    .unwrap()
                    .len(),
                1
            );
        }
    }
}

#[test]
fn unrelated_interrupted_temporary_file_is_not_presented_as_a_document() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    let partial = fixture.namespace("scope").join("data/.epost-tmp-invented");
    fs::write(&partial, b"%PDF-1.4\npartial").unwrap();
    assert!(store.verify_existing("id", "inbox").unwrap().is_none());
    let saved = store.save("id", "inbox", &pdf(), None).unwrap();
    assert_ne!(saved.path, partial);
    assert_eq!(fs::read(partial).unwrap(), b"%PDF-1.4\npartial");
}

#[test]
fn concurrent_namespace_run_is_rejected_and_lock_releases_on_drop() {
    let fixture = Fixture::new();
    let first = Store::open(&fixture.root, "scope").unwrap();
    assert!(Store::open(&fixture.root, "scope").is_err());
    assert!(Store::open(&fixture.root, "another-scope").is_ok());
    drop(first);
    assert!(Store::open(&fixture.root, "scope").is_ok());
}

#[test]
fn classifies_successful_content_from_bytes_and_preserves_it_exactly() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    let pdf_bytes = pdf();
    let scenarios = [
        (
            "pdf",
            pdf_bytes.as_slice(),
            Some("application/octet-stream"),
            "saved_pdf",
            "pdf",
        ),
        (
            "json",
            b"{\"synthetic\":true}".as_slice(),
            Some("application/octet-stream"),
            "saved_json",
            "json",
        ),
        (
            "scalar-json",
            b"true".as_slice(),
            Some("application/json; charset=utf-8"),
            "saved_json",
            "json",
        ),
        (
            "binary",
            b"\0\xff\x01synthetic".as_slice(),
            Some("application/octet-stream"),
            "saved_binary",
            "binary",
        ),
    ];
    for (id, bytes, content_type, status, media_kind) in scenarios {
        let saved = store.save(id, "inbox", bytes, content_type).unwrap();
        assert_eq!(saved.status, status);
        assert_eq!(saved.media_kind, media_kind);
        assert_eq!(saved.content_type.as_deref(), content_type);
        assert_eq!(saved.bytes, bytes.len() as u64);
        assert_eq!(saved.sha256, hex::encode(Sha256::digest(bytes)));
        assert_eq!(fs::read(&saved.path).unwrap(), bytes);
        let repeat = store.verify_existing(id, "archive:root").unwrap().unwrap();
        assert_eq!(repeat.media_kind, media_kind);
        assert_eq!(repeat.status, "already_saved");
        assert_eq!(repeat.content_type.as_deref(), content_type);
        let json = serde_json::to_value(repeat).unwrap();
        assert!(json["path"].is_string());
        assert_eq!(json["content_type"], serde_json::json!(content_type));
    }
}

#[test]
fn rejects_empty_truncated_pdf_and_explicitly_invalid_json() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    for (bytes, content_type) in [
        (b"".as_slice(), None),
        (b"%PDF-1.7\ntruncated".as_slice(), None),
        (b"%PDF-x.y\nobjects\n%%EOF".as_slice(), None),
        (b"not a PDF".as_slice(), Some("application/pdf")),
        (b"{\"unfinished\":".as_slice(), Some("application/json")),
        (b"[1,".as_slice(), Some("application/problem+json")),
    ] {
        assert!(store.save("id", "inbox", bytes, content_type).is_err());
    }
    assert!(!fixture.manifest("scope").exists());
    assert_eq!(
        fs::read_dir(fixture.namespace("scope").join("data"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn rejects_empty_ids_and_parent_components_in_root() {
    let fixture = Fixture::new();
    assert!(Store::open(&fixture.root, "").is_err());
    assert!(Store::open(&fixture.base.join("missing/../documents"), "scope").is_err());
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    assert!(store.save("", "inbox", &pdf(), None).is_err());
    assert!(store.save("id", "", &pdf(), None).is_err());
    assert!(
        store
            .save("id", "inbox", &pdf(), Some("application/pdf\r\ninvalid"))
            .is_err()
    );
}

#[cfg(unix)]
fn symlink(source: &Path, target: &Path) {
    std::os::unix::fs::symlink(source, target).unwrap();
}

#[cfg(unix)]
#[test]
fn rejects_symlink_root_ancestor_namespace_data_manifest_and_lock() {
    for target in ["root", "ancestor", "namespace", "data", "manifest", "lock"] {
        let fixture = Fixture::new();
        let outside = fixture.base.join("outside");
        fs::create_dir(&outside).unwrap();
        let root = if target == "ancestor" {
            symlink(&outside, &fixture.base.join("link"));
            fixture.base.join("link/documents")
        } else {
            fixture.root.clone()
        };
        if target == "root" {
            symlink(&outside, &root);
        } else if target != "ancestor" {
            fs::create_dir(&root).unwrap();
            if target == "namespace" {
                symlink(&outside, &fixture.namespace("scope"));
            } else {
                fs::create_dir(fixture.namespace("scope")).unwrap();
                if target == "data" {
                    symlink(&outside, &fixture.namespace("scope").join("data"));
                } else {
                    let external_file = outside.join("sentinel");
                    fs::write(&external_file, b"untouched synthetic sentinel").unwrap();
                    let name = if target == "manifest" {
                        "manifest.json"
                    } else {
                        ".lock"
                    };
                    symlink(&external_file, &fixture.namespace("scope").join(name));
                }
            }
        }
        assert!(Store::open(&root, "scope").is_err(), "{target}");
        if outside.join("sentinel").exists() {
            assert_eq!(
                fs::read(outside.join("sentinel")).unwrap(),
                b"untouched synthetic sentinel"
            );
        }
        assert!(!outside.join("documents").exists());
    }
}

#[cfg(unix)]
#[test]
fn rejects_document_symlinks_and_hardlinks_without_modifying_the_target() {
    for hardlink in [false, true] {
        let fixture = Fixture::new();
        let mut store = Store::open(&fixture.root, "scope").unwrap();
        let saved = store.save("id", "inbox", &pdf(), None).unwrap();
        let sentinel = fixture.base.join("sentinel");
        fs::write(&sentinel, b"untouched synthetic sentinel").unwrap();
        fs::remove_file(&saved.path).unwrap();
        if hardlink {
            fs::hard_link(&sentinel, &saved.path).unwrap();
        } else {
            symlink(&sentinel, &saved.path);
        }
        assert!(store.verify_existing("id", "inbox").is_err());
        assert!(store.save("id", "inbox", &pdf(), None).is_err());
        assert_eq!(fs::read(sentinel).unwrap(), b"untouched synthetic sentinel");
    }
}

#[cfg(unix)]
#[test]
fn detects_directory_replacement_after_open() {
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    let data = fixture.namespace("scope").join("data");
    fs::rename(&data, fixture.namespace("scope").join("old-data")).unwrap();
    fs::create_dir(&data).unwrap();
    assert!(store.save("id", "inbox", &pdf(), None).is_err());
    assert_eq!(fs::read_dir(data).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn rejects_nonregular_file_without_blocking_on_a_fifo() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    let saved = store.save("id", "inbox", &pdf(), None).unwrap();
    fs::remove_file(&saved.path).unwrap();
    let name = std::ffi::CString::new(saved.path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is a terminated synthetic temporary test path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(store.verify_existing("id", "inbox").is_err());
}

#[cfg(unix)]
#[test]
fn managed_directories_and_files_have_private_posix_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let mut store = Store::open(&fixture.root, "scope").unwrap();
    let saved = store.save("id", "inbox", &pdf(), None).unwrap();
    for path in [
        &fixture.root,
        &fixture.namespace("scope"),
        &fixture.namespace("scope").join("data"),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [
        &saved.path,
        &fixture.manifest("scope"),
        &fixture.namespace("scope").join(".lock"),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
