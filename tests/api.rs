use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use epost_cli::api::{ApiClient, ClientOptions};
use epost_cli::error::Error;
use epost_cli::models::{Mailbox, MailboxKind};
use epost_cli::secret::Secret;
use reqwest::Url;
use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn mailbox(kind: MailboxKind) -> Mailbox {
    Mailbox {
        tenant_id: "invented-tenant".into(),
        company_id: 42,
        kind,
    }
}

fn options() -> ClientOptions {
    ClientOptions {
        max_retries: 0,
        ..ClientOptions::default()
    }
}

fn client(server: &MockServer, kind: MailboxKind) -> ApiClient {
    let mut client = ApiClient::for_test(&server.uri(), Some(mailbox(kind)), options()).unwrap();
    client
        .set_bearer(Secret::new("synthetic-bearer".into()))
        .unwrap();
    client
}

fn form(request: &Request) -> BTreeMap<String, String> {
    let body = std::str::from_utf8(&request.body).unwrap();
    let mut url = Url::parse("http://127.0.0.1/").unwrap();
    url.set_query(Some(body));
    url.query_pairs().into_owned().collect()
}

fn token(access: &str, refresh: &str, expires: i64) -> serde_json::Value {
    json!({
        "access_token": access,
        "refreshToken": refresh,
        "expires_in": expires,
        "refresh_expires_in": 3600,
        "token_type": "Bearer"
    })
}

#[tokio::test]
async fn tenant_discovery_encodes_credentials_and_never_selects_a_mailbox() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/tenants"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"tenant_id":"first-invented","company_id":11,"company_name":"Example One"},
            {"tenant_id":"second-invented","company_id":22}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(&server.uri(), None, options()).unwrap();
    let tenants = client
        .tenants_password(
            "person+test@example.invalid",
            &Secret::new("fake&=+ password".into()),
        )
        .await
        .unwrap();
    assert_eq!(tenants.len(), 2);
    assert_eq!(tenants[1].tenant_id, "second-invented");
    assert!(tenants[1].company_name.is_none());
    assert!(matches!(
        client.inbox_page(48, 0).await,
        Err(Error::Config(_))
    ));
    let requests = server.received_requests().await.unwrap();
    let fields = form(&requests[0]);
    assert_eq!(fields.len(), 2);
    assert_eq!(fields["username"], "person+test@example.invalid");
    assert_eq!(fields["password"], "fake&=+ password");
    assert!(!requests[0].headers.contains_key("authorization"));
}

#[tokio::test]
async fn bearer_discovery_uses_the_documented_form_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/tenants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(&server.uri(), None, options()).unwrap();
    assert!(
        client
            .tenants_bearer(&Secret::new("fake-discovery-token".into()))
            .await
            .unwrap()
            .is_empty()
    );
    let request = server.received_requests().await.unwrap().remove(0);
    assert_eq!(
        form(&request),
        BTreeMap::from([("access_token".into(), "fake-discovery-token".into())])
    );
}

#[tokio::test]
async fn discovery_rejects_missing_required_tenant_ids() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/tenants"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{"company_id":5}])))
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(&server.uri(), None, options()).unwrap();
    assert!(matches!(
        client.tenants_bearer(&Secret::new("fake".into())).await,
        Err(Error::InvalidResponse(_))
    ));
}

#[tokio::test]
async fn password_login_sends_explicit_tenant_and_decimal_company() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token(
            "new-access",
            "new-refresh",
            3600,
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(header("authorization", "Bearer new-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        options(),
    )
    .unwrap();
    client
        .login(
            "user@example.invalid",
            &Secret::new("invented-password".into()),
        )
        .await
        .unwrap();
    client.inbox_page(48, 0).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        form(&requests[0]),
        BTreeMap::from([
            ("grant_type".into(), "password".into()),
            ("username".into(), "user@example.invalid".into()),
            ("password".into(), "invented-password".into()),
            ("tenant_id".into(), "invented-tenant".into()),
            ("company_id".into(), "42".into()),
        ])
    );
}

