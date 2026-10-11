use epost_cli::summary::{attention_lines, document_attention, sync_summary};
use serde_json::{Value, json};

fn report(documents: Value) -> Value {
    json!({
        "documents": documents,
        "issues": [],
        "inventories": [],
        "archive_directories": [],
        "pagination_complete": true,
        "mailbox_coverage_verified": false,
    })
}

#[test]
fn cached_non_pdf_formats_are_counted_without_claiming_they_are_new() {
    let report = report(json!([
        { "status": "saved_pdf", "document": { "media_kind": "pdf" } },
        { "status": "already_saved", "document": { "media_kind": "pdf" } },
        { "status": "already_saved", "document": { "media_kind": "json" } },
        { "status": "already_saved", "document": { "media_kind": "binary" } },
        { "status": "saved_json", "document": { "media_kind": "json" } },
        { "status": "saved_binary", "document": { "media_kind": "binary" } },
    ]));
    let lines = sync_summary(&report, 2);
    assert_eq!(
        lines[0],
        "Sync: 1 new PDFs, 3 already saved, 4 other formats (new or cached), 0 require action in ePost, 0 failed."
    );
    assert!(lines.iter().any(|line| line
        == "Attention (4): unsupported content preserved locally; review its format manually."));
    assert!(
        lines
            .iter()
            .any(|line| line == "Sync needs attention (exit 2).")
    );
}

#[test]
fn unavailable_content_uses_the_verified_metadata_diagnostic() {
    let report = report(json!([
        { "status": "failed", "error": "provider returned HTTP 404", "diagnostic": { "reason_code": "content_unavailable_metadata_available" } },
        { "status": "failed", "error": "provider returned HTTP 404", "diagnostic": { "reason_code": "content_unavailable_metadata_available" } },
        { "status": "failed", "error": "provider returned HTTP 404", "diagnostic": { "reason_code": "letter_unavailable" } },
        { "status": "failed", "error": "provider returned HTTP 404", "diagnostic": { "reason_code": "metadata_check_failed" } },
        { "status": "failed", "error": "provider returned HTTP 404", "diagnostic": { "reason_code": "metadata_id_mismatch" } },
    ]));
    let lines = attention_lines(&report);
    for expected in [
        "Attention (2): document metadata is available, but content returned HTTP 404; check the document in ePost.",
        "Attention (1): document metadata and content returned HTTP 404; check the document in ePost.",
        "Attention (1): content returned HTTP 404; metadata check failed, so availability is uncertain.",
        "Attention (1): content returned HTTP 404; metadata ID did not match, so availability is uncertain.",
    ] {
        assert!(
            lines.iter().any(|line| line == expected),
            "missing reason {expected}"
        );
    }
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("provider returned HTTP 404"))
    );
}

#[test]
fn short_messages_explain_metadata_without_displaying_the_message() {
    let report = report(json!([
        {
            "status": "failed",
            "letter_id": "invented-private-message-id",
            "error": "provider returned HTTP 404",
            "diagnostic": {
                "reason_code": "content_unavailable_metadata_available",
                "letter_type": "SIMPLE_SHORT_MESSAGE",
                "has_message": true,
                "message": "synthetic-private-message-never-echo",
            }
        }
    ]));
    let lines = attention_lines(&report).join("\n");
    assert!(lines.contains("Attention (1): message exists in letter metadata, but downloadable content is unavailable; review it in ePost."));
    assert!(lines.contains("Unavailable content: SIMPLE_SHORT_MESSAGE (1); metadata available; messages present: 1, filenames present: 0, content references present: 0."));
    assert!(!lines.contains("synthetic-private-message"));
    assert!(!lines.contains("invented-private-message-id"));
}

#[test]
fn missing_message_flag_does_not_claim_a_message_exists() {
    for flag in [Value::Null, json!(false), json!("true")] {
        let report = report(json!([
            { "status": "failed", "diagnostic": {
                "reason_code": "content_unavailable_metadata_available",
                "letter_type": "SIMPLE_SHORT_MESSAGE",
                "has_message": flag,
            } }
        ]));
        let lines = attention_lines(&report).join("\n");
        assert!(lines.contains("document metadata is available, but content returned HTTP 404"));
        assert!(!lines.contains("message exists in letter metadata"));
    }
}

#[test]
fn diagnostic_type_and_flags_are_aggregated_using_only_known_values() {
    let report = report(json!([
        { "status": "failed", "diagnostic": { "reason_code": "content_unavailable_metadata_available", "letter_type": "SMART_LETTER", "has_message": false, "has_file_name": true, "has_content_reference": true } },
        { "status": "failed", "diagnostic": { "reason_code": "content_unavailable_metadata_available", "letter_type": "SMART_LETTER", "has_message": true, "has_file_name": false, "has_content_reference": true } },
        { "status": "failed", "diagnostic": { "reason_code": "content_unavailable_metadata_available", "letter_type": "synthetic-secret-type\n\u{202e}", "has_message": "synthetic-private-text" } },
    ]));
    let lines = attention_lines(&report).join("\n");
    assert!(lines.contains("Unavailable content: SMART_LETTER (2); metadata available; messages present: 1, filenames present: 1, content references present: 2."));
    assert!(lines.contains("Unavailable content: unknown type (1); metadata available; messages present: 0, filenames present: 0, content references present: 0."));
    assert!(!lines.contains("synthetic-secret-type"));
    assert!(!lines.contains("synthetic-private-text"));
}

