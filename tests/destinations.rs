//! Named delivery tests use only synthetic temporary stores and localhost APIs.
use epost_cli::delivery::{self, DeliveryOptions, NamedDeliveryOptions, PaperlessTarget};
use epost_cli::destinations::{
    self, CredentialBinding, Destination, DestinationKind, DocumentTypePolicy, MetadataValue,
    SourceField,
};
use epost_cli::models::Letter;
use epost_cli::paperless::{PaperlessClient, PaperlessOptions, UploadMetadata};
use epost_cli::secret::Secret;
use epost_cli::storage::Store;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TASK: &str = "11111111-2222-4333-8444-555555555555";
const LETTER: &str = "synthetic-letter";

struct Fixture {
    temp: TempDir,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("source");
        Self { temp, root }
    }
    fn store(&self, namespace: &str) -> Store {
        Store::open(&self.root, namespace).unwrap()
    }
    fn path(&self, namespace: &str, name: &str) -> PathBuf {
        self.root
            .join(hex::encode(Sha256::digest(namespace.as_bytes())))
            .join(name)
    }
    fn folder(&self, name: &str) -> Destination {
        Destination {
            name: name.into(),
            credential_binding: CredentialBinding::Named,
            kind: DestinationKind::Folder {
                path: self.temp.path().canonicalize().unwrap().join(name),
            },
        }
    }
}
fn pdf() -> Vec<u8> {
    b"%PDF-1.4\nsynthetic PDF\n%%EOF\n".to_vec()
}
fn seed(store: &mut Store) {
    store
        .save(LETTER, "inbox", &pdf(), Some("application/pdf"))
        .unwrap();
}
fn options() -> DeliveryOptions {
    DeliveryOptions {
        max_documents: 10,
        poll_attempts: 1,
        poll_interval_ms: 0,
    }
}
fn named_options(audit: bool) -> NamedDeliveryOptions {
    NamedDeliveryOptions {
        delivery: options(),
        audit,
    }
}
fn make_client(server: &MockServer) -> PaperlessClient {
    PaperlessClient::for_test(
        &server.uri(),
        Secret::new("fake-test-token".into()),
        PaperlessOptions {
            max_retries: 0,
            ..Default::default()
        },
    )
    .unwrap()
}
fn target(client: &PaperlessClient) -> PaperlessTarget {
    PaperlessTarget {
        schema_version: 1,
        url: client.canonical_url().into(),
        inbox_tag_id: 11,
        source_tag_id: Some(12),
        remember_token: false,
    }
}
fn paperless(client: &PaperlessClient, name: &str) -> Destination {
    Destination {
        name: name.into(),
        credential_binding: CredentialBinding::Named,
        kind: DestinationKind::Paperless(target(client).into()),
    }
}
async fn mock_tag(server: &MockServer, id: i64, inbox: bool) {
    Mock::given(method("GET"))
        .and(path(format!("/api/tags/{id}/")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"id":id,"name":format!("Synthetic {id}"),"is_inbox_tag":inbox}),
            ),
        )
        .mount(server)
        .await;
}
async fn mock_catalogs(server: &MockServer) {
    mock_tag(server, 11, true).await;
    mock_tag(server, 12, false).await;
}
async fn mock_success(server: &MockServer, tags: Vec<i64>) {
    mock_catalogs(server).await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!([{"task_id":TASK,"status":"SUCCESS","related_document_ids":[41]}]),
        ))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/41/download/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(pdf()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/41/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":41,"tags":tags})))
        .mount(server)
        .await;
}

#[test]
fn all_destinations_are_selected_and_configuration_is_bound_strict_and_bounded() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    for name in ["second", "first"] {
        destinations::save(&mut store, &fixture.folder(name)).unwrap();
    }
    assert_eq!(
        destinations::select(&store, &[])
            .unwrap()
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert_eq!(
        destinations::select(&store, &["second".into()]).unwrap()[0].name,
        "second"
    );
    assert!(destinations::select(&store, &["absent".into()]).is_err());
    assert!(destinations::select(&store, &["first".into(), "first".into()]).is_err());
    for name in ["", "..", ".hidden", "a/b", "space name", "é"] {
        assert!(destinations::validate_name(name).is_err());
    }
    assert!(destinations::validate_name(&"a".repeat(65)).is_err());
    for n in 2..64 {
        destinations::save(&mut store, &fixture.folder(&format!("folder-{n}"))).unwrap();
    }
    assert!(destinations::save(&mut store, &fixture.folder("over-limit")).is_err());
    let raw = store
        .read_private_state("destinations.json")
        .unwrap()
        .unwrap();
    let mut other = fixture.store("other-synthetic");
    other
        .write_private_state("destinations.json", &raw)
        .unwrap();
    assert!(destinations::list(&other).is_err());
}

