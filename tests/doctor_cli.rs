//! Doctor runs use synthetic temp profiles, fake runtime tokens and localhost.

use epost_cli::{
    api::Environment,
    config::{self, Login, LoginMethod, Profile, SyncScope},
    delivery::{self, DeliveryOptions, NamedDeliveryOptions, PaperlessTarget},
    destinations::{self, CredentialBinding, Destination, DestinationKind},
    models::{Mailbox, MailboxKind},
    paperless::{PaperlessClient, PaperlessOptions},
    secret::Secret,
    storage::Store,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const BEARER: &str = "synthetic-doctor-bearer-secret";
const PAPERLESS: &str = "synthetic-doctor-paperless-secret";
const TASK_ID: &str = "00000000-0000-4000-8000-000000000001";
const PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\n%%EOF\n";

struct Fixture {
    _temp: tempfile::TempDir,
    config: PathBuf,
    root: PathBuf,
    base: PathBuf,
    namespace: String,
}

impl Fixture {
    fn new(server: &MockServer, method: LoginMethod) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let config = base.join("config.json");
        let root = base.join("source-documents");
        let mailbox = Mailbox {
            tenant_id: "synthetic-doctor-private-tenant".into(),
            company_id: 42,
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
            "personal",
            &Profile {
                mailbox,
                environment: Environment::Production,
                output: root.clone(),
                scope: SyncScope::Inbox,
                login: Some(Login {
                    username: "synthetic-doctor-account@example.invalid".into(),
                    method,
                }),
            },
        )
        .unwrap();
        Self {
            _temp: temp,
            config,
            root,
            base,
            namespace,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.root, &self.namespace).unwrap()
    }

    fn command(&self, server: &MockServer) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_epost-cli"));
        command
            .env_clear()
            .stdin(Stdio::null())
            .args([
                "--json",
                "--no-progress",
                "--timeout-seconds",
                "2",
                "--retries",
                "0",
                "--mock-url",
                &server.uri(),
            ])
            .arg("--config")
            .arg(&self.config)
            .arg("doctor");
        command
    }

    fn paperless(&self, store: &mut Store, name: &str, url: &str, remember_token: bool) {
        destinations::save(
            store,
            &Destination {
                name: name.into(),
                credential_binding: CredentialBinding::Named,
                kind: DestinationKind::Paperless(
                    PaperlessTarget {
                        schema_version: 1,
                        url: format!("{}/", url.trim_end_matches('/')),
                        inbox_tag_id: 7,
                        source_tag_id: None,
                        remember_token,
                    }
                    .into(),
                ),
            },
        )
        .unwrap();
    }

    fn report(&self, output: &Output, code: i32) -> Value {
        assert_eq!(
            output.status.code(),
            Some(code),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8_lossy(&output.stdout);
        for excluded in [
            BEARER,
            PAPERLESS,
            "synthetic-doctor-private-tenant",
            "synthetic-doctor-account@example.invalid",
            "synthetic-doctor-private-letter",
            self.base.to_str().unwrap(),
            "127.0.0.1",
            "private-gateway-body",
        ] {
            assert!(
                !text.contains(excluded),
                "diagnostics exposed synthetic sensitive detail: {excluded}"
            );
        }
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["data"]["healthy"], code == 0);
        for row in value["data"]["checks"].as_array().unwrap() {
            assert!(row["component"].is_string());
            assert!(row["status"].is_string());
            assert!(row["message"].is_string());
        }
        value["data"].clone()
    }
}

