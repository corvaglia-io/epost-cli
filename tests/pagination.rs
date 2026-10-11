use epost_cli::api::{ApiClient, ClientOptions};
use epost_cli::error::Error;
use epost_cli::models::{Mailbox, MailboxKind};
use epost_cli::pagination::{Source, collect};
use epost_cli::secret::Secret;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer) -> ApiClient {
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(Mailbox {
            tenant_id: "synthetic-tenant".into(),
            company_id: 0,
            kind: MailboxKind::Private,
        }),
        ClientOptions {
            max_retries: 0,
            ..ClientOptions::default()
        },
    )
    .unwrap();
    client
        .set_bearer(Secret::new("synthetic-pagination-token".into()))
        .unwrap();
    client
}

async fn inbox_page(server: &MockServer, offset: u32, response: Value) {
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("limit", "2"))
        .and(query_param("offset", offset.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(response))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_inventory_terminates_after_one_page() {
    let server = MockServer::start().await;
    inbox_page(&server, 0, json!([])).await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 3)
        .await
        .unwrap();
    assert!(inventory.pagination_complete);
    assert_eq!(inventory.pages, 1);
    assert!(inventory.letters.is_empty());
    assert!(inventory.issues.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exact_full_page_requires_a_following_empty_page() {
    let server = MockServer::start().await;
    inbox_page(&server, 0, json!([{"id":"one"},{"id":"two"}])).await;
    inbox_page(&server, 2, json!([{"id":"three"},{"id":"four"}])).await;
    inbox_page(&server, 4, json!([])).await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 3)
        .await
        .unwrap();
    assert!(inventory.pagination_complete);
    assert_eq!(inventory.pages, 3);
    assert_eq!(inventory.letters.len(), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlaps_are_deduplicated_and_prevent_a_complete_claim() {
    let server = MockServer::start().await;
    inbox_page(&server, 0, json!([{"id":"one"},{"id":"two"}])).await;
    inbox_page(&server, 2, json!([{"id":"two"},{"id":"three"}])).await;
    inbox_page(&server, 4, json!([])).await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 3)
        .await
        .unwrap();
    assert!(!inventory.pagination_complete);
    assert_eq!(inventory.letters.len(), 3);
    assert!(!inventory.issues.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reordered_repeated_page_stops_without_requesting_another_offset() {
    let server = MockServer::start().await;
    inbox_page(&server, 0, json!([{"id":"one"},{"id":"two"}])).await;
    inbox_page(&server, 2, json!([{"id":"two"},{"id":"one"}])).await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 30)
        .await
        .unwrap();
    assert!(!inventory.pagination_complete);
    assert_eq!(inventory.pages, 2);
    assert_eq!(inventory.letters.len(), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_limit_preserves_retrieved_metadata() {
    let server = MockServer::start().await;
    inbox_page(&server, 0, json!([{"id":"one"},{"id":"two"}])).await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 1)
        .await
        .unwrap();
    assert!(!inventory.pagination_complete);
    assert_eq!(inventory.pages, 1);
    assert_eq!(inventory.letters.len(), 2);
    assert!(!inventory.issues.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_http_failure_preserves_earlier_pages_and_redacts_the_body() {
    let server = MockServer::start().await;
    inbox_page(&server, 0, json!([{"id":"one"},{"id":"two"}])).await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(500).set_body_string("fake-secret-response-body"))
        .expect(1)
        .mount(&server)
        .await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 3)
        .await
        .unwrap();
    assert!(!inventory.pagination_complete);
    assert_eq!(inventory.letters.len(), 2);
    assert_eq!(inventory.pages, 1);
    let rendered = serde_json::to_string(&inventory).unwrap();
    assert!(rendered.contains("500"));
    assert!(!rendered.contains("fake-secret-response-body"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_initial_authentication_failure_is_not_an_empty_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let error = collect(&mut client(&server), Source::Inbox, 2, 3)
        .await
        .unwrap_err();
    assert_eq!(error.exit_code(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_authentication_failure_preserves_inventory_and_failure_class() {
    let server = MockServer::start().await;
    inbox_page(&server, 0, json!([{"id":"one"},{"id":"two"}])).await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 3)
        .await
        .unwrap();
    assert!(!inventory.pagination_complete);
    assert_eq!(inventory.letters.len(), 2);
    assert_eq!(
        serde_json::to_value(inventory).unwrap()["failure_exit_code"],
        3
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_ids_preserve_metadata_but_prevent_a_complete_claim() {
    let server = MockServer::start().await;
    inbox_page(
        &server,
        0,
        json!([{"documentMessage":"synthetic short message"}]),
    )
    .await;
    let inventory = collect(&mut client(&server), Source::Inbox, 2, 3)
        .await
        .unwrap();
    assert!(!inventory.pagination_complete);
    assert_eq!(inventory.letters.len(), 1);
    assert!(inventory.letters[0].id.is_none());
    assert!(!inventory.issues.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_directory_traversal_retains_the_opaque_selector() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .and(query_param("directory-id", "invented /?&+ folder"))
        .and(query_param("offset", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let inventory = collect(
        &mut client(&server),
        Source::ArchiveDirectory("invented /?&+ folder".into()),
        2,
        3,
    )
    .await
    .unwrap();
    assert!(inventory.pagination_complete);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_bounds_fail_before_any_request() {
    let server = MockServer::start().await;
    let mut client = client(&server);
    for (size, max_pages) in [(0, 3), (1001, 3), (2, 0), (2, 100_001)] {
        assert!(matches!(
            collect(&mut client, Source::Inbox, size, max_pages).await,
            Err(Error::Config(_))
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