#[test]
fn format_hints_are_known_bounded_and_count_new_and_cached_content() {
    let mut documents = Vec::new();
    for hint in ["pdf", "png", "jpeg", "tiff", "gif", "zip", "html", "text"] {
        documents.push(json!({ "status": "saved_binary", "document": { "media_kind": "binary", "format_hint": hint } }));
    }
    documents.push(json!({ "status": "already_saved", "document": { "media_kind": "binary", "format_hint": "png" } }));
    documents.push(json!({ "status": "saved_binary", "document": { "media_kind": "binary" }, "diagnostic": { "format_hint": "png" } }));
    documents.push(json!({ "status": "saved_binary", "document": { "media_kind": "binary", "format_hint": "synthetic-private-hint\n\u{202e}" } }));
    documents.push(
        json!({ "status": "saved_pdf", "document": { "media_kind": "pdf", "format_hint": "png" } }),
    );
    let report = report(json!(documents));
    let lines = attention_lines(&report).join("\n");
    assert!(lines.contains("Format hints for other content (not validated): 1 GIF, 1 HTML, 1 JPEG, 1 PDF, 3 PNG, 1 TIFF, 1 ZIP, 1 text."));
    assert!(lines.contains("Attention (11): unsupported content preserved locally"));
    assert!(!lines.contains("synthetic-private-hint"));
}

#[test]
fn unsupported_unknown_hints_are_not_displayed() {
    let report = report(json!([
        { "status": "saved_binary", "document": { "media_kind": "binary", "format_hint": "synthetic-private-hint" } }
    ]));
    let lines = attention_lines(&report).join("\n");
    assert!(lines.contains("unsupported content preserved locally"));
    assert!(!lines.contains("Format hints"));
    assert!(!lines.contains("synthetic-private-hint"));
}

#[test]
fn directory_observations_do_not_double_count_existing_attention_issues() {
    let mut report = report(json!([]));
    report["issues"] = json!(["archive directory has no documented usable selector"]);
    report["archive_directories"] = json!([
        { "directory": { "directoryId": "invented-private-folder", "directoryName": "Synthetic private folder name" }, "selection": "unselectable", "reason_code": "archive_directory_unselectable" }
    ]);
    report["pagination_complete"] = json!(false);
    let lines = sync_summary(&report, 2).join("\n");
    assert_eq!(
        lines
            .matches("Attention (1): archive folder has no API selector")
            .count(),
        1
    );
    assert!(!lines.contains("Attention (2)"));
    assert!(!lines.contains("invented-private-folder"));
    assert!(!lines.contains("Synthetic private folder name"));
    assert_eq!(lines.matches("Inventory is incomplete.").count(), 1);
}

#[test]
fn reported_empty_folders_are_informational_and_do_not_claim_coverage() {
    let mut report = report(json!([]));
    report["archive_directories"] = json!([
        { "directory": { "directoryName": "Synthetic private empty folder" }, "selection": "unselectable_empty" },
        { "directory": {}, "selection": "unselectable_empty" },
    ]);
    let lines = sync_summary(&report, 0).join("\n");
    assert!(lines.contains("Archive: 2 folders reported empty with no subfolders; no API selector (coverage unverified)."));
    assert!(!lines.contains("Attention"));
    assert!(!lines.contains("Sync needs attention"));
    assert!(!lines.contains("Synthetic private empty folder"));
    assert!(lines.contains("API pagination cannot prove complete mailbox coverage"));
}

#[test]
fn arbitrary_diagnostics_and_issue_text_are_not_echoed() {
    let mut report = report(json!([
        { "status": "failed", "error": "synthetic-private-body\n\u{202e}", "diagnostic": { "reason_code": "synthetic-private-reason" } },
        { "status": "failed", "error": "provider returned HTTP 9876" },
        { "status": "failed", "error": "provider returned HTTP 500" },
    ]));
    report["issues"] = json!(["synthetic-private-directory-id: synthetic-private-body"]);
    let lines = attention_lines(&report).join("\n");
    assert!(!lines.contains("synthetic-private"));
    assert!(!lines.contains("9876"));
    assert!(!lines.contains('\u{202e}'));
    assert!(lines.contains(
        "Attention (3): retrieval or inventory issue; inspect the detailed JSON report and ePost."
    ));
    assert!(lines.contains(
        "Attention (1): provider returned HTTP 500; retry or inspect the detailed report."
    ));
}

#[test]
fn failed_numbered_download_uses_the_same_safe_reason_as_sync() {
    let document = json!({
        "status": "failed",
        "error": "synthetic-private-error-never-echo",
        "letter_id": "synthetic-private-identifier-never-echo",
        "diagnostic": { "reason_code": "metadata_id_mismatch" },
    });
    assert_eq!(
        document_attention(&document),
        vec![
            "Download needs attention: content returned HTTP 404; metadata ID did not match, so availability is uncertain."
        ]
    );
    assert!(document_attention(&json!({ "status": "saved_pdf" })).is_empty());
}
