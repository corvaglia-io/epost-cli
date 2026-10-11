use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use epost_cli::error::Error;
use epost_cli::paperless::{PaperlessClient, PaperlessOptions, TaskStatus, normalize_url};
use epost_cli::secret::Secret;
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const TASK_ID: &str = "11111111-2222-3333-4444-555555555555";
const OTHER_TASK_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
const PDF: &[u8] = b"%PDF-1.7\nsynthetic-document\n%%EOF\n";

fn options() -> PaperlessOptions {
    PaperlessOptions {
        max_retries: 0,
        ..PaperlessOptions::default()
    }
}

fn client(server: &MockServer) -> PaperlessClient {
    PaperlessClient::for_test(
        &server.uri(),
        Secret::new("synthetic-paperless-token".into()),
        options(),
    )
    .unwrap()
}

fn page(results: Vec<Value>) -> Value {
    json!({"count": results.len(), "next": null, "previous": null, "results": results})
}

fn tag(id: i64, name: &str, inbox: bool) -> Value {
    json!({"id": id, "name": name, "is_inbox_tag": inbox})
}

fn raw_server(response: &'static [u8]) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = [0u8; 8_192];
        assert!(socket.read(&mut request).unwrap() > 0);
        socket.write_all(response).unwrap();
    });
    (uri, handle)
}

#[test]
fn installation_url_and_test_destinations_are_constrained() {
    assert_eq!(
        normalize_url("https://paperless.example.invalid/installation", false).unwrap(),
        "https://paperless.example.invalid/installation/"
    );
    for url in [
        "http://paperless.example.invalid/",
        "https://fake-token@paperless.example.invalid/",
        "https://@paperless.example.invalid/",
        "https://fake:fake-password@paperless.example.invalid/",
        "https://paperless.example.invalid/?fake-token=secret",
        "https://paperless.example.invalid/#secret",
        "https://paperless.example.invalid/escape%2fpath",
        "https://paperless.example.invalid/escape%5cpath",
        "https://paperless.example.invalid/\n",
        " https://paperless.example.invalid/",
        "file:///private/synthetic-file",
    ] {
        assert!(normalize_url(url, false).is_err());
    }
    for url in [
        "https://paperless.example.invalid/",
        "http://localhost:1234/",
        "http://192.0.2.1:1234/",
        "http://127.0.0.1:1234/?query=1",
    ] {
        assert!(PaperlessClient::for_test(url, Secret::new("fake".into()), options()).is_err());
    }
    for url in ["http://127.0.0.1:1234/", "http://[::1]:1234/installation/"] {
        assert!(PaperlessClient::for_test(url, Secret::new("fake".into()), options()).is_ok());
        assert!(PaperlessClient::new(url, Secret::new("fake".into()), options()).is_err());
    }
}

#[test]
fn invalid_token_and_hard_resource_bounds_are_rejected() {
    for token in ["", "fake token", "fake\nInjected: true", "ü"] {
        assert!(matches!(
            PaperlessClient::for_test(
                "http://127.0.0.1:1234/",
                Secret::new(token.into()),
                options()
            ),
            Err(Error::Auth)
        ));
    }
    for options in [
        PaperlessOptions {
            timeout_secs: 0,
            ..options()
        },
        PaperlessOptions {
            timeout_secs: 121,
            ..options()
        },
        PaperlessOptions {
            max_retries: 6,
            ..options()
        },
        PaperlessOptions {
            max_response_bytes: 0,
            ..options()
        },
        PaperlessOptions {
            max_response_bytes: 64 * 1024 * 1024 + 1,
            ..options()
        },
    ] {
        assert!(
            PaperlessClient::for_test(
                "http://127.0.0.1:1234/",
                Secret::new("fake".into()),
                options
            )
            .is_err()
        );
    }
}

