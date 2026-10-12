use epost_cli::{
    delivery::{self, PaperlessTarget},
    models::MailboxKind,
    storage::Store,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    process::{Command, Output, Stdio},
};
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "synthetic-paperless-token-never-real";
const EPOST_TOKEN: &str = "synthetic-epost-token-never-real";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n";
const TASK: &str = "11111111-1111-4111-8111-111111111111";

fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("synthetic-paperless-cli-")
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

fn command(epost: &MockServer, output: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
    c.env_clear().stdin(Stdio::null()).args([
        "--json",
        "--mock-url",
        &epost.uri(),
        "--tenant-kind",
        "private",
        "--tenant-id",
        "synthetic-tenant",
        "--company-id",
        "0",
        "--retries",
        "0",
        "--timeout-seconds",
        "2",
    ]);
    // Output is passed on each command, to verify it is not read from host profiles.
    assert!(output.is_absolute());
    c
}

fn result(out: &Output, code: i32) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    for bytes in [&out.stdout, &out.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(TOKEN));
        assert!(!text.contains(EPOST_TOKEN));
    }
    serde_json::from_slice(text).unwrap()
}

fn namespace(epost: &MockServer) -> String {
    serde_json::to_string(&(epost.uri(), "synthetic-tenant", 0, MailboxKind::Private)).unwrap()
}

fn save(epost: &MockServer, paperless: &MockServer, root: &std::path::Path, unsupported: bool) {
    let mut store = Store::open(root, &namespace(epost)).unwrap();
    store
        .save("synthetic-letter", "inbox", PDF, Some("application/pdf"))
        .unwrap();
    if unsupported {
        store
            .save(
                "synthetic-message",
                "inbox",
                b"{\"message\":\"synthetic\"}",
                Some("application/json"),
            )
            .unwrap();
    }
    delivery::save_target(
        &mut store,
        &PaperlessTarget {
            schema_version: 1,
            url: format!("{}/", paperless.uri()),
            inbox_tag_id: 1,
            source_tag_id: Some(2),
            remember_token: false,
        },
    )
    .unwrap();
}

