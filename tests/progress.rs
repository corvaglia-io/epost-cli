use epost_cli::api::{ApiClient, ClientOptions};
use epost_cli::delivery::{DeliveryEvent, DeliveryPhase};
use epost_cli::models::{Mailbox, MailboxKind};
use epost_cli::pagination::{PageProgress, Source, collect_with_progress};
use epost_cli::progress::{
    DiagnosticStatus, Event, Reporter, RetrievalCounts, Telemetry, Update, publish_line,
    publish_update, subscribe,
};
use epost_cli::secret::Secret;
use serde_json::{Value, json};
use std::sync::Mutex;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Subscription is intentionally process-wide and exclusive. Keep capture tests
// independent from the test runner's scheduling; render-only tests need no lock.
static CAPTURE_TEST: Mutex<()> = Mutex::new(());

fn retrieval(update: Update) -> (String, Event, RetrievalCounts) {
    let Some(Telemetry::Retrieval { event, counts }) = update.telemetry else {
        panic!("expected typed retrieval progress");
    };
    (update.line, event, counts)
}

#[test]
fn typed_progress_matches_plain_lines_and_counts_disjoint_outcomes() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    let capture = subscribe().unwrap();
    let mut reporter = Reporter::new(true);
    let mut plain = Reporter::new(true);
    let mut expected = RetrievalCounts::default();
    for (status, media, count) in [
        ("saved_pdf", Some("pdf"), 0),
        ("already_saved", Some("pdf"), 1),
        ("saved_json", Some("json"), 2),
        ("saved_binary", Some("binary"), 2),
        ("already_saved", Some("json"), 2),
        ("already_saved", Some("binary"), 2),
        ("already_saved", None, 2),
        ("awaiting_manual_acceptance_or_status_verification", None, 3),
        ("failed", None, 4),
        ("deferred_due_to_authentication_failure", None, 5),
        ("deferred_limit", None, 5),
    ] {
        let event = Event::DocumentComplete {
            status: status.into(),
            media_kind: media.map(str::to_owned),
        };
        let expected_line = plain.render(event.clone()).unwrap();
        reporter.event(event.clone());
        match count {
            0 => expected.new_pdf += 1,
            1 => expected.reused_pdf += 1,
            2 => expected.other += 1,
            3 => expected.pending += 1,
            4 => expected.failed += 1,
            5 => expected.deferred += 1,
            _ => unreachable!(),
        }
        let (line, actual_event, counts) = retrieval(capture.try_update().unwrap());
        assert_eq!(line, expected_line);
        assert_eq!(actual_event, event);
        assert_eq!(counts, expected);
    }
    reporter.event(Event::Finished { exit_code: 2 });
    let (line, event, counts) = retrieval(capture.try_update().unwrap());
    assert_eq!(
        line,
        plain.render(Event::Finished { exit_code: 2 }).unwrap()
    );
    assert_eq!(event, Event::Finished { exit_code: 2 });
    assert_eq!(counts, expected, "Finished retains the pre-reset snapshot");

    reporter.event(Event::Authenticating);
    assert_eq!(
        retrieval(capture.try_update().unwrap()).2,
        RetrievalCounts::default()
    );
    reporter.event(Event::DocumentComplete {
        status: "saved_pdf".into(),
        media_kind: Some("pdf".into()),
    });
    assert_eq!(retrieval(capture.try_update().unwrap()).2.new_pdf, 1);
    reporter.reset();
    assert!(capture.try_update().is_none(), "reset is silent");
    reporter.event(Event::Finished { exit_code: 0 });
    assert_eq!(
        retrieval(capture.try_update().unwrap()).2,
        RetrievalCounts::default()
    );
}

#[test]
fn disabled_reporter_publishes_neither_lines_nor_telemetry() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    let capture = subscribe().unwrap();
    let mut reporter = Reporter::new(false);
    for event in [
        Event::Authenticating,
        Event::DocumentComplete {
            status: "saved_pdf".into(),
            media_kind: Some("pdf".into()),
        },
        Event::Finished { exit_code: 0 },
    ] {
        reporter.event(event);
    }
    assert!(!reporter.finish_in_frontend(0));
    assert!(capture.try_update().is_none());
}

