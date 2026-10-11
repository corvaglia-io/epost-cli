use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const FAKE_TOKEN: &str = "synthetic-cli-token-never-a-real-credential";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n";

fn command(server: &MockServer) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
    command.env_clear().stdin(Stdio::null());
    command.args([
        "--json",
        "--mock-url",
        &server.uri(),
        "--timeout-seconds",
        "3",
        "--retries",
        "0",
    ]);
    command
}

fn selected_command(server: &MockServer) -> Command {
    let mut command = command(server);
    command.env("EPOST_BEARER_TOKEN", FAKE_TOKEN).args([
        "--tenant-kind",
        "private",
        "--tenant-id",
        "synthetic-tenant",
        "--company-id",
        "0",
    ]);
    command
}

fn successful_output(output: &Output, expected_code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(expected_code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "successful reports belong on stdout"
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["schema_version"], 1);
    assert_eq!(
        payload["status"],
        if expected_code == 0 { "ok" } else { "partial" }
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(FAKE_TOKEN));
    payload
}

fn failure_output(output: &Output, expected_code: i32) -> Value {
    assert_eq!(output.status.code(), Some(expected_code));
    assert!(output.stdout.is_empty());
    let diagnostic: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(diagnostic["schema_version"], 1);
    assert_eq!(diagnostic["status"], "error");
    assert_eq!(diagnostic["exit_code"], expected_code);
    assert!(!String::from_utf8_lossy(&output.stderr).contains(FAKE_TOKEN));
    diagnostic
}

fn temp_store() -> tempfile::TempDir {
    // Canonicalize the macOS /var and /tmp aliases that the store deliberately
    // rejects, while also supporting the Linux CI runner.
    let temp_root = std::env::temp_dir().canonicalize().unwrap();
    tempfile::Builder::new()
        .prefix("epost-cli-synthetic-")
        .tempdir_in(temp_root)
        .unwrap()
}

async fn inbox(server: &MockServer, letters: Value) {
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(letters))
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

