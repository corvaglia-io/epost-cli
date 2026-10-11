//! Optional human progress on stderr, separate from structured stdout.
//!
//! Events carry counts and fixed retrieval statuses, never document titles,
//! IDs, account details or authenticated response bodies. A broken progress
//! output stream must not interrupt retrieval or change its result.

use std::io::{self, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Authenticating,
    ArchiveFolders,
    InventoryStarted { label: String },
    Page { page: u32, documents: usize },
    InventoryComplete { documents: usize, complete: bool },
    DocumentStarted { index: usize, total: usize },
    DocumentComplete { status: String },
    Finished { exit_code: u8 },
}

#[derive(Default)]
struct Counts {
    pdf: usize,
    json: usize,
    binary: usize,
    reused: usize,
    pending: usize,
    failed: usize,
    deferred: usize,
}

pub struct Reporter {
    enabled: bool,
    counts: Counts,
}

impl Reporter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            counts: Counts::default(),
        }
    }

    /// Emits one complete line on stderr, ignoring any output failure.
    pub fn event(&mut self, event: Event) {
        if let Some(line) = self.render(event) {
            let _ = writeln!(io::stderr().lock(), "{line}");
        }
    }

    /// Renders progress without accessing a terminal, useful to alternative
    /// frontends and synthetic tests. `Finished` begins a fresh count sequence
    /// for the next run. Disabled reporting skips rendering and counting.
    pub fn render(&mut self, event: Event) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let line = match event {
            Event::Authenticating => "Signing in…".into(),
            Event::ArchiveFolders => "Scanning archive folders…".into(),
            Event::InventoryStarted { label } => {
                let label = safe_label(&label);
                if label.is_empty() {
                    "Scanning documents…".into()
                } else {
                    format!("Scanning {label}…")
                }
            }
            Event::Page { page, documents } => {
                format!("Scanning page {page}… ({documents} documents found so far)")
            }
            Event::InventoryComplete {
                documents,
                complete,
            } => {
                if complete {
                    format!("Inventory finished: {documents} documents found.")
                } else {
                    format!("Inventory incomplete: {documents} documents found so far.")
                }
            }
            Event::DocumentStarted { index, total } => {
                format!("Checking document {index} of {total}…")
            }
            Event::DocumentComplete { status } => match status.as_str() {
                "saved_pdf" => {
                    self.counts.pdf += 1;
                    "  Saved PDF.".into()
                }
                "saved_json" => {
                    self.counts.json += 1;
                    "  Saved JSON content; review its format.".into()
                }
                "saved_binary" => {
                    self.counts.binary += 1;
                    "  Saved binary content; review its format.".into()
                }
                "already_saved" => {
                    self.counts.reused += 1;
                    "  Already saved; local file verified.".into()
                }
                "awaiting_manual_acceptance_or_status_verification" => {
                    self.counts.pending += 1;
                    "  Requires acceptance or status verification in ePost.".into()
                }
                "failed" => {
                    self.counts.failed += 1;
                    "  Retrieval failed; see the report for details.".into()
                }
                "deferred_due_to_authentication_failure" => {
                    self.counts.deferred += 1;
                    "  Deferred because sign-in failed.".into()
                }
                _ => {
                    // Do not echo unrecognized status text supplied by a caller.
                    self.counts.deferred += 1;
                    "  Unrecognized retrieval result; review the report.".into()
                }
            },
            Event::Finished { exit_code } => {
                let result = match exit_code {
                    0 => "Sync finished",
                    2 => "Sync finished with items requiring attention",
                    3 => "Sync stopped after a sign-in failure",
                    4 => "Sync stopped after a file integrity failure",
                    _ => "Sync stopped after an error",
                };
                let summary = self.counts.summary();
                self.counts = Counts::default();
                format!("{result}: {summary}.")
            }
        };
        Some(line)
    }
}

impl Counts {
    fn summary(&self) -> String {
        let counts = [
            (self.pdf, "PDF saved"),
            (self.json, "JSON saved"),
            (self.binary, "binary saved"),
            (self.reused, "already saved"),
            (self.pending, "pending manual action"),
            (self.failed, "failed"),
            (self.deferred, "deferred"),
        ];
        let nonzero: Vec<_> = counts
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, label)| format!("{count} {label}"))
            .collect();
        if nonzero.is_empty() {
            "no documents processed".into()
        } else {
            nonzero.join(", ")
        }
    }
}

fn safe_label(label: &str) -> String {
    label
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    *character,
                    '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'
                )
        })
        .take(120)
        .collect()
}
