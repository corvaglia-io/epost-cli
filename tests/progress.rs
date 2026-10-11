use epost_cli::api::{ApiClient, ClientOptions};
use epost_cli::models::{Mailbox, MailboxKind};
use epost_cli::pagination::{PageProgress, Source, collect_with_progress};
use epost_cli::progress::{Event, Reporter};
use epost_cli::secret::Secret;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn progress_shows_work_before_an_unknown_total_is_available() {
    let mut reporter = Reporter::new(true);
    let lines = [
        Event::Authenticating,
        Event::ArchiveFolders,
        Event::InventoryStarted {
            label: "inbox".into(),
        },
        Event::Page {
            page: 2,
            documents: 48,
        },
        Event::InventoryComplete {
            documents: 63,
            complete: true,
        },
        Event::DocumentStarted {
            index: 1,
            total: 63,
        },
    ]
    .map(|event| reporter.render(event).unwrap());
    assert_eq!(lines[0], "Signing in…");
    assert_eq!(lines[1], "Scanning archive folders…");
    assert_eq!(lines[2], "Scanning inbox…");
    assert_eq!(lines[3], "Scanning page 2… (48 documents found so far)");
    assert_eq!(lines[4], "Inventory finished: 63 documents found.");
    assert_eq!(lines[5], "Checking document 1 of 63…");
    assert!(lines.iter().all(|line| !line.contains('%')));
}

#[test]
fn incomplete_inventory_keeps_uncertainty_visible() {
    assert_eq!(
        Reporter::new(true).render(Event::InventoryComplete {
            documents: 5,
            complete: false,
        }),
        Some("Inventory incomplete: 5 documents found so far.".into())
    );
}

#[test]
fn finished_summary_counts_each_retrieval_outcome_and_resets_for_repeat_runs() {
    let mut reporter = Reporter::new(true);
    for status in [
        "saved_pdf",
        "saved_pdf",
        "saved_json",
        "saved_binary",
        "already_saved",
        "already_saved",
        "awaiting_manual_acceptance_or_status_verification",
        "failed",
        "deferred_due_to_authentication_failure",
    ] {
        reporter.render(Event::DocumentComplete {
            status: status.into(),
            media_kind: Some("pdf".into()),
        });
    }
    assert_eq!(
        reporter.render(Event::Finished { exit_code: 2 }),
        Some(
            "Sync finished with items requiring attention: 2 PDF saved, 1 JSON saved, 1 binary saved, 2 already saved, 1 pending manual action, 1 failed, 1 deferred."
                .into()
        )
    );
    assert_eq!(
        reporter.render(Event::Finished { exit_code: 0 }),
        Some("Sync finished: no documents processed.".into())
    );
}

#[test]
fn saved_file_reuse_is_distinct_from_a_new_download() {
    let mut reporter = Reporter::new(true);
    assert_eq!(
        reporter.render(Event::DocumentComplete {
            status: "already_saved".into(),
            media_kind: Some("pdf".into()),
        }),
        Some("  Already saved; local file verified.".into())
    );
    assert_eq!(
        reporter.render(Event::Finished { exit_code: 0 }),
        Some("Sync finished: 1 already saved.".into())
    );
}

#[test]
fn unknown_status_text_is_not_echoed_or_reported_as_success() {
    let mut reporter = Reporter::new(true);
    let rendered = reporter
        .render(Event::DocumentComplete {
            status: "synthetic-secret-response\n\x1b[31m".into(),
            media_kind: Some("synthetic-secret-media".into()),
        })
        .unwrap();
    assert!(!rendered.contains("synthetic-secret"));
    assert!(!rendered.contains('\n'));
    assert!(!rendered.contains('\x1b'));
    assert_eq!(
        reporter.render(Event::Finished { exit_code: 2 }),
        Some("Sync finished with items requiring attention: 1 deferred.".into())
    );
}

#[test]
fn display_labels_cannot_inject_terminal_controls_or_unbounded_lines() {
    let label = format!("\x1b\n\r\u{202e}\u{2066}{}", "x".repeat(500));
    let rendered = Reporter::new(true)
        .render(Event::InventoryStarted { label })
        .unwrap();
    assert_eq!(rendered, format!("Scanning {}…", "x".repeat(120)));
    assert_eq!(
        Reporter::new(true).render(Event::InventoryStarted {
            label: "\n\r\u{200e}".into(),
        }),
        Some("Scanning documents…".into())
    );
}

#[test]
fn disabled_progress_emits_no_human_text() {
    let mut reporter = Reporter::new(false);
    for event in [
        Event::Authenticating,
        Event::Page {
            page: 1,
            documents: 0,
        },
        Event::DocumentComplete {
            status: "saved_pdf".into(),
            media_kind: Some("pdf".into()),
        },
        Event::Finished { exit_code: 0 },
    ] {
        assert!(reporter.render(event).is_none());
    }
}

