//! Shared human sync summaries without provider IDs, titles or arbitrary text.
//!
//! Detailed retrieval reports remain available as JSON. Human explanations use
//! fixed reason messages and a bounded format-hint allowlist so progress and
//! interactive frontends can share useful, safe diagnostics.

use serde_json::Value;
use std::collections::BTreeMap;

/// A verified metadata response does not prove deletion or web availability.
/// Keep the diagnosis scoped to the public API and offer a manual recovery path.
pub const PUBLIC_API_CONTENT_UNAVAILABLE_MESSAGE: &str = "public API cannot download this item (HTTP 404), although its metadata is accessible; check or download it manually in the ePost app or website";

/// Aggregated fixed explanations for partial retrieval and uncertain inventory.
pub fn attention_lines(report: &Value) -> Vec<String> {
    let mut reasons = BTreeMap::<String, usize>::new();
    let mut add = |reason: String| *reasons.entry(reason).or_default() += 1;
    for document in documents(report) {
        if let Some(reason) = document_reason(document) {
            add(reason);
        }
    }
    // Folder observations provide detail but repeat existing issue records.
    // Count attention from issues only, rather than double-count each folder.
    for issue in report["issues"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        add(summary_reason(issue));
    }
    for inventory in report["inventories"].as_array().into_iter().flatten() {
        for issue in inventory["issues"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            add(summary_reason(issue));
        }
    }
    let mut lines: Vec<_> = reasons
        .into_iter()
        .map(|(reason, count)| format!("Attention ({count}): {reason}."))
        .collect();
    if let Some(hints) = format_hints(report) {
        lines.push(hints);
    }
    lines.extend(unavailable_content_lines(report));
    if report["pagination_complete"] == false {
        lines.push("Inventory is incomplete. Check missing folders/documents in ePost; retrying may resolve temporary failures.".into());
    }
    lines
}

/// Fixed explanations for one numbered download, without identifiers or raw
/// response text. A failed retrieval must be visible even when no file exists.
pub fn document_attention(document: &Value) -> Vec<String> {
    document_reason(document)
        .map(|reason| vec![format!("Download needs attention: {reason}.")])
        .unwrap_or_default()
}

/// Final human report shared by interactive and standalone sync commands.
/// The other-format count overlaps the already-saved count when cached content
/// still needs review; the wording makes this explicit instead of hiding it.
pub fn sync_summary(report: &Value, code: u8) -> Vec<String> {
    let documents = documents(report);
    let count = |status: &str| documents.iter().filter(|d| d["status"] == status).count();
    let other_formats = documents.iter().filter(|d| other_format(d)).count();
    let mut lines = vec![format!(
        "Sync: {} new PDFs, {} already saved, {} other formats (new or cached), {} require action in ePost, {} failed.",
        count("saved_pdf"),
        count("already_saved"),
        other_formats,
        count("awaiting_manual_acceptance_or_status_verification"),
        count("failed")
    )];
    if code != 0 {
        lines.push(format!("Sync needs attention (exit {code})."));
    }
    let empty_folders = report["archive_directories"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["selection"] == "unselectable_empty")
        .count();
    if empty_folders > 0 {
        lines.push(format!(
            "Archive: {empty_folders} folders reported empty with no subfolders; no API selector (coverage unverified)."
        ));
    }
    lines.extend(attention_lines(report));
    lines.push("Compare with ePost; API pagination cannot prove complete mailbox coverage. Use --json for the detailed report.".into());
    lines
}

