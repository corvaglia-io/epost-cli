//! CLI destination scenarios use synthetic temp profiles, runtime tokens and
//! literal loopback services only. No terminal or native credential lookup.
use epost_cli::{
    api::Environment,
    config::{self, Login, LoginMethod, Profile, SyncScope},
    delivery::{self, DeliveryOptions, PaperlessTarget},
    destinations::{self, CredentialBinding, Destination, DestinationKind},
    models::{Mailbox, MailboxKind},
    storage::Store,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
};
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIRST_TOKEN: &str = "synthetic-destination-first-token";
const SECOND_TOKEN: &str = "synthetic-destination-second-token";
const GENERIC_TOKEN: &str = "synthetic-ambiguous-generic-token";
const SOURCE_TOKEN: &str = "synthetic-status-bearer-token";
const PDF: &[u8] = b"%PDF-1.4\nsynthetic document\n%%EOF\n";
const TASK: &str = "11111111-2222-4333-8444-555555555555";

struct Fixture {
    _temp: tempfile::TempDir,
    base: PathBuf,
    config: PathBuf,
    root: PathBuf,
    namespace: String,
}

impl Fixture {
    fn new(server: &MockServer) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let config = base.join("config.json");
        let root = base.join("synthetic-documents");
        let mailbox = Mailbox {
            tenant_id: "synthetic-destination-tenant".into(),
            company_id: 7,
            kind: MailboxKind::Private,
        };
        let namespace = serde_json::to_string(&(
            server.uri(),
            &mailbox.tenant_id,
            mailbox.company_id,
            mailbox.kind,
        ))
        .unwrap();
        config::save_profile(
            &config,
            "synthetic-profile",
            &Profile {
                mailbox,
                environment: Environment::Production,
                output: root.clone(),
                scope: SyncScope::Inbox,
                login: None,
            },
        )
        .unwrap();
        Self {
            _temp: temp,
            base,
            config,
            root,
            namespace,
        }
    }

    fn command(&self, server: &MockServer) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
        command
            .env_clear()
            .stdin(Stdio::null())
            .args([
                "--json",
                "--non-interactive",
                "--no-progress",
                "--retries",
                "0",
                "--timeout-seconds",
                "2",
                "--mock-url",
                &server.uri(),
            ])
            .arg("--config")
            .arg(&self.config);
        command
    }

    fn store(&self) -> Store {
        Store::open(&self.root, &self.namespace).unwrap()
    }

    fn folder(&self, name: &str) -> Destination {
        Destination {
            name: name.into(),
            credential_binding: CredentialBinding::Named,
            kind: DestinationKind::Folder {
                path: self.base.join(name),
            },
        }
    }

    fn paperless(&self, name: &str, server: &MockServer) -> Destination {
        Destination {
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
        }
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
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        for secret in [FIRST_TOKEN, SECOND_TOKEN, GENERIC_TOKEN, SOURCE_TOKEN] {
            assert!(!text.contains(secret));
        }
        assert!(!text.contains('\x1b'));
    }
    serde_json::from_slice(if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    })
    .unwrap()
}

fn seed(store: &mut Store) {
    store
        .save("synthetic-letter", "inbox", PDF, Some("application/pdf"))
        .unwrap();
}