#[test]
fn cached_non_pdf_content_still_needs_review_and_is_counted() {
    for media_kind in [
        Some("json"),
        Some("binary"),
        None,
        Some("synthetic-secret-media"),
    ] {
        let mut reporter = Reporter::new(true);
        assert_eq!(
            reporter.render(Event::DocumentComplete {
                status: "already_saved".into(),
                media_kind: media_kind.map(str::to_owned),
            }),
            Some("  Already saved; local file verified, but format still needs review.".into())
        );
        assert_eq!(
            reporter.render(Event::Finished { exit_code: 2 }),
            Some("Sync finished with items requiring attention: 1 already saved, 1 cached non-PDF needing format review.".into())
        );
    }
}

#[test]
fn silent_reset_clears_counts_without_emitting_a_second_final_summary() {
    let mut reporter = Reporter::new(true);
    reporter.render(Event::DocumentComplete {
        status: "saved_pdf".into(),
        media_kind: Some("pdf".into()),
    });
    reporter.reset();
    assert_eq!(
        reporter.render(Event::Finished { exit_code: 0 }),
        Some("Sync finished: no documents processed.".into())
    );
}

#[test]
fn failures_are_reported_by_their_actual_exit_class() {
    for (code, message) in [
        (1, "Sync stopped after an error"),
        (3, "Sync stopped after a sign-in failure"),
        (4, "Sync stopped after a file integrity failure"),
    ] {
        assert_eq!(
            Reporter::new(true).render(Event::Finished { exit_code: code }),
            Some(format!("{message}: no documents processed."))
        );
    }
}

fn client(server: &MockServer) -> ApiClient {
    let mut client = ApiClient::for_test(
        &server.uri(),
        Some(Mailbox {
            tenant_id: "invented-progress-tenant".into(),
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
        .set_bearer(Secret::new("synthetic-progress-token".into()))
        .unwrap();
    client
}

async fn page(server: &MockServer, offset: u32, response: Value) {
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
async fn page_observer_reports_before_requests_and_after_deduplication() {
    let server = MockServer::start().await;
    page(
        &server,
        0,
        json!([{"id":"invented-one"},{"id":"invented-two"}]),
    )
    .await;
    page(
        &server,
        2,
        json!([{"id":"invented-two"},{"id":"invented-three"}]),
    )
    .await;
    page(&server, 4, json!([])).await;
    let mut events = Vec::new();
    let inventory = collect_with_progress(&mut client(&server), Source::Inbox, 2, 3, |event| {
        events.push(event);
    })
    .await
    .unwrap();
    assert_eq!(inventory.letters.len(), 3);
    assert!(!inventory.pagination_complete);
    assert_eq!(
        events,
        vec![
            PageProgress::Started {
                page: 1,
                documents: 0,
            },
            PageProgress::Completed {
                page: 1,
                documents: 2,
            },
            PageProgress::Started {
                page: 2,
                documents: 2,
            },
            PageProgress::Completed {
                page: 2,
                documents: 3,
            },
            PageProgress::Started {
                page: 3,
                documents: 3,
            },
            PageProgress::Completed {
                page: 3,
                documents: 3,
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_page_emits_completed_without_losing_previous_documents() {
    let server = MockServer::start().await;
    page(
        &server,
        0,
        json!([{"id":"invented-one"},{"id":"invented-two"}]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/epost/v2/letters"))
        .and(query_param("offset", "2"))
        .respond_with(ResponseTemplate::new(500).set_body_string("synthetic-secret-body"))
        .expect(1)
        .mount(&server)
        .await;
    let mut events = Vec::new();
    let inventory = collect_with_progress(&mut client(&server), Source::Inbox, 2, 3, |event| {
        events.push(event);
    })
    .await
    .unwrap();
    assert_eq!(inventory.pages, 1);
    assert_eq!(inventory.letters.len(), 2);
    assert_eq!(events.len(), 4);
    assert_eq!(
        events[3],
        PageProgress::Completed {
            page: 2,
            documents: 2,
        }
    );
    assert!(!format!("{events:?}").contains("synthetic-secret-body"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_first_page_and_oversized_page_both_balance_progress_events() {
    for response in [
        ResponseTemplate::new(401),
        ResponseTemplate::new(200).set_body_json(json!([
            {"id":"invented-one"},
            {"id":"invented-two"},
            {"id":"invented-three"}
        ])),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/epost/v2/letters"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let mut events = Vec::new();
        let result = collect_with_progress(&mut client(&server), Source::Inbox, 2, 3, |event| {
            events.push(event);
        })
        .await;
        assert!(result.is_err() || !result.unwrap().pagination_complete);
        assert_eq!(
            events,
            vec![
                PageProgress::Started {
                    page: 1,
                    documents: 0,
                },
                PageProgress::Completed {
                    page: 1,
                    documents: 0,
                },
            ]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_pagination_bounds_do_not_start_a_progress_request() {
    let server = MockServer::start().await;
    let mut events = Vec::new();
    let result = collect_with_progress(&mut client(&server), Source::Inbox, 0, 3, |event| {
        events.push(event);
    })
    .await;
    assert!(result.is_err());
    assert!(events.is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
}
