//! Wholly synthetic delivery/state tests. No developer environment, host
//! credentials, ePost records or non-loopback destination is used here.

use epost_cli::delivery::{
    DeliveryItemStatus, DeliveryOptions, DeliveryPhase, DeliveryReasonCode, DeliveryRecoveryAction,
    PaperlessTarget, deliver, load_target, reconcile_document, save_target, status,
};
use epost_cli::paperless::{PaperlessClient, PaperlessOptions};
use epost_cli::secret::Secret;
use epost_cli::storage::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TASK_ID: &str = "11111111-2222-4333-8444-555555555555";
const LETTER: &str = "invented-letter-for-delivery";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("documents");
        Self { _temp: temp, root }
    }

    fn store(&self, namespace: &str) -> Store {
        Store::open(&self.root, namespace).unwrap()
    }

    fn state_path(&self, namespace: &str, name: &str) -> PathBuf {
        self.root
            .join(hex::encode(Sha256::digest(namespace.as_bytes())))
            .join(name)
    }
}

fn pdf() -> Vec<u8> {
    b"%PDF-1.4\nwholly synthetic document\n%%EOF\n".to_vec()
}

fn make_client(server: &MockServer) -> PaperlessClient {
    PaperlessClient::for_test(
        &server.uri(),
        Secret::new("fake-paperless-token".into()),
        PaperlessOptions {
            max_retries: 0,
            ..Default::default()
        },
    )
    .unwrap()
}

fn make_target(client: &PaperlessClient) -> PaperlessTarget {
    PaperlessTarget {
        schema_version: 1,
        url: client.canonical_url().into(),
        inbox_tag_id: 11,
        source_tag_id: Some(12),
        remember_token: false,
    }
}

fn options() -> DeliveryOptions {
    DeliveryOptions {
        max_documents: 100,
        poll_attempts: 1,
        poll_interval_ms: 0,
    }
}

fn seeded(fixture: &Fixture) -> Store {
    let mut store = fixture.store("synthetic-private");
    store
        .save(LETTER, "inbox", &pdf(), Some("application/pdf"))
        .unwrap();
    store
}

fn ledger(store: &Store) -> Value {
    serde_json::from_slice(
        &store
            .read_private_state("paperless-delivery.json")
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

fn record(ledger: &Value) -> &Value {
    ledger["entries"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
}

async fn upload(server: &MockServer, status: u16) {
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!(TASK_ID)))
        .expect(1)
        .mount(server)
        .await;
}

async fn task(server: &MockServer, status: &str, document_id: Option<i64>) {
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .and(query_param("task_id", TASK_ID))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"task_id":TASK_ID,"status":status,"related_document":document_id}
        ])))
        .mount(server)
        .await;
}

async fn original(server: &MockServer, document_id: i64, bytes: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(format!("/api/documents/{document_id}/download/")))
        .and(query_param("original", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .mount(server)
        .await;
}

async fn review_metadata(server: &MockServer, document_id: i64, tags: Vec<i64>, inbox: bool) {
    Mock::given(method("GET"))
        .and(path("/api/tags/11/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":11,"name":"Inbox","is_inbox_tag":inbox})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/documents/{document_id}/")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id":document_id,"tags":tags})),
        )
        .mount(server)
        .await;
}

async fn success(server: &MockServer, document_id: i64) {
    task(server, "SUCCESS", Some(document_id)).await;
    original(server, document_id, pdf()).await;
    review_metadata(server, document_id, vec![11, 12], true).await;
}

async fn lookup(server: &MockServer, filename: &str, ids: &[i64]) {
    let entries: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id":id,"original_file_name":filename}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/api/documents/"))
        .and(query_param("original_filename__iexact", filename))
        .respond_with(ResponseTemplate::new(200).set_body_json(entries))
        .mount(server)
        .await;
}

async fn post_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .count()
}