fn check<'a>(report: &'a Value, component: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["component"] == component)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_checks_local_integrity_and_all_destinations_without_network_or_keychain() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source, LoginMethod::MacosKeychain);
    let mut store = fixture.store();
    store
        .save(
            "synthetic-doctor-private-letter",
            "inbox",
            PDF,
            Some("application/pdf"),
        )
        .unwrap();
    fixture.paperless(&mut store, "first", "https://first.example.invalid", true);
    fixture.paperless(&mut store, "second", "https://second.example.invalid", true);
    drop(store);
    let original = fs::read(&fixture.config).unwrap();
    let report = fixture.report(
        &fixture.command(&source).arg("--offline").output().unwrap(),
        0,
    );
    assert_eq!(report["offline"], true);
    assert_eq!(report["network_checks_attempted"], false);
    assert_eq!(
        check(&report, "source_credentials")["status"],
        "not_checked"
    );
    assert_eq!(
        check(&report, "source_store")["details"]["saved_documents"],
        1
    );
    assert_eq!(
        check(&report, "destinations")["details"]["configured_selection"],
        2
    );
    assert_eq!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["component"] == "delivery_state")
            .count(),
        2
    );
    assert_eq!(fs::read(&fixture.config).unwrap(), original);
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_local_store_is_reported_without_creating_it() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source, LoginMethod::Prompt);
    assert!(!fixture.root.exists());
    let report = fixture.report(
        &fixture.command(&source).arg("--offline").output().unwrap(),
        2,
    );
    assert_eq!(check(&report, "source_store")["status"], "error");
    assert!(!fixture.root.exists());
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_or_corrupt_profiles_return_diagnostic_rows_without_account_discovery() {
    for corrupt in [false, true] {
        let source = MockServer::start().await;
        let fixture = Fixture::new(&source, LoginMethod::Prompt);
        if corrupt {
            fs::write(&fixture.config, b"{synthetic-corrupt-configuration}").unwrap();
        } else {
            fs::remove_file(&fixture.config).unwrap();
        }
        let report = fixture.report(
            &fixture.command(&source).arg("--offline").output().unwrap(),
            2,
        );
        assert_eq!(check(&report, "configuration")["status"], "error");
        assert!(!fixture.root.exists());
        assert!(source.received_requests().await.unwrap().is_empty());
        if corrupt {
            assert_eq!(
                fs::read(&fixture.config).unwrap(),
                b"{synthetic-corrupt-configuration}"
            );
        } else {
            assert!(!fixture.config.exists());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_documents_are_reported_without_repairing_or_overwriting_them() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source, LoginMethod::Prompt);
    let mut store = fixture.store();
    let saved = store
        .save(
            "synthetic-doctor-private-letter",
            "inbox",
            PDF,
            Some("application/pdf"),
        )
        .unwrap();
    drop(store);
    fs::write(&saved.path, b"synthetic-corrupt-document").unwrap();
    let report = fixture.report(
        &fixture.command(&source).arg("--offline").output().unwrap(),
        2,
    );
    assert_eq!(check(&report, "source_store")["status"], "error");
    assert_eq!(
        fs::read(&saved.path).unwrap(),
        b"synthetic-corrupt-document"
    );
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_pending_delivery_needs_attention_and_does_not_resubmit_or_change_its_journal() {
    let source = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&source, LoginMethod::Prompt);
    let mut store = fixture.store();
    store
        .save(
            "synthetic-doctor-private-letter",
            "inbox",
            PDF,
            Some("application/pdf"),
        )
        .unwrap();
    fixture.paperless(&mut store, "pending", &paperless.uri(), false);
    let destination = destinations::load(&store, "pending").unwrap();
    let remote = PaperlessClient::for_test(
        &paperless.uri(),
        Secret::new(PAPERLESS.into()),
        PaperlessOptions {
            max_retries: 0,
            ..Default::default()
        },
    )
    .unwrap();
    Mock::given(method("GET"))
        .and(path("/api/tags/7/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":7,"name":"synthetic-inbox","is_inbox_tag":true})),
        )
        .mount(&paperless)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"count":0,"next":null,"previous":null,"results":[]})),
        )
        .mount(&paperless)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK_ID))
        .mount(&paperless)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!([{"task_id":TASK_ID,"status":"STARTED","related_document":null}]),
            ),
        )
        .mount(&paperless)
        .await;
    let delivered = delivery::deliver_paperless(
        &mut store,
        &remote,
        &destination,
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
    assert_eq!(delivered.pending, 1);
    let ledger_name = destinations::ledger_name(&destination);
    let before = store.read_private_state(&ledger_name).unwrap().unwrap();
    drop(store);
    paperless.reset().await;
    let report = fixture.report(
        &fixture.command(&source).arg("--offline").output().unwrap(),
        2,
    );
    assert_eq!(check(&report, "delivery_state")["status"], "warning");
    assert_eq!(check(&report, "delivery_state")["details"]["pending"], 1);
    let store = fixture.store();
    assert_eq!(
        store.read_private_state(&ledger_name).unwrap().unwrap(),
        before
    );
    assert!(paperless.received_requests().await.unwrap().is_empty());
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn access_redirect_is_probed_without_token_and_never_followed() {
    let source = MockServer::start().await;
    let gateway = MockServer::start().await;
    let redirected = MockServer::start().await;
    let fixture = Fixture::new(&source, LoginMethod::Prompt);
    let mut store = fixture.store();
    fixture.paperless(&mut store, "gateway", &gateway.uri(), true);
    fixture.paperless(&mut store, "unselected", &redirected.uri(), true);
    drop(store);
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/inbox/count"))
        .respond_with(ResponseTemplate::new(200).set_body_json(0))
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags/"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", redirected.uri())
                .insert_header("server", "cloudflare")
                .set_body_string("private-gateway-body"),
        )
        .expect(1)
        .mount(&gateway)
        .await;
    let report = fixture.report(
        &fixture
            .command(&source)
            .env("EPOST_BEARER_TOKEN", BEARER)
            .env("PAPERLESS_TOKEN", PAPERLESS)
            .args(["--destination", "gateway"])
            .output()
            .unwrap(),
        2,
    );
    assert_eq!(check(&report, "source_access")["status"], "ok");
    assert_eq!(check(&report, "destination_transport")["status"], "warning");
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["component"] != "destination_credentials"
                && row["component"] != "destination_catalogs")
    );
    let requests = gateway.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(redirected.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_transport_rejects_destination_dns_names_before_lookup_or_token_loading() {
    let source = MockServer::start().await;
    let fixture = Fixture::new(&source, LoginMethod::Prompt);
    let mut store = fixture.store();
    fixture.paperless(
        &mut store,
        "forbidden",
        "https://never-resolve.example.invalid",
        true,
    );
    drop(store);
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/inbox/count"))
        .respond_with(ResponseTemplate::new(200).set_body_json(0))
        .expect(1)
        .mount(&source)
        .await;
    let report = fixture.report(
        &fixture
            .command(&source)
            .env("EPOST_BEARER_TOKEN", BEARER)
            .output()
            .unwrap(),
        2,
    );
    assert_eq!(check(&report, "destination_dns")["status"], "error");
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["component"] != "destination_transport"
                && row["component"] != "destination_credentials")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_catalog_checks_follow_an_unauthenticated_probe_and_stay_read_only() {
    let source = MockServer::start().await;
    let paperless = MockServer::start().await;
    let fixture = Fixture::new(&source, LoginMethod::Prompt);
    let mut store = fixture.store();
    fixture.paperless(&mut store, "paperless", &paperless.uri(), false);
    drop(store);
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/inbox/count"))
        .respond_with(ResponseTemplate::new(200).set_body_json(0))
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(method("GET")).and(path("/api/tags/")).respond_with(|request: &Request| {
        if request.headers.contains_key("authorization") {ResponseTemplate::new(200).set_body_json(json!({"count":1,"next":null,"previous":null,"results":[{"id":7,"name":"synthetic-inbox","is_inbox_tag":true}]}))}
        else {ResponseTemplate::new(401)}
    }).mount(&paperless).await;
    Mock::given(method("GET"))
        .and(path("/api/tags/7/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":7,"name":"synthetic-inbox","is_inbox_tag":true})),
        )
        .mount(&paperless)
        .await;
    let report = fixture.report(
        &fixture
            .command(&source)
            .env("EPOST_BEARER_TOKEN", BEARER)
            .env("PAPERLESS_TOKEN", PAPERLESS)
            .env("EPOST_DESTINATION_70617065726C657373_TOKEN", PAPERLESS)
            .output()
            .unwrap(),
        0,
    );
    assert_eq!(check(&report, "destination_catalogs")["status"], "ok");
    let requests = paperless.received_requests().await.unwrap();
    assert!(requests.len() >= 2);
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(requests[1..].iter().any(|request| {
        request
            .headers
            .get("authorization")
            .is_some_and(|header| header == &format!("Token {PAPERLESS}"))
    }));
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
    assert!(
        source
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
}
