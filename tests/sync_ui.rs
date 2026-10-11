use serde_json::{Value, json};
use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "synthetic-sync-ui-token-never-a-real-credential";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n";
const TITLE: &str = "Invented private document title";
const TENANT: &str = "invented-sync-ui-tenant";

fn command(server: &MockServer) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
    command
        .env_clear()
        .env("EPOST_BEARER_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .args([
            "--json",
            "--mock-url",
            &server.uri(),
            "--timeout-seconds",
            "5",
            "--retries",
            "0",
            "--tenant-kind",
            "private",
            "--tenant-id",
            TENANT,
            "--company-id",
            "0",
        ]);
    command
}

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("epost-cli-sync-ui-synthetic-")
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

fn report(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // Parsing the entire stream rejects any progress prefix, suffix or second
    // JSON value, preserving the command's script contract.
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["status"], if code == 0 { "ok" } else { "partial" });
    assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
    value
}

fn private_progress(stderr: &[u8], identifiers: &[&str]) -> String {
    let progress = String::from_utf8(stderr.to_vec()).unwrap();
    for private_value in identifiers.iter().copied().chain([TOKEN, TITLE, TENANT]) {
        assert!(
            !progress.contains(private_value),
            "progress exposed an identifier, title or credential"
        );
    }
    progress
}

async fn inbox(server: &MockServer, offset: u32, letters: Value, expected: u64) {
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", offset.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(letters))
        .expect(expected)
        .mount(server)
        .await;
}