#[tokio::test]
async fn upload_is_durable_before_post_and_verified_repeat_never_reposts() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let private_ledger_path = fixture.state_path("synthetic-private", "paperless-delivery.json");
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(move |request: &Request| {
            let persisted: Value =
                serde_json::from_slice(&fs::read(&private_ledger_path).unwrap()).unwrap();
            assert_eq!(record(&persisted)["state"]["kind"], "uploading");
            let body = String::from_utf8_lossy(&request.body);
            assert!(body.contains(record(&persisted)["filename"].as_str().unwrap()));
            assert!(!body.contains(LETTER));
            ResponseTemplate::new(200).set_body_json(json!(TASK_ID))
        })
        .expect(1)
        .mount(&server)
        .await;
    success(&server, 101).await;
    let client = make_client(&server);
    let target = make_target(&client);
    let mut events = Vec::new();
    let first = deliver(&mut store, &client, &target, options(), |event| {
        events.push(event)
    })
    .await
    .unwrap();
    assert_eq!(first.delivered, 1);
    assert!(!first.has_attention());
    assert_eq!(events.first().unwrap().phase, DeliveryPhase::Started);
    assert_eq!(events.last().unwrap().phase, DeliveryPhase::Finished);
    assert_eq!(events.last().unwrap().delivered, 1);
    assert_eq!(record(&ledger(&store))["state"]["kind"], "verified");
    assert!(!serde_json::to_string(&first).unwrap().contains(LETTER));
    assert!(
        !serde_json::to_string(&ledger(&store))
            .unwrap()
            .contains(LETTER)
    );
    drop(store);
    let mut store = fixture.store("synthetic-private");
    let second = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(second.already_delivered, 1);
    assert_eq!(post_count(&server).await, 1);
    assert_eq!(status(&store, &target).unwrap().verified, 1);
}

#[tokio::test]
async fn queued_upload_is_pending_and_next_run_resumes_saved_task() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    task(&server, "PENDING", None).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    let first = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(first.pending, 1);
    assert!(first.has_attention());
    assert_eq!(status(&store, &target).unwrap().pending, 1);
    assert_eq!(record(&ledger(&store))["state"]["task_id"], TASK_ID);
    drop(store);
    server.reset().await;
    success(&server, 101).await;
    let mut store = fixture.store("synthetic-private");
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(post_count(&server).await, 0);
}

#[tokio::test]
async fn absent_task_reconciles_and_stays_uncertain_without_upload_retry() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    for _ in 0..2 {
        assert_eq!(
            deliver(&mut store, &client, &target, options(), |_| {})
                .await
                .unwrap()
                .uncertain,
            1
        );
    }
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn repeat_limit_advances_through_unresolved_documents() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(TASK_ID)))
        .expect(3)
        .mount(&server)
        .await;
    success(&server, 101).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    store
        .save("second-synthetic-letter", "inbox", &pdf(), None)
        .unwrap();
    store
        .save("third-synthetic-letter", "inbox", &pdf(), None)
        .unwrap();
    let client = make_client(&server);
    let target = make_target(&client);
    let options = DeliveryOptions {
        max_documents: 1,
        ..options()
    };
    for index in 0..3 {
        let report = deliver(&mut store, &client, &target, options, |_| {})
            .await
            .unwrap();
        assert_eq!(report.delivered, 1);
        assert_eq!(report.already_delivered, index);
        assert_eq!(report.deferred_limit, 2 - index);
        assert_eq!(post_count(&server).await, index + 1);
    }
    assert_eq!(status(&store, &target).unwrap().verified, 3);
}

#[tokio::test]
async fn local_upload_size_rejection_does_not_record_intent_and_can_retry_safely() {
    let server = MockServer::start().await;
    let client = PaperlessClient::for_test(
        &server.uri(),
        Secret::new("fake-paperless-token".into()),
        PaperlessOptions {
            max_response_bytes: 2,
            max_retries: 0,
            ..Default::default()
        },
    )
    .unwrap();
    let target = make_target(&client);
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.uncertain, 0);
    assert!(
        store
            .read_private_state("paperless-delivery.json")
            .unwrap()
            .is_none()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    upload(&server, 200).await;
    success(&server, 101).await;
    let client = make_client(&server);
    assert_eq!(
        deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap()
            .delivered,
        1
    );
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn expired_task_can_reconcile_to_original_without_resubmitting() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    task(&server, "PENDING", None).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    assert_eq!(
        deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap()
            .pending,
        1
    );
    let filename = record(&ledger(&store))["filename"]
        .as_str()
        .unwrap()
        .to_owned();
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    lookup(&server, &filename, &[101]).await;
    original(&server, 101, pdf()).await;
    review_metadata(&server, 101, vec![11, 12], true).await;
    assert_eq!(
        deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap()
            .delivered,
        1
    );
    assert_eq!(post_count(&server).await, 0);
}

