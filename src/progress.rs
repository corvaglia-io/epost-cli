//! Optional human progress on stderr, separate from structured stdout.
//!
//! Events carry counts and fixed retrieval statuses, never document titles,
//! IDs, account details or authenticated response bodies. A broken progress
//! output stream must not interrupt retrieval or change its result.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

const BUFFERED_LINES: usize = 256;
const TELEMETRY_SLOTS: usize = 7;
static NEXT_OBSERVER: AtomicU64 = AtomicU64::new(1);
static OBSERVER: Mutex<Option<(u64, Arc<Mutex<Stream>>)>> = Mutex::new(None);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// A redacted line and optional typed state for alternative frontends. Counts
/// are authoritative snapshots; renderers must assign them rather than add them.
#[derive(Clone, Debug)]
pub struct Update {
    pub line: String,
    pub telemetry: Option<Telemetry>,
}

#[derive(Clone, Debug)]
pub enum Telemetry {
    Retrieval {
        event: Event,
        counts: RetrievalCounts,
    },
    DestinationStarted {
        name: String,
        index: usize,
        total: usize,
        audit: bool,
    },
    Delivery(crate::delivery::DeliveryEvent),
    Diagnostic {
        component: String,
        status: DiagnosticStatus,
    },
}

/// Disjoint source outcomes. `other` includes newly saved and reused non-PDF
/// content; `reused_pdf` never counts a cached file needing format review.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetrievalCounts {
    pub new_pdf: usize,
    pub reused_pdf: usize,
    pub other: usize,
    pub pending: usize,
    pub failed: usize,
    pub deferred: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticStatus {
    Ok,
    Warning,
    Error,
    NotChecked,
}

struct BufferedUpdate {
    sequence: u64,
    update: Update,
}

struct Stream {
    queue: VecDeque<Update>,
    latest: [Option<BufferedUpdate>; TELEMETRY_SLOTS],
    sequence: u64,
}

impl Stream {
    fn new() -> Self {
        Self {
            queue: VecDeque::with_capacity(BUFFERED_LINES),
            latest: std::array::from_fn(|_| None),
            sequence: 0,
        }
    }

    fn push(&mut self, update: Update) {
        self.sequence = self.sequence.saturating_add(1);
        if self.queue.len() < BUFFERED_LINES && self.latest.iter().all(Option::is_none) {
            self.queue.push_back(update);
        } else if let Some(telemetry) = &update.telemetry {
            let domain = match telemetry {
                Telemetry::Retrieval {
                    event: Event::InventoryStarted { .. },
                    ..
                } => {
                    self.latest[1] = None;
                    self.latest[2] = None;
                    self.latest[3] = None;
                    0
                }
                Telemetry::Retrieval {
                    event: Event::InventoryComplete { .. },
                    ..
                } => 1,
                Telemetry::Retrieval {
                    event: Event::DocumentStarted { .. },
                    ..
                } => {
                    self.latest[3] = None;
                    2
                }
                Telemetry::Retrieval { event, .. } => {
                    if matches!(event, Event::Authenticating) {
                        self.latest[0] = None;
                        self.latest[1] = None;
                        self.latest[2] = None;
                    }
                    3
                }
                Telemetry::DestinationStarted { .. } => {
                    self.latest[5] = None;
                    4
                }
                Telemetry::Delivery(_) => 5,
                Telemetry::Diagnostic { .. } => 6,
            };
            self.latest[domain] = Some(BufferedUpdate {
                sequence: self.sequence,
                update,
            });
        }
        // Line-only overflow retains the historical drop behavior. Typed
        // overflow keeps bounded state, including the current retrieval batch,
        // inventory certainty and document landmarks. Never wait for capacity
        // or lose the active destination to a delivery update.
    }

    fn pop(&mut self) -> Option<Update> {
        self.queue.pop_front().or_else(|| {
            let domain = self
                .latest
                .iter()
                .enumerate()
                .filter_map(|(domain, entry)| entry.as_ref().map(|item| (domain, item.sequence)))
                .min_by_key(|(_, sequence)| *sequence)
                .map(|(domain, _)| domain)?;
            self.latest[domain].take().map(|item| item.update)
        })
    }
}

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
    stream: Arc<Mutex<Stream>>,
}

impl Subscription {
    /// Poll without blocking the async operation or terminal input loop.
    pub fn try_recv(&self) -> Option<String> {
        self.try_update().map(|update| update.line)
    }

