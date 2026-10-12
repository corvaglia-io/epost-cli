//! Compact operator results. Detailed API diagnostics are opt-in.
use super::{Cli, Command, Error, Value, human_summary, interactive, tui};
use std::collections::BTreeMap;
use tui::{ResultIssue, ResultSection, ResultStatus, ResultView};

fn section(title: impl Into<String>, lines: Vec<String>) -> ResultSection {
    ResultSection {
        title: title.into(),
        lines,
    }
}
fn issue(
    heading: impl Into<String>,
    explanation: impl Into<String>,
    next_step: impl Into<String>,
) -> ResultIssue {
    ResultIssue {
        heading: heading.into(),
        explanation: explanation.into(),
        next_step: next_step.into(),
    }
}
pub(super) fn title(cli: &Cli) -> &'static str {
    match cli.command {
        Command::Check(ref args) => match args.target {
            super::modern::Location::Inbox => "Check inbox",
            super::modern::Location::Archive => "Check archive",
            super::modern::Location::All => "Check all mail",
        },
        Command::Deliver(_) => "Deliver saved documents",
        Command::Verify(_) => "Verify documents",
        Command::Reconcile(_) => "Reconcile existing document",
        Command::Doctor(_) => "Doctor",
        Command::Sync(_) => "Sync documents",
        Command::Status(_) => "Mailbox status",
        Command::Setup { .. } => "Mailbox setup",
        Command::Browse { .. } => "Browse mailbox",
        Command::Login { .. } => "Sign in",
        Command::Destination { .. } => "Destinations",
        Command::Profile { .. } => "Profiles",
        _ => "ePost",
    }
}
pub(super) fn report(cli: &Cli, data: &Value, code: u8) -> ResultView {
    let mut view = ResultView {
        title: title(cli).into(),
        status: if code == 0 {
            ResultStatus::Success
        } else {
            ResultStatus::Attention
        },
        sections: Vec::new(),
        issues: Vec::new(),
        details: human_summary(cli, data, code),
    };
    let sync = if data["sync"].is_object() {
        &data["sync"]
    } else {
        data
    };
    if let Some(documents) = sync["documents"].as_array() {
        let count = |status: &str| documents.iter().filter(|d| d["status"] == status).count();
        // Cached non-PDF items belong only to "other", not reused PDFs as well.
        let reused = documents
            .iter()
            .filter(|d| d["status"] == "already_saved" && d["document"]["media_kind"] == "pdf")
            .count();
        let other = documents
            .iter()
            .filter(|d| {
                matches!(d["status"].as_str(), Some("saved_json" | "saved_binary"))
                    || (d["status"] == "already_saved" && d["document"]["media_kind"] != "pdf")
            })
            .count();
        view.sections.push(section(
            "ePost retrieval",
            vec![
                format!(
                    "{} new PDFs · {reused} reused PDFs · {other} other files",
                    count("saved_pdf")
                ),
                format!(
                    "{} need action in ePost · {} unavailable or failed",
                    count("awaiting_manual_acceptance_or_status_verification"),
                    count("failed")
                ),
            ],
        ));
        let unavailable = documents
            .iter()
            .filter(|d| {
                d["status"] == "failed"
                    && d["diagnostic"]["reason_code"] == "content_unavailable_metadata_available"
            })
            .count();
        if unavailable > 0 {
            view.issues.push(issue(
                format!("{unavailable} ePost files unavailable through the public API"),
                "Their metadata is accessible, but the content route returns HTTP 404.",
                "Download these files manually from ePost. Retrying cannot guarantee availability.",
            ));
        }
        let failed = count("failed").saturating_sub(unavailable);
        if failed > 0 {
            view.issues.push(issue(
                format!("{failed} ePost downloads failed"),
                "These documents have not been saved.",
                "Run Doctor, restore access if needed, then retry Check inbox.",
            ));
        }
        if other > 0 {
            view.issues.push(issue(
                format!("{other} other files kept locally"),
                "These files are not validated PDFs and cannot be delivered to Paperless.",
                "Review their format manually; their local bytes are retained.",
            ));
        }
        let pending = count("awaiting_manual_acceptance_or_status_verification");
        if pending > 0 {
            view.issues.push(issue(
                format!("{pending} letters need action in ePost"),
                "Acceptance or status verification is required.",
                "Open ePost, complete the required action, then retry.",
            ));
        }
        let deferred = count("deferred_sample_limit")
            + count("deferred_limit")
            + count("deferred_due_to_authentication_failure");
        if deferred > 0 {
            view.issues.push(issue(
                format!("{deferred} downloads deferred"),
                "This run did not attempt every eligible item.",
                "Restore sign-in if needed and run Check inbox again.",
            ));
        }
        if sync["pagination_complete"] == false {
            view.issues.push(issue(
                "Mailbox inventory incomplete",
                "Some folders or pages could not be inspected.",
                "Compare with ePost; retry temporary failures. Details explain API limitations.",
            ));
        }
        let unselectable = sync["issues"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|issue| {
                issue.as_str() == Some("archive directory has no documented usable selector")
            })
            .count();
        if unselectable > 0 {
            view.issues.push(issue(
                format!("{unselectable} archive folders have no API selector"),
                "These folders cannot be inspected through the documented public API.",
                "Check their contents manually in ePost; retrying does not add API support.",
            ));
        }
        view.sections.push(section(
            "Coverage",
            vec!["API observations cannot prove complete mailbox coverage.".into()],
        ));
    }
    let inbox = if data["inbox"].is_object() {
        &data["inbox"]
    } else {
        data
    };
    if inbox["mode"].is_string() {
        let mut lines = vec![format!("{} unread observed", n(inbox, "unread_count"))];
        if inbox["new_documents_count"].is_number() {
            lines.push(format!(
                "{} unsaved · {} already saved",
                n(inbox, "new_documents_count"),
                n(inbox, "already_saved_count")
            ));
        } else {
            lines.push("Locally unsaved documents were not checked.".into());
        }
        view.sections.push(section("Inbox", lines));
        if inbox["unknown_read_status_count"].as_u64().unwrap_or(0) > 0
            || inbox["pagination_complete"] == false
        {
            view.issues.push(issue(
                "Inbox status incomplete",
                "Some read states or pages could not be determined.",
                "Compare with ePost; run Check inbox to attempt retrieval.",
            ));
        }
    }
    if data["archive"].is_object() {
        view.sections.push(section(
            "Archive",
            vec![
                format!(
                    "{} documents observed across {} folders",
                    n(&data["archive"], "observed_documents"),
                    n(&data["archive"], "folders")
                ),
                "Observed metadata does not establish complete mailbox coverage.".into(),
            ],
        ));
        if data["archive"]["pagination_complete"] == false {
            view.issues.push(issue(
                "Archive inventory incomplete",
                "Not every archive folder or page could be inspected.",
                "Compare with ePost and open Details for API limitations.",
            ));
        }
    }
    for key in [
        "source_error",
        "delivery_error",
        "archive_error",
        "paperless_error",
        "sync_error",
    ] {
        if let Some(error) = data[key].as_str() {
            view.issues.push(issue(
                match key {
                    "source_error" | "sync_error" => "ePost access needs attention",
                    "archive_error" => "Archive access needs attention",
                    _ => "Delivery access needs attention",
                },
                interactive::safe_line(error),
                "Run Doctor to diagnose access and configuration.",
            ));
        }
    }
    if let Some(rows) = data["destinations"].as_array() {
        if rows.is_empty() {
            view.sections.push(section(
                "Destinations",
                vec!["None configured. Downloads stay local; use Destinations to add one.".into()],
            ));
        }
        for row in rows {
            let name = interactive::safe_line(row["name"].as_str().unwrap_or("Destination"));
            if let Some(error) = row["error"].as_str() {
                view.issues.push(issue(
                    format!("{name}: needs attention"),
                    interactive::safe_line(error),
                    "Run Doctor for this destination, then Deliver to resume.",
                ));
                continue;
            }
            if row["delivery"].is_object() {
                delivery(&mut view, &name, &row["delivery"]);
            } else if row["state"].is_object() {
                let s = &row["state"];
                view.sections.push(section(
                    name,
                    vec![
                        format!(
                            "{} confirmed · {} pending · {} uncertain",
                            n(s, "verified"),
                            n(s, "pending"),
                            n(s, "uncertain")
                        ),
                        "Local history only; use Verify to audit remote copies.".into(),
                    ],
                ));
            }
        }
    }
    if data["paperless"].is_object() {
        delivery(&mut view, "Paperless", &data["paperless"]);
    }
    if let Some(checks) = data["checks"].as_array() {
        let mut good = Vec::new();
        for row in checks {
            let component = interactive::safe_line(row["component"].as_str().unwrap_or("Check"));
            let message =
                interactive::safe_line(row["message"].as_str().unwrap_or("No diagnostic detail"));
            if matches!(row["status"].as_str(), Some("warning" | "error")) {
                view.issues.push(issue(
                    component,
                    message,
                    "Follow this diagnostic before retrying.",
                ));
            } else {
                good.push(format!("{component}: {message}"));
            }
        }
        if !good.is_empty() {
            view.sections.push(section("Diagnostics", good));
        }
    }
    if view.sections.is_empty() && view.issues.is_empty() {
        view.sections
            .push(section("Result", human_summary(cli, data, code)));
    }
    if code != 0 && view.issues.is_empty() {
        view.issues.push(issue(
            "Run needs attention",
            "Some work is unfinished; success has not been confirmed.",
            "Open Details or run Doctor before retrying.",
        ));
    }
    view
}
fn n(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}
fn delivery(view: &mut ResultView, name: &str, d: &Value) {
    let duplicates = d["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["reason_code"] == "duplicate" && i["status"] == "already_delivered")
        .count() as u64;
    let confirmed = n(d, "delivered") + n(d, "already_delivered") + n(d, "previously_confirmed");
    let mut lines = vec![
        format!(
            "{confirmed} confirmed: {} new · {} verified existing · {} previously confirmed",
            n(d, "delivered"),
            n(d, "already_delivered"),
            n(d, "previously_confirmed")
        ),
        format!(
            "{} pending · {} uncertain · {} failed · {} unsupported",
            n(d, "pending"),
            n(d, "uncertain"),
            n(d, "failed"),
            n(d, "unsupported")
        ),
    ];
    if duplicates > 0 {
        lines.push(format!(
            "{duplicates} duplicate files matched an existing document; its tags were preserved."
        ));
    }
    if n(d, "deferred_limit") + n(d, "not_attempted") > 0 {
        lines.push(format!(
            "{} deferred or not attempted; Deliver resumes remaining work.",
            n(d, "deferred_limit") + n(d, "not_attempted")
        ));
    }
    view.sections.push(section(name, lines));
    let mut groups = BTreeMap::<&str, usize>::new();
    for item in d["items"].as_array().into_iter().flatten() {
        if matches!(
            item["status"].as_str(),
            Some("delivered" | "already_delivered" | "previously_confirmed")
        ) {
            continue;
        }
        let reason = item["reason_code"]
            .as_str()
            .unwrap_or_else(|| item["status"].as_str().unwrap_or("processing_failure"));
        *groups.entry(reason).or_default() += 1;
    }
    for (reason, count) in groups {
        let (heading, why, next) = match reason {
            "duplicate" => (
                "duplicate needs verification",
                "Paperless reported an existing document, but its original bytes could not be confirmed.",
                "Run Deliver to retry verification; use Reconcile only with an explicitly checked existing document.",
            ),
            "protected_pdf" => (
                "protected PDF rejected",
                "Paperless could not process this protected PDF.",
                "Review the file and the task in Paperless; do not keep uploading it.",
            ),
            "unsupported" | "unsupported_content" => (
                "unsupported files",
                "Only locally validated PDFs are supported for Paperless delivery.",
                "Review the retained source files manually.",
            ),
            "upload_outcome_unknown" | "uncertain" | "reconciliation_ambiguous" => (
                "upload outcome uncertain",
                "An upload may already exist. Automatic resubmission is blocked to avoid duplicates.",
                "Find the matching Paperless document and explicitly reconcile it. Details show the recovery route.",
            ),
            "pending" => (
                "processing still pending",
                "Paperless has accepted the upload but ingestion is not yet confirmed.",
                "Run Deliver later; it checks the existing task without uploading again.",
            ),
            "authentication_failed" | "access_blocked" => (
                "access unavailable",
                "Credentials or the sign-in proxy prevented destination access.",
                "Run Doctor, restore access, then Deliver.",
            ),
            "verification_failed" => (
                "verification failed",
                "The saved original could not be confirmed at the destination.",
                "Run Verify and inspect the original file before reconciling.",
            ),
            "deferred_limit" | "not_attempted" => (
                "work deferred",
                "This run stopped before these items were attempted.",
                "Run Deliver to resume remaining work.",
            ),
            _ => (
                "processing failed",
                "Paperless reported an ingestion failure; no successful delivery was recorded.",
                "Inspect the failed task in Paperless. Details distinguish known causes; no automatic resubmission occurs.",
            ),
        };
        view.issues
            .push(issue(format!("{name}: {count} {heading}"), why, next));
    }
}
pub(super) fn error(cli: &Cli, error: &Error) -> ResultView {
    ResultView {
        title: title(cli).into(),
        status: ResultStatus::Error,
        sections: Vec::new(),
        issues: vec![issue(
            "Command could not finish",
            error.to_string(),
            "Run Doctor to diagnose access and configuration; retry after resolving the issue.",
        )],
        details: vec![format!("Exit code: {}", error.exit_code())],
    }
}
pub(super) fn text(view: &ResultView) -> Vec<String> {
    let mut lines = vec![format!(
        "{} — {}",
        view.title,
        match view.status {
            ResultStatus::Success => "finished",
            ResultStatus::Attention => "needs attention",
            ResultStatus::Error => "failed",
        }
    )];
    for section in &view.sections {
        lines.push(format!("{}:", section.title));
        lines.extend(section.lines.iter().cloned());
    }
    for issue in &view.issues {
        lines.push(format!(
            "{}: {} Next: {}",
            issue.heading, issue.explanation, issue.next_step
        ));
    }
    if !view.issues.is_empty() {
        lines
            .push("Use --details for technical explanations or --json for the full report.".into());
    }
    lines
        .into_iter()
        .map(|line| interactive::safe_line(&line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;
    #[test]
    fn cached_other_files_do_not_count_as_reused_pdfs() {
        let cli = Cli::parse_from(["epost", "check", "inbox"]);
        let v = report(
            &cli,
            &json!({"sync":{"documents":[{"status":"already_saved","document":{"media_kind":"pdf"}},{"status":"already_saved","document":{"media_kind":"binary"}}],"pagination_complete":true},"destinations":[]}),
            2,
        );
        assert!(v.sections[0].lines[0].contains("1 reused PDFs · 1 other files"));
    }
    #[test]
    fn failed_task_text_is_never_used_as_operator_guidance() {
        let cli = Cli::parse_from(["epost", "deliver"]);
        let v = report(
            &cli,
            &json!({"destinations":[{"name":"test","delivery":{"failed":1,"items":[{"status":"failed","reason_code":"protected_pdf","error":"synthetic-private-title-token"}]}}]}),
            2,
        );
        let text = text(&v).join("\n");
        assert!(text.contains("protected PDF rejected"));
        assert!(!text.contains("synthetic-private-title-token"));
    }
    #[test]
    fn verified_duplicate_is_success_and_preserves_review_state() {
        let cli = Cli::parse_from(["epost", "deliver"]);
        let v = report(
            &cli,
            &json!({"destinations":[{"name":"test","delivery":{"already_delivered":1,"items":[{"status":"already_delivered","reason_code":"duplicate"}]}}]}),
            0,
        );
        assert!(v.issues.is_empty());
        assert!(text(&v).join("\n").contains("tags were preserved"));
    }
    #[test]
    fn status_keeps_source_counts_beside_destination_state() {
        let cli = Cli::parse_from(["epost", "status", "all"]);
        let v = report(
            &cli,
            &json!({"inbox":{"mode":"metadata_and_local_store","unread_count":2,"new_documents_count":3,"already_saved_count":4,"unknown_read_status_count":1,"pagination_complete":true},"archive":{"observed_documents":5,"folders":2,"pagination_complete":false},"destinations":[]}),
            2,
        );
        let text = text(&v).join("\n");
        assert!(text.contains("2 unread observed"));
        assert!(text.contains("3 unsaved · 4 already saved"));
        assert!(text.contains("5 documents observed across 2 folders"));
        assert!(text.contains("Inbox status incomplete"));
        assert!(text.contains("Archive inventory incomplete"));
    }
}