#[tokio::test]
async fn private_and_business_inbox_queries_request_all_types() {
    for kind in [MailboxKind::Private, MailboxKind::Business] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters"))
            .and(query_param("is-business-tenant", kind.as_bool().to_string()))
            .and(query_param("letter-folder", "INBOX_FOLDER"))
            .and(query_param("read-status", "ALL"))
            .and(query_param("limit", "2"))
            .and(query_param("offset", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id":"invented-letter","letterType":"FUTURE_TYPE","documentMessage":"Synthetic message"},
                {"letterTitle":"Missing optional ID","futureField":{"number":1}}
            ])))
            .expect(1)
            .mount(&server)
            .await;
        let letters = client(&server, kind).inbox_page(2, 3).await.unwrap();
        assert_eq!(letters[0].letter_type.as_deref(), Some("FUTURE_TYPE"));
        assert!(letters[1].id.is_none());
        assert!(letters[1].extra.contains_key("futureField"));
        let request = server.received_requests().await.unwrap().remove(0);
        let types: Vec<String> = request
            .url
            .query_pairs()
            .filter(|(name, _)| name == "letter-types")
            .map(|(_, value)| value.into_owned())
            .collect();
        assert_eq!(
            types,
            [
                "CLASSIC_LETTER",
                "SMART_LETTER",
                "SMART_LETTER_ANSWER",
                "SIMPLE_SHORT_MESSAGE",
                "INCAMAIL",
                "SECURESEND"
            ]
        );
    }
}

#[tokio::test]
async fn invalid_pagination_is_rejected_before_network_io() {
    let server = MockServer::start().await;
    let mut client = client(&server, MailboxKind::Private);
    for (limit, offset) in [(0, 0), (1001, 0), (48, u32::MAX)] {
        assert!(matches!(
            client.inbox_page(limit, offset).await,
            Err(Error::Config(_))
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn archive_root_and_opaque_directory_id_are_distinct() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(&server)
        .await;
    let mut client = client(&server, MailboxKind::Business);
    client.archive_page(None, 48, 0).await.unwrap();
    client
        .archive_page(Some("invented &/+ folder"), 48, 48)
        .await
        .unwrap();
    assert!(matches!(
        client.archive_page(Some(""), 48, 0).await,
        Err(Error::Config(_))
    ));
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
            .any(|(name, value)| name == "directory-id" && value == "invented &/+ folder")
    );
}

#[tokio::test]
async fn folders_preserve_branded_and_multiple_parent_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"directoryId":"","directoryName":"Synthetic branded","parentDirectoryIds":[]},
            {"directoryId":"child","parentDirectoryIds":["parent-a","parent-b"],"numberOfDocuments":2,"futureFlag":true},
            {"directoryId":"root","parentDirectoryIds":null}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let directories = client(&server, MailboxKind::Private)
        .directories()
        .await
        .unwrap();
    assert_eq!(directories[0].directory_id.as_deref(), Some(""));
    assert_eq!(
        directories[1].parent_directory_ids,
        ["parent-a", "parent-b"]
    );
    assert_eq!(directories[1].extra["futureFlag"], true);
    assert!(directories[2].parent_directory_ids.is_empty());
}

#[tokio::test]
async fn content_preserves_binary_and_json_and_encodes_ids_as_one_segment() {
    let server = MockServer::start().await;
    let bytes = vec![0, 255, 10, 32, 13];
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/a%2Fb%3F%23/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(bytes.clone())
                .insert_header("content-type", "application/octet-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let json_bytes = br#"{"synthetic":"content"}"#.to_vec();
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/json-letter/content"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(json_bytes.clone())
                .insert_header("content-type", "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut client = client(&server, MailboxKind::Private);
    let content = client.content("a/b?#").await.unwrap();
    assert_eq!(content.bytes, bytes);
    assert_eq!(
        content.content_type.as_deref(),
        Some("application/octet-stream")
    );
    let content = client.content("json-letter").await.unwrap();
    assert_eq!(content.bytes, json_bytes);
    assert_eq!(content.content_type.as_deref(), Some("application/json"));
    assert!(matches!(client.content("..").await, Err(Error::Config(_))));
}

#[tokio::test]
async fn an_untrusted_content_reference_is_never_followed() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/fake-letter"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"fake-letter", "letterContentReference":format!("{}/steal", destination.uri())
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/fake-letter/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
        .mount(&server)
        .await;
    let mut client = client(&server, MailboxKind::Private);
    let letter = client.letter("fake-letter").await.unwrap();
    assert!(letter.extra.contains_key("letterContentReference"));
    client.content(letter.id.as_deref().unwrap()).await.unwrap();
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirects_are_errors_and_never_forward_credentials() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/redirect/content"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/steal", destination.uri())),
        )
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client(&server, MailboxKind::Private)
            .content("redirect")
            .await,
        Err(Error::Http(302))
    ));
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn expired_tokens_refresh_and_rotation_replaces_the_old_refresh_token() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let response_calls = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(move |_: &Request| {
            let value = match response_calls.fetch_add(1, Ordering::SeqCst) {
                0 => token("access-one", "refresh-one", 0),
                1 => token("access-two", "refresh-two", 0),
                _ => token("access-three", "refresh-three", 3600),
            };
            ResponseTemplate::new(200).set_body_json(value)
        })
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        options(),
    )
    .unwrap();
    client
        .login("fake@example.invalid", &Secret::new("fake-password".into()))
        .await
        .unwrap();
    client.inbox_page(48, 0).await.unwrap();
    client.inbox_page(48, 48).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let forms: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(form)
        .collect();
    assert_eq!(forms[1]["refresh_token"], "refresh-one");
    assert_eq!(forms[2]["refresh_token"], "refresh-two");
    assert_eq!(forms[1].len(), 2);
    assert_eq!(forms[1]["grant_type"], "refresh_token");
    let auth: Vec<_> = requests
        .iter()
        .filter_map(|request| request.headers.get("authorization"))
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(auth, ["Bearer access-two", "Bearer access-three"]);
}