    /// Poll typed state with the same line as the plain progress reporter.
    /// After overflow, queued lines precede the latest snapshots in publication
    /// order. At most seven overflow snapshots retain the latest state and the
    /// current inventory/document landmarks, without duplicate event replay.
    pub fn try_update(&self) -> Option<Update> {
        self.stream.lock().ok()?.pop()
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
    let stream = Arc::new(Mutex::new(Stream::new()));
    *observer = Some((id, Arc::clone(&stream)));
    Some(Subscription { id, stream })
}

/// Publishes a fixed, redacted human progress line when a frontend is active.
/// Returns false when the caller should write to stderr instead. Callers must
/// never supply titles, document/account IDs, secrets or response bodies.
/// Excess updates are dropped, never printed over an active terminal view.
pub fn publish_line(line: &str) -> bool {
    publish(Update {
        line: safe_line(line),
        telemetry: None,
    })
}

/// Publishes typed, normalized state alongside its existing human line. Names
/// and labels must be fixed workflow labels or configured destination names,
/// never document/account identifiers, secrets or provider response text.
/// Returns false when the caller should write its existing line to stderr.
pub fn publish_update(telemetry: Telemetry, line: &str) -> bool {
    publish(Update {
        line: safe_line(line),
        telemetry: Some(normalize_telemetry(telemetry)),
    })
}

fn publish(update: Update) -> bool {
    let Ok(observer) = OBSERVER.lock() else {
        return false;
    };
    let Some((_, stream)) = observer.as_ref() else {
        return false;
    };
    let Ok(mut stream) = stream.lock() else {
        return false;
    };
    stream.push(update);
    true
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

#[derive(Clone, Copy, Default)]
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
        if !self.enabled {
            return;
        }
        let event = normalize_event(event);
        let final_counts = matches!(event, Event::Finished { .. }).then(|| self.counts.telemetry());
        if let Some(line) = self.render(event.clone()) {
            let telemetry = Telemetry::Retrieval {
                event,
                counts: final_counts.unwrap_or_else(|| self.counts.telemetry()),
            };
            if !publish_update(telemetry, &line) {
                let _ = writeln!(io::stderr().lock(), "{line}");
            }
        }
    }

    /// Completes the source stage only for an active frontend. Returns false
    /// without changing counts or writing stderr when reporting is disabled or
    /// no frontend is displaying progress. A successful publication resets the
    /// count sequence after preserving its final authoritative snapshot.
    pub fn finish_in_frontend(&mut self, exit_code: u8) -> bool {
        if !self.enabled {
            return false;
        }
        let event = Event::Finished { exit_code };
        let mut snapshot = Self {
            enabled: true,
            counts: self.counts,
        };
        let Some(line) = snapshot.render(event.clone()) else {
            return false;
        };
        if publish_update(
            Telemetry::Retrieval {
                event,
                counts: self.counts.telemetry(),
            },
            &line,
        ) {
            self.reset();
            true
        } else {
            false
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
                "deferred_limit" => {
                    self.counts.deferred += 1;
                    "  Deferred by the download limit.".into()
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
    fn telemetry(&self) -> RetrievalCounts {
        RetrievalCounts {
            new_pdf: self.pdf,
            reused_pdf: self.reused.saturating_sub(self.reused_other),
            other: self
                .json
                .saturating_add(self.binary)
                .saturating_add(self.reused_other),
            pending: self.pending,
            failed: self.failed,
            deferred: self.deferred,
        }
    }

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

fn normalize_telemetry(telemetry: Telemetry) -> Telemetry {
    match telemetry {
        Telemetry::Retrieval { event, counts } => Telemetry::Retrieval {
            event: normalize_event(event),
            counts,
        },
        Telemetry::DestinationStarted {
            name,
            index,
            total,
            audit,
        } => Telemetry::DestinationStarted {
            name: safe_label(&name),
            index,
            total,
            audit,
        },
        Telemetry::Diagnostic { component, status } => Telemetry::Diagnostic {
            component: safe_label(&component),
            status,
        },
        Telemetry::Delivery(event) => Telemetry::Delivery(event),
    }
}

fn normalize_event(event: Event) -> Event {
    match event {
        Event::InventoryStarted { label } => Event::InventoryStarted {
            label: safe_label(&label),
        },
        Event::DocumentComplete { status, media_kind } => Event::DocumentComplete {
            status: match status.as_str() {
                "saved_pdf"
                | "saved_json"
                | "saved_binary"
                | "already_saved"
                | "awaiting_manual_acceptance_or_status_verification"
                | "failed"
                | "deferred_due_to_authentication_failure"
                | "deferred_limit" => status,
                _ => "unrecognized".into(),
            },
            media_kind: media_kind
                .filter(|kind| matches!(kind.as_str(), "pdf" | "json" | "binary")),
        },
        other => other,
    }
}

fn safe_line(line: &str) -> String {
    line.chars()
        .filter(|character| safe_character(*character))
        .take(512)
        .collect()
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
