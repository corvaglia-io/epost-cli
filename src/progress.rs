//! Optional human progress on stderr, separate from structured stdout.
//!
//! Events carry counts and fixed retrieval statuses, never document titles,
//! IDs, account details or authenticated response bodies. A broken progress
//! output stream must not interrupt retrieval or change its result.

use std::io::{self, Write};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
};

const BUFFERED_LINES: usize = 256;
static NEXT_OBSERVER: AtomicU64 = AtomicU64::new(1);
static OBSERVER: Mutex<Option<(u64, SyncSender<String>)>> = Mutex::new(None);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Requests a cooperative stop between requests. Never discard an in-flight
/// upload: the workflow must preserve its result and recovery journal first.
pub fn request_stop() {
    STOP_REQUESTED.store(true, Ordering::Release);
}

pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Acquire)
}

/// Begins or ends the CLI's single operation. Frontends must not reset this
/// while a workflow is still finishing an in-flight request.
pub fn reset_stop() {
    STOP_REQUESTED.store(false, Ordering::Release);
}

/// An exclusive, bounded progress stream. Dropping it restores plain stderr
/// reporting. Only a frontend actively displaying progress should subscribe.
pub struct Subscription {
    id: u64,
    receiver: Receiver<String>,
}

impl Subscription {
    /// Poll without blocking the async operation or terminal input loop.
    pub fn try_recv(&self) -> Option<String> {
        self.receiver.try_recv().ok()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Ok(mut observer) = OBSERVER.lock()
            && observer.as_ref().is_some_and(|(id, _)| *id == self.id)
        {
            *observer = None;
        }
    }
}

/// Opens one stream without displacing an existing frontend. The buffer has a
/// fixed capacity so a slow renderer cannot grow memory or stall retrieval.
pub fn subscribe() -> Option<Subscription> {
    let mut observer = OBSERVER.lock().ok()?;
    if observer.is_some() {
        return None;
    }
    let id = NEXT_OBSERVER.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::sync_channel(BUFFERED_LINES);
    *observer = Some((id, sender));
    Some(Subscription { id, receiver })
}

/// Publishes a fixed, redacted human progress line when a frontend is active.
/// Returns false when the caller should write to stderr instead. Callers must
/// never supply titles, document/account IDs, secrets or response bodies.
/// Excess updates are dropped, never printed over an active terminal view.
pub fn publish_line(line: &str) -> bool {
    let Ok(mut observer) = OBSERVER.lock() else {
        return false;
    };
    let Some((_, sender)) = observer.as_ref() else {
        return false;
    };
    let line = line
        .chars()
        .filter(|character| safe_character(*character))
        .take(512)
        .collect();
    match sender.try_send(line) {
        Ok(()) | Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => {
            *observer = None;
            false
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Authenticating,
    ArchiveFolders,
    InventoryStarted {
        label: String,
    },
    Page {
        page: u32,
        documents: usize,
    },
    InventoryComplete {
        documents: usize,
        complete: bool,
    },
    DocumentStarted {
        index: usize,
        total: usize,
    },
    DocumentComplete {
        status: String,
        media_kind: Option<String>,
    },
    Finished {
        exit_code: u8,
    },
}

#[derive(Default)]
struct Counts {
    pdf: usize,
    json: usize,
    binary: usize,
    reused: usize,
    reused_other: usize,
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
        if let Some(line) = self.render(event)
            && !publish_line(&line)
        {
            let _ = writeln!(io::stderr().lock(), "{line}");
        }
    }

    /// Starts a fresh count sequence without displaying a final summary.
    /// Frontends that own a shared final summary can use this between runs.
    pub fn reset(&mut self) {
        self.counts = Counts::default();
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
            Event::DocumentComplete { status, media_kind } => match status.as_str() {
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
                    if media_kind.as_deref() == Some("pdf") {
                        "  Already saved; local file verified.".into()
                    } else {
                        self.counts.reused_other += 1;
                        "  Already saved; local file verified, but format still needs review."
                            .into()
                    }
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
                self.reset();
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
            (self.reused_other, "cached non-PDF needing format review"),
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
        .filter(|character| safe_character(*character))
        .take(120)
        .collect()
}

fn safe_character(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_is_exclusive_bounded_sanitized_and_restored_on_drop() {
        assert!(!publish_line("Signing in…"));
        let capture = subscribe().unwrap();
        assert!(subscribe().is_none());
        let mut reporter = Reporter::new(true);
        reporter.event(Event::Authenticating);
        assert_eq!(capture.try_recv().as_deref(), Some("Signing in…"));
        assert!(publish_line("Paperless: waiting\n\x1b\u{202e}"));
        assert_eq!(capture.try_recv().as_deref(), Some("Paperless: waiting"));
        assert!(publish_line(&"x".repeat(2048)));
        assert_eq!(capture.try_recv().unwrap().len(), 512);
        for _ in 0..BUFFERED_LINES * 2 {
            assert!(publish_line("Checking documents…"));
        }
        let mut received = 0;
        while capture.try_recv().is_some() {
            received += 1;
        }
        assert_eq!(received, BUFFERED_LINES);
        drop(capture);
        assert!(!publish_line("Finished."));
        assert!(subscribe().is_some());
    }
}