#[tokio::test]
async fn a_401_causes_at_most_one_refresh_and_one_replay() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token(
            "fake-access",
            "fake-refresh",
            3600,
        )))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(401).set_body_string("fake-access fake-refresh"))
        .expect(2)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        options(),
    )
    .unwrap();
    client
        .login("fake@example.invalid", &Secret::new("fake-password".into()))
        .await
        .unwrap();
    assert!(matches!(client.inbox_page(48, 0).await, Err(Error::Auth)));
    assert_eq!(server.received_requests().await.unwrap().len(), 4);
}

#[tokio::test]
async fn a_refresh_without_rotation_preserves_the_current_refresh_secret() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let response_calls = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(move |_: &Request| {
            let value = match response_calls.fetch_add(1, Ordering::SeqCst) {
                0 => token("access-one", "refresh-retained", 0),
                1 => json!({"access_token":"access-two","expires_in":0}),
                _ => json!({"access_token":"access-three","expires_in":3600}),
            };
            ResponseTemplate::new(200).set_body_json(value)
        })
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        options(),
    )
    .unwrap();
    client
        .login("fake@example.invalid", &Secret::new("fake-password".into()))
        .await
        .unwrap();
    client.inbox_page(48, 0).await.unwrap();
    client.inbox_page(48, 48).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let forms: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "POST")
        .map(form)
        .collect();
    assert_eq!(forms[1]["refresh_token"], "refresh-retained");
    assert_eq!(forms[2]["refresh_token"], "refresh-retained");
}

#[tokio::test]
async fn expired_refresh_tokens_fail_without_attempting_another_auth_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token":"fake-access","refreshToken":"fake-refresh",
            "expires_in":0,"refresh_expires_in":0
        })))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        options(),
    )
    .unwrap();
    client
        .login("fake@example.invalid", &Secret::new("fake-password".into()))
        .await
        .unwrap();
    assert!(matches!(client.inbox_page(48, 0).await, Err(Error::Auth)));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn token_creation_is_not_retried_even_when_retrieval_retries_are_enabled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        ClientOptions::default(),
    )
    .unwrap();
    assert!(matches!(
        client
            .login("fake@example.invalid", &Secret::new("fake-password".into()))
            .await,
        Err(Error::Http(429))
    ));
}

