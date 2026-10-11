use epost_cli::{
    api::Environment,
    config::{self, Profile, SyncScope},
    models::{Mailbox, MailboxKind},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "synthetic-profile-cli-token";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n";

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("epost-cli-profile-synthetic-")
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap()
}

fn profile(folder: &Path, kind: MailboxKind, scope: SyncScope) -> Profile {
    Profile {
        mailbox: Mailbox {
            tenant_id: "synthetic-profile-tenant".into(),
            company_id: 42,
            kind,
        },
        environment: Environment::Production,
        output: folder.join("documents"),
        scope,
    }
}

fn base(server: &MockServer, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
    command
        .env_clear()
        .stdin(Stdio::null())
        .args([
            "--mock-url",
            &server.uri(),
            "--timeout-seconds",
            "3",
            "--retries",
            "0",
        ])
        .arg("--config")
        .arg(config);
    command
}

fn selected(server: &MockServer, config: &Path) -> Command {
    let mut command = base(server, config);
    command
        .env("EPOST_BEARER_TOKEN", TOKEN)
        .args(["--json", "--profile", "personal"]);
    command
}

fn report(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["status"], if code == 0 { "ok" } else { "partial" });
    assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
    value
}

fn failure(output: &Output) {
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let value: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["status"], "error");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_profile_sync_downloads_only_unsaved_content_on_repeat_runs() {
    let server = MockServer::start().await;
    let temp = scratch();
    let config = temp.path().join("config.json");
    config::save_profile(
        &config,
        "personal",
        &profile(temp.path(), MailboxKind::Private, SyncScope::Inbox),
    )
    .unwrap();
    let list_calls = Arc::new(AtomicUsize::new(0));
    let responder_calls = list_calls.clone();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("is-business-tenant", "false"))
        .respond_with(move |_: &Request| {
            let letters = if responder_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                json!([{ "id": "synthetic-already-seen" }])
            } else {
                json!([{ "id": "synthetic-already-seen" }, { "id": "synthetic-new" }])
            };
            ResponseTemplate::new(200).set_body_json(letters)
        })
        .expect(2)
        .mount(&server)
        .await;
    for id in ["synthetic-already-seen", "synthetic-new"] {
        Mock::given(method("GET"))
            .and(path(format!("/epost/v2/letters/{id}/content")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(PDF)
                    .insert_header("content-type", "application/pdf"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let first = report(&selected(&server, &config).arg("sync").output().unwrap(), 0);
    let second = report(&selected(&server, &config).arg("sync").output().unwrap(), 0);
    assert_eq!(first["data"]["scope"], "inbox");
    assert_eq!(first["data"]["documents"].as_array().unwrap().len(), 1);
    assert_eq!(second["data"]["documents"].as_array().unwrap().len(), 2);
    assert_eq!(second["data"]["documents"][0]["status"], "already_saved");
    assert_eq!(second["data"]["documents"][1]["status"], "saved_pdf");
    assert_eq!(second["data"]["mailbox_coverage_verified"], false);
    for record in second["data"]["documents"].as_array().unwrap() {
        let saved = Path::new(record["document"]["path"].as_str().unwrap());
        assert!(saved.starts_with(temp.path().join("documents")));
        assert_eq!(fs::read(saved).unwrap(), PDF);
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_sync_scope_and_output_can_be_explicitly_overridden() {
    let server = MockServer::start().await;
    let temp = scratch();
    let config = temp.path().join("config.json");
    let override_output = temp.path().join("override-output");
    config::save_profile(
        &config,
        "personal",
        &profile(temp.path(), MailboxKind::Business, SyncScope::Archive),
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("is-business-tenant", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let output = selected(&server, &config)
        .args(["sync", "--scope", "inbox", "--output"])
        .arg(&override_output)
        .output()
        .unwrap();
    let value = report(&output, 0);
    assert_eq!(value["data"]["scope"], "inbox");
    assert!(override_output.exists());
    assert!(!temp.path().join("documents").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        config::load_profile(&config, "personal").unwrap().scope,
        SyncScope::Archive
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_preserves_the_existing_structured_show_contract() {
    let server = MockServer::start().await;
    let temp = scratch();
    let config = temp.path().join("config.json");
    config::save_profile(
        &config,
        "personal",
        &profile(temp.path(), MailboxKind::Private, SyncScope::Inbox),
    )
    .unwrap();
    Mock::given(method("GET")).and(path("/epost/v2/letters/synthetic-show"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "synthetic-show", "letterType": "FUTURE_TYPE", "documentMessage": "invented message" })))
        .expect(1).mount(&server).await;
    let output = selected(&server, &config)
        .args(["show", "synthetic-show"])
        .output()
        .unwrap();
    let value = report(&output, 0);
    assert_eq!(value["data"]["id"], "synthetic-show");
    assert_eq!(value["data"]["letterType"], "FUTURE_TYPE");
    assert_eq!(value["data"]["documentMessage"], "invented message");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_download_defaults_to_its_saved_output_folder() {
    let server = MockServer::start().await;
    let temp = scratch();
    let config = temp.path().join("config.json");
    config::save_profile(
        &config,
        "personal",
        &profile(temp.path(), MailboxKind::Private, SyncScope::Inbox),
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-download"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"synthetic-download"})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/synthetic-download/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("content-type", "application/pdf"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let value = report(
        &selected(&server, &config)
            .args(["download", "synthetic-download"])
            .output()
            .unwrap(),
        0,
    );
    let saved = Path::new(value["data"]["document"]["path"].as_str().unwrap());
    assert!(saved.starts_with(temp.path().join("documents")));
    assert_eq!(fs::read(saved).unwrap(), PDF);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_invalid_or_conflicting_profiles_fail_before_network() {
    let server = MockServer::start().await;
    let temp = scratch();
    let config = temp.path().join("config.json");
    failure(&selected(&server, &config).arg("sync").output().unwrap());
    config::save_profile(
        &config,
        "personal",
        &profile(temp.path(), MailboxKind::Private, SyncScope::Inbox),
    )
    .unwrap();
    for flags in [
        vec!["--tenant-kind", "business"],
        vec!["--tenant-id", "synthetic-other"],
        vec!["--company-id", "8"],
        vec!["--environment", "sandbox"],
    ] {
        failure(
            &selected(&server, &config)
                .args(flags)
                .arg("sync")
                .output()
                .unwrap(),
        );
    }
    failure(
        &base(&server, &config)
            .env("EPOST_BEARER_TOKEN", TOKEN)
            .args(["--json", "--profile", "../invalid", "sync"])
            .output()
            .unwrap(),
    );
    fs::write(&config, b"{invalid synthetic config}").unwrap();
    failure(&selected(&server, &config).arg("sync").output().unwrap());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guided_commands_require_a_terminal_and_reject_script_options_before_network() {
    let server = MockServer::start().await;
    let temp = scratch();
    let config = temp.path().join("config.json");
    for action in ["setup", "browse"] {
        for flags in [vec![], vec!["--json"], vec!["--password-stdin"]] {
            let output = base(&server, &config)
                .args(flags)
                .arg(action)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(output.stdout.is_empty());
            assert!(!output.stderr.is_empty());
        }
        for key in ["EPOST_BEARER_TOKEN", "EPOST_API_KEY"] {
            let output = base(&server, &config)
                .env(key, TOKEN)
                .arg(action)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
        }
    }
    assert!(!config.exists());
    assert!(server.received_requests().await.unwrap().is_empty());
}