#[test]
fn frontend_finish_preserves_plain_counts_without_an_observer_and_resets_on_success() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    let mut reporter = Reporter::new(true);
    reporter.render(Event::DocumentComplete {
        status: "saved_pdf".into(),
        media_kind: Some("pdf".into()),
    });
    assert!(!reporter.finish_in_frontend(2));
    let capture = subscribe().unwrap();
    assert!(capture.try_update().is_none());
    assert!(reporter.finish_in_frontend(2));
    let (line, event, counts) = retrieval(capture.try_update().unwrap());
    assert_eq!(
        line,
        "Sync finished with items requiring attention: 1 PDF saved."
    );
    assert_eq!(event, Event::Finished { exit_code: 2 });
    assert_eq!(counts.new_pdf, 1);
    drop(capture);
    assert!(!reporter.finish_in_frontend(0));
    assert_eq!(
        reporter.render(Event::Finished { exit_code: 0 }),
        Some("Sync finished: no documents processed.".into())
    );
}

#[test]
fn typed_payloads_exclude_arbitrary_status_media_and_terminal_controls() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    let capture = subscribe().unwrap();
    let mut reporter = Reporter::new(true);
    reporter.event(Event::DocumentComplete {
        status: "synthetic-private-body\n\x1b\u{202e}".into(),
        media_kind: Some("synthetic-private-media".into()),
    });
    let update = capture.try_update().unwrap();
    assert!(!format!("{update:?}").contains("synthetic-private"));
    let (_, event, counts) = retrieval(update);
    assert_eq!(
        event,
        Event::DocumentComplete {
            status: "unrecognized".into(),
            media_kind: None,
        }
    );
    assert_eq!(counts.deferred, 1);
    reporter.event(Event::DocumentComplete {
        status: "already_saved".into(),
        media_kind: Some("synthetic-private-media".into()),
    });
    let (_, event, counts) = retrieval(capture.try_update().unwrap());
    assert_eq!(
        event,
        Event::DocumentComplete {
            status: "already_saved".into(),
            media_kind: None,
        }
    );
    assert_eq!(counts.other, 1);
    assert_eq!(counts.reused_pdf, 0);

    let label = format!("\n\r\x1b\u{202e}\u{2066}{}", "x".repeat(500));
    reporter.event(Event::InventoryStarted {
        label: label.clone(),
    });
    assert_eq!(
        retrieval(capture.try_update().unwrap()).1,
        Event::InventoryStarted {
            label: "x".repeat(120),
        }
    );
    assert!(publish_update(
        Telemetry::DestinationStarted {
            name: label.clone(),
            index: 1,
            total: 2,
            audit: true,
        },
        &format!("\n\x1b\u{202e}{}", "l".repeat(900)),
    ));
    let update = capture.try_update().unwrap();
    assert_eq!(update.line, "l".repeat(512));
    assert!(matches!(
        update.telemetry,
        Some(Telemetry::DestinationStarted { name, index: 1, total: 2, audit: true })
            if name == "x".repeat(120)
    ));
    assert!(publish_update(
        Telemetry::Diagnostic {
            component: label,
            status: DiagnosticStatus::Warning,
        },
        "Doctor: review configuration",
    ));
    assert!(matches!(
        capture.try_update().unwrap().telemetry,
        Some(Telemetry::Diagnostic { component, status: DiagnosticStatus::Warning })
            if component == "x".repeat(120)
    ));
}