fn assert_read_only(requests: &[Request]) {
    for request in requests {
        assert_eq!(request.method.as_str(), "GET");
        let path = request.url.path();
        assert!(
            path == "/epost/v2/letters"
                || path == "/epost/v2/archives/directories"
                || path == "/epost/v2/archives/letters"
                || path.starts_with("/epost/v2/letters/"),
            "unexpected path {path}"
        );
        assert!(
            ![
                "accept",
                "reject",
                "read-status",
                "restore",
                "upload",
                "delete"
            ]
            .iter()
            .any(|fragment| path.contains(fragment))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_mailbox_selection_is_required_before_any_network_call() {
    let server = MockServer::start().await;
    for selection in [
        vec![],
        vec!["--tenant-id", "synthetic-tenant", "--company-id", "0"],
        vec!["--tenant-kind", "private", "--company-id", "0"],
        vec![
            "--tenant-kind",
            "private",
            "--tenant-id",
            "synthetic-tenant",
        ],
    ] {
        let output = command(&server)
            .env("EPOST_BEARER_TOKEN", FAKE_TOKEN)
            .args(selection)
            .args(["inbox", "list"])
            .output()
            .unwrap();
        failure_output(&output, 1);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_child_environment_never_loads_host_credentials() {
    let server = MockServer::start().await;
    let output = command(&server)
        .args([
            "--tenant-kind",
            "private",
            "--tenant-id",
            "synthetic-tenant",
            "--company-id",
            "0",
            "inbox",
            "list",
        ])
        .output()
        .unwrap();
    failure_output(&output, 1);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_are_structured_and_do_not_echo_argument_values() {
    let server = MockServer::start().await;
    for arguments in [
        vec!["archive", "list"],
        vec![
            "archive",
            "list",
            "--root",
            "--directory-id",
            "invented-folder",
        ],
        vec!["inbox", "list", "--page-size", "0"],
        vec![
            "inbox",
            "list",
            "--password",
            "fake-password-never-print-this",
        ],
    ] {
        let output = selected_command(&server).args(arguments).output().unwrap();
        failure_output(&output, 2);
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("fake-password-never-print-this")
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn help_and_version_remain_successful_without_credentials_or_network() {
    for option in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_epost-cli"))
            .env_clear()
            .stdin(Stdio::null())
            .args(["--json", option])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_discovery_is_structured_and_does_not_require_mailbox_selection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/tenants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"tenant_id":"synthetic-one","company_id":11},
            {"tenant_id":"synthetic-two","company_id":22}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let output = command(&server)
        .env("EPOST_BEARER_TOKEN", FAKE_TOKEN)
        .arg("tenants")
        .output()
        .unwrap();
    let payload = successful_output(&output, 0);
    assert_eq!(payload["data"].as_array().unwrap().len(), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_stdin_uses_only_the_explicit_synthetic_input() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/tenants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let mut child = command(&server)
        .env("EPOST_USERNAME", "invented+user@example.invalid")
        .args(["--password-stdin", "tenants"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"fake&+password\n")
        .unwrap();
    successful_output(&child.wait_with_output().unwrap(), 0);
    let request = server.received_requests().await.unwrap().remove(0);
    let fields: Vec<_> = reqwest::Url::parse(&format!(
        "http://127.0.0.1/?{}",
        String::from_utf8(request.body).unwrap()
    ))
    .unwrap()
    .query_pairs()
    .into_owned()
    .collect();
    assert!(fields.contains(&("password".into(), "fake&+password".into())));
    assert!(fields.contains(&("username".into(), "invented+user@example.invalid".into())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_preserves_unknown_metadata_without_polluting_stderr() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-letter"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"synthetic-letter", "letterType":"FUTURE_TYPE", "documentMessage":"invented message"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["show", "synthetic-letter"])
        .output()
        .unwrap();
    let payload = successful_output(&output, 0);
    assert_eq!(payload["data"]["letterType"], "FUTURE_TYPE");
    assert_eq!(payload["data"]["documentMessage"], "invented message");
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_root_and_opaque_directory_are_distinct_selectors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(&server)
        .await;
    for selector in [
        vec!["--root"],
        vec!["--directory-id", "invented /?+& folder"],
    ] {
        let output = selected_command(&server)
            .args(["archive", "list"])
            .args(selector)
            .output()
            .unwrap();
        successful_output(&output, 0);
    }
    let requests = server.received_requests().await.unwrap();
    assert!(
        !requests[0]
            .url
            .query_pairs()
            .any(|(name, _)| name == "directory-id")
    );
    assert!(
        requests[1]
            .url
            .query_pairs()
            .any(|(name, value)| name == "directory-id" && value == "invented /?+& folder")
    );
    assert_read_only(&requests);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forbidden_and_unauthorized_responses_have_redacted_auth_exit_codes() {
    for status in [401, 403] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string(format!("fake-private-body {FAKE_TOKEN}")),
            )
            .expect(1)
            .mount(&server)
            .await;
        let output = selected_command(&server)
            .args(["inbox", "list"])
            .output()
            .unwrap();
        failure_output(&output, 3);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fake-private-body"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_json_errors_do_not_echo_response_content() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("invalid fake-body {FAKE_TOKEN}")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["inbox", "list"])
        .output()
        .unwrap();
    failure_output(&output, 1);
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fake-body"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_preserves_json_bytes_and_reports_partial_retrieval() {
    let server = MockServer::start().await;
    let store = temp_store();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"synthetic-json", "fileName":"../../misleading.pdf"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bytes = b"{\"invented\":\"short message\"}\n";
    content(&server, "synthetic-json", bytes, "application/json", 1).await;
    let output = selected_command(&server)
        .args(["download", "synthetic-json", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 2);
    let saved = Path::new(payload["data"]["document"]["path"].as_str().unwrap());
    assert!(saved.starts_with(store.path()));
    assert_eq!(saved.extension().unwrap(), "json");
    assert_eq!(fs::read(saved).unwrap(), bytes);
    assert_eq!(payload["data"]["document"]["media_kind"], "json");
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_registered_mail_is_reported_without_fetching_or_accepting_content() {
    let server = MockServer::start().await;
    let store = temp_store();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-pending"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"synthetic-pending", "eLetterRegistered":true, "registeredStatus":"PENDING"})))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["download", "synthetic-pending", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 2);
    assert!(
        payload["data"]["status"]
            .as_str()
            .unwrap()
            .contains("manual_acceptance")
    );
    assert!(payload["data"]["document"].is_null());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_content_does_not_hide_a_current_pending_registered_status() {
    let server = MockServer::start().await;
    let store = temp_store();
    let calls = Arc::new(AtomicUsize::new(0));
    let response_calls = calls.clone();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-status-change"))
        .respond_with(move |_: &Request| {
            let status = if response_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "ACCEPTED"
            } else {
                "PENDING"
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "id":"synthetic-status-change", "eLetterRegistered":true, "registeredStatus":status
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    content(
        &server,
        "synthetic-status-change",
        PDF,
        "application/pdf",
        1,
    )
    .await;
    let run = || {
        selected_command(&server)
            .args(["download", "synthetic-status-change", "--output"])
            .arg(store.path())
            .output()
            .unwrap()
    };
    let first = successful_output(&run(), 0);
    let saved_path = Path::new(first["data"]["document"]["path"].as_str().unwrap());
    let second = successful_output(&run(), 2);
    assert!(
        second["data"]["status"]
            .as_str()
            .unwrap()
            .contains("manual_acceptance")
    );
    assert_eq!(fs::read(saved_path).unwrap(), PDF);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn both_scope_reuses_duplicate_content_tracks_sources_and_detects_corruption() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(&server, json!([{"id":"synthetic-shared"}])).await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{ "directoryId":"invented-folder", "parentDirectoryIds":["parent-one","parent-two"]}])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"id":"synthetic-shared"}])))
        .mount(&server)
        .await;
    content(&server, "synthetic-shared", PDF, "application/pdf", 1).await;
    let run = || {
        selected_command(&server)
            .args(["sync", "--scope", "both", "--output"])
            .arg(store.path())
            .output()
            .unwrap()
    };
    let first = successful_output(&run(), 0);
    assert_eq!(first["data"]["mailbox_coverage_verified"], false);
    let documents = first["data"]["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 3);
    let document_path = Path::new(documents[0]["document"]["path"].as_str().unwrap());
    assert!(
        documents
            .iter()
            .all(|document| document["document"]["path"] == documents[0]["document"]["path"])
    );
    assert_eq!(fs::read(document_path).unwrap(), PDF);
    let manifest_path = document_path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let entries = manifest["entries"].as_object().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries.values().next().unwrap()["sources"],
        json!(["archive:directory:invented-folder", "archive:root", "inbox"])
    );
    let second = successful_output(&run(), 0);
    assert!(
        second["data"]["documents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["status"] == "already_saved")
    );
    fs::write(document_path, b"synthetic corrupt bytes").unwrap();
    let before = server.received_requests().await.unwrap().len();
    failure_output(&run(), 4);
    assert_eq!(server.received_requests().await.unwrap().len(), before);
    assert_eq!(fs::read(document_path).unwrap(), b"synthetic corrupt bytes");
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unselectable_branded_folder_reports_partial_without_aliasing_it_to_root() {
    let server = MockServer::start().await;
    let store = temp_store();
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{ "directoryId":"", "numberOfDocuments":2, "directoryName":"Invented brand"}]),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["sync", "--scope", "archive", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 2);
    assert!(!payload["data"]["issues"].as_array().unwrap().is_empty());
    assert_eq!(payload["data"]["inventories"].as_array().unwrap().len(), 1);
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn next_page_failure_retains_and_downloads_the_first_page_with_partial_status() {
    let server = MockServer::start().await;
    let store = temp_store();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id":"synthetic-one"},{"id":"synthetic-two"}])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(500).set_body_string("fake-private-response"))
        .expect(1)
        .mount(&server)
        .await;
    for id in ["synthetic-one", "synthetic-two"] {
        content(&server, id, PDF, "application/pdf", 1).await;
    }
    let output = selected_command(&server)
        .args([
            "sync",
            "--scope",
            "inbox",
            "--page-size",
            "2",
            "--max-pages",
            "3",
            "--output",
        ])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 2);
    assert_eq!(payload["data"]["pagination_complete"], false);
    assert_eq!(payload["data"]["documents"].as_array().unwrap().len(), 2);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fake-private-response"));
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_document_failure_is_reported_and_later_documents_can_succeed() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(
        &server,
        json!([{"id":"synthetic-failure"},{"id":"synthetic-good"}]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-failure/content"))
        .respond_with(ResponseTemplate::new(500).set_body_string("fake-private-content"))
        .expect(1)
        .mount(&server)
        .await;
    content(&server, "synthetic-good", PDF, "application/pdf", 1).await;
    let output = selected_command(&server)
        .args(["sync", "--scope", "inbox", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 2);
    assert_eq!(payload["data"]["documents"][0]["status"], "failed");
    assert_eq!(payload["data"]["documents"][1]["status"], "saved_pdf");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fake-private-content"));
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_loss_during_pagination_retains_metadata_with_exit_three() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id":"synthetic-one"},{"id":"synthetic-two"}])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(403).set_body_string(FAKE_TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args([
            "inbox",
            "list",
            "--all",
            "--page-size",
            "2",
            "--max-pages",
            "3",
        ])
        .output()
        .unwrap();
    let payload = successful_output(&output, 3);
    assert_eq!(payload["data"]["letters"].as_array().unwrap().len(), 2);
    assert_eq!(payload["data"]["pagination_complete"], false);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_stops_network_activity_after_authentication_loss_in_inventory() {
    let server = MockServer::start().await;
    let store = temp_store();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id":"synthetic-one"},{"id":"synthetic-two"}])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(401).set_body_string(FAKE_TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args([
            "sync",
            "--scope",
            "inbox",
            "--page-size",
            "2",
            "--max-pages",
            "3",
            "--output",
        ])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 3);
    assert_eq!(payload["data"]["pagination_complete"], false);
    let documents = payload["data"]["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 2);
    assert!(documents.iter().all(|entry| entry["document"].is_null()));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_folder_authentication_failure_is_reported_before_inventory_calls() {
    let server = MockServer::start().await;
    let store = temp_store();
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(401).set_body_string(FAKE_TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["sync", "--scope", "archive", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 3);
    assert_eq!(payload["data"]["pagination_complete"], false);
    assert!(!payload["data"]["issues"].as_array().unwrap().is_empty());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn non_loopback_test_transport_is_rejected_without_contacting_a_provider() {
    let output = Command::new(env!("CARGO_BIN_EXE_epost-cli"))
        .env_clear()
        .stdin(Stdio::null())
        .env("EPOST_BEARER_TOKEN", FAKE_TOKEN)
        .args([
            "--json",
            "--mock-url",
            "http://192.0.2.1",
            "--tenant-kind",
            "private",
            "--tenant-id",
            "synthetic-tenant",
            "--company-id",
            "0",
            "inbox",
            "list",
        ])
        .output()
        .unwrap();
    failure_output(&output, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_404_is_diagnosed_with_one_canonical_metadata_check() {
    let cases = [
        (
            200,
            json!({"id":"invented-unavailable", "letterType":"CLASSIC_LETTER", "fileName":"synthetic-document.pdf", "letterContentReference":"https://untrusted.invalid/must-not-follow"}),
            "content_unavailable_metadata_available",
            "failed",
        ),
        (
            200,
            json!({"id":"invented-unavailable", "letterType":"SIMPLE_SHORT_MESSAGE", "documentMessage":"invented message", "fileName":"invented.pdf", "letterContentReference":"https://untrusted.invalid/must-not-follow"}),
            "content_unavailable_metadata_available",
            "failed",
        ),
        (
            404,
            json!({"message": "fake-private-body"}),
            "letter_unavailable",
            "failed",
        ),
        (
            500,
            json!({"message": "fake-private-body"}),
            "metadata_check_failed",
            "failed",
        ),
        (
            200,
            json!({"id":"invented-wrong-id"}),
            "metadata_id_mismatch",
            "failed",
        ),
        (
            200,
            json!({"letterType":"SMART_LETTER"}),
            "metadata_id_mismatch",
            "failed",
        ),
        (
            200,
            json!({"id":"invented-unavailable", "registeredStatus":"PENDING", "eLetterRegistered":true}),
            "content_unavailable_metadata_available",
            "awaiting_manual_acceptance_or_status_verification",
        ),
    ];
    for (metadata_status, metadata, reason, status) in cases {
        let server = MockServer::start().await;
        let store = temp_store();
        inbox(
            &server,
            json!([{"id":"invented-unavailable"}, {"id":"invented-good"}]),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters/invented-unavailable/content"))
            .respond_with(ResponseTemplate::new(404).set_body_string(FAKE_TOKEN))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters/invented-unavailable"))
            .respond_with(ResponseTemplate::new(metadata_status).set_body_json(metadata))
            .expect(1)
            .mount(&server)
            .await;
        content(&server, "invented-good", PDF, "application/pdf", 1).await;
        let output = selected_command(&server)
            .args(["sync", "--scope", "inbox", "--output"])
            .arg(store.path())
            .output()
            .unwrap();
        let payload = successful_output(&output, 2);
        let failed = &payload["data"]["documents"][0];
        assert_eq!(failed["status"], status);
        assert!(failed["document"].is_null());
        assert_eq!(failed["diagnostic"]["reason_code"], reason);
        assert_eq!(failed["diagnostic"]["content_http_status"], 404);
        assert_eq!(
            failed["diagnostic"]["metadata_http_status"],
            metadata_status
        );
        let manual_recovery =
            reason == "content_unavailable_metadata_available" && status == "failed";
        assert_eq!(
            failed["diagnostic"]["recovery_action"].as_str(),
            manual_recovery.then_some("check_or_download_in_epost")
        );
        if manual_recovery {
            assert_eq!(
                failed["error"],
                "public API cannot download this item (HTTP 404), although its metadata is accessible; check or download it manually in the ePost app or website"
            );
        } else {
            assert_eq!(failed["error"], "provider returned HTTP 404");
        }
        assert_eq!(payload["data"]["documents"][1]["status"], "saved_pdf");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("fake-private-body"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("untrusted.invalid"));
        if failed["diagnostic"]["letter_type"] == "SIMPLE_SHORT_MESSAGE" {
            assert_eq!(failed["diagnostic"]["has_message"], true);
            assert_eq!(failed["diagnostic"]["has_content_reference"], true);
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        assert_read_only(&requests);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn content_404_metadata_authentication_failure_stops_further_retrieval() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(
        &server,
        json!([{"id":"invented-unavailable"}, {"id":"invented-deferred"}]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-unavailable/content"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-unavailable"))
        .respond_with(ResponseTemplate::new(403).set_body_string(FAKE_TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["sync", "--scope", "inbox", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 3);
    assert_eq!(payload["data"]["documents"].as_array().unwrap().len(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_404_metadata_is_redacted_and_cannot_hide_future_content() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(&server, json!([{"id":"invented-later-content"}])).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-later-content/content"))
        .respond_with(move |_: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(404)
            } else {
                ResponseTemplate::new(200)
                    .set_body_bytes(PDF)
                    .insert_header("content-type", "application/pdf")
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-later-content"))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("malformed {FAKE_TOKEN}")))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["sync", "--scope", "inbox", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 2);
    assert_eq!(
        payload["data"]["documents"][0]["diagnostic"]["reason_code"],
        "metadata_check_failed"
    );
    assert!(payload["data"]["documents"][0]["document"].is_null());
    let output = selected_command(&server)
        .args(["sync", "--scope", "inbox", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 0);
    assert_eq!(payload["data"]["documents"][0]["status"], "saved_pdf");
    assert_eq!(
        payload["data"]["documents"][0]["document"]["format_hint"],
        "pdf"
    );
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unselectable_archive_observations_only_skip_explicit_empty_leaf_folders() {
    for (count, children, expected_exit, selection) in [
        (json!(0), json!(false), 0, "unselectable_empty"),
        (json!(0), json!(true), 2, "unselectable"),
        (json!(0), Value::Null, 2, "unselectable"),
        (json!(1), json!(false), 2, "unselectable"),
        (json!(-1), json!(false), 2, "unselectable"),
        (Value::Null, json!(false), 2, "unselectable"),
    ] {
        let server = MockServer::start().await;
        let store = temp_store();
        let mut observation =
            json!({"directoryId":"", "directoryName":"invented empty branded folder"});
        if !count.is_null() {
            observation["numberOfDocuments"] = count;
        }
        if !children.is_null() {
            observation["hasSubDirectories"] = children;
        }
        Mock::given(method("GET"))
            .and(path("/epost/v2/archives/directories"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                observation, {"directoryId":"invented-selectable", "numberOfDocuments":0}
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
        let output = selected_command(&server)
            .args(["sync", "--scope", "archive", "--output"])
            .arg(store.path())
            .output()
            .unwrap();
        let payload = successful_output(&output, expected_exit);
        assert_eq!(payload["data"]["mailbox_coverage_verified"], false);
        assert_eq!(payload["data"]["pagination_complete"], expected_exit == 0);
        assert_eq!(
            payload["data"]["archive_directories"][0]["selection"],
            selection
        );
        assert_eq!(
            payload["data"]["archive_directories"][0]["directory"]["name"],
            "invented empty branded folder"
        );
        let requests = server.received_requests().await.unwrap();
        // Even zero-count selectable folders are visited; empty IDs never become
        // an extra root request and no undocumented search/folder selector is used.
        assert_eq!(
            requests
                .iter()
                .filter(|r| r
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == "directory-id" && v == "invented-selectable"))
                .count(),
            1
        );
        assert_read_only(&requests);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_binary_retains_format_hint_and_partial_status() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(&server, json!([{"id":"invented-image"}])).await;
    content(
        &server,
        "invented-image",
        b"\x89PNG\r\n\x1a\nsynthetic-not-a-valid-image",
        "application/octet-stream",
        1,
    )
    .await;
    for status in ["saved_binary", "already_saved"] {
        let output = selected_command(&server)
            .args(["--progress", "sync", "--scope", "inbox", "--output"])
            .arg(store.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
        let record = &payload["data"]["documents"][0];
        assert_eq!(record["status"], status);
        assert_eq!(record["document"]["media_kind"], "binary");
        assert_eq!(record["document"]["format_hint"], "png");
        let progress = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            progress
                .matches("unsupported content preserved locally")
                .count(),
            1
        );
        assert!(progress.contains("PNG"));
        assert!(!progress.contains("invented-image"));
        assert!(!progress.contains(FAKE_TOKEN));
    }
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_download_content_404_keeps_partial_status_and_diagnostics() {
    let server = MockServer::start().await;
    let store = temp_store();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-direct"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"invented-direct", "letterType":"SMART_LETTER"
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-direct/content"))
        .respond_with(ResponseTemplate::new(404).set_body_string(FAKE_TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["download", "invented-direct", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 2);
    assert_eq!(payload["data"]["status"], "failed");
    assert_eq!(
        payload["data"]["diagnostic"]["reason_code"],
        "content_unavailable_metadata_available"
    );
    assert_eq!(payload["data"]["diagnostic"]["letter_type"], "SMART_LETTER");
    assert!(payload["data"]["document"].is_null());
    assert_eq!(
        payload["data"]["diagnostic"]["recovery_action"],
        "check_or_download_in_epost"
    );
    assert!(
        payload["data"]["error"]
            .as_str()
            .unwrap()
            .contains("public API cannot download this item")
    );
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accessible_metadata_does_not_cache_a_failed_content_download_as_success() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(&server, json!([{"id":"invented-retryable"}])).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-retryable/content"))
        .respond_with(move |_: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(404).set_body_string(FAKE_TOKEN)
            } else {
                ResponseTemplate::new(200)
                    .set_body_bytes(PDF)
                    .insert_header("content-type", "application/pdf")
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-retryable"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"invented-retryable", "letterType":"CLASSIC_LETTER", "fileName":"synthetic-document.pdf"
        }))).expect(1).mount(&server).await;
    for (exit_code, status) in [(2, "failed"), (0, "saved_pdf")] {
        let output = selected_command(&server)
            .args(["sync", "--scope", "inbox", "--output"])
            .arg(store.path())
            .output()
            .unwrap();
        let payload = successful_output(&output, exit_code);
        let document = &payload["data"]["documents"][0];
        assert_eq!(document["status"], status);
        if status == "failed" {
            assert_eq!(
                document["diagnostic"]["recovery_action"],
                "check_or_download_in_epost"
            );
            assert!(document["document"].is_null());
        } else {
            assert!(document["diagnostic"].is_null());
            assert_eq!(document["document"]["media_kind"], "pdf");
        }
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5);
    assert_read_only(&requests);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quick_inbox_check_uses_only_unread_count_and_never_downloads() {
    for unread in [0, 5] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters/inbox/count"))
            .respond_with(ResponseTemplate::new(200).set_body_json(unread))
            .expect(1)
            .mount(&server)
            .await;
        let output = selected_command(&server)
            .args(["inbox", "check", "--unread-only"])
            .output()
            .unwrap();
        let payload = successful_output(&output, 0);
        assert_eq!(payload["data"]["unread_count"], unread);
        assert_eq!(payload["data"]["has_unread"], unread > 0);
        assert!(payload["data"]["has_new"].is_null());
        assert!(payload["data"]["pagination_complete"].is_null());
        assert_eq!(payload["data"]["sync_performed"], false);
        assert_eq!(payload["data"]["mailbox_coverage_verified"], false);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_read_only(&server.received_requests().await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_check_reports_saved_unread_separately_from_unsaved_read_and_can_sync() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(
        &server,
        json!([
            {"id":"invented-unread", "readStatus":"UNREAD"},
            {"id":"invented-read", "readStatus":"READ"}
        ]),
    )
    .await;
    content(&server, "invented-unread", PDF, "application/pdf", 1).await;
    content(&server, "invented-read", PDF, "application/pdf", 1).await;
    // Seed only the first item using a wholly synthetic document and namespace.
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/invented-unread"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"invented-unread"})))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args(["download", "invented-unread", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    successful_output(&output, 0);
    let output = selected_command(&server)
        .args(["inbox", "check", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 0);
    assert_eq!(payload["data"]["unread_count"], 1);
    assert_eq!(payload["data"]["new_documents_count"], 1);
    assert_eq!(payload["data"]["already_saved_count"], 1);
    assert_eq!(payload["data"]["has_new"], true);
    assert_eq!(payload["data"]["has_unread"], true);
    assert_eq!(payload["data"]["sync_performed"], false);
    let output = selected_command(&server)
        .args(["inbox", "check", "--sync-if-needed", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 0);
    assert_eq!(payload["data"]["sync_performed"], true);
    assert_eq!(payload["data"]["sync"]["scope"], "inbox");
    assert_eq!(
        payload["data"]["sync"]["documents"][0]["status"],
        "already_saved"
    );
    assert_eq!(
        payload["data"]["sync"]["documents"][1]["status"],
        "saved_pdf"
    );
    let output = selected_command(&server)
        .args(["inbox", "check", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 0);
    assert_eq!(payload["data"]["has_new"], false);
    assert_eq!(payload["data"]["has_unread"], true);
    assert_eq!(payload["data"]["sync_needed"], true);
    assert_read_only(&server.received_requests().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_check_never_claims_quiet_after_unknown_status_or_incomplete_inventory() {
    for letters in [
        json!([{"id":"invented-new", "readStatus":FAKE_TOKEN}]),
        json!([{"readStatus":"READ"}]),
    ] {
        let server = MockServer::start().await;
        let store = temp_store();
        inbox(&server, letters).await;
        let output = selected_command(&server)
            .args(["inbox", "check", "--output"])
            .arg(store.path())
            .output()
            .unwrap();
        let payload = successful_output(&output, 2);
        assert!(payload["data"]["has_unread"].is_null());
        assert_ne!(payload["data"]["sync_needed"], false);
        assert_eq!(payload["data"]["sync_performed"], false);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_read_only(&server.received_requests().await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_quick_check_syncs_positive_count_but_skips_zero_without_content_calls() {
    for unread in [0, 1] {
        let server = MockServer::start().await;
        let store = temp_store();
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters/inbox/count"))
            .respond_with(ResponseTemplate::new(200).set_body_json(unread))
            .expect(1)
            .mount(&server)
            .await;
        if unread > 0 {
            inbox(
                &server,
                json!([{"id":"invented-quick-sync", "readStatus":"UNREAD"}]),
            )
            .await;
            content(&server, "invented-quick-sync", PDF, "application/pdf", 1).await;
        }
        let output = selected_command(&server)
            .args([
                "inbox",
                "check",
                "--unread-only",
                "--sync-if-needed",
                "--output",
            ])
            .arg(store.path())
            .output()
            .unwrap();
        let payload = successful_output(&output, 0);
        assert_eq!(payload["data"]["sync_performed"], unread > 0);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            if unread > 0 { 3 } else { 1 }
        );
        assert_read_only(&server.received_requests().await.unwrap());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_full_check_skips_read_empty_inbox_and_stops_on_late_auth_failure() {
    let server = MockServer::start().await;
    let store = temp_store();
    inbox(&server, json!([])).await;
    let output = selected_command(&server)
        .args(["inbox", "check", "--sync-if-needed", "--output"])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 0);
    assert_eq!(payload["data"]["sync_needed"], false);
    assert_eq!(payload["data"]["sync_performed"], false);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id":"invented-new", "readStatus":"UNREAD"}])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "1"))
        .respond_with(ResponseTemplate::new(403).set_body_string(FAKE_TOKEN))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected_command(&server)
        .args([
            "inbox",
            "check",
            "--sync-if-needed",
            "--page-size",
            "1",
            "--output",
        ])
        .arg(store.path())
        .output()
        .unwrap();
    let payload = successful_output(&output, 3);
    assert_eq!(payload["data"]["sync_needed"], true);
    assert_eq!(payload["data"]["sync_performed"], false);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_read_only(&server.received_requests().await.unwrap());
}