#[test]
fn metadata_is_minimal_optional_validated_and_namespace_bound() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    assert_eq!(store.source_metadata(LETTER).unwrap(), None);
    let letter:Letter = serde_json::from_value(json!({"id":LETTER,"letterTitle":"Synthetic title","fileName":"original.pdf","receivedDateTime":"2026-10-12T09:10:11.123+02:00","documentTypes":["invoice"],"senderUserId":"excluded-sender","letterContentReference":"https://example.invalid/excluded-content","description":"excluded-description"})).unwrap();
    store.record_metadata(&letter).unwrap();
    let metadata = store.source_metadata(LETTER).unwrap().unwrap();
    assert_eq!(
        metadata.received_at.as_deref(),
        Some("2026-10-12T09:10:11.123+02:00")
    );
    let raw = store
        .read_private_state("source-metadata.json")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(raw.clone()).unwrap();
    assert!(!text.contains("excluded"));
    let mut other = fixture.store("other-synthetic");
    other
        .write_private_state("source-metadata.json", &raw)
        .unwrap();
    assert!(other.source_metadata(LETTER).is_err());
    let invalid: Letter =
        serde_json::from_value(json!({"id":"another","documentTypes":"wrong-type"})).unwrap();
    assert!(store.record_metadata(&invalid).is_err());
    store
        .record_metadata(&Letter {
            id: Some("optional".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        store.source_metadata("optional").unwrap().unwrap(),
        Default::default()
    );
}

#[tokio::test]
async fn legacy_migration_preserves_verified_history_and_normal_check_uses_no_remote_calls() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    let server = MockServer::start().await;
    let client = make_client(&server);
    let target = target(&client);
    mock_success(&server, vec![11, 12]).await;
    delivery::save_target(&mut store, &target).unwrap();
    assert_eq!(
        delivery::deliver(&mut store, &client, &target, options(), |_| {})
            .await
            .unwrap()
            .delivered,
        1
    );
    let migrated = destinations::load(&store, "paperless").unwrap();
    assert_eq!(migrated.credential_binding, CredentialBinding::Legacy);
    destinations::save(&mut store, &fixture.folder("copies")).unwrap();
    assert_eq!(destinations::list(&store).unwrap().len(), 2);
    server.reset().await;
    let report =
        delivery::deliver_paperless(&mut store, &client, &migrated, named_options(false), |_| {})
            .await
            .unwrap();
    assert_eq!(report.previously_confirmed, 1);
    assert!(!report.has_attention());
    assert!(server.received_requests().await.unwrap().is_empty());
    let audit =
        delivery::deliver_paperless(&mut store, &client, &migrated, named_options(true), |_| {})
            .await
            .unwrap();
    assert_eq!(audit.failed, 1);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].url.path().ends_with("/download/"));
    destinations::remove(&mut store, "paperless").unwrap();
    assert!(destinations::save(&mut store, &paperless(&client, "paperless")).is_err());
    destinations::save(&mut store, &migrated).unwrap();
    assert_eq!(
        delivery::destination_status(&store, &migrated)
            .unwrap()
            .verified,
        1
    );
}