fn documents(report: &Value) -> &[Value] {
    report["documents"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn other_format(document: &Value) -> bool {
    match document["status"].as_str() {
        Some("saved_json" | "saved_binary") => true,
        Some("already_saved") => document["document"]["media_kind"].as_str() != Some("pdf"),
        _ => false,
    }
}

fn document_reason(document: &Value) -> Option<String> {
    match document["status"].as_str() {
        Some("saved_json" | "saved_binary" | "already_saved") if other_format(document) => {
            Some("unsupported content preserved locally; review its format manually".into())
        }
        Some("awaiting_manual_acceptance_or_status_verification") => {
            Some("check acceptance or status in ePost, then retry".into())
        }
        Some("failed") => Some(
            diagnostic_reason(&document["diagnostic"])
                .map(str::to_owned)
                .unwrap_or_else(|| summary_reason(document["error"].as_str().unwrap_or(""))),
        ),
        Some("deferred_due_to_authentication_failure") => {
            Some("sign-in failed; retrieval deferred until the next sync".into())
        }
        _ => None,
    }
}

fn diagnostic_reason(diagnostic: &Value) -> Option<&'static str> {
    match diagnostic["reason_code"].as_str()? {
        "content_unavailable_metadata_available"
            if diagnostic["letter_type"] == "SIMPLE_SHORT_MESSAGE"
                && diagnostic["has_message"] == true =>
        {
            Some(
                "message exists in letter metadata, but the public API cannot download content (HTTP 404); review this item in the ePost app or website",
            )
        }
        "content_unavailable_metadata_available" => Some(PUBLIC_API_CONTENT_UNAVAILABLE_MESSAGE),
        "letter_unavailable" => {
            Some("document metadata and content returned HTTP 404; check the document in ePost")
        }
        "metadata_check_failed" => {
            Some("content returned HTTP 404; metadata check failed, so availability is uncertain")
        }
        "metadata_id_mismatch" => Some(
            "content returned HTTP 404; metadata ID did not match, so availability is uncertain",
        ),
        _ => None,
    }
}

fn format_hints(report: &Value) -> Option<String> {
    let mut hints = BTreeMap::<&'static str, usize>::new();
    for document in documents(report).iter().filter(|d| other_format(d)) {
        let hint = document["document"]["format_hint"]
            .as_str()
            .or_else(|| document["diagnostic"]["format_hint"].as_str());
        let known = match hint {
            Some("pdf") => "PDF",
            Some("png") => "PNG",
            Some("jpeg") => "JPEG",
            Some("tiff") => "TIFF",
            Some("gif") => "GIF",
            Some("zip") => "ZIP",
            Some("html") => "HTML",
            Some("text") => "text",
            _ => continue,
        };
        *hints.entry(known).or_default() += 1;
    }
    if hints.is_empty() {
        return None;
    }
    let summary: Vec<_> = hints
        .into_iter()
        .map(|(hint, count)| format!("{count} {hint}"))
        .collect();
    Some(format!(
        "Format hints for other content (not validated): {}.",
        summary.join(", ")
    ))
}

fn unavailable_content_lines(report: &Value) -> Vec<String> {
    let mut kinds = BTreeMap::<&'static str, (usize, usize, usize, usize)>::new();
    for document in documents(report) {
        let diagnostic = &document["diagnostic"];
        if document["status"] != "failed"
            || diagnostic["reason_code"] != "content_unavailable_metadata_available"
        {
            continue;
        }
        let kind = match diagnostic["letter_type"].as_str() {
            Some("CLASSIC_LETTER") => "CLASSIC_LETTER",
            Some("SMART_LETTER") => "SMART_LETTER",
            Some("SMART_LETTER_ANSWER") => "SMART_LETTER_ANSWER",
            Some("SIMPLE_SHORT_MESSAGE") => "SIMPLE_SHORT_MESSAGE",
            Some("INCAMAIL") => "INCAMAIL",
            Some("SECURESEND") => "SECURESEND",
            _ => "unknown type",
        };
        let counts = kinds.entry(kind).or_default();
        counts.0 += 1;
        counts.1 += usize::from(diagnostic["has_message"] == true);
        counts.2 += usize::from(diagnostic["has_file_name"] == true);
        counts.3 += usize::from(diagnostic["has_content_reference"] == true);
    }
    kinds
        .into_iter()
        .map(|(kind, (count, messages, filenames, references))| {
            format!("Unavailable content: {kind} ({count}); metadata available; messages present: {messages}, filenames present: {filenames}, content references present: {references}.")
        })
        .collect()
}

/// Only known messages are rendered; arbitrary text stays in detailed JSON.
fn summary_reason(reason: &str) -> String {
    if let Some(code) = reason
        .strip_prefix("provider returned HTTP ")
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|code| (100..=599).contains(code))
    {
        return if code == 404 {
            "provider returned HTTP 404; check unavailable documents in ePost".into()
        } else {
            format!("provider returned HTTP {code}; retry or inspect the detailed report")
        };
    }
    match reason {
        "archive directory has no documented usable selector" => {
            "archive folder has no API selector; check its documents manually in ePost"
        }
        "duplicate archive directory ID" => "duplicate archive folder; inventory is uncertain",
        "duplicate IDs observed across pages" => {
            "documents overlapped across pages; inventory is uncertain"
        }
        "letter without usable ID" => "document has no usable ID; check it manually in ePost",
        "maximum page count reached" => "page limit reached; increase --max-pages and retry",
        "pagination made no progress" => {
            "pagination made no progress; retry and compare with ePost"
        }
        "network request failed or timed out" => "network request failed or timed out; retry",
        "authentication failed or expired" => "sign-in failed or expired; sign in and retry",
        _ if reason.contains("safety limit")
            || reason == "pagination offset exceeded API range" =>
        {
            "safety limit reached; inventory is incomplete"
        }
        _ if reason.contains("authentication failed during") => {
            "sign-in failed; some retrieval was deferred"
        }
        _ => "retrieval or inventory issue; inspect the detailed JSON report and ePost",
    }
    .into()
}