#[tokio::test]
async fn installation_prefix_and_authenticated_tag_creation_are_exact() {
    let server = MockServer::start().await;
    let client = PaperlessClient::for_test(
        &format!("{}/installation", server.uri()),
        Secret::new("synthetic-paperless-token".into()),
        options(),
    )
    .unwrap();
    assert_eq!(
        client.canonical_url(),
        format!("{}/installation/", server.uri())
    );
    Mock::given(method("POST"))
        .and(path("/installation/api/tags/"))
        .and(header("Authorization", "Token synthetic-paperless-token"))
        .respond_with(ResponseTemplate::new(201).set_body_json(tag(7, "Inbox", true)))
        .expect(1)
        .mount(&server)
        .await;
    let result = client.create_tag("Inbox", true).await.unwrap();
    assert_eq!(result.id, 7);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({
            "name": "Inbox", "is_inbox_tag": true
        })
    );
    Mock::given(method("GET"))
        .and(path("/installation/api/tags/7/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tag(7, "Inbox", true)))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(client.tag(7).await.unwrap(), result);
}

#[tokio::test]
async fn pagination_uses_controlled_page_numbers_and_never_response_origins() {
    let server = MockServer::start().await;
    let forbidden = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tags/"))
        .and(query_param("page", "1"))
        .and(query_param("page_size", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "count": 2,
            "next": format!("{}/credential-trap", forbidden.uri()),
            "results": [tag(1, "Inbox", true)]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/tags/"))
        .and(query_param("page", "2"))
        .and(header("Authorization", "Token synthetic-paperless-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "count": 2, "next": null, "results": [tag(2, "ePost", false)]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let tags = client(&server).tags().await.unwrap();
    assert_eq!(
        tags.iter().map(|tag| tag.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(forbidden.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn changed_or_incomplete_pagination_is_an_error() {
    for next_page in [
        json!({"count": 3, "next": null, "results": [tag(2, "ePost", false)]}),
        json!({"count": 2, "next": null, "results": []}),
        json!({"count": 2, "next": "https://untrusted.invalid/", "results": []}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags/"))
            .and(query_param("page", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "count": 2, "next": "https://untrusted.invalid/", "results": [tag(1, "Inbox", true)]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags/"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(next_page))
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server).tags().await,
            Err(Error::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn malformed_and_duplicate_tag_payloads_are_rejected() {
    for body in [
        page(vec![tag(1, "Inbox", true), tag(1, "other", false)]),
        page(vec![tag(0, "Inbox", true)]),
        page(vec![tag(1, "\u{1b}Injected", true)]),
        json!({"count": 5001, "next": null, "results": []}),
        json!({"count": 1, "next": null, "results": "secret-body"}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server).tags().await,
            Err(Error::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn upload_is_multipart_and_success_means_only_a_task_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/documents/post_document/"))
        .and(header("Authorization", "Token synthetic-paperless-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(TASK_ID))
        .expect(1)
        .mount(&server)
        .await;
    let result = client(&server)
        .upload_pdf("synthetic-marker.pdf", PDF.into(), &[3, 2, 3])
        .await
        .unwrap();
    assert_eq!(result, TASK_ID);
    let requests = server.received_requests().await.unwrap();
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("name=\"document\"; filename=\"synthetic-marker.pdf\""));
    assert!(body.contains("Content-Type: application/pdf"));
    assert!(body.contains(std::str::from_utf8(PDF).unwrap()));
    assert_eq!(body.matches("name=\"tags\"").count(), 2);
    assert!(body.contains("name=\"tags\"\r\n\r\n2\r\n"));
    assert!(body.contains("name=\"tags\"\r\n\r\n3\r\n"));
}

#[tokio::test]
async fn upload_and_tag_creation_are_never_retried_on_transient_failures() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "0"))
        .expect(2)
        .mount(&server)
        .await;
    let client = PaperlessClient::for_test(
        &server.uri(),
        Secret::new("fake".into()),
        PaperlessOptions {
            max_retries: 5,
            ..options()
        },
    )
    .unwrap();
    assert!(matches!(
        client.upload_pdf("synthetic.pdf", PDF.into(), &[]).await,
        Err(Error::Http(503))
    ));
    assert!(matches!(
        client.create_tag("Inbox", true).await,
        Err(Error::Http(503))
    ));
}

#[tokio::test]
async fn upload_network_failure_is_not_replayed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let counter = attempts.clone();
    let finished = done.clone();
    let handle = std::thread::spawn(move || {
        while !finished.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    counter.fetch_add(1, Ordering::SeqCst);
                    socket
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = [0u8; 8_192];
                    let _ = socket.read(&mut request);
                    // Drop without a response, simulating a lost upload receipt.
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("synthetic listener failed: {error}"),
            }
        }
    });
    let client = PaperlessClient::for_test(
        &uri,
        Secret::new("synthetic-token".into()),
        PaperlessOptions {
            max_retries: 5,
            ..options()
        },
    )
    .unwrap();
    assert!(matches!(
        client.upload_pdf("synthetic.pdf", PDF.into(), &[1]).await,
        Err(Error::Network)
    ));
    done.store(true, Ordering::SeqCst);
    handle.join().unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn upload_redirects_never_send_documents_or_tokens_to_another_origin() {
    let server = MockServer::start().await;
    let trap = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("Location", format!("{}/document-trap", trap.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server)
            .upload_pdf("synthetic.pdf", PDF.into(), &[1])
            .await,
        Err(Error::Http(307))
    ));
    assert!(trap.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn invalid_upload_inputs_do_not_make_requests() {
    let server = MockServer::start().await;
    let client = client(&server);
    for filename in [
        "",
        "../synthetic.pdf",
        "synthetic\\file.pdf",
        "quote\".pdf",
        "line\n.pdf",
    ] {
        assert!(client.upload_pdf(filename, PDF.into(), &[]).await.is_err());
    }
    for bytes in [b"not-a-pdf".to_vec(), b"%PDF-1.7\ntruncated".to_vec()] {
        assert!(
            client
                .upload_pdf("synthetic.pdf", bytes, &[])
                .await
                .is_err()
        );
    }
    assert!(
        client
            .upload_pdf("synthetic.pdf", PDF.into(), &[0])
            .await
            .is_err()
    );
    assert!(
        client
            .upload_pdf("synthetic.pdf", PDF.into(), &[-1])
            .await
            .is_err()
    );
    assert!(client.task("bad-task-id").await.is_err());
    assert!(client.document(-1).await.is_err());
    assert!(client.original(0).await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
    let bounded = PaperlessClient::for_test(
        &server.uri(),
        Secret::new("fake".into()),
        PaperlessOptions {
            max_response_bytes: 5,
            ..options()
        },
    )
    .unwrap();
    assert!(matches!(
        bounded.validate_upload("synthetic.pdf", PDF, &[1]),
        Err(Error::Config(_))
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn malformed_upload_responses_are_not_delivery_success() {
    for body in [
        json!({"task_id": TASK_ID}),
        json!("not-a-uuid"),
        Value::Null,
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/documents/post_document/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server)
                .upload_pdf("synthetic.pdf", PDF.into(), &[])
                .await,
            Err(Error::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn legacy_and_current_task_contracts_are_supported_without_guessing_ids() {
    for (body, status, document_id) in [
        (
            json!([{"task_id": TASK_ID, "status": "SUCCESS", "related_document": "7"}]),
            TaskStatus::Success,
            Some(7),
        ),
        (
            json!([{"task_id": TASK_ID, "status": "STARTED", "related_document": null}]),
            TaskStatus::Pending,
            None,
        ),
        (
            page(vec![
                json!({"task_id": TASK_ID, "status": "success", "related_document_ids": [9]}),
            ]),
            TaskStatus::Success,
            Some(9),
        ),
        (
            page(vec![
                json!({"task_id": TASK_ID, "status": "pending", "related_document_ids": []}),
            ]),
            TaskStatus::Pending,
            None,
        ),
        (
            page(vec![
                json!({"task_id": TASK_ID, "status": "failure", "related_document_ids": []}),
            ]),
            TaskStatus::Failure,
            None,
        ),
        (
            json!([{"task_id": TASK_ID, "status": "REVOKED"}]),
            TaskStatus::Failure,
            None,
        ),
        (
            json!([{"task_id": TASK_ID, "status": "RETRY"}]),
            TaskStatus::Pending,
            None,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tasks/"))
            .and(query_param("task_id", TASK_ID))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let task = client(&server).task(TASK_ID).await.unwrap().unwrap();
        assert_eq!(task.task_id, TASK_ID);
        assert_eq!(task.status, status);
        assert_eq!(task.document_id, document_id);
    }
}

#[tokio::test]
async fn absent_task_is_pending_but_mismatched_and_ambiguous_tasks_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/tasks/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![])))
        .mount(&server)
        .await;
    assert_eq!(client(&server).task(TASK_ID).await.unwrap(), None);
    for entry in [
        json!({"task_id": OTHER_TASK_ID, "status": "SUCCESS", "related_document": 1}),
        json!({"task_id": TASK_ID, "status": "UNKNOWN"}),
        json!({"task_id": TASK_ID, "status": "SUCCESS", "related_document_ids": [1, 2]}),
        json!({"task_id": TASK_ID, "status": "SUCCESS", "related_document": 0}),
        json!({"task_id": TASK_ID, "status": "SUCCESS", "related_document": "https://untrusted.invalid/"}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tasks/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![entry]))
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server).task(TASK_ID).await,
            Err(Error::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn original_download_and_document_tags_use_canonical_id_routes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/documents/17/download/"))
        .and(query_param("original", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PDF))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/documents/17/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 17, "tags": [2, 1], "download_url": "https://untrusted.invalid/"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    assert_eq!(client.original(17).await.unwrap(), PDF);
    let document = client.document(17).await.unwrap();
    assert_eq!(document.id, 17);
    assert_eq!(document.tags, vec![1, 2]);
}

#[tokio::test]
async fn incorrect_document_identity_or_tags_is_rejected() {
    for body in [
        json!({"id": 18, "tags": [1, 2]}),
        json!({"id": 17, "tags": [1, 1]}),
        json!({"id": 17, "tags": [0]}),
        json!({"id": 17, "tags": "synthetic-bad-tags"}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/documents/17/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server).document(17).await,
            Err(Error::InvalidResponse(_))
        ));
    }
}

#[tokio::test]
async fn document_search_uses_official_filter_and_exact_response_filename() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/documents/"))
        .and(query_param(
            "original_filename__iexact",
            "synthetic-marker.pdf",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(vec![
            json!({"id": 7, "original_file_name": "synthetic-marker.pdf"}),
            json!({"id": 8, "original_file_name": "SYNTHETIC-MARKER.PDF"}),
            json!({"id": 9, "original_file_name": "unrelated.pdf"}),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client(&server)
            .find_documents("synthetic-marker.pdf")
            .await
            .unwrap(),
        vec![7]
    );
}

#[tokio::test]
async fn redirects_are_not_followed_and_error_bodies_are_redacted() {
    let server = MockServer::start().await;
    let trap = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/token-trap", trap.uri()))
                .set_body_string("synthetic-token PRIVATE-DOCUMENT-TEXT"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let error = client(&server).original(1).await.unwrap_err();
    assert!(matches!(error, Error::Http(302)));
    assert!(!format!("{error:?} {error}").contains("synthetic-token"));
    assert!(!format!("{error:?} {error}").contains("PRIVATE-DOCUMENT-TEXT"));
    assert!(!format!("{error:?} {error}").contains(&server.uri()));
    assert!(trap.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn authenticated_failures_are_redacted_and_not_retried() {
    for status in [401, 403] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string("PRIVATE-TOKEN-SECRET"))
            .expect(1)
            .mount(&server)
            .await;
        let client = PaperlessClient::for_test(
            &server.uri(),
            Secret::new("synthetic-token".into()),
            PaperlessOptions {
                max_retries: 5,
                ..options()
            },
        )
        .unwrap();
        let error = client.tags().await.unwrap_err();
        assert!(matches!(error, Error::Auth));
        assert!(!format!("{error:?} {error}").contains("PRIVATE-TOKEN-SECRET"));
        assert!(!format!("{error:?} {error}").contains("synthetic-token"));
    }
}

#[tokio::test]
async fn safe_get_retries_honor_bounded_retry_after() {
    let server = MockServer::start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).insert_header("Retry-After", "0")
            } else {
                ResponseTemplate::new(200).set_body_json(page(vec![]))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let client = PaperlessClient::for_test(
        &server.uri(),
        Secret::new("fake".into()),
        PaperlessOptions {
            max_retries: 1,
            ..options()
        },
    )
    .unwrap();
    assert!(client.tags().await.unwrap().is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    for value in ["31", "invalid-delay"] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", value))
            .expect(1)
            .mount(&server)
            .await;
        let client = PaperlessClient::for_test(
            &server.uri(),
            Secret::new("fake".into()),
            PaperlessOptions {
                max_retries: 2,
                ..options()
            },
        )
        .unwrap();
        assert!(matches!(client.tags().await, Err(Error::Http(429))));
    }
}

#[tokio::test]
async fn content_length_and_chunked_responses_are_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PDF))
        .mount(&server)
        .await;
    let client = PaperlessClient::for_test(
        &server.uri(),
        Secret::new("fake".into()),
        PaperlessOptions {
            max_response_bytes: 5,
            ..options()
        },
    )
    .unwrap();
    assert!(matches!(
        client.original(1).await,
        Err(Error::InvalidResponse(_))
    ));
    let (uri, handle) = raw_server(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nabcdef\r\n0\r\n\r\n");
    let client = PaperlessClient::for_test(
        &uri,
        Secret::new("fake".into()),
        PaperlessOptions {
            max_response_bytes: 5,
            ..options()
        },
    )
    .unwrap();
    assert!(matches!(
        client.original(1).await,
        Err(Error::InvalidResponse(_))
    ));
    handle.join().unwrap();
}

#[tokio::test]
async fn truncated_original_and_malformed_json_never_succeed() {
    let (uri, handle) =
        raw_server(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc");
    let client = PaperlessClient::for_test(&uri, Secret::new("fake".into()), options()).unwrap();
    assert!(matches!(client.original(1).await, Err(Error::Network)));
    handle.join().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("synthetic-token PRIVATE malformed JSON"),
        )
        .mount(&server)
        .await;
    let error = self::client(&server).tags().await.unwrap_err();
    assert!(matches!(error, Error::InvalidResponse(_)));
    assert!(!format!("{error:?} {error}").contains("synthetic-token"));
    assert!(!format!("{error:?} {error}").contains("PRIVATE"));
}