#[tokio::test]
async fn named_histories_are_isolated_and_endpoint_rebinding_is_rejected_even_after_removal() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    let server = MockServer::start().await;
    let client = make_client(&server);
    mock_success(&server, vec![11, 12]).await;
    let first = paperless(&client, "first");
    let second = paperless(&client, "second");
    destinations::save(&mut store, &first).unwrap();
    destinations::save(&mut store, &second).unwrap();
    assert_eq!(
        delivery::deliver_paperless(&mut store, &client, &first, named_options(false), |_| {})
            .await
            .unwrap()
            .delivered,
        1
    );
    assert_eq!(
        delivery::destination_status(&store, &second)
            .unwrap()
            .not_submitted,
        1
    );
    assert_eq!(
        delivery::deliver_paperless(&mut store, &client, &second, named_options(false), |_| {})
            .await
            .unwrap()
            .delivered,
        1
    );
    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .collect();
    assert_eq!(posts.len(), 2);
    assert_ne!(posts[0].body, posts[1].body);
    let other_server = MockServer::start().await;
    let rebound = paperless(&make_client(&other_server), "first");
    assert!(destinations::save(&mut store, &rebound).is_err());
    destinations::remove(&mut store, "first").unwrap();
    assert!(destinations::save(&mut store, &rebound).is_err());
    destinations::save(&mut store, &first).unwrap();
    assert_eq!(
        delivery::destination_status(&store, &first)
            .unwrap()
            .verified,
        1
    );
    let raw = store
        .read_private_state(&destinations::ledger_name(&first))
        .unwrap()
        .unwrap();
    let mut other = fixture.store("other-synthetic");
    seed(&mut other);
    other
        .write_private_state(&destinations::ledger_name(&first), &raw)
        .unwrap();
    assert!(delivery::destination_status(&other, &first).is_err());
}

