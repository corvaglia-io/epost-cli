//! High-level CLI integration uses synthetic profiles, temporary stores, fake
//! tokens and literal localhost servers. Environment inheritance is disabled.
use epost_cli::api::Environment;
use epost_cli::config::{self, Login, LoginMethod, Profile, SyncScope};
use epost_cli::delivery::{self, DeliveryOptions, NamedDeliveryOptions, PaperlessTarget};
use epost_cli::destinations::{self, CredentialBinding, Destination, DestinationKind};
use epost_cli::models::{Mailbox, MailboxKind};
use epost_cli::paperless::{PaperlessClient, PaperlessOptions};
use epost_cli::secret::Secret;
use epost_cli::storage::Store;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BEARER: &str = "synthetic-modern-source-bearer";
const DESTINATION_TOKEN: &str = "synthetic-modern-paperless-token";
const PDF: &[u8] = b"%PDF-1.4\nsynthetic modern CLI PDF\n%%EOF\n";
const LETTER: &str = "synthetic-modern-letter";
const TASK: &str = "11111111-2222-4333-8444-555555555555";

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    config: PathBuf,
    root: PathBuf,
    namespace: String,
}

impl Fixture {
    fn new(source: &MockServer) -> Self {
        Self::with_login(source, LoginMethod::Prompt)
    }
    fn with_login(source: &MockServer, method: LoginMethod) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let config = base.join("config.json");
        let root = base.join("source");
        let mailbox = Mailbox {
            tenant_id: "synthetic-modern-primary".into(),
            company_id: 42,
            kind: MailboxKind::Private,
        };
        let namespace = serde_json::to_string(&(
            source.uri(),
            &mailbox.tenant_id,
            mailbox.company_id,
            mailbox.kind,
        ))
        .unwrap();
        for (name, mailbox, output) in [
            ("primary", mailbox, root.clone()),
            (
                "secondary",
                Mailbox {
                    tenant_id: "synthetic-modern-secondary".into(),
                    company_id: 43,
                    kind: MailboxKind::Business,
                },
                base.join("unselected-source"),
            ),
        ] {
            config::save_profile(
                &config,
                name,
                &Profile {
                    mailbox,
                    environment: Environment::Production,
                    output,
                    scope: SyncScope::Both,
                    login: Some(Login {
                        username: "synthetic-modern@example.invalid".into(),
                        method,
                    }),
                },
            )
            .unwrap();
        }
        config::set_default_profile(&config, "primary").unwrap();
        Self {
            _temp: temp,
            base,
            config,
            root,
            namespace,
        }
    }
    fn store(&self) -> Store {
        Store::open(&self.root, &self.namespace).unwrap()
    }
    fn command(&self, source: &MockServer) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
        command
            .env_clear()
            .stdin(Stdio::null())
            .args([
                "--json",
                "--no-progress",
                "--non-interactive",
                "--timeout-seconds",
                "2",
                "--retries",
                "0",
                "--mock-url",
                &source.uri(),
            ])
            .arg("--config")
            .arg(&self.config);
        command
    }
    fn authenticated(&self, source: &MockServer) -> Command {
        let mut command = self.command(source);
        command.env("EPOST_BEARER_TOKEN", BEARER);
        command
    }
    fn folder(&self, store: &mut Store, name: &str) -> Destination {
        let destination = Destination {
            name: name.into(),
            credential_binding: CredentialBinding::Named,
            kind: DestinationKind::Folder {
                path: self.base.join(name),
            },
        };
        destinations::save(store, &destination).unwrap();
        destination
    }
    fn paperless(&self, store: &mut Store, name: &str, server: &MockServer) -> Destination {
        let destination = Destination {
            name: name.into(),
            credential_binding: CredentialBinding::Named,
            kind: DestinationKind::Paperless(
                PaperlessTarget {
                    schema_version: 1,
                    url: format!("{}/", server.uri()),
                    inbox_tag_id: 11,
                    source_tag_id: Some(12),
                    remember_token: false,
                }
                .into(),
            ),
        };
        destinations::save(store, &destination).unwrap();
        destination
    }
    fn folder_store(&self, source: &Store, destination: &Destination) -> Store {
        let DestinationKind::Folder { path } = &destination.kind else {
            unreachable!()
        };
        Store::open(
            path,
            &format!(
                "epost-folder-v1:{}:{}",
                source.identity_hash(),
                destinations::destination_identity(source, destination)
            ),
        )
        .unwrap()
    }
}

fn report(output: Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains(BEARER));
    assert!(!text.contains(DESTINATION_TOKEN));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["status"], if code == 0 { "ok" } else { "partial" });
    value["data"].clone()
}