async fn tags(server: &MockServer) {
    for (id, name, inbox) in [(1, "ePost Inbox", true), (2, "ePost", false)] {
        Mock::given(method("GET"))
            .and(path(format!("/api/tags/{id}/")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":id,"name":name,"is_inbox_tag":inbox})),
            )
            .mount(server)
            .await;
    }
}

async fn delivery_mocks(server: &MockServer, original: &[u8]) {
    tags(server).await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .and(query_param("task_id", TASK))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!([{"task_id":TASK,"status":"SUCCESS","related_document":"42"}]),
            ),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/42/download/"))
        .and(query_param("original", "true"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(original)
                .insert_header("Content-Type", "application/pdf"),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/42/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":42,"tags":[1,2]})))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_creates_review_tags_and_writes_no_token_without_epost_login() {
    let epost = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    Mock::given(method("GET"))
        .and(path("/api/tags/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"count":0,"next":null,"results":[]})),
        )
        .expect(1)
        .mount(&p)
        .await;
    for (id, name, inbox) in [(1, "ePost Inbox", true), (2, "ePost", false)] {
        Mock::given(method("POST"))
            .and(path("/api/tags/"))
            .and(body_partial_json(json!({"name":name,"is_inbox_tag":inbox})))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({"id":id,"name":name,"is_inbox_tag":inbox})),
            )
            .expect(1)
            .mount(&p)
            .await;
    }
    let out = command(&epost, root.path())
        .env("PAPERLESS_TOKEN", TOKEN)
        .args(["paperless", "setup", "--url", &p.uri(), "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(result(&out, 0)["data"]["configured"], true);
    let store = Store::open(root.path(), &namespace(&epost)).unwrap();
    let raw = store
        .read_private_state("paperless-target.json")
        .unwrap()
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains(TOKEN));
    assert!(!delivery::load_target(&store).unwrap().remember_token);
    assert!(epost.received_requests().await.unwrap().is_empty());
    for req in p.received_requests().await.unwrap() {
        assert_eq!(
            req.headers.get("authorization").unwrap(),
            &format!("Token {TOKEN}")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_once_rechecks_bytes_and_status_needs_no_token_or_network() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    save(&e, &p, root.path(), false);
    delivery_mocks(&p, PDF).await;
    for (first, field) in [(true, "delivered"), (false, "already_delivered")] {
        let mut selected = command(&e, root.path());
        if first {
            selected.arg("--progress");
        }
        let out = selected
            .env("PAPERLESS_TOKEN", TOKEN)
            .args(["paperless", "upload", "--wait-seconds", "0", "--output"])
            .arg(root.path())
            .output()
            .unwrap();
        assert_eq!(result(&out, 0)["data"][field], 1);
        if first {
            assert!(e.received_requests().await.unwrap().is_empty());
            let progress = String::from_utf8_lossy(&out.stderr);
            assert!(progress.contains("uploading PDF"));
            assert!(progress.contains("waiting for ingestion"));
            assert!(progress.contains("verifying original"));
        }
    }
    let before = p.received_requests().await.unwrap().len();
    let out = command(&e, root.path())
        .args(["paperless", "status", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(result(&out, 0)["data"]["verified"], 1);
    assert_eq!(p.received_requests().await.unwrap().len(), before);
    let reqs = p.received_requests().await.unwrap();
    assert_eq!(reqs.iter().filter(|r| r.method == "POST").count(), 1);
    assert_eq!(
        reqs.iter()
            .filter(|r| r.url.path().ends_with("/download/"))
            .count(),
        2
    );
    let body = String::from_utf8_lossy(&reqs.iter().find(|r| r.method == "POST").unwrap().body);
    assert!(body.contains("name=\"document\""));
    assert!(body.contains("name=\"tags\""));
    assert!(!body.contains(TOKEN));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_task_resumes_instead_of_uploading_twice() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    save(&e, &p, root.path(), false);
    tags(&p).await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK))
        .expect(1)
        .mount(&p)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!([{"task_id":TASK,"status":"STARTED","related_document":null}]),
            ),
        )
        .mount(&p)
        .await;
    for _ in 0..2 {
        let out = command(&e, root.path())
            .env("PAPERLESS_TOKEN", TOKEN)
            .args(["paperless", "upload", "--wait-seconds", "0", "--output"])
            .arg(root.path())
            .output()
            .unwrap();
        assert_eq!(result(&out, 2)["data"]["pending"], 1);
    }
    assert!(e.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_original_and_unsupported_formats_stay_partial() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    save(&e, &p, root.path(), true);
    delivery_mocks(&p, b"%PDF-1.4\nchanged\n%%EOF\n").await;
    let out = command(&e, root.path())
        .env("PAPERLESS_TOKEN", TOKEN)
        .args(["paperless", "upload", "--wait-seconds", "0", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    let data = result(&out, 2)["data"].clone();
    assert_eq!(data["delivered"], 0);
    assert_eq!(data["failed"], 1);
    assert_eq!(data["unsupported"], 1);
    let store = Store::open(root.path(), &namespace(&e)).unwrap();
    assert_eq!(store.read_saved("synthetic-letter").unwrap(), PDF);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_preserves_successful_downloads_if_paperless_auth_fails() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    save(&e, &p, root.path(), false);
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!([{"id":"synthetic-new-letter","letterType":"CLASSIC_LETTER"}]),
            ),
        )
        .mount(&e)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-new-letter/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("Content-Type", "application/pdf"),
        )
        .mount(&e)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags/1/"))
        .respond_with(ResponseTemplate::new(401).set_body_string(TOKEN))
        .mount(&p)
        .await;
    let out = command(&e, root.path())
        .env("EPOST_BEARER_TOKEN", EPOST_TOKEN)
        .env("PAPERLESS_TOKEN", TOKEN)
        .args(["sync", "--scope", "inbox", "--paperless", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    let data = result(&out, 3)["data"].clone();
    assert!(
        data["documents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["status"] == "saved_pdf")
    );
    assert_eq!(data["paperless_exit_code"], 3);
    let store = Store::open(root.path(), &namespace(&e)).unwrap();
    assert_eq!(store.read_saved("synthetic-new-letter").unwrap(), PDF);
    for req in e.received_requests().await.unwrap() {
        assert!(!String::from_utf8_lossy(&req.body).contains(TOKEN));
        assert!(req.method == "GET");
    }
    assert_eq!(p.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_password_stdin_and_separate_paperless_token_work_together() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    {
        let mut store = Store::open(root.path(), &namespace(&e)).unwrap();
        delivery::save_target(
            &mut store,
            &PaperlessTarget {
                schema_version: 1,
                url: format!("{}/", p.uri()),
                inbox_tag_id: 1,
                source_tag_id: Some(2),
                remember_token: false,
            },
        )
        .unwrap();
    }
    Mock::given(method("POST"))
        .and(path("/core/latest/tenants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{"tenant_id":"synthetic-tenant","company_id":0,"company_name":"Synthetic"}]),
        ))
        .mount(&e)
        .await;
    Mock::given(method("POST")).and(path("/core/latest/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token":EPOST_TOKEN,"refreshToken":"synthetic-refresh","expires_in":3600,"refresh_expires_in":86400,"token_type":"Bearer"})))
        .mount(&e).await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id":"synthetic-letter","letterType":"CLASSIC_LETTER"}])),
        )
        .mount(&e)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-letter/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("Content-Type", "application/pdf"),
        )
        .mount(&e)
        .await;
    delivery_mocks(&p, PDF).await;
    let mut c = command(&e, root.path());
    c.env("EPOST_USERNAME", "synthetic@example.invalid")
        .env("PAPERLESS_TOKEN", TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args([
            "--password-stdin",
            "sync",
            "--scope",
            "inbox",
            "--paperless",
            "--output",
        ])
        .arg(root.path());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-password\n")
        .unwrap();
    let data = result(&child.wait_with_output().unwrap(), 0)["data"].clone();
    assert_eq!(data["paperless"]["delivered"], 1);
    for req in p.received_requests().await.unwrap() {
        assert!(!String::from_utf8_lossy(&req.body).contains("synthetic-password"));
        assert!(!String::from_utf8_lossy(&req.body).contains(EPOST_TOKEN));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_input_is_explicit_and_remembered_host_keychain_is_blocked_for_mocks() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    save(&e, &p, root.path(), false);
    let out = command(&e, root.path())
        .args(["paperless", "upload", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        result(&out, 1)["error"]
            .as_str()
            .unwrap()
            .contains("PAPERLESS_TOKEN")
    );
    let out = command(&e, root.path())
        .env("PAPERLESS_TOKEN", TOKEN)
        .args([
            "paperless",
            "setup",
            "--remember",
            "--url",
            &p.uri(),
            "--output",
        ])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        result(&out, 1)["error"]
            .as_str()
            .unwrap()
            .contains("synthetic transports")
    );
    assert!(p.received_requests().await.unwrap().is_empty());
    assert!(e.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_url_and_inbox_source_tag_fail_without_uploading() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    let out = command(&e, root.path())
        .env("PAPERLESS_TOKEN", TOKEN)
        .args([
            "paperless",
            "setup",
            "--url",
            "https://fake-token@example.invalid/?secret=fake-token",
            "--output",
        ])
        .arg(root.path())
        .output()
        .unwrap();
    let raw = String::from_utf8_lossy(&out.stderr);
    assert!(!raw.contains("fake-token"));
    result(&out, 1);
    Mock::given(method("GET")).and(path("/api/tags/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count":2,"next":null,"results":[{"id":1,"name":"ePost Inbox","is_inbox_tag":true},{"id":2,"name":"ePost","is_inbox_tag":true}]})))
        .mount(&p).await;
    let out = command(&e, root.path())
        .env("PAPERLESS_TOKEN", TOKEN)
        .args(["paperless", "setup", "--url", &p.uri(), "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        result(&out, 1)["error"]
            .as_str()
            .unwrap()
            .contains("source tag")
    );
    assert_eq!(p.received_requests().await.unwrap().len(), 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_token_file_works_and_unsafe_files_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    save(&e, &p, root.path(), false);
    let token_file = root.path().join("runtime-token.txt");
    fs::write(&token_file, format!("{TOKEN}\n")).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    delivery_mocks(&p, PDF).await;
    let out = command(&e, root.path())
        .arg("--paperless-token-file")
        .arg(&token_file)
        .args(["paperless", "upload", "--wait-seconds", "0", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    result(&out, 0);
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o644)).unwrap();
    let out = command(&e, root.path())
        .arg("--paperless-token-file")
        .arg(&token_file)
        .args(["paperless", "upload", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    result(&out, 1);
    let link = root.path().join("symlink-token.txt");
    symlink(&token_file, &link).unwrap();
    let out = command(&e, root.path())
        .arg("--paperless-token-file")
        .arg(&link)
        .args(["paperless", "upload", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    result(&out, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_reconciliation_proves_bytes_and_logout_is_local() {
    let e = MockServer::start().await;
    let p = MockServer::start().await;
    let root = temp();
    save(&e, &p, root.path(), false);
    tags(&p).await;
    Mock::given(method("GET"))
        .and(path("/api/documents/42/download/"))
        .and(query_param("original", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PDF))
        .mount(&p)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/42/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":42,"tags":[1,2]})))
        .mount(&p)
        .await;
    let out = command(&e, root.path())
        .env("PAPERLESS_TOKEN", TOKEN)
        .args([
            "paperless",
            "reconcile",
            "synthetic-letter",
            "--document-id",
            "42",
            "--output",
        ])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(result(&out, 0)["data"]["reconciled"], true);
    let before = p.received_requests().await.unwrap().len();
    let out = command(&e, root.path())
        .args(["paperless", "logout", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    assert_eq!(result(&out, 0)["data"]["token_forgotten"], true);
    assert_eq!(p.received_requests().await.unwrap().len(), before);
    assert!(e.received_requests().await.unwrap().is_empty());
}