#[tokio::test]
async fn title_tags_text_provenance_and_exact_unambiguous_document_type_are_uploaded_without_created()
 {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    store.record_metadata(&serde_json::from_value(json!({"id":LETTER,"letterTitle":"Synthetic bill","fileName":"bill.pdf","receivedDateTime":"2026-10-12T09:10:11+02:00","documentTypes":["invoice","unrecognized"]})).unwrap()).unwrap();
    let server = MockServer::start().await;
    let client = make_client(&server);
    mock_success(&server, vec![11, 12, 13]).await;
    mock_tag(&server, 13, false).await;
    Mock::given(method("GET"))
        .and(path("/api/custom_fields/21/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":21,"name":"Received","data_type":"string"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/document_types/31/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":31,"name":"Invoice"})))
        .mount(&server)
        .await;
    let mut destination = paperless(&client, "metadata");
    let DestinationKind::Paperless(config) = &mut destination.kind else {
        unreachable!()
    };
    config.extra_tag_ids = vec![13];
    config
        .custom_fields
        .insert(21, MetadataValue::Source(SourceField::ReceivedAt));
    config.document_type = DocumentTypePolicy::Mapping(BTreeMap::from([("invoice".into(), 31)]));
    let report = delivery::deliver_paperless(
        &mut store,
        &client,
        &destination,
        named_options(false),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.delivered, 1);
    let requests = server.received_requests().await.unwrap();
    let post = requests
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .unwrap();
    let body = String::from_utf8_lossy(&post.body);
    assert!(body.contains("name=\"title\"\r\n\r\nSynthetic bill"));
    assert!(body.contains("name=\"document_type\"\r\n\r\n31"));
    assert!(body.contains("{\"21\":\"2026-10-12T09:10:11+02:00\"}"));
    assert!(!body.contains("name=\"created\""));
    server.reset().await;
    assert_eq!(
        delivery::deliver_paperless(
            &mut store,
            &client,
            &destination,
            named_options(false),
            |_| {}
        )
        .await
        .unwrap()
        .previously_confirmed,
        1
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn conflicting_type_matches_are_omitted_and_old_cached_pdfs_need_no_metadata() {
    for with_metadata in [false, true] {
        let fixture = Fixture::new();
        let mut store = fixture.store("synthetic");
        seed(&mut store);
        if with_metadata {
            store
                .record_metadata(
                    &serde_json::from_value(
                        json!({"id":LETTER,"documentTypes":["invoice","contract"]}),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let server = MockServer::start().await;
        let client = make_client(&server);
        mock_success(&server, vec![11, 12]).await;
        for id in [31, 32] {
            Mock::given(method("GET"))
                .and(path(format!("/api/document_types/{id}/")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"id":id,"name":"Synthetic type"})),
                )
                .mount(&server)
                .await;
        }
        let mut destination = paperless(&client, "mapping");
        let DestinationKind::Paperless(config) = &mut destination.kind else {
            unreachable!()
        };
        config.document_type = DocumentTypePolicy::Mapping(BTreeMap::from([
            ("invoice".into(), 31),
            ("contract".into(), 32),
        ]));
        assert_eq!(
            delivery::deliver_paperless(
                &mut store,
                &client,
                &destination,
                named_options(false),
                |_| {}
            )
            .await
            .unwrap()
            .delivered,
            1
        );
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(
            &requests
                .iter()
                .find(|r| r.method.as_str() == "POST")
                .unwrap()
                .body,
        );
        assert!(!body.contains("name=\"document_type\""));
        assert!(!body.contains("name=\"created\""));
        assert!(!body.contains("name=\"title\""));
    }
}

#[tokio::test]
async fn folder_atomic_repeat_limits_and_failure_of_one_destination_preserve_other_destinations() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    store
        .save("second-letter", "archive", &pdf(), Some("application/pdf"))
        .unwrap();
    store
        .save("non-pdf", "inbox", b"{}", Some("application/json"))
        .unwrap();
    let first = fixture.folder("first");
    let second = fixture.folder("second");
    destinations::save(&mut store, &first).unwrap();
    destinations::save(&mut store, &second).unwrap();
    let report = delivery::deliver_folder(
        &mut store,
        &first,
        DeliveryOptions {
            max_documents: 1,
            ..options()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(report.deferred_limit, 1);
    assert_eq!(report.unsupported, 1);
    let repeat = delivery::deliver_folder(
        &mut store,
        &first,
        DeliveryOptions {
            max_documents: 1,
            ..options()
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(repeat.delivered, 1);
    assert_eq!(repeat.already_delivered, 1);
    let third = fixture.folder("blocked");
    let DestinationKind::Folder { path } = &third.kind else {
        unreachable!()
    };
    fs::write(path, b"blocked synthetic destination").unwrap();
    assert!(
        delivery::deliver_folder(&mut store, &third, options(), |_| {})
            .await
            .is_err()
    );
    assert_eq!(
        delivery::deliver_folder(&mut store, &second, options(), |_| {})
            .await
            .unwrap()
            .delivered,
        2
    );
    let same = Destination {
        name: "same-root".into(),
        credential_binding: CredentialBinding::Named,
        kind: DestinationKind::Folder {
            path: fixture.root.clone(),
        },
    };
    assert!(destinations::save(&mut store, &same).is_err());
    assert!(
        delivery::deliver_folder(&mut store, &same, options(), |_| {})
            .await
            .is_err()
    );
    let mut changed = first.clone();
    changed.kind = second.kind.clone();
    assert!(destinations::save(&mut store, &changed).is_err());
}

#[tokio::test]
async fn named_uncertain_post_and_pending_task_never_repost() {
    for successful_post in [false, true] {
        let fixture = Fixture::new();
        let mut store = fixture.store("synthetic");
        seed(&mut store);
        let server = MockServer::start().await;
        let client = make_client(&server);
        let destination = paperless(&client, "test");
        mock_catalogs(&server).await;
        let response = if successful_post {
            ResponseTemplate::new(200).set_body_json(TASK)
        } else {
            ResponseTemplate::new(503)
        };
        Mock::given(method("POST"))
            .and(path("/api/documents/post_document/"))
            .respond_with(response)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tasks/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!([{"task_id":TASK,"status":"PENDING","related_document_ids":[]}]),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/documents/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"count":0,"next":null,"results":[]})),
            )
            .mount(&server)
            .await;
        for _ in 0..2 {
            let report = delivery::deliver_paperless(
                &mut store,
                &client,
                &destination,
                named_options(false),
                |_| {},
            )
            .await
            .unwrap();
            assert!(report.has_attention());
            assert_eq!(report.delivered, 0);
        }
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.method.as_str() == "POST")
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn named_explicit_reconciliation_preserves_reviewed_existing_metadata_after_byte_proof() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    let server = MockServer::start().await;
    let client = make_client(&server);
    let mut destination = paperless(&client, "reviewed");
    let DestinationKind::Paperless(config) = &mut destination.kind else {
        unreachable!()
    };
    config.extra_tag_ids = vec![13];
    // The existing PDF has been reviewed: no inbox/source/extra tags are
    // required, fetched or changed by this explicit adoption.
    Mock::given(method("GET"))
        .and(path("/api/documents/42/download/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(pdf()))
        .mount(&server)
        .await;
    let report = delivery::reconcile_paperless(&mut store, &client, &destination, LETTER, 42)
        .await
        .unwrap();
    assert_eq!(
        report.status,
        delivery::DeliveryItemStatus::AlreadyDelivered
    );
    assert_eq!(report.document_id, Some(42));
    let name = destinations::ledger_name(&destination);
    let ledger: Value =
        serde_json::from_slice(&store.read_private_state(&name).unwrap().unwrap()).unwrap();
    let records = ledger["entries"].as_object().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records.values().next().unwrap()["state"]["provenance"],
        "existing_document"
    );
    assert!(
        store
            .read_private_state("paperless-delivery.json")
            .unwrap()
            .is_none()
    );
    let repeated = delivery::reconcile_paperless(&mut store, &client, &destination, LETTER, 42)
        .await
        .unwrap();
    assert_eq!(
        repeated.status,
        delivery::DeliveryItemStatus::AlreadyDelivered
    );
    let cached = delivery::deliver_paperless(
        &mut store,
        &client,
        &destination,
        named_options(false),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(cached.previously_confirmed, 1);
    let audit = delivery::verify_paperless(&mut store, &client, &destination, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(audit.already_delivered, 1);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests
            .iter()
            .all(|request| request.method.as_str() == "GET"
                && request.url.path() == "/api/documents/42/download/")
    );
}

#[tokio::test]
async fn named_explicit_reconciliation_rejects_wrong_bytes_unknown_letters_and_cross_destination_ids()
 {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    let server = MockServer::start().await;
    let client = make_client(&server);
    let destination = paperless(&client, "explicit");
    let mut wrong_bytes = pdf();
    wrong_bytes[12] ^= 1;
    Mock::given(method("GET"))
        .and(path("/api/documents/73/download/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wrong_bytes))
        .mount(&server)
        .await;
    for (letter, id) in [(LETTER, 73), ("unknown-synthetic-letter", 73), (LETTER, 0)] {
        assert!(
            delivery::reconcile_paperless(&mut store, &client, &destination, letter, id)
                .await
                .is_err()
        );
    }
    assert!(
        store
            .read_private_state(&destinations::ledger_name(&destination))
            .unwrap()
            .is_none()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    Mock::given(method("GET"))
        .and(path("/api/documents/42/download/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(pdf()))
        .mount(&server)
        .await;
    delivery::reconcile_paperless(&mut store, &client, &destination, LETTER, 42)
        .await
        .unwrap();
    assert!(
        delivery::reconcile_paperless(&mut store, &client, &destination, LETTER, 73)
            .await
            .is_err()
    );
    let mut other_instance = destination.clone();
    let DestinationKind::Paperless(config) = &mut other_instance.kind else {
        unreachable!()
    };
    config.target.url = "https://synthetic-paperless.invalid/".into();
    assert!(
        delivery::reconcile_paperless(&mut store, &client, &other_instance, LETTER, 42)
            .await
            .is_err()
    );
    // The binding and already verified ID are checked before remote requests.
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn custom_fields_must_be_text_and_validation_does_not_record_upload_intent() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    let server = MockServer::start().await;
    let client = make_client(&server);
    mock_catalogs(&server).await;
    Mock::given(method("GET"))
        .and(path("/api/custom_fields/21/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":21,"name":"Wrong type","data_type":"date"})),
        )
        .mount(&server)
        .await;
    let mut destination = paperless(&client, "fields");
    let DestinationKind::Paperless(config) = &mut destination.kind else {
        unreachable!()
    };
    config
        .custom_fields
        .insert(21, MetadataValue::Literal("synthetic".into()));
    let report = delivery::deliver_paperless(
        &mut store,
        &client,
        &destination,
        named_options(false),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(report.failed, 1);
    assert!(
        store
            .read_private_state(&destinations::ledger_name(&destination))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST")
            .count(),
        0
    );
    assert!(
        client
            .validate_upload_metadata(
                "synthetic.pdf",
                &pdf(),
                &UploadMetadata {
                    custom_fields: BTreeMap::from([(21, json!({"unexpected":"object"}))]),
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[test]
fn new_state_names_are_controlled_and_newly_injected_ledgers_are_not_adopted() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    let destination = fixture.folder("folder");
    let name = destinations::ledger_name(&destination);
    assert!(
        store
            .read_private_state("destination-../outside.json")
            .is_err()
    );
    assert!(
        store
            .read_private_state("destination-arbitrary.json")
            .is_err()
    );
    fs::write(fixture.path("synthetic", &name), b"injected").unwrap();
    assert!(store.read_private_state(&name).is_err());
    assert!(store.write_private_state(&name, b"{} ").is_err());
}

#[tokio::test]
async fn catalogs_and_text_creation_use_exact_routes_and_bounded_readback() {
    let server = MockServer::start().await;
    let client = make_client(&server);
    Mock::given(method("GET"))
        .and(path("/api/document_types/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"count":1,"next":null,"results":[{"id":31,"name":"Synthetic invoice"}]}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/api/custom_fields/")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"count":1,"next":null,"results":[{"id":21,"name":"Synthetic received","data_type":"string"}]}))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/custom_fields/"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({"id":22,"name":"Synthetic text","data_type":"string"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/document_types/"))
        .respond_with(
            ResponseTemplate::new(201).set_body_json(json!({"id":32,"name":"Synthetic contract"})),
        )
        .mount(&server)
        .await;
    assert_eq!(client.document_types().await.unwrap()[0].id, 31);
    assert_eq!(client.custom_fields().await.unwrap()[0].data_type, "string");
    assert_eq!(
        client
            .create_custom_field("Synthetic text")
            .await
            .unwrap()
            .id,
        22
    );
    assert_eq!(
        client
            .create_document_type("Synthetic contract")
            .await
            .unwrap()
            .id,
        32
    );
    let requests = server.received_requests().await.unwrap();
    let create = requests
        .iter()
        .find(|r| r.method.as_str() == "POST" && r.url.path() == "/api/custom_fields/")
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&create.body).unwrap(),
        json!({"name":"Synthetic text","data_type":"string"})
    );
}

#[tokio::test]
async fn one_conflicting_folder_copy_does_not_prevent_remaining_atomic_copies() {
    let fixture = Fixture::new();
    let mut source = fixture.store("synthetic");
    seed(&mut source);
    source
        .save("other-letter", "inbox", &pdf(), Some("application/pdf"))
        .unwrap();
    let destination = fixture.folder("copies");
    let DestinationKind::Folder { path } = &destination.kind else {
        unreachable!()
    };
    let namespace = format!(
        "epost-folder-v1:{}:{}",
        source.identity_hash(),
        destinations::destination_identity(&source, &destination)
    );
    let output = Store::open(path, &namespace).unwrap();
    let hash = output.identity_hash().to_owned();
    drop(output);
    let mut key = Sha256::new();
    key.update(b"epost-document-v1\0");
    key.update(hash.as_bytes());
    key.update(b"\0");
    key.update(LETTER.as_bytes());
    let conflict = path
        .join(hash)
        .join("data")
        .join(format!("{}.pdf", hex::encode(key.finalize())));
    let conflicting_bytes = b"%PDF-1.4\nconflicting synthetic content\n%%EOF\n";
    fs::write(&conflict, conflicting_bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&conflict, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let report = delivery::deliver_folder(&mut source, &destination, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.delivered, 1);
    assert!(report.has_attention());
    assert_eq!(fs::read(conflict).unwrap(), conflicting_bytes);
    assert_eq!(
        delivery::destination_status(&source, &destination)
            .unwrap()
            .verified,
        1
    );
    let output = Store::open(path, &namespace).unwrap();
    assert_eq!(output.read_saved("other-letter").unwrap(), pdf());
}

#[test]
fn credential_effect_failure_leaves_registry_unchanged_and_exact_hash_names_are_private() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    let first = fixture.folder("first");
    destinations::save(&mut store, &first).unwrap();
    let error = destinations::save_with_effect(
        &mut store,
        &fixture.folder("second"),
        || -> epost_cli::error::Result<()> {
            Err(epost_cli::error::Error::Config(
                "synthetic credential failure",
            ))
        },
        |_| Ok(()),
    );
    assert!(error.is_err());
    assert_eq!(destinations::list(&store).unwrap(), [first]);
    let candidate = fixture.folder("new");
    let name = destinations::ledger_name(&candidate);
    store.write_private_state(&name, b"{} ").unwrap();
    drop(store);
    let mut reopened = fixture.store("synthetic");
    assert_eq!(reopened.read_private_state(&name).unwrap().unwrap(), b"{} ");
    fs::write(fixture.path("synthetic", &name), b"changed").unwrap();
    assert!(reopened.write_private_state(&name, b"restored").is_err());
}

#[cfg(unix)]
#[test]
fn existing_named_state_symlinks_hardlinks_and_nonprivate_permissions_are_rejected_at_open() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for variant in 0..3 {
        let fixture = Fixture::new();
        let mut store = fixture.store("synthetic");
        let name = destinations::ledger_name(&fixture.folder("copies"));
        store.write_private_state(&name, b"{} ").unwrap();
        drop(store);
        let state = fixture.path("synthetic", &name);
        match variant {
            0 => {
                let other = fixture.temp.path().join("symlink-target");
                fs::write(&other, b"{} ").unwrap();
                fs::remove_file(&state).unwrap();
                symlink(other, &state).unwrap();
            }
            1 => {
                fs::hard_link(&state, fixture.temp.path().join("hardlink-target")).unwrap();
            }
            _ => fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap(),
        }
        assert!(Store::open(&fixture.root, "synthetic").is_err());
    }
}

#[tokio::test]
async fn missing_extra_tag_is_not_confirmed_and_preflight_auth_failure_stops_only_this_batch() {
    for auth_fail in [false, true] {
        let fixture = Fixture::new();
        let mut store = fixture.store("synthetic");
        seed(&mut store);
        store
            .save("other-letter", "inbox", &pdf(), Some("application/pdf"))
            .unwrap();
        let server = MockServer::start().await;
        let client = make_client(&server);
        let mut destination = paperless(&client, "tags");
        if auth_fail {
            Mock::given(method("GET"))
                .and(path("/api/tags/11/"))
                .respond_with(ResponseTemplate::new(403))
                .mount(&server)
                .await;
        } else {
            mock_success(&server, vec![11, 12]).await;
            mock_tag(&server, 13, false).await;
            let DestinationKind::Paperless(config) = &mut destination.kind else {
                unreachable!()
            };
            config.extra_tag_ids.push(13);
        }
        let report = delivery::deliver_paperless(
            &mut store,
            &client,
            &destination,
            named_options(false),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(report.delivered, 0);
        assert!(report.has_attention());
        if auth_fail {
            assert!(report.authentication_failed);
            assert_eq!(report.failed, 1);
            assert_eq!(report.not_attempted, 1);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        } else {
            assert_eq!(report.failed, 2);
            assert_eq!(
                delivery::destination_status(&store, &destination)
                    .unwrap()
                    .pending,
                2
            );
        }
    }
}

#[tokio::test]
async fn verify_never_submits_an_unrecorded_pdf_or_creates_a_folder_copy() {
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    seed(&mut store);
    let server = MockServer::start().await;
    let client = make_client(&server);
    let destination = paperless(&client, "verify");
    let report = delivery::verify_paperless(&mut store, &client, &destination, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.not_attempted, 1);
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(
        store
            .read_private_state(&destinations::ledger_name(&destination))
            .unwrap()
            .is_none()
    );
    let folder = fixture.folder("verify-folder");
    let DestinationKind::Folder { path: folder_path } = &folder.kind else {
        unreachable!()
    };
    let report = delivery::verify_folder(&mut store, &folder, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.not_attempted, 1);
    assert!(!folder_path.exists());
    mock_success(&server, vec![11, 12]).await;
    assert_eq!(
        delivery::deliver_paperless(
            &mut store,
            &client,
            &destination,
            named_options(false),
            |_| {}
        )
        .await
        .unwrap()
        .delivered,
        1
    );
    assert_eq!(
        delivery::deliver_folder(&mut store, &folder, options(), |_| {})
            .await
            .unwrap()
            .delivered,
        1
    );
    store
        .save(
            "new-unsubmitted-letter",
            "inbox",
            &pdf(),
            Some("application/pdf"),
        )
        .unwrap();
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/documents/41/download/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(pdf()))
        .mount(&server)
        .await;
    let report = delivery::verify_paperless(&mut store, &client, &destination, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.already_delivered, 1);
    assert_eq!(report.not_attempted, 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let report = delivery::verify_folder(&mut store, &folder, options(), |_| {})
        .await
        .unwrap();
    assert_eq!(report.already_delivered, 1);
    assert_eq!(report.not_attempted, 1);
    assert_eq!(
        delivery::destination_status(&store, &folder)
            .unwrap()
            .verified,
        1
    );
}

#[cfg(unix)]
#[test]
fn identical_replacement_of_a_pinned_private_state_file_is_detected() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let mut store = fixture.store("synthetic");
    let name = destinations::ledger_name(&fixture.folder("copies"));
    store.write_private_state(&name, b"{} ").unwrap();
    let replacement = fixture.temp.path().join("replacement");
    fs::write(&replacement, b"{} ").unwrap();
    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(replacement, fixture.path("synthetic", &name)).unwrap();
    assert!(store.read_private_state(&name).is_err());
}