fn named_token(command: &mut Command, name: &str) {
    command.env(
        format!(
            "EPOST_DESTINATION_{}_TOKEN",
            hex::encode(name.as_bytes()).to_ascii_uppercase()
        ),
        DESTINATION_TOKEN,
    );
}

fn destination<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap()
}

async fn inbox(source: &MockServer, letters: Value) {
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(letters))
        .mount(source)
        .await;
}
async fn content(source: &MockServer, id: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/epost/v2/letters/{id}/content")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("content-type", "application/pdf"),
        )
        .mount(source)
        .await;
}
async fn tags(server: &MockServer) {
    for (id, inbox) in [(11, true), (12, false)] {
        Mock::given(method("GET"))
            .and(path(format!("/api/tags/{id}/")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"id":id,"name":format!("Synthetic tag {id}"),"is_inbox_tag":inbox}),
            ))
            .mount(server)
            .await;
    }
}
async fn successful_remote_document(server: &MockServer) {
    tags(server).await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{"task_id":TASK,"status":"SUCCESS","related_document_ids":[41]}]),
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/41/download/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PDF))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/41/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":41,"tags":[11,12]})))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_uses_default_profile_and_every_configured_destination_then_reuses_confirmed_delivery()
 {
    let source = MockServer::start().await;
    let remote = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut store = fixture.store();
    let first = fixture.folder(&mut store, "first");
    let second = fixture.folder(&mut store, "second");
    fixture.paperless(&mut store, "remote", &remote);
    drop(store);
    inbox(&source,json!([{"id":LETTER,"letterTitle":"Synthetic modern title","receivedDateTime":"2026-10-12T10:11:12Z","documentTypes":["invoice"]}])).await;
    content(&source, LETTER).await;
    successful_remote_document(&remote).await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK))
        .mount(&remote)
        .await;
    for run in 0..2 {
        let mut command = fixture.authenticated(&source);
        named_token(&mut command, "remote");
        let data = report(command.arg("check").output().unwrap(), 0);
        assert_eq!(data["sync"]["scope"], "inbox");
        assert_eq!(data["destinations"].as_array().unwrap().len(), 3);
        assert_eq!(
            destination(&data, "remote")["delivery"][if run == 0 {
                "delivered"
            } else {
                "previously_confirmed"
            }],
            1
        );
        for name in ["first", "second"] {
            assert_eq!(
                destination(&data, name)["delivery"][if run == 0 {
                    "delivered"
                } else {
                    "already_delivered"
                }],
                1
            );
        }
    }
    let store = fixture.store();
    for target in [&first, &second] {
        assert_eq!(
            fixture
                .folder_store(&store, target)
                .read_saved(LETTER)
                .unwrap(),
            PDF
        );
    }
    assert!(!fixture.base.join("unselected-source").exists());
    assert_eq!(
        store
            .source_metadata(LETTER)
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("Synthetic modern title")
    );
    let requests = source.received_requests().await.unwrap();
    assert!(requests.iter().all(|r| r.method.as_str() == "GET"));
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().ends_with("/content"))
            .count(),
        1
    );
    for request in requests {
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            &format!("Bearer {BEARER}")
        );
        if request.url.path() == "/epost/v2/letters" {
            assert!(
                request
                    .url
                    .query_pairs()
                    .any(|(key, value)| key == "is-business-tenant" && value == "false")
            );
        }
    }
    let requests = remote.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path().ends_with("/download/"))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_inbox_still_resumes_a_journalled_pending_paperless_upload() {
    let source = MockServer::start().await;
    let remote = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut store = fixture.store();
    store
        .save(LETTER, "inbox", PDF, Some("application/pdf"))
        .unwrap();
    let target = fixture.paperless(&mut store, "pending", &remote);
    tags(&remote).await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK))
        .mount(&remote)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!([{"task_id":TASK,"status":"PENDING","related_document_ids":[]}]),
            ),
        )
        .mount(&remote)
        .await;
    let client = PaperlessClient::for_test(
        &remote.uri(),
        Secret::new(DESTINATION_TOKEN.into()),
        PaperlessOptions {
            max_retries: 0,
            ..Default::default()
        },
    )
    .unwrap();
    let initial = delivery::deliver_paperless(
        &mut store,
        &client,
        &target,
        NamedDeliveryOptions {
            delivery: DeliveryOptions {
                max_documents: 1,
                poll_attempts: 1,
                poll_interval_ms: 0,
            },
            audit: false,
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(initial.pending, 1);
    drop(store);
    remote.reset().await;
    inbox(&source, json!([])).await;
    successful_remote_document(&remote).await;
    let mut command = fixture.authenticated(&source);
    named_token(&mut command, "pending");
    let data = report(command.args(["check", "inbox"]).output().unwrap(), 0);
    assert!(data["sync"]["documents"].as_array().unwrap().is_empty());
    assert_eq!(destination(&data, "pending")["delivery"]["delivered"], 1);
    assert_eq!(source.received_requests().await.unwrap().len(), 1);
    assert!(
        remote
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method.as_str() == "GET")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_destination_selection_does_not_touch_other_destinations_or_credentials() {
    let source = MockServer::start().await;
    let remote = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut store = fixture.store();
    fixture.folder(&mut store, "unselected-folder");
    let selected = fixture.folder(&mut store, "chosen");
    let mut skipped = fixture.paperless(&mut store, "unselected-remote", &remote);
    let DestinationKind::Paperless(config) = &mut skipped.kind else {
        unreachable!()
    };
    config.target.remember_token = true;
    destinations::save(&mut store, &skipped).unwrap();
    drop(store);
    inbox(&source, json!([{"id":LETTER}])).await;
    content(&source, LETTER).await;
    let data = report(
        fixture
            .authenticated(&source)
            .args(["check", "inbox", "--destination", "chosen"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(data["destinations"].as_array().unwrap().len(), 1);
    assert_eq!(destination(&data, "chosen")["delivery"]["delivered"], 1);
    assert!(remote.received_requests().await.unwrap().is_empty());
    assert!(!fixture.base.join("unselected-folder").exists());
    let store = fixture.store();
    assert_eq!(
        delivery::destination_status(&store, &skipped)
            .unwrap()
            .not_submitted,
        1
    );
    assert_eq!(
        fixture
            .folder_store(&store, &selected)
            .read_saved(LETTER)
            .unwrap(),
        PDF
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_paperless_access_does_not_suppress_a_healthy_folder_destination() {
    let source = MockServer::start().await;
    let remote = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut store = fixture.store();
    fixture.paperless(&mut store, "a-failed", &remote);
    let healthy = fixture.folder(&mut store, "z-healthy");
    drop(store);
    inbox(&source, json!([{"id":LETTER}])).await;
    content(&source, LETTER).await;
    Mock::given(method("GET"))
        .and(path("/api/tags/11/"))
        .respond_with(ResponseTemplate::new(403).set_body_string(DESTINATION_TOKEN))
        .mount(&remote)
        .await;
    let mut command = fixture.authenticated(&source);
    named_token(&mut command, "a-failed");
    let data = report(command.arg("check").output().unwrap(), 3);
    assert!(
        destination(&data, "a-failed")["error"]
            .as_str()
            .unwrap()
            .contains("authentication")
    );
    assert_eq!(destination(&data, "z-healthy")["delivery"]["delivered"], 1);
    let store = fixture.store();
    assert_eq!(
        fixture
            .folder_store(&store, &healthy)
            .read_saved(LETTER)
            .unwrap(),
        PDF
    );
    assert_eq!(remote.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_repeated_checks_advance_past_saved_documents_without_redownloading() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let ids = ["synthetic-first", "synthetic-second", "synthetic-third"];
    inbox(&source, json!(ids.map(|id| json!({"id":id})))).await;
    for id in ids {
        content(&source, id).await;
    }
    for run in 0..4 {
        let data = report(
            fixture
                .authenticated(&source)
                .args(["check", "--local", "--limit", "1"])
                .output()
                .unwrap(),
            if run < 2 { 2 } else { 0 },
        );
        let documents = data["sync"]["documents"].as_array().unwrap();
        assert_eq!(
            documents
                .iter()
                .filter(|d| d["status"] == "saved_pdf")
                .count(),
            usize::from(run < 3)
        );
        assert_eq!(
            documents
                .iter()
                .filter(|d| d["status"] == "already_saved")
                .count(),
            run.min(3)
        );
        assert_eq!(
            documents
                .iter()
                .filter(|d| d["status"] == "deferred_limit")
                .count(),
            2_usize.saturating_sub(run)
        );
        assert!(data.get("destinations").is_none());
    }
    let store = fixture.store();
    assert_eq!(store.saved_documents().unwrap().len(), 3);
    let requests = source.received_requests().await.unwrap();
    for id in ids {
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path() == format!("/epost/v2/letters/{id}/content"))
                .count(),
            1
        );
    }
    assert!(requests.iter().all(|r| r.method.as_str() == "GET"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_profile_destination_delivery_and_verify_commands_do_not_authenticate_source() {
    let source = MockServer::start().await;
    let fixture = Fixture::with_login(&source, LoginMethod::MacosKeychain);
    let mut store = fixture.store();
    store
        .save(LETTER, "inbox", PDF, Some("application/pdf"))
        .unwrap();
    fixture.folder(&mut store, "copies");
    drop(store);
    let profiles = report(
        fixture
            .command(&source)
            .args(["profile", "list"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(profiles["profiles"].as_array().unwrap().len(), 2);
    let targets = report(
        fixture
            .command(&source)
            .args(["destination", "list"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(targets["destinations"][0]["name"], "copies");
    let delivered = report(fixture.command(&source).arg("deliver").output().unwrap(), 0);
    assert_eq!(
        destination(&delivered, "copies")["delivery"]["delivered"],
        1
    );
    let audited = report(fixture.command(&source).arg("verify").output().unwrap(), 0);
    assert_eq!(audited["audit"], true);
    assert_eq!(
        destination(&audited, "copies")["delivery"]["already_delivered"],
        1
    );
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_authentication_failure_still_delivers_saved_pdf_to_folder() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut store = fixture.store();
    store
        .save(LETTER, "inbox", PDF, Some("application/pdf"))
        .unwrap();
    fixture.folder(&mut store, "copies");
    drop(store);
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(403).set_body_string(BEARER))
        .mount(&source)
        .await;
    let data = report(
        fixture
            .authenticated(&source)
            .arg("check")
            .output()
            .unwrap(),
        3,
    );
    assert_eq!(data["sync_exit_code"], 3);
    assert_eq!(destination(&data, "copies")["delivery"]["delivered"], 1);
    assert_eq!(source.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_inbox_inspects_metadata_and_local_delivery_state_without_downloading() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut store = fixture.store();
    fixture.folder(&mut store, "copies");
    drop(store);
    inbox(&source, json!([{"id":LETTER,"readStatus":"READ"}])).await;
    let data = report(
        fixture
            .authenticated(&source)
            .args(["status", "inbox"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(data["inbox"]["new_documents_count"], 1);
    assert_eq!(data["destinations"][0]["state"]["saved_documents"], 0);
    assert_eq!(source.received_requests().await.unwrap().len(), 1);
    assert!(fixture.store().saved_documents().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_destination_selection_fails_before_source_requests() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let output = fixture
        .authenticated(&source)
        .args(["check", "--destination", "absent"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let diagnostic: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(diagnostic["status"], "error");
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[test]
fn no_arguments_in_a_pipe_shows_help_without_configuration_access_or_terminal_control() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("corrupt-config.json");
    fs::write(&config, b"deliberately corrupt synthetic config").unwrap();
    for explicit in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
        command.env_clear().stdin(Stdio::null());
        if explicit {
            command.arg("--config").arg(&config);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Usage:"));
        assert!(!text.contains('\u{1b}'));
        assert_eq!(
            fs::read(&config).unwrap(),
            b"deliberately corrupt synthetic config"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_pending_mail_does_not_consume_the_new_download_attempt_budget() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    inbox(&source,json!([{"id":"synthetic-awaiting-acceptance","eLetterRegistered":true,"registeredStatus":"PENDING"},{"id":LETTER}])).await;
    content(&source, LETTER).await;
    let data = report(
        fixture
            .authenticated(&source)
            .args(["check", "--local", "--limit", "1"])
            .output()
            .unwrap(),
        2,
    );
    let documents = data["sync"]["documents"].as_array().unwrap();
    assert_eq!(
        documents[0]["status"],
        "awaiting_manual_acceptance_or_status_verification"
    );
    assert_eq!(documents[1]["status"], "saved_pdf");
    let requests = source.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|r| r.method.as_str() == "GET"));
    assert!(
        requests
            .iter()
            .all(|r| !r.url.path().contains("synthetic-awaiting-acceptance"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_returns_attention_when_local_destination_history_is_corrupt() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut store = fixture.store();
    let target = fixture.folder(&mut store, "copies");
    store
        .write_private_state(
            &destinations::ledger_name(&target),
            b"{synthetic-corrupt-ledger}",
        )
        .unwrap();
    drop(store);
    inbox(&source, json!([])).await;
    let data = report(
        fixture
            .authenticated(&source)
            .args(["status", "inbox"])
            .output()
            .unwrap(),
        2,
    );
    assert!(
        destination(&data, "copies")["error"]
            .as_str()
            .unwrap()
            .contains("corrupt")
    );
    assert_eq!(source.received_requests().await.unwrap().len(), 1);
}