#[tokio::test]
async fn api_keys_are_business_only_and_replace_bearer_auth() {
    let server = MockServer::start().await;
    let mut private = client(&server, MailboxKind::Private);
    assert!(matches!(
        private.set_api_key(Secret::new("fake-api-key".into())),
        Err(Error::Config(_))
    ));
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .and(header("X-API-KEY", "fake-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    let mut business = client(&server, MailboxKind::Business);
    business
        .set_api_key(Secret::new("fake-api-key".into()))
        .unwrap();
    business.directories().await.unwrap();
    assert!(
        !server.received_requests().await.unwrap()[0]
            .headers
            .contains_key("authorization")
    );
}

#[tokio::test]
async fn rate_limits_are_retried_within_the_configured_bound() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let response_calls = Arc::clone(&calls);
    Mock::given(method("GET"))
        .and(path("/epost/v2/archives/directories"))
        .respond_with(move |_: &Request| {
            if response_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(429).insert_header("retry-after", "0")
            } else {
                ResponseTemplate::new(200).set_body_json(json!([]))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        ClientOptions {
            max_retries: 1,
            ..options()
        },
    )
    .unwrap();
    client.set_bearer(Secret::new("fake".into())).unwrap();
    client.directories().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn excessive_retry_after_and_repeated_failures_do_not_retry_forever() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/rate-limit/content"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "99999"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/unavailable/content"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .expect(2)
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        ClientOptions {
            max_retries: 1,
            ..options()
        },
    )
    .unwrap();
    client.set_bearer(Secret::new("fake".into())).unwrap();
    assert!(matches!(
        client.content("rate-limit").await,
        Err(Error::Http(429))
    ));
    assert!(matches!(
        client.content("unavailable").await,
        Err(Error::Http(503))
    ));
}

#[tokio::test]
async fn error_bodies_and_invalid_json_do_not_leak_secrets() {
    let server = MockServer::start().await;
    for (id, status) in [("registered", 403), ("missing", 404), ("broken", 500)] {
        Mock::given(method("GET"))
            .and(path(format!("/epost/v2/letters/{id}/content")))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string("synthetic-sensitive-body synthetic-bearer"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/bad-json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string("synthetic-sensitive-body synthetic-bearer"),
        )
        .mount(&server)
        .await;
    let mut client = client(&server, MailboxKind::Private);
    for id in ["registered", "missing", "broken"] {
        let error = client.content(id).await.unwrap_err();
        assert!(!format!("{error:?}").contains("synthetic"));
        assert!(!format!("{error}").contains("synthetic"));
    }
    let error = client.letter("bad-json").await.unwrap_err();
    assert!(matches!(error, Error::InvalidResponse(_)));
    assert!(!format!("{error:?}").contains("synthetic"));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method == "GET")
    );
}

#[tokio::test]
async fn successful_responses_are_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters/large/content"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 1025]))
        .mount(&server)
        .await;
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(mailbox(MailboxKind::Private)),
        ClientOptions {
            max_response_bytes: 1024,
            ..options()
        },
    )
    .unwrap();
    client.set_bearer(Secret::new("fake".into())).unwrap();
    assert!(matches!(
        client.content("large").await,
        Err(Error::InvalidResponse(_))
    ));
}

fn raw_server(response: &'static [u8]) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut buffer = [0; 4096];
        let received = socket.read(&mut buffer).unwrap();
        assert!(received > 0);
        socket.write_all(response).unwrap();
    });
    (uri, handle)
}

#[tokio::test]
async fn a_truncated_body_is_never_returned_as_successful_content() {
    let (uri, handle) =
        raw_server(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabc");
    let mut client =
        ApiClient::for_test(&uri, Some(mailbox(MailboxKind::Private)), options()).unwrap();
    client.set_bearer(Secret::new("fake".into())).unwrap();
    assert!(matches!(
        client.content("fake-letter").await,
        Err(Error::Network)
    ));
    handle.join().unwrap();
}

#[tokio::test]
async fn chunked_content_without_a_length_is_also_bounded() {
    let (uri, handle) = raw_server(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nabcdef\r\n0\r\n\r\n");
    let mut client = ApiClient::for_test(
        &uri,
        Some(mailbox(MailboxKind::Private)),
        ClientOptions {
            max_response_bytes: 5,
            ..options()
        },
    )
    .unwrap();
    client.set_bearer(Secret::new("fake".into())).unwrap();
    assert!(matches!(
        client.content("fake-letter").await,
        Err(Error::InvalidResponse(_))
    ));
    handle.join().unwrap();
}

#[tokio::test]
async fn malformed_token_fields_and_header_injection_are_rejected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/core/latest/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"refreshToken":"fake"})))
        .mount(&server)
        .await;
    let mut client = client(&server, MailboxKind::Private);
    assert!(matches!(
        client.set_bearer(Secret::new("fake\r\nX-Synthetic: value".into())),
        Err(Error::Auth)
    ));
    assert!(matches!(
        client
            .login("fake@example.invalid", &Secret::new("fake-password".into()))
            .await,
        Err(Error::InvalidResponse(_))
    ));
}

#[test]
fn test_constructor_rejects_provider_hosts_and_ambiguous_origins() {
    for origin in [
        "https://api.epost.ch/",
        "https://api-test.klara-epost.tech/",
        "http://example.invalid/",
        "http://localhost/",
        "http://192.0.2.1/",
        "http://fake:password@127.0.0.1/",
        "http://127.0.0.1/not-root",
        "http://127.0.0.1/?query=secret",
        "http://127.0.0.1/#fragment",
        "ftp://127.0.0.1/",
    ] {
        assert!(ApiClient::for_test(origin, None, options()).is_err());
    }
    assert!(ApiClient::for_test("http://127.0.0.1:12345/", None, options()).is_ok());
    assert!(ApiClient::for_test("http://[::1]:12345/", None, options()).is_ok());
}