#[test]
fn overflow_keeps_current_batch_destination_and_latest_authoritative_counts() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    let capture = subscribe().unwrap();
    for _ in 0..512 {
        assert!(publish_line("Scanning documents…"));
    }
    let mut reporter = Reporter::new(true);
    reporter.event(Event::InventoryStarted {
        label: "old batch".into(),
    });
    reporter.event(Event::InventoryComplete {
        documents: 400,
        complete: true,
    });
    reporter.event(Event::DocumentStarted {
        index: 1,
        total: 400,
    });
    for _ in 0..400 {
        reporter.event(Event::DocumentComplete {
            status: "saved_pdf".into(),
            media_kind: Some("pdf".into()),
        });
    }
    reporter.event(Event::InventoryStarted {
        label: "current batch".into(),
    });
    reporter.event(Event::InventoryComplete {
        documents: 7,
        complete: false,
    });
    for index in 1..=7 {
        reporter.event(Event::DocumentStarted { index, total: 7 });
        reporter.event(Event::DocumentComplete {
            status: "already_saved".into(),
            media_kind: Some("pdf".into()),
        });
    }
    assert!(publish_update(
        Telemetry::DestinationStarted {
            name: "first destination".into(),
            index: 1,
            total: 2,
            audit: false,
        },
        "Starting first destination",
    ));
    assert!(publish_update(
        Telemetry::Delivery(DeliveryEvent {
            phase: DeliveryPhase::Finished,
            processed: 407,
            total: 407,
            delivered: 407,
            attention: 0,
        }),
        "First destination finished",
    ));
    assert!(publish_update(
        Telemetry::DestinationStarted {
            name: "current destination".into(),
            index: 2,
            total: 2,
            audit: true,
        },
        "Starting current destination",
    ));
    for processed in 1..=301 {
        assert!(publish_update(
            Telemetry::Delivery(DeliveryEvent {
                phase: DeliveryPhase::Verifying,
                processed,
                total: 407,
                delivered: processed - 1,
                attention: 1,
            }),
            "Verifying current destination",
        ));
    }
    assert!(publish_update(
        Telemetry::Diagnostic {
            component: "local store".into(),
            status: DiagnosticStatus::Ok,
        },
        "Doctor: local store checked",
    ));
    let mut updates = Vec::new();
    while let Some(update) = capture.try_update() {
        updates.push(update);
    }
    assert_eq!(updates.len(), 256 + 7);
    assert!(
        updates[..256]
            .iter()
            .all(|update| update.telemetry.is_none())
    );
    let typed = updates.into_iter().skip(256).collect::<Vec<_>>();
    let (_, event, counts) = retrieval(typed[0].clone());
    assert_eq!(
        event,
        Event::InventoryStarted {
            label: "current batch".into()
        }
    );
    assert_eq!(counts.new_pdf, 400, "batch base remains available");
    let (_, event, counts) = retrieval(typed[1].clone());
    assert_eq!(
        event,
        Event::InventoryComplete {
            documents: 7,
            complete: false,
        }
    );
    assert_eq!(counts.new_pdf, 400, "inventory certainty remains available");
    let (_, event, counts) = retrieval(typed[2].clone());
    assert_eq!(event, Event::DocumentStarted { index: 7, total: 7 });
    assert_eq!(counts.reused_pdf, 6);
    let (_, event, counts) = retrieval(typed[3].clone());
    assert!(matches!(event, Event::DocumentComplete { .. }));
    assert_eq!(counts.new_pdf, 400);
    assert_eq!(counts.reused_pdf, 7);
    assert!(matches!(
        &typed[4].telemetry,
        Some(Telemetry::DestinationStarted { name, index: 2, total: 2, audit: true })
            if name == "current destination"
    ));
    assert!(matches!(
        typed[5].telemetry,
        Some(Telemetry::Delivery(DeliveryEvent {
            phase: DeliveryPhase::Verifying,
            processed: 301,
            delivered: 300,
            attention: 1,
            ..
        }))
    ));
    assert!(matches!(
        &typed[6].telemetry,
        Some(Telemetry::Diagnostic { component, status: DiagnosticStatus::Ok })
            if component == "local store"
    ));
    assert!(
        capture.try_update().is_none(),
        "snapshots are never replayed"
    );
}