async fn successful_delivery(server: &MockServer, token: &str) {
    for (id, inbox) in [(11, true), (12, false)] {
        Mock::given(method("GET"))
            .and(path(format!("/api/tags/{id}/")))
            .and(header("authorization", format!("Token {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"id":id,"name":format!("Synthetic {id}"),"is_inbox_tag":inbox}),
            ))
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .and(header("authorization", format!("Token {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .and(query_param("task_id", TASK))
        .and(header("authorization", format!("Token {token}")))
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
        .and(header("authorization", format!("Token {token}")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(PDF)
                .insert_header("Content-Type", "application/pdf"),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/42/"))
        .and(header("authorization", format!("Token {token}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":42,"tags":[11,12]})))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folder_add_list_remove_retains_source_documents_copy_and_history() {
    let epost = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    let destination = fixture.folder("copies");
    let DestinationKind::Folder { path: copy_path } = &destination.kind else {
        unreachable!()
    };
    let added = report(
        fixture
            .command(&epost)
            .args(["destination", "add", "copies", "--folder"])
            .arg(copy_path)
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(added["data"]["configured"], "copies");
    let listed = report(
        fixture
            .command(&epost)
            .args(["destination", "list"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(listed["data"]["destinations"][0]["name"], "copies");
    {
        let mut store = fixture.store();
        seed(&mut store);
        let copied = delivery::deliver_folder(
            &mut store,
            &destination,
            DeliveryOptions {
                max_documents: 1,
                poll_attempts: 1,
                poll_interval_ms: 0,
            },
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(copied.delivered, 1);
    }
    let before = fixture
        .store()
        .read_private_state(&destinations::ledger_name(&destination))
        .unwrap()
        .unwrap();
    let removed = report(
        fixture
            .command(&epost)
            .args(["destination", "remove", "copies", "--yes"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(removed["data"]["history_retained"], true);
    let store = fixture.store();
    assert!(destinations::list(&store).unwrap().is_empty());
    assert_eq!(store.read_saved("synthetic-letter").unwrap(), PDF);
    assert_eq!(
        store
            .read_private_state(&destinations::ledger_name(&destination))
            .unwrap()
            .unwrap(),
        before
    );
    let copy_namespace = format!(
        "epost-folder-v1:{}:{}",
        store.identity_hash(),
        destinations::destination_identity(&store, &destination)
    );
    assert_eq!(
        Store::open(copy_path, &copy_namespace)
            .unwrap()
            .read_saved("synthetic-letter")
            .unwrap(),
        PDF
    );
    assert!(epost.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_list_and_folder_add_preserve_token_binding_without_native_access() {
    let epost = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    {
        let mut store = fixture.store();
        delivery::save_target(
            &mut store,
            &PaperlessTarget {
                schema_version: 1,
                url: format!("{}/", paperless.uri()),
                inbox_tag_id: 11,
                source_tag_id: Some(12),
                remember_token: true,
            },
        )
        .unwrap();
    }
    let listed = report(
        fixture
            .command(&epost)
            .args(["destination", "list"])
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(listed["data"]["destinations"][0]["name"], "paperless");
    report(
        fixture
            .command(&epost)
            .args(["destination", "add", "copies", "--folder"])
            .arg(fixture.base.join("copies"))
            .output()
            .unwrap(),
        0,
    );
    let store = fixture.store();
    let migrated = destinations::load(&store, "paperless").unwrap();
    assert_eq!(migrated.credential_binding, CredentialBinding::Legacy);
    let DestinationKind::Paperless(settings) = &migrated.kind else {
        unreachable!()
    };
    assert!(settings.target.remember_token);
    drop(store);
    // Synthetic mode must reject a destructive saved-token action before
    // touching the native store, keeping its existing configuration intact.
    let denied = report(
        fixture
            .command(&epost)
            .args(["destination", "remove", "paperless", "--yes"])
            .output()
            .unwrap(),
        1,
    );
    assert!(
        denied["error"]
            .as_str()
            .unwrap()
            .contains("requires interactive mode")
    );
    assert_eq!(
        destinations::load(&fixture.store(), "paperless").unwrap(),
        migrated
    );
    assert!(epost.received_requests().await.unwrap().is_empty());
    assert!(paperless.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn distinct_punctuation_names_use_distinct_runtime_tokens_without_epost_requests() {
    let epost = MockServer::start().await;
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    {
        let mut store = fixture.store();
        seed(&mut store);
        destinations::save(&mut store, &fixture.paperless("a-b", &first)).unwrap();
        destinations::save(&mut store, &fixture.paperless("a_b", &second)).unwrap();
    }
    successful_delivery(&first, FIRST_TOKEN).await;
    successful_delivery(&second, SECOND_TOKEN).await;
    let delivered = report(
        fixture
            .command(&epost)
            .env("EPOST_DESTINATION_612D62_TOKEN", FIRST_TOKEN)
            .env("EPOST_DESTINATION_615F62_TOKEN", SECOND_TOKEN)
            .env("PAPERLESS_TOKEN", GENERIC_TOKEN)
            .arg("deliver")
            .output()
            .unwrap(),
        0,
    );
    let rows = delivered["data"]["destinations"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row["delivery"]["delivered"] == 1));
    for (server, token) in [(&first, FIRST_TOKEN), (&second, SECOND_TOKEN)] {
        for request in server.received_requests().await.unwrap() {
            assert_eq!(request.headers["authorization"], format!("Token {token}"));
        }
    }
    let raw = fixture
        .store()
        .read_private_state("destinations.json")
        .unwrap()
        .unwrap();
    for secret in [FIRST_TOKEN, SECOND_TOKEN, GENERIC_TOKEN] {
        assert!(!String::from_utf8_lossy(&raw).contains(secret));
    }
    assert!(epost.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_generic_token_is_rejected_before_either_destination_receives_it() {
    let epost = MockServer::start().await;
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    {
        let mut store = fixture.store();
        seed(&mut store);
        destinations::save(&mut store, &fixture.paperless("first", &first)).unwrap();
        destinations::save(&mut store, &fixture.paperless("second", &second)).unwrap();
    }
    let rejected = report(
        fixture
            .command(&epost)
            .env("PAPERLESS_TOKEN", GENERIC_TOKEN)
            .arg("deliver")
            .output()
            .unwrap(),
        1,
    );
    let rows = rejected["data"]["destinations"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row["error"].as_str().unwrap().contains("ambiguous"))
    );
    for server in [&epost, &first, &second] {
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_token_file_conflict_fails_without_echoing_or_using_either_secret() {
    let epost = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    {
        let mut store = fixture.store();
        seed(&mut store);
        destinations::save(&mut store, &fixture.paperless("one", &paperless)).unwrap();
    }
    let token_file = fixture.base.join("synthetic-token.txt");
    fs::write(&token_file, FIRST_TOKEN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let rejected = report(
        fixture
            .command(&epost)
            .env("PAPERLESS_TOKEN", GENERIC_TOKEN)
            .arg("--paperless-token-file")
            .arg(token_file)
            .arg("deliver")
            .output()
            .unwrap(),
        1,
    );
    assert!(
        rejected["data"]["destinations"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not both")
    );
    assert!(epost.received_requests().await.unwrap().is_empty());
    assert!(paperless.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noninteractive_add_and_edit_cannot_enter_the_wizard_or_contact_servers() {
    let epost = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    destinations::save(&mut fixture.store(), &fixture.paperless("one", &paperless)).unwrap();
    for args in [
        vec!["destination", "add", "two", "--url", &paperless.uri()],
        vec!["destination", "edit", "one"],
    ] {
        let rejected = report(
            fixture
                .command(&epost)
                .env("PAPERLESS_TOKEN", FIRST_TOKEN)
                .args(args)
                .output()
                .unwrap(),
            1,
        );
        assert!(
            rejected["error"]
                .as_str()
                .unwrap()
                .contains("interactive terminal")
        );
    }
    assert_eq!(destinations::list(&fixture.store()).unwrap().len(), 1);
    assert!(epost.received_requests().await.unwrap().is_empty());
    assert!(paperless.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_tty_delivery_has_plain_live_progress_and_a_lasting_human_summary() {
    let epost = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    {
        let mut store = fixture.store();
        seed(&mut store);
        destinations::save(&mut store, &fixture.folder("copies")).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_epost-cli"))
        .env_clear()
        .stdin(Stdio::null())
        .args(["--non-interactive", "--mock-url", &epost.uri(), "--config"])
        .arg(&fixture.config)
        .arg("deliver")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    let progress = String::from_utf8(output.stderr).unwrap();
    let summary = String::from_utf8(output.stdout).unwrap();
    assert!(progress.contains("starting"));
    assert!(progress.contains("finished"));
    assert!(summary.contains("copies: 1 newly verified"));
    assert!(!progress.contains('\x1b'));
    assert!(!summary.contains('\x1b'));
    assert!(epost.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_commands_cannot_drift_migrated_configuration_or_retire_its_saved_token() {
    let epost = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&epost);
    let (target_before, registry_before, migrated) = {
        let mut store = fixture.store();
        seed(&mut store);
        delivery::save_target(
            &mut store,
            &PaperlessTarget {
                schema_version: 1,
                url: format!("{}/", paperless.uri()),
                inbox_tag_id: 11,
                source_tag_id: Some(12),
                remember_token: true,
            },
        )
        .unwrap();
        destinations::save(&mut store, &fixture.folder("copies")).unwrap();
        (
            store
                .read_private_state("paperless-target.json")
                .unwrap()
                .unwrap(),
            store
                .read_private_state("destinations.json")
                .unwrap()
                .unwrap(),
            destinations::load(&store, "paperless").unwrap(),
        )
    };
    let url = paperless.uri();
    for args in [
        vec!["paperless", "setup", "--url", &url],
        vec!["paperless", "logout"],
        vec!["paperless", "upload", "--wait-seconds", "0"],
        vec!["paperless", "status"],
        vec![
            "paperless",
            "reconcile",
            "synthetic-letter",
            "--document-id",
            "42",
        ],
    ] {
        let rejected = report(
            fixture
                .command(&epost)
                .env("PAPERLESS_TOKEN", FIRST_TOKEN)
                .args(args)
                .output()
                .unwrap(),
            1,
        );
        assert!(
            rejected["error"]
                .as_str()
                .unwrap()
                .contains("named destinations are configured")
        );
        let store = fixture.store();
        assert_eq!(
            store
                .read_private_state("paperless-target.json")
                .unwrap()
                .unwrap(),
            target_before
        );
        assert_eq!(
            store
                .read_private_state("destinations.json")
                .unwrap()
                .unwrap(),
            registry_before
        );
        assert_eq!(destinations::load(&store, "paperless").unwrap(), migrated);
        assert_eq!(store.read_saved("synthetic-letter").unwrap(), PDF);
    }
    assert!(epost.received_requests().await.unwrap().is_empty());
    assert!(paperless.received_requests().await.unwrap().is_empty());
}

async fn archive_directories(server: &MockServer) {
    Mock::given(method("GET")).and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"directoryId":"first","directoryName":"Synthetic first","numberOfDocuments":48,"hasSubDirectories":false},
            {"directoryId":"later","directoryName":"Synthetic later","numberOfDocuments":1,"hasSubDirectories":false},
        ]))).expect(1).mount(server).await;
}

fn full_archive_page() -> Value {
    json!(
        (0..48)
            .map(|index| json!({"id":format!("synthetic-status-{index}")}))
            .collect::<Vec<_>>()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_authentication_failure_stops_later_folders_and_retains_exit_class() {
    for late_page in [false, true] {
        let source = MockServer::start().await;
        let fixture = Fixture::new(&source);
        archive_directories(&source).await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/archives/letters"))
            .and(query_param_is_missing("directory-id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&source)
            .await;
        if late_page {
            Mock::given(method("GET"))
                .and(path("/epost/v2/archives/letters"))
                .and(query_param("directory-id", "first"))
                .and(query_param("offset", "0"))
                .respond_with(ResponseTemplate::new(200).set_body_json(full_archive_page()))
                .expect(1)
                .mount(&source)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/epost/v2/archives/letters"))
            .and(query_param("directory-id", "first"))
            .and(query_param("offset", if late_page { "48" } else { "0" }))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message":SOURCE_TOKEN})))
            .expect(1)
            .mount(&source)
            .await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/archives/letters"))
            .and(query_param("directory-id", "later"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(0)
            .mount(&source)
            .await;
        let value = report(
            fixture
                .command(&source)
                .env("EPOST_BEARER_TOKEN", SOURCE_TOKEN)
                .args(["status", "archive"])
                .output()
                .unwrap(),
            3,
        );
        assert_eq!(value["status"], "partial");
        assert_eq!(value["data"]["archive"]["pagination_complete"], false);
        assert_eq!(
            value["data"]["archive"]["observed_documents"],
            if late_page { 48 } else { 0 }
        );
        assert!(fixture.store().saved_documents().unwrap().is_empty());
        assert!(
            source
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.method == "GET")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_root_late_authentication_failure_never_starts_folder_requests() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    archive_directories(&source).await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .and(query_param_is_missing("directory-id"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(full_archive_page()))
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .and(query_param_is_missing("directory-id"))
        .and(query_param("offset", "48"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&source)
        .await;
    for name in ["first", "later"] {
        Mock::given(method("GET"))
            .and(path("/epost/v2/archives/letters"))
            .and(query_param("directory-id", name))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(0)
            .mount(&source)
            .await;
    }
    let value = report(
        fixture
            .command(&source)
            .env("EPOST_BEARER_TOKEN", SOURCE_TOKEN)
            .args(["status", "archive"])
            .output()
            .unwrap(),
        3,
    );
    assert_eq!(value["data"]["archive"]["observed_documents"], 48);
    assert_eq!(value["data"]["archive"]["pagination_complete"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_all_retains_completed_inbox_observation_after_archive_auth_error() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source);
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&source)
        .await;
    let value = report(
        fixture
            .command(&source)
            .env("EPOST_BEARER_TOKEN", SOURCE_TOKEN)
            .args(["status", "all"])
            .output()
            .unwrap(),
        3,
    );
    assert_eq!(value["data"]["inbox"]["new_documents_count"], 0);
    assert_eq!(value["data"]["inbox"]["pagination_complete"], true);
    assert!(
        value["data"]["archive_error"]
            .as_str()
            .unwrap()
            .contains("HTTP 403")
    );
    assert_eq!(source.received_requests().await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn named_delivery_preserves_authentication_exit_after_catalog_validation_succeeds() {
    let source = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&source);
    {
        let mut store = fixture.store();
        seed(&mut store);
        destinations::save(&mut store, &fixture.paperless("one", &paperless)).unwrap();
    }
    for (id, inbox) in [(11, true), (12, false)] {
        Mock::given(method("GET"))
            .and(path(format!("/api/tags/{id}/")))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"id":id,"name":format!("Synthetic {id}"),"is_inbox_tag":inbox}),
            ))
            .mount(&paperless)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"message":FIRST_TOKEN})))
        .expect(1)
        .mount(&paperless)
        .await;
    let value = report(
        fixture
            .command(&source)
            .env("PAPERLESS_TOKEN", FIRST_TOKEN)
            .arg("deliver")
            .output()
            .unwrap(),
        3,
    );
    assert_eq!(value["data"]["destinations"][0]["exit_code"], 3);
    assert_eq!(
        value["data"]["destinations"][0]["delivery"]["authentication_failed"],
        true
    );
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noninteractive_credential_changes_are_rejected_before_mock_native_backstop() {
    let source = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&source);
    let mut profile = config::load_profile(&fixture.config, "synthetic-profile").unwrap();
    profile.login = Some(Login {
        username: "synthetic@example.invalid".into(),
        method: LoginMethod::MacosKeychain,
    });
    config::save_profile(&fixture.config, "synthetic-profile", &profile).unwrap();
    let target = PaperlessTarget {
        schema_version: 1,
        url: format!("{}/", paperless.uri()),
        inbox_tag_id: 11,
        source_tag_id: Some(12),
        remember_token: true,
    };
    delivery::save_target(&mut fixture.store(), &target).unwrap();
    for args in [vec!["logout"], vec!["paperless", "logout"]] {
        let value = report(fixture.command(&source).args(args).output().unwrap(), 1);
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("requires interactive mode")
        );
        assert_eq!(
            config::load_profile(&fixture.config, "synthetic-profile").unwrap(),
            profile
        );
        assert_eq!(delivery::load_target(&fixture.store()).unwrap(), target);
    }
    destinations::save(&mut fixture.store(), &fixture.folder("copies")).unwrap();
    let migrated = destinations::load(&fixture.store(), "paperless").unwrap();
    let value = report(
        fixture
            .command(&source)
            .args(["destination", "remove", "paperless", "--yes"])
            .output()
            .unwrap(),
        1,
    );
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("requires interactive mode")
    );
    assert_eq!(
        destinations::load(&fixture.store(), "paperless").unwrap(),
        migrated
    );
    assert_eq!(delivery::load_target(&fixture.store()).unwrap(), target);
    assert!(source.received_requests().await.unwrap().is_empty());
    assert!(paperless.received_requests().await.unwrap().is_empty());
}
