//! Progress instrumentation must keep metadata checks and bounded retrieval
//! request budgets unchanged. All runs use synthetic localhost data only.

use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "synthetic-dashboard-cli-token";
const TENANT: &str = "synthetic-dashboard-cli-tenant";
const TITLE: &str = "Synthetic dashboard document title";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n";

fn command(server: &MockServer) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
    command
        .env_clear()
        .env("EPOST_BEARER_TOKEN", TOKEN)
        .stdin(Stdio::null())
        .args([
            "--json",
            "--non-interactive",
            "--mock-url",
            &server.uri(),
            "--timeout-seconds",
            "2",
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
        .prefix("synthetic-progress-dashboard-cli-")
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

fn report(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["status"], if code == 0 { "ok" } else { "partial" });
    assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
    value
}

fn private_progress(output: &Output, ids: &[&str]) -> String {
    let text = String::from_utf8(output.stderr.clone()).unwrap();
    for value in ids.iter().copied().chain([TOKEN, TENANT, TITLE]) {
        assert!(
            !text.contains(value),
            "progress exposed synthetic private data"
        );
    }
    assert!(
        !text.contains('\u{1b}'),
        "piped progress must not contain terminal escapes"
    );
    text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_progress_preserves_json_and_the_same_read_only_page_budget() {
    let server = MockServer::start().await;
    let root = scratch();
    let id = "synthetic-dashboard-metadata-letter";
    for (offset, letters) in [
        (
            0,
            json!([{"id":id,"letterTitle":TITLE,"readStatus":"UNREAD"}]),
        ),
        (1, json!([])),
    ] {
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters"))
            .and(query_param("limit", "1"))
            .and(query_param("offset", offset.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(letters))
            .expect(3)
            .mount(&server)
            .await;
    }
    let mut baseline = None;
    for (index, flags) in [vec![], vec!["--no-progress"], vec!["--progress"]]
        .into_iter()
        .enumerate()
    {
        let output = command(&server)
            .args(flags)
            .args([
                "inbox",
                "check",
                "--page-size",
                "1",
                "--max-pages",
                "3",
                "--output",
            ])
            .arg(root.path())
            .output()
            .unwrap();
        let value = report(&output, 0);
        assert_eq!(value["data"]["inbox_documents"], 1);
        assert_eq!(value["data"]["new_documents_count"], 1);
        assert_eq!(value["data"]["pagination_complete"], true);
        assert_eq!(value["data"]["mailbox_coverage_verified"], false);
        match &baseline {
            Some(expected) => assert_eq!(&value, expected),
            None => baseline = Some(value),
        }
        let progress = private_progress(&output, &[id]);
        if index < 2 {
            assert!(progress.is_empty());
        } else {
            assert!(progress.contains("Scanning inbox…"));
            assert!(progress.contains("Scanning page 2…"));
            assert!(progress.contains("Inventory finished: 1 documents found."));
            assert!(!progress.contains("Checking document"));
            assert!(!progress.contains("Sync finished"));
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), (index + 1) * 2);
        assert!(requests.iter().all(|request| {
            request.method.as_str() == "GET" && request.url.path() == "/epost/v2/letters"
        }));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_progress_reports_deferrals_without_spending_more_download_attempts() {
    let server = MockServer::start().await;
    let root = scratch();
    let ids = [
        "synthetic-dashboard-first-letter",
        "synthetic-dashboard-second-letter",
        "synthetic-dashboard-third-letter",
    ];
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(
            ids.iter()
                .map(|id| json!({"id":id,"letterTitle":TITLE}))
                .collect::<Vec<_>>()
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/epost/v2/letters/{}/content", ids[0])))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("content-type", "application/pdf"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let output = command(&server)
        .args(["--progress", "check", "--local", "--limit", "1", "--output"])
        .arg(root.path())
        .output()
        .unwrap();
    let value = report(&output, 2);
    assert_eq!(value["data"]["sync_exit_code"], 2);
    let documents = value["data"]["sync"]["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 3);
    assert_eq!(documents[0]["status"], "saved_pdf");
    assert_eq!(documents[1]["status"], "deferred_limit");
    assert_eq!(documents[2]["status"], "deferred_limit");
    let progress = private_progress(&output, &ids);
    assert!(progress.contains("Checking document 3 of 3…"));
    assert_eq!(
        progress.matches("Deferred by the download limit.").count(),
        2
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "limit must still permit just one content request"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/content"))
            .count(),
        1
    );
}