#[test]
fn publications_cannot_jump_ahead_of_a_partially_drained_overflow_suffix() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    let capture = subscribe().unwrap();
    for _ in 0..256 {
        assert!(publish_line("Queued progress"));
    }
    assert!(publish_update(
        Telemetry::DestinationStarted {
            name: "destination".into(),
            index: 1,
            total: 1,
            audit: false,
        },
        "Starting destination",
    ));
    assert!(publish_update(
        Telemetry::Delivery(DeliveryEvent {
            phase: DeliveryPhase::Waiting,
            processed: 1,
            total: 2,
            delivered: 1,
            attention: 0,
        }),
        "Waiting for destination",
    ));
    for _ in 0..256 {
        assert!(capture.try_update().unwrap().telemetry.is_none());
    }
    assert!(matches!(
        capture.try_update().unwrap().telemetry,
        Some(Telemetry::DestinationStarted { .. })
    ));
    assert!(publish_update(
        Telemetry::Diagnostic {
            component: "configuration".into(),
            status: DiagnosticStatus::Ok,
        },
        "Configuration checked",
    ));
    assert!(matches!(
        capture.try_update().unwrap().telemetry,
        Some(Telemetry::Delivery(_))
    ));
    assert!(matches!(
        capture.try_update().unwrap().telemetry,
        Some(Telemetry::Diagnostic { .. })
    ));
    assert!(capture.try_update().is_none());
}

#[test]
fn overflow_does_not_apply_previous_delivery_to_a_new_destination() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    let capture = subscribe().unwrap();
    for _ in 0..256 {
        assert!(publish_line("Queued progress"));
    }
    for index in 1..=3 {
        assert!(publish_update(
            Telemetry::DestinationStarted {
                name: format!("destination {index}"),
                index,
                total: 3,
                audit: false,
            },
            "Starting destination",
        ));
        if index < 3 {
            assert!(publish_update(
                Telemetry::Delivery(DeliveryEvent {
                    phase: DeliveryPhase::Finished,
                    processed: index,
                    total: index,
                    delivered: index,
                    attention: 0,
                }),
                "Previous destination finished",
            ));
        }
    }
    for _ in 0..256 {
        assert!(capture.try_update().unwrap().telemetry.is_none());
    }
    assert!(matches!(
        capture.try_update().unwrap().telemetry,
        Some(Telemetry::DestinationStarted { index: 3, .. })
    ));
    assert!(capture.try_update().is_none());
}

#[test]
fn new_batch_purges_overflow_document_position_and_drop_restores_plain_output() {
    let _guard = CAPTURE_TEST.lock().unwrap();
    assert!(!publish_line("Plain output available"));
    let capture = subscribe().unwrap();
    assert!(subscribe().is_none());
    for _ in 0..256 {
        assert!(publish_line("Queued progress"));
    }
    let mut reporter = Reporter::new(true);
    reporter.event(Event::InventoryStarted {
        label: "previous".into(),
    });
    reporter.event(Event::InventoryComplete {
        documents: 100,
        complete: false,
    });
    reporter.event(Event::DocumentStarted {
        index: 99,
        total: 100,
    });
    reporter.event(Event::DocumentComplete {
        status: "saved_pdf".into(),
        media_kind: Some("pdf".into()),
    });
    reporter.event(Event::InventoryStarted {
        label: "next".into(),
    });
    for _ in 0..256 {
        assert_eq!(capture.try_recv().as_deref(), Some("Queued progress"));
    }
    let (_, event, counts) = retrieval(capture.try_update().unwrap());
    assert_eq!(
        event,
        Event::InventoryStarted {
            label: "next".into()
        }
    );
    assert_eq!(counts.new_pdf, 1);
    assert!(capture.try_update().is_none());
    drop(capture);
    assert!(!publish_update(
        Telemetry::Diagnostic {
            component: "configuration".into(),
            status: DiagnosticStatus::NotChecked,
        },
        "Plain output restored",
    ));
    let fresh = subscribe().unwrap();
    assert!(fresh.try_update().is_none());
    assert!(publish_line("Fresh stream"));
    assert_eq!(fresh.try_recv().as_deref(), Some("Fresh stream"));
}

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