async fn content(server: &MockServer, id: &str, body: &[u8], mime: &str, expected: u64) {
    Mock::given(method("GET"))
        .and(path(format!("/epost/v2/letters/{id}/content")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(body)
                .insert_header("content-type", mime),
        )
        .expect(expected)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_progress_keeps_json_stdout_and_reports_generic_sync_phases() {
    let server = MockServer::start().await;
    let temp = scratch();
    let id = "invented-progress-document";
    let folder = "invented-private-folder-id";
    inbox(&server, 0, json!([{ "id": id, "letterTitle": TITLE }]), 1).await;
    inbox(&server, 1, json!([]), 1).await;
    content(&server, id, PDF, "application/pdf", 1).await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "directoryId": folder, "directoryName": "Invented private folder title" }
        ])))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(&server)
        .await;
    let output = command(&server)
        .args([
            "--progress",
            "sync",
            "--scope",
            "both",
            "--page-size",
            "1",
            "--output",
        ])
        .arg(temp.path())
        .output()
        .unwrap();
    let value = report(&output, 0);
    assert_eq!(value["data"]["documents"][0]["status"], "saved_pdf");
    assert_eq!(value["data"]["inventories"].as_array().unwrap().len(), 3);
    let progress = private_progress(
        &output.stderr,
        &[id, folder, "Invented private folder title"],
    );
    for phase in [
        "Signing in…",
        "Scanning archive folders…",
        "Scanning inbox…",
        "Scanning archive root…",
        "Scanning archive folder…",
        "Scanning page 1…",
        "Scanning page 2…",
        "Inventory finished: 1 documents found.",
        "Checking document 1 of 1…",
        "Saved PDF.",
        "Sync finished: 1 PDF saved.",
    ] {
        assert!(progress.contains(phase), "missing progress phase {phase}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_progress_explains_preserved_content_manual_action_and_failed_retrieval() {
    let server = MockServer::start().await;
    let temp = scratch();
    let binary = "invented-binary-document";
    let missing = "invented-missing-document";
    let pending = "invented-pending-document";
    inbox(
        &server,
        0,
        json!([
            { "id": binary, "letterTitle": TITLE },
            { "id": missing, "letterTitle": TITLE },
            { "id": pending, "letterTitle": TITLE, "eLetterRegistered": true, "registeredStatus": "PENDING" }
        ]),
        1,
    )
    .await;
    content(
        &server,
        binary,
        b"synthetic binary bytes",
        "application/octet-stream",
        1,
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/epost/v2/letters/{missing}/content")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_string(format!("synthetic-private-error-body {TOKEN} {TITLE}")),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/epost/v2/letters/{missing}")))
        .respond_with(ResponseTemplate::new(404).set_body_string(TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    let output = command(&server)
        .args(["--progress", "sync", "--scope", "inbox", "--output"])
        .arg(temp.path())
        .output()
        .unwrap();
    let value = report(&output, 2);
    let documents = value["data"]["documents"].as_array().unwrap();
    assert_eq!(documents[0]["status"], "saved_binary");
    assert_eq!(documents[1]["status"], "failed");
    assert!(documents[1]["error"].as_str().unwrap().contains("404"));
    assert_eq!(
        documents[2]["status"],
        "awaiting_manual_acceptance_or_status_verification"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-private-error-body"));
    let progress = private_progress(&output.stderr, &[binary, missing, pending]);
    for reason in [
        "Saved binary content; review its format.",
        "Retrieval failed; see the report for details.",
        "Requires acceptance or status verification in ePost.",
        "Sync finished with items requiring attention:",
        "1 binary saved",
        "1 pending manual action",
        "1 failed",
        "Attention (1): document metadata and content returned HTTP 404; check the document in ePost.",
        "Attention (1): unsupported content preserved locally; review its format manually.",
    ] {
        assert!(
            progress.contains(reason),
            "missing partial-sync explanation {reason}"
        );
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
    assert!(
        requests
            .iter()
            .all(|request| !request.url.path().contains(pending))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_sync_progress_distinguishes_a_verified_file_from_a_download() {
    let server = MockServer::start().await;
    let temp = scratch();
    let id = "invented-reusable-document";
    inbox(&server, 0, json!([{ "id": id, "letterTitle": TITLE }]), 2).await;
    content(&server, id, PDF, "application/pdf", 1).await;
    let run = || {
        command(&server)
            .args(["--progress", "sync", "--scope", "inbox", "--output"])
            .arg(temp.path())
            .output()
            .unwrap()
    };
    let first = run();
    let second = run();
    assert_eq!(
        report(&first, 0)["data"]["documents"][0]["status"],
        "saved_pdf"
    );
    assert_eq!(
        report(&second, 0)["data"]["documents"][0]["status"],
        "already_saved"
    );
    assert!(private_progress(&first.stderr, &[id]).contains("Sync finished: 1 PDF saved."));
    let repeated = private_progress(&second.stderr, &[id]);
    assert!(repeated.contains("Already saved; local file verified."));
    assert!(repeated.contains("Sync finished: 1 already saved."));
    assert!(!repeated.contains("Saved PDF."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn piped_json_sync_is_quiet_by_default_and_with_no_progress() {
    let server = MockServer::start().await;
    inbox(&server, 0, json!([]), 2).await;
    for flags in [vec![], vec!["--no-progress"]] {
        let temp = scratch();
        let output = command(&server)
            .args(flags)
            .args(["sync", "--scope", "inbox", "--output"])
            .arg(temp.path())
            .output()
            .unwrap();
        report(&output, 0);
        assert!(output.stderr.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflicting_progress_flags_fail_before_network() {
    let server = MockServer::start().await;
    let temp = scratch();
    let output = command(&server)
        .args([
            "--progress",
            "--no-progress",
            "sync",
            "--scope",
            "inbox",
            "--output",
        ])
        .arg(temp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["status"], "error");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_progress_arrives_while_content_download_is_still_running() {
    let server = MockServer::start().await;
    let temp = scratch();
    let id = "invented-delayed-document";
    inbox(&server, 0, json!([{ "id": id, "letterTitle": TITLE }]), 1).await;
    Mock::given(method("GET"))
        .and(path(format!("/epost/v2/letters/{id}/content")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("content-type", "application/pdf")
                .set_delay(Duration::from_secs(3)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut child = command(&server)
        .args(["--progress", "sync", "--scope", "inbox", "--output"])
        .arg(temp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line.clone());
            lines.push(line);
        }
        lines.join("\n")
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = receiver
            .recv_timeout(remaining)
            .expect("no early document progress");
        if line.contains("Checking document 1 of 1…") {
            break;
        }
    }
    assert!(
        child.try_wait().unwrap().is_none(),
        "progress appeared only after sync finished"
    );
    let output = child.wait_with_output().unwrap();
    report(&output, 0);
    let progress = reader.join().unwrap();
    private_progress(progress.as_bytes(), &[id]);
    assert!(progress.contains("Saved PDF."));
}