#[tokio::test]
async fn successful_task_without_document_id_uses_verified_reconciliation() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    task(&server, "SUCCESS", None).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let state_path = fixture.state_path("synthetic-private", "paperless-delivery.json");
    Mock::given(method("GET"))
        .and(path("/api/documents/"))
        .respond_with(move |_request: &Request| {
            let state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
            let filename = record(&state)["filename"].as_str().unwrap();
            ResponseTemplate::new(200)
                .set_body_json(json!([{"id":101,"original_file_name":filename}]))
        })
        .mount(&server)
        .await;
    original(&server, 101, pdf()).await;
    review_metadata(&server, 101, vec![11, 12], true).await;
    let client = make_client(&server);
    let report = deliver(
        &mut store,
        &client,
        &make_target(&client),
        options(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn authentication_failure_stops_batch_and_preserves_partial_report() {
    let server = MockServer::start().await;
    upload(&server, 401).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    store
        .save("second-synthetic-letter", "inbox", &pdf(), None)
        .unwrap();
    let client = make_client(&server);
    let target = make_target(&client);
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert!(report.authentication_failed);
    assert_eq!(report.uncertain, 1);
    assert_eq!(report.not_attempted, 1);
    assert!(report.has_attention());
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(ledger(&store)["entries"].as_object().unwrap().len(), 1);
}

#[tokio::test]
async fn upload_redirect_stops_batch_and_preserves_only_one_uncertain_intent() {
    let server = MockServer::start().await;
    let redirect_destination = MockServer::start().await;
    let location = format!(
        "{}/login/?token=fake-redirect-query",
        redirect_destination.uri()
    );
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(302).insert_header("Location", location.as_str()))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    store
        .save("second-synthetic-letter", "inbox", &pdf(), None)
        .unwrap();
    let client = make_client(&server);
    let target = make_target(&client);
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert!(report.access_blocked);
    assert!(!report.authentication_failed);
    assert_eq!(report.uncertain, 1);
    assert_eq!(report.not_attempted, 1);
    assert!(report.has_attention());
    let rendered = serde_json::to_string(&report).unwrap();
    assert!(!rendered.contains("fake-redirect-query"));
    assert!(!rendered.contains(&location));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert!(
        redirect_destination
            .received_requests()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(ledger(&store)["entries"].as_object().unwrap().len(), 1);
    assert_eq!(record(&ledger(&store))["state"]["kind"], "uploading");
}

#[tokio::test]
async fn polling_redirect_stops_without_reconciliation_and_keeps_saved_task() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = MockServer::start().await;
    upload(&server, 200).await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let received = attempts.clone();
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(move |_request: &Request| {
            if received.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(200).set_body_json(json!([]))
            } else {
                ResponseTemplate::new(302)
                    .insert_header("Location", "/accounts/login/?next=fake-sensitive-query")
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    store
        .save("second-synthetic-letter", "inbox", &pdf(), None)
        .unwrap();
    let client = make_client(&server);
    let target = make_target(&client);
    let options = DeliveryOptions {
        poll_attempts: 2,
        ..options()
    };
    let report = deliver(&mut store, &client, &target, options, |_| {})
        .await
        .unwrap();
    assert!(report.access_blocked);
    assert!(!report.authentication_failed);
    assert_eq!(report.pending, 1);
    assert_eq!(report.not_attempted, 1);
    assert!(report.has_attention());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert_eq!(ledger(&store)["entries"].as_object().unwrap().len(), 1);
    assert_eq!(record(&ledger(&store))["state"]["kind"], "pending");
    assert_eq!(record(&ledger(&store))["state"]["task_id"], TASK_ID);
    assert!(
        !serde_json::to_string(&report)
            .unwrap()
            .contains("fake-sensitive-query")
    );
}

#[tokio::test]
async fn failed_or_interrupted_post_is_uncertain_and_never_blindly_retried() {
    let server = MockServer::start().await;
    upload(&server, 503).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    let first = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(first.uncertain, 1);
    assert_eq!(status(&store, &target).unwrap().uncertain, 1);
    let filename = record(&ledger(&store))["filename"]
        .as_str()
        .unwrap()
        .to_owned();
    lookup(&server, &filename, &[]).await;
    drop(store);
    let mut store = fixture.store("synthetic-private");
    let second = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(second.uncertain, 1);
    assert!(
        second.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("no automatic resubmission")
    );
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn uncertain_post_reconciles_only_exact_filename_and_original_bytes() {
    let server = MockServer::start().await;
    upload(&server, 503).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    let filename = record(&ledger(&store))["filename"]
        .as_str()
        .unwrap()
        .to_owned();
    lookup(&server, &filename, &[101]).await;
    original(&server, 101, pdf()).await;
    review_metadata(&server, 101, vec![11, 12], true).await;
    let reconciled = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(reconciled.delivered, 1);
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn task_failure_is_not_success_and_may_reconcile_a_verified_duplicate() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    task(&server, "FAILURE", None).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    // Respond to the filename discovered after the write before the POST.
    Mock::given(method("GET"))
        .and(path("/api/documents/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let failed = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(failed.failed, 1);
    assert_eq!(failed.uncertain, 0);
    assert_eq!(
        failed.items[0].reason_code,
        Some(DeliveryReasonCode::ProcessingFailure)
    );
    assert_eq!(
        failed.items[0].recovery_action,
        Some(DeliveryRecoveryAction::InspectPaperless)
    );
    assert!(
        !failed.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("upload outcome")
    );
    assert_eq!(status(&store, &target).unwrap().task_failed, 1);
    assert_eq!(record(&ledger(&store))["state"]["kind"], "task_failed");
    let filename = record(&ledger(&store))["filename"]
        .as_str()
        .unwrap()
        .to_owned();
    server.reset().await;
    lookup(&server, &filename, &[201]).await;
    original(&server, 201, pdf()).await;
    review_metadata(&server, 201, vec![11, 12], true).await;
    let reconciled = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(reconciled.delivered, 1);
    assert_eq!(post_count(&server).await, 0);
}

#[tokio::test]
async fn confirmed_failures_remain_redacted_and_distinct_from_lost_uploads_after_task_expiry() {
    for (result, code) in [
        (
            "private-synthetic.pdf: Unsupported mime type application/example fake-secret",
            "unsupported",
        ),
        (
            "private-synthetic.pdf: Error occurred while consuming document private-synthetic.pdf: EncryptedPdfError: fake-secret",
            "protected_pdf",
        ),
        (
            "private synthetic title fake-secret: arbitrary processor error",
            "processing_failure",
        ),
    ] {
        let server = MockServer::start().await;
        upload(&server, 200).await;
        Mock::given(method("GET"))
            .and(path("/api/tasks/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"task_id":TASK_ID,"status":"FAILURE","result":result}
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/documents/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let fixture = Fixture::new();
        let mut store = seeded(&fixture);
        let client = make_client(&server);
        let target = make_target(&client);
        let report = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!((report.failed, report.uncertain), (1, 0));
        assert_eq!(record(&ledger(&store))["state"]["failure_code"], code);
        let serialized = serde_json::to_string(&report).unwrap();
        let journal = serde_json::to_string(&ledger(&store)).unwrap();
        for private in [
            "fake-secret",
            "private-synthetic.pdf",
            "private synthetic title",
        ] {
            assert!(!serialized.contains(private));
            assert!(!journal.contains(private));
        }
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/api/tasks/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/documents/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        let repeat = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!((repeat.failed, repeat.uncertain), (1, 0));
        assert_eq!(
            serde_json::to_value(&repeat.items[0]).unwrap()["reason_code"],
            code
        );
        assert_eq!(post_count(&server).await, 0);
    }
}

#[tokio::test]
async fn existing_duplicates_require_original_bytes_and_preserve_their_review_state() {
    for related_document in [Some(42), None] {
        let server = MockServer::start().await;
        upload(&server, 200).await;
        Mock::given(method("GET"))
            .and(path("/api/tasks/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"task_id":TASK_ID,"status":"FAILURE","related_document":related_document,
                 "result":"synthetic.pdf: Not consuming synthetic.pdf: It is a duplicate of private synthetic title fake-secret (#42)."}
            ])))
            .mount(&server)
            .await;
        original(&server, 42, pdf()).await;
        let fixture = Fixture::new();
        let mut store = seeded(&fixture);
        let client = make_client(&server);
        let target = make_target(&client);
        let report = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!(
            (report.delivered, report.already_delivered, report.failed),
            (0, 1, 0)
        );
        assert_eq!(
            report.items[0].reason_code,
            Some(DeliveryReasonCode::Duplicate)
        );
        assert_eq!(report.items[0].document_id, Some(42));
        assert_eq!(
            record(&ledger(&store))["state"]["provenance"],
            "existing_duplicate"
        );
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains("fake-secret"));
        assert!(!serialized.contains("private synthetic title"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3); // POST, task GET, original GET; no retagging.
        server.reset().await;
        original(&server, 42, pdf()).await;
        let repeat = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!(repeat.already_delivered, 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn a_duplicate_reference_with_wrong_original_never_confirms_or_reposts() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    task(&server, "FAILURE", Some(73)).await;
    let mut different = pdf();
    different[12] ^= 1; // Same length still requires an exact SHA256 match.
    original(&server, 73, different).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    for _ in 0..2 {
        let report = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!(
            (report.delivered, report.already_delivered, report.failed),
            (0, 0, 1)
        );
        assert_eq!(
            report.items[0].reason_code,
            Some(DeliveryReasonCode::Duplicate)
        );
        assert_eq!(
            report.items[0].recovery_action,
            Some(DeliveryRecoveryAction::ReconcileExplicitly)
        );
        assert_eq!(report.items[0].document_id, Some(73));
        assert_eq!(record(&ledger(&store))["state"]["kind"], "task_failed");
    }
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn an_old_failed_journal_can_recover_a_duplicate_reference_using_only_get_requests() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    task(&server, "FAILURE", None).await;
    Mock::given(method("GET"))
        .and(path("/api/documents/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    let mut old = ledger(&store);
    for record in old["entries"].as_object_mut().unwrap().values_mut() {
        record["state"] = json!({"kind":"task_failed","task_id":TASK_ID});
    }
    store
        .write_private_state(
            "paperless-delivery.json",
            &serde_json::to_vec(&old).unwrap(),
        )
        .unwrap();
    assert_eq!(status(&store, &target).unwrap().task_failed, 1);
    server.reset().await;
    task(&server, "FAILURE", Some(42)).await;
    original(&server, 42, pdf()).await;
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.already_delivered, 1);
    assert_eq!(
        record(&ledger(&store))["state"]["provenance"],
        "existing_duplicate"
    );
    assert_eq!(post_count(&server).await, 0);
}

#[tokio::test]
async fn a_retained_duplicate_reference_survives_a_missing_or_sparse_task_without_reposting() {
    for sparse_task in [false, true] {
        let server = MockServer::start().await;
        upload(&server, 200).await;
        task(&server, "FAILURE", Some(42)).await;
        Mock::given(method("GET"))
            .and(path("/api/documents/42/download/"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let fixture = Fixture::new();
        let mut store = seeded(&fixture);
        let client = make_client(&server);
        let target = make_target(&client);
        let failed = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!(failed.failed, 1);
        assert_eq!(
            failed.items[0].reason_code,
            Some(DeliveryReasonCode::Duplicate)
        );
        assert_eq!(
            failed.items[0].recovery_action,
            Some(DeliveryRecoveryAction::ReconcileExplicitly)
        );
        assert_eq!(record(&ledger(&store))["state"]["related_document_id"], 42);
        server.reset().await;
        if sparse_task {
            task(&server, "FAILURE", None).await;
        } else {
            Mock::given(method("GET"))
                .and(path("/api/tasks/"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
                .mount(&server)
                .await;
        }
        original(&server, 42, pdf()).await;
        let recovered = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!(recovered.already_delivered, 1);
        assert_eq!(
            recovered.items[0].reason_code,
            Some(DeliveryReasonCode::Duplicate)
        );
        assert_eq!(post_count(&server).await, 0);
    }
}

#[tokio::test]
async fn duplicate_original_access_failures_keep_authentication_or_redirect_recovery_guidance() {
    for status in [401, 403, 302] {
        let server = MockServer::start().await;
        upload(&server, 200).await;
        task(&server, "FAILURE", Some(42)).await;
        Mock::given(method("GET"))
            .and(path("/api/documents/42/download/"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", "/synthetic-sign-in")
                    .set_body_string("private synthetic title fake-secret"),
            )
            .mount(&server)
            .await;
        let fixture = Fixture::new();
        let mut store = seeded(&fixture);
        let client = make_client(&server);
        let target = make_target(&client);
        let report = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!(
            (report.failed, report.delivered, report.already_delivered),
            (1, 0, 0)
        );
        let (reason, action) = if status == 302 {
            assert!(report.access_blocked);
            (
                DeliveryReasonCode::AccessBlocked,
                DeliveryRecoveryAction::CheckDestination,
            )
        } else {
            assert!(report.authentication_failed);
            (
                DeliveryReasonCode::AuthenticationFailed,
                DeliveryRecoveryAction::RestoreAccess,
            )
        };
        assert_eq!(report.items[0].reason_code, Some(reason));
        assert_eq!(report.items[0].recovery_action, Some(action));
        assert_eq!(record(&ledger(&store))["state"]["kind"], "task_failed");
        assert_eq!(post_count(&server).await, 1);
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("fake-secret")
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
}

#[tokio::test]
async fn conflicting_duplicate_references_stay_failed_without_guessing_a_document_or_reposting() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"task_id":TASK_ID,"status":"FAILURE","related_document":42,
             "result":"Not consuming: It is a duplicate of document #73"}
        ])))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    for _ in 0..2 {
        let report = deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap();
        assert_eq!(
            (
                report.failed,
                report.uncertain,
                report.delivered,
                report.already_delivered
            ),
            (1, 0, 0, 0)
        );
        assert_eq!(record(&ledger(&store))["state"]["kind"], "pending");
    }
    assert_eq!(post_count(&server).await, 1);
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().contains("/download/"))
    );
}

#[tokio::test]
async fn different_source_letters_with_identical_bytes_keep_separate_duplicate_records() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK_ID))
        .expect(2)
        .mount(&server)
        .await;
    task(&server, "FAILURE", Some(42)).await;
    original(&server, 42, pdf()).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    store
        .save(
            "different-synthetic-letter",
            "archive:root",
            &pdf(),
            Some("application/pdf"),
        )
        .unwrap();
    let client = make_client(&server);
    let target = make_target(&client);
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.already_delivered, 2);
    assert_eq!(status(&store, &target).unwrap().verified, 2);
    assert_eq!(ledger(&store)["entries"].as_object().unwrap().len(), 2);
    assert_ne!(report.items[0].document_ref, report.items[1].document_ref);
}

#[tokio::test]
async fn wrong_remote_original_never_confirms_delivery_and_is_reverified() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    task(&server, "SUCCESS", Some(101)).await;
    original(
        &server,
        101,
        b"%PDF-1.4\ndifferent synthetic document\n%%EOF\n".to_vec(),
    )
    .await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    let failed = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(failed.failed, 1);
    assert_eq!(failed.delivered, 0);
    assert_eq!(record(&ledger(&store))["state"]["kind"], "pending");
    server.reset().await;
    success(&server, 101).await;
    assert_eq!(
        deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap()
            .delivered,
        1
    );
    assert_eq!(post_count(&server).await, 0);
}

#[tokio::test]
async fn missing_review_tags_or_inbox_flag_are_explicit_failures() {
    for (tags, inbox) in [(vec![12], true), (vec![11], true), (vec![11, 12], false)] {
        let server = MockServer::start().await;
        upload(&server, 200).await;
        task(&server, "SUCCESS", Some(101)).await;
        original(&server, 101, pdf()).await;
        review_metadata(&server, 101, tags, inbox).await;
        let fixture = Fixture::new();
        let mut store = seeded(&fixture);
        let client = make_client(&server);
        let report = deliver(
            &mut store,
            &client,
            &make_target(&client),
            options(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(report.delivered, 0);
        assert!(report.has_attention());
    }
}

#[tokio::test]
async fn reviewed_document_can_remove_inbox_tag_on_verified_repeat() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    success(&server, 101).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    server.reset().await;
    original(&server, 101, pdf()).await;
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.already_delivered, 1);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.path().ends_with("/download/"));
}

#[tokio::test]
async fn deleted_previously_verified_document_reports_attention_without_reupload() {
    let server = MockServer::start().await;
    upload(&server, 200).await;
    success(&server, 101).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/documents/101/download/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.already_delivered, 0);
    assert_eq!(record(&ledger(&store))["state"]["kind"], "verified");
    assert_eq!(post_count(&server).await, 0);
}

#[tokio::test]
async fn explicit_adoption_requires_hash_and_tags_and_never_posts() {
    let server = MockServer::start().await;
    original(&server, 301, pdf()).await;
    review_metadata(&server, 301, vec![11, 12], true).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    let item = reconcile_document(&mut store, &client, &target, LETTER, 301)
        .await
        .unwrap();
    assert_eq!(item.status, DeliveryItemStatus::Delivered);
    assert_eq!(item.document_id, Some(301));
    assert_eq!(status(&store, &target).unwrap().verified, 1);
    assert_eq!(post_count(&server).await, 0);
    assert!(
        reconcile_document(&mut store, &client, &target, LETTER, 302)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn adoption_rejects_unknown_letter_non_pdf_and_changed_bytes() {
    let server = MockServer::start().await;
    original(&server, 301, b"different bytes".to_vec()).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    store
        .save("synthetic-binary", "inbox", b"\0binary", None)
        .unwrap();
    let client = make_client(&server);
    let target = make_target(&client);
    assert!(
        reconcile_document(&mut store, &client, &target, "unknown", 301)
            .await
            .is_err()
    );
    assert!(
        reconcile_document(&mut store, &client, &target, "synthetic-binary", 301)
            .await
            .is_err()
    );
    assert!(
        reconcile_document(&mut store, &client, &target, LETTER, 301)
            .await
            .is_err()
    );
    assert!(
        store
            .read_private_state("paperless-delivery.json")
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn ambiguous_identical_candidates_stay_uncertain_until_explicit_adoption() {
    let server = MockServer::start().await;
    upload(&server, 503).await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    let filename = record(&ledger(&store))["filename"]
        .as_str()
        .unwrap()
        .to_owned();
    lookup(&server, &filename, &[101, 102]).await;
    original(&server, 101, pdf()).await;
    original(&server, 102, pdf()).await;
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.uncertain, 1);
    assert!(report.items[0].error.as_ref().unwrap().contains("multiple"));
    review_metadata(&server, 102, vec![11, 12], true).await;
    assert_eq!(
        reconcile_document(&mut store, &client, &target, LETTER, 102)
            .await
            .unwrap()
            .document_id,
        Some(102)
    );
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn unsupported_content_and_document_limit_are_partial_with_no_silent_loss() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    let mut store = fixture.store("unsupported");
    store
        .save("synthetic-text", "inbox", b"plain synthetic message", None)
        .unwrap();
    let client = make_client(&server);
    let target = make_target(&client);
    let report = deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.unsupported, 1);
    assert!(report.has_attention());
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(
        store
            .read_private_state("paperless-delivery.json")
            .unwrap()
            .is_none()
    );
    drop(store);
    let mut store = fixture.store("limited");
    store.save("one-synthetic", "inbox", &pdf(), None).unwrap();
    store
        .save("two-synthetic", "archive:root", &pdf(), None)
        .unwrap();
    upload(&server, 200).await;
    success(&server, 101).await;
    let options = DeliveryOptions {
        max_documents: 1,
        ..options()
    };
    let report = deliver(&mut store, &client, &target, options, |_| {})
        .await
        .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(report.deferred_limit, 1);
    assert!(report.has_attention());
    assert_eq!(post_count(&server).await, 1);
}

#[tokio::test]
async fn namespace_url_hash_and_version_conflicts_fail_before_any_remote_call() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    upload(&server, 503).await;
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    let valid = ledger(&store);
    drop(store);
    server.reset().await;
    for mutate in 0..9 {
        let mut corrupt = valid.clone();
        match mutate {
            0 => corrupt["namespace_hash"] = json!("different namespace"),
            1 => corrupt["url"] = json!("https://different.example.invalid/"),
            2 => corrupt["schema_version"] = json!(2),
            3 => {
                corrupt["entries"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()["sha256"] = json!("00".repeat(32))
            }
            4 => {
                corrupt["entries"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()["filename"] = json!("../../outside.pdf")
            }
            5 => {
                corrupt["entries"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()["bytes"] = json!(1)
            }
            6 => {
                corrupt["entries"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()["state"] = json!({"kind":"pending","task_id":"not-a-valid-task"})
            }
            7 => {
                corrupt["entries"]
                    .as_object_mut()
                    .unwrap()
                    .values_mut()
                    .next()
                    .unwrap()["state"] = json!({"kind":"verified","document_id":0})
            }
            _ => corrupt["token"] = json!("synthetic-secret-field-that-must-be-rejected"),
        }
        fs::write(
            fixture.state_path("synthetic-private", "paperless-delivery.json"),
            serde_json::to_vec(&corrupt).unwrap(),
        )
        .unwrap();
        let mut store = fixture.store("synthetic-private");
        assert!(status(&store, &target).is_err());
        assert!(
            deliver(&mut store, &client, &target, options(), |_| {})
                .await
                .is_err()
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn copied_ledger_cannot_cross_mailbox_and_client_cannot_cross_instance() {
    let server = MockServer::start().await;
    let other_server = MockServer::start().await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    upload(&server, 503).await;
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    let raw = store
        .read_private_state("paperless-delivery.json")
        .unwrap()
        .unwrap();
    let mut other_store = fixture.store("synthetic-business");
    other_store.save(LETTER, "inbox", &pdf(), None).unwrap();
    other_store
        .write_private_state("paperless-delivery.json", &raw)
        .unwrap();
    assert!(
        deliver(&mut other_store, &client, &target, options(), |_| {})
            .await
            .is_err()
    );
    let other_client = make_client(&other_server);
    assert!(
        deliver(&mut store, &other_client, &target, options(), |_| {})
            .await
            .is_err()
    );
    let other_target = make_target(&other_client);
    assert!(save_target(&mut store, &other_target).is_err());
    assert!(other_server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn local_file_and_live_ledger_changes_are_detected_before_remote_io() {
    let server = MockServer::start().await;
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let client = make_client(&server);
    let target = make_target(&client);
    upload(&server, 503).await;
    deliver(&mut store, &client, &target, options(), |_| {})
        .await
        .unwrap();
    fs::write(
        fixture.state_path("synthetic-private", "paperless-delivery.json"),
        b"{}",
    )
    .unwrap();
    server.reset().await;
    assert!(
        deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .is_err()
    );
    drop(store);
    let fixture = Fixture::new();
    let mut store = seeded(&fixture);
    let saved = store.verify_existing(LETTER, "inbox").unwrap().unwrap();
    fs::write(saved.path, b"changed bytes").unwrap();
    assert!(
        deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .is_err()
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn configuration_is_secret_free_strict_and_stays_in_the_exact_namespace() {
    let fixture = Fixture::new();
    let mut store = fixture.store("one");
    let target = PaperlessTarget {
        schema_version: 1,
        url: "https://paperless.example.invalid/".into(),
        inbox_tag_id: 11,
        source_tag_id: None,
        remember_token: true,
    };
    assert!(load_target(&store).is_err());
    save_target(&mut store, &target).unwrap();
    assert_eq!(load_target(&store).unwrap(), target);
    let raw = store
        .read_private_state("paperless-target.json")
        .unwrap()
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("fake-paperless-token"));
    let other = fixture.store("two");
    assert!(load_target(&other).is_err());
    let mut invalid: Value = serde_json::from_slice(&raw).unwrap();
    invalid["token"] = json!("synthetic-secret-that-must-be-rejected");
    store
        .write_private_state(
            "paperless-target.json",
            &serde_json::to_vec(&invalid).unwrap(),
        )
        .unwrap();
    assert!(load_target(&store).is_err());
}

#[test]
fn invalid_options_urls_and_tag_ids_are_rejected() {
    let fixture = Fixture::new();
    let mut store = fixture.store("one");
    for (url, inbox, source, version) in [
        ("http://paperless.example.invalid/", 11, None, 1),
        ("https://paperless.example.invalid", 11, None, 1),
        ("https://paperless.example.invalid/", 0, None, 1),
        ("https://paperless.example.invalid/", 11, Some(-1), 1),
        ("https://paperless.example.invalid/", 11, None, 2),
    ] {
        assert!(
            save_target(
                &mut store,
                &PaperlessTarget {
                    schema_version: version,
                    url: url.into(),
                    inbox_tag_id: inbox,
                    source_tag_id: source,
                    remember_token: false
                }
            )
            .is_err()
        );
    }
}
