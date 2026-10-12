//! Terminal frontend. Rendering is separate from terminal ownership and is
//! tested with Ratatui's synthetic backend; tests never open a real terminal.
use std::{
    collections::VecDeque,
    future::Future,
    io::{self, IsTerminal, Write},
    pin::Pin,
    sync::{
        Once,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use epost_cli::{
    delivery::{DeliveryEvent, DeliveryPhase},
    error::{Error, Result},
    progress::{self, DiagnosticStatus, RetrievalCounts, Telemetry},
    secret::Secret,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap},
};
use zeroize::{Zeroize, Zeroizing};

const HISTORY_LIMIT: usize = 100;
const INPUT_LIMIT: usize = 8192;
const RESULT_SECTION_LIMIT: usize = 12;
const RESULT_LINE_LIMIT: usize = 32;
const RESULT_ISSUE_LIMIT: usize = 12;
const RESULT_DETAIL_LIMIT: usize = 100;
const RESULT_TEXT_LIMIT: usize = 512;
const ACCENT: Color = Color::Cyan;
static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// A pipe, redirected output or TERM=dumb always keeps the plain frontend.
pub(crate) fn available() -> bool {
    supported_terminal(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        std::env::var_os("TERM").as_deref(),
    )
}

fn supported_terminal(
    input: bool,
    output: bool,
    diagnostics: bool,
    term: Option<&std::ffi::OsStr>,
) -> bool {
    input && output && diagnostics && term.is_none_or(|term| term != "dumb" && !term.is_empty())
}

type RealTerminal = Terminal<CrosstermBackend<io::Stderr>>;

/// Owns all terminal changes. Both partial initialization and every return path
/// restore the screen; the panic hook restores it before diagnostics appear.
struct TerminalGuard {
    terminal: Option<RealTerminal>,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        if !available() {
            return Err(io::Error::other("interactive terminal unavailable"));
        }
        if TERMINAL_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::other("another terminal view is active"));
        }
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |information| {
                restore_terminal();
                previous(information);
            }));
        });
        let mut guard = Self { terminal: None };
        enable_raw_mode()?;
        execute!(
            io::stderr(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )?;
        guard.terminal = Some(Terminal::new(CrosstermBackend::new(io::stderr()))?);
        // A new renderer has no previous buffer to erase. Clear explicitly so
        // margins cannot retain text from the preceding progress or prompt view.
        if let Some(terminal) = &mut guard.terminal {
            terminal.clear()?;
        }
        Ok(guard)
    }

    fn draw(&mut self, draw: impl FnOnce(&mut Frame)) -> io::Result<()> {
        if let Some(terminal) = &mut self.terminal {
            terminal.draw(draw)?;
        }
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    if TERMINAL_ACTIVE.swap(false, Ordering::AcqRel) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stderr(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            Show
        );
        let _ = io::stderr().flush();
    }
}

struct StopGuard;
impl StopGuard {
    fn new() -> Self {
        progress::reset_stop();
        Self
    }
}
impl Drop for StopGuard {
    fn drop(&mut self) {
        progress::reset_stop();
    }
}

/// Shows live progress while polling the same future. A stop request restores
/// the terminal and lets the workflow finish its current request and journal;
/// neither keyboard input nor a display failure drops an upload future.
pub(crate) async fn operation<T>(
    title: &str,
    kind: OperationKind,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let _stop = StopGuard::new();
    if !available() {
        return future.await;
    }
    let Ok(mut terminal) = TerminalGuard::new() else {
        return future.await;
    };
    let Some(capture) = progress::subscribe() else {
        drop(terminal);
        return future.await;
    };
    let mut future = std::pin::pin!(future);
    let outcome = operation_loop(
        title,
        kind,
        future.as_mut(),
        Some(&capture),
        |state| terminal.draw(|frame| state.render(frame)),
        read_events,
    )
    .await;
    drop(capture);
    drop(terminal);
    match outcome {
        LiveOutcome::Completed(result) => result,
        LiveOutcome::StopRequested => {
            progress::request_stop();
            eprintln!("Stopping after the current request; preserving completed results…");
            future.await
        }
        LiveOutcome::DisplayUnavailable => future.await,
    }
}

enum LiveOutcome<T> {
    Completed(Result<T>),
    StopRequested,
    DisplayUnavailable,
}

async fn operation_loop<T>(
    title: &str,
    kind: OperationKind,
    mut future: Pin<&mut impl Future<Output = Result<T>>>,
    capture: Option<&progress::Subscription>,
    mut draw: impl FnMut(&Activity) -> io::Result<()>,
    mut events: impl FnMut() -> io::Result<Vec<Event>>,
) -> LiveOutcome<T> {
    let mut state = Activity::new(title, kind);
    if draw(&state).is_err() {
        return LiveOutcome::DisplayUnavailable;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = &mut future => return LiveOutcome::Completed(result),
            _ = tick.tick() => {
                if let Some(capture) = capture {
                    for _ in 0..512 {
                        let Some(update) = capture.try_update() else { break; };
                        state.update(update);
                    }
                }
                let Ok(events) = events() else { return LiveOutcome::DisplayUnavailable; };
                for event in events {
                    if let Event::Key(key) = event {
                        if stop_key(key) { return LiveOutcome::StopRequested; }
                        if !pressed(key) { continue; }
                        state.key(key);
                    }
                }
                state.spinner = (state.spinner + 1) % 10;
                if draw(&state).is_err() { return LiveOutcome::DisplayUnavailable; }
            }
        }
    }
}

fn read_events() -> io::Result<Vec<Event>> {
    let mut events = Vec::new();
    // A paste or repeated resize must not starve the operation future.
    while events.len() < 64 && event::poll(Duration::ZERO)? {
        events.push(event::read()?);
    }
    Ok(events)
}

fn pressed(key: KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn cancel_key(key: KeyEvent) -> bool {
    pressed(key)
        && (key.code == KeyCode::Esc
            || key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c' | 'd')))
}

fn stop_key(key: KeyEvent) -> bool {
    cancel_key(key) || pressed(key) && key.code == KeyCode::Char('q')
}

fn display(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|character| visible_character(*character))
        .take(limit)
        .collect()
}

fn visible_character(character: char) -> bool {
    !character.is_control()
        && !matches!(character,
            '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'
        )
}

fn heading(title: &str, subtitle: &str) -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(Span::styled(
            display(title, 120),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            display(subtitle, 240),
            Style::default().fg(Color::Gray),
        )),
    ])
}

/// The command supplies its actual workflow; display text is never parsed to
/// infer phases, totals or successful work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationKind {
    RetrieveAndDeliver,
    Retrieve,
    Deliver,
    Verify,
    Doctor,
    Inspect,
    Reconcile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Connect,
    Inventory,
    Retrieve,
    Deliver,
    Verify,
    Diagnose,
    Inspect,
    Reconcile,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Connect => "Connect",
            Self::Inventory => "Inventory",
            Self::Retrieve => "Retrieve",
            Self::Deliver => "Deliver",
            Self::Verify => "Verify",
            Self::Diagnose => "Checks",
            Self::Inspect => "Inspect",
            Self::Reconcile => "Reconcile",
        }
    }
}

impl OperationKind {
    fn phases(self) -> &'static [Phase] {
        match self {
            Self::RetrieveAndDeliver => &[
                Phase::Connect,
                Phase::Inventory,
                Phase::Retrieve,
                Phase::Deliver,
            ],
            Self::Retrieve => &[Phase::Connect, Phase::Inventory, Phase::Retrieve],
            Self::Deliver => &[Phase::Deliver],
            Self::Verify => &[Phase::Verify],
            Self::Doctor => &[Phase::Diagnose],
            Self::Inspect => &[Phase::Inspect],
            Self::Reconcile => &[Phase::Reconcile],
        }
    }

    fn has_source(self) -> bool {
        matches!(self, Self::RetrieveAndDeliver | Self::Retrieve)
    }

    fn has_destination(self) -> bool {
        matches!(
            self,
            Self::RetrieveAndDeliver | Self::Deliver | Self::Verify
        )
    }
}

#[derive(Default)]
struct InventoryProgress {
    label: String,
    batch: usize,
    page: u32,
    found: usize,
    total: Option<usize>,
    complete: Option<bool>,
    baseline: usize,
    processed: usize,
    current: Option<usize>,
}

struct DestinationProgress {
    name: String,
    index: usize,
    total_destinations: usize,
    audit: bool,
    event: Option<DeliveryEvent>,
}

struct Activity {
    title: String,
    kind: OperationKind,
    phase: Phase,
    history: VecDeque<String>,
    spinner: usize,
    scroll: usize,
    show_details: bool,
    source: RetrievalCounts,
    source_seen: bool,
    connection_observed: bool,
    source_exit: Option<u8>,
    inventory: InventoryProgress,
    destination: Option<DestinationProgress>,
    diagnostics: Vec<(String, DiagnosticStatus)>,
    diagnostic_overflow: bool,
}

impl Activity {
    fn new(title: &str, kind: OperationKind) -> Self {
        Self {
            title: result_text(title, 120),
            kind,
            phase: kind.phases()[0],
            history: VecDeque::new(),
            spinner: 0,
            scroll: 0,
            show_details: false,
            source: RetrievalCounts::default(),
            source_seen: false,
            connection_observed: false,
            source_exit: None,
            inventory: InventoryProgress::default(),
            destination: None,
            diagnostics: Vec::new(),
            diagnostic_overflow: false,
        }
    }

    fn line(&mut self, text: &str) {
        let text = result_text(text, 512);
        if text.is_empty() {
            return;
        }
        if self.history.len() == HISTORY_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(text);
        self.scroll = self.scroll.min(self.history.len().saturating_sub(1));
    }

    fn update(&mut self, update: progress::Update) {
        self.line(&update.line);
        let Some(telemetry) = update.telemetry else {
            return;
        };
        match telemetry {
            Telemetry::Retrieval { event, counts } => {
                // The reporter supplies a disjoint authoritative snapshot.
                // Subtract the inventory landmark rather than counting lines
                // or treating a DocumentStarted index as completed work.
                let total_processed = source_processed(&counts);
                if total_processed < source_processed(&self.source) {
                    self.inventory.baseline = 0;
                }
                self.source = counts;
                self.source_seen = true;
                match event {
                    progress::Event::Authenticating => {
                        self.phase = Phase::Connect;
                        self.connection_observed = false;
                        self.source_exit = None;
                    }
                    progress::Event::ArchiveFolders => self.phase = Phase::Inventory,
                    progress::Event::InventoryStarted { label } => {
                        self.connection_observed = true;
                        self.phase = Phase::Inventory;
                        self.source_exit = None;
                        self.inventory = InventoryProgress {
                            label: result_text(&label, 80),
                            batch: self.inventory.batch.saturating_add(1),
                            baseline: total_processed,
                            ..Default::default()
                        };
                    }
                    progress::Event::Page { page, documents } => {
                        self.connection_observed = true;
                        self.phase = Phase::Inventory;
                        self.inventory.page = page;
                        self.inventory.found = documents;
                    }
                    progress::Event::InventoryComplete {
                        documents,
                        complete,
                    } => {
                        self.connection_observed = true;
                        self.phase = Phase::Retrieve;
                        self.inventory.total = Some(documents);
                        self.inventory.found = documents;
                        self.inventory.complete = Some(complete);
                    }
                    progress::Event::DocumentStarted { index, total } => {
                        self.connection_observed = true;
                        self.phase = Phase::Retrieve;
                        self.inventory.current = Some(index);
                        self.inventory.total = Some(total);
                    }
                    progress::Event::DocumentComplete { .. } => {
                        self.phase = Phase::Retrieve;
                        self.inventory.current = None;
                    }
                    progress::Event::Finished { exit_code } => {
                        self.source_exit = Some(exit_code);
                        // The source result is not the whole workflow result.
                        // Delivery begins only when its typed update arrives.
                    }
                }
                self.inventory.processed = total_processed
                    .saturating_sub(self.inventory.baseline)
                    .min(self.inventory.total.unwrap_or(usize::MAX));
                if self.kind == OperationKind::Inspect {
                    self.phase = Phase::Inspect;
                }
            }
            Telemetry::DestinationStarted {
                name,
                index,
                total,
                audit,
            } => {
                self.phase = if self.kind == OperationKind::Verify {
                    Phase::Verify
                } else if self.kind == OperationKind::Reconcile {
                    Phase::Reconcile
                } else {
                    Phase::Deliver
                };
                // Per-destination counters must never carry across targets.
                self.destination = Some(DestinationProgress {
                    name: result_text(&name, 80),
                    index,
                    total_destinations: total,
                    audit,
                    event: None,
                });
            }
            Telemetry::Delivery(event) => {
                self.phase = if self.kind == OperationKind::Verify {
                    Phase::Verify
                } else if self.kind == OperationKind::Reconcile {
                    Phase::Reconcile
                } else {
                    Phase::Deliver
                };
                let target = self.destination.get_or_insert_with(|| DestinationProgress {
                    name: "Destination".into(),
                    index: 0,
                    total_destinations: 0,
                    audit: self.kind == OperationKind::Verify,
                    event: None,
                });
                target.event = Some(event);
            }
            Telemetry::Diagnostic { component, status } => {
                self.phase = Phase::Diagnose;
                let component = result_text(&component, 120);
                if let Some((_, previous)) = self
                    .diagnostics
                    .iter_mut()
                    .find(|(name, _)| *name == component)
                {
                    *previous = status;
                } else if self.diagnostics.len() < 128 {
                    self.diagnostics.push((component, status));
                } else {
                    self.diagnostic_overflow = true;
                }
            }
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if !pressed(key) || !key.modifiers.is_empty() {
            return;
        }
        match key.code {
            KeyCode::Char('d') => {
                self.show_details = !self.show_details;
                self.scroll = 0;
            }
            KeyCode::Up | KeyCode::PageUp if self.show_details => {
                self.scroll = self
                    .scroll
                    .saturating_add(1)
                    .min(self.history.len().saturating_sub(1));
            }
            KeyCode::Down | KeyCode::PageDown if self.show_details => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            KeyCode::End if self.show_details => self.scroll = 0,
            _ => {}
        }
    }

    fn phase_line(&self, short: bool) -> Line<'static> {
        let mut spans = Vec::new();
        for (index, phase) in self.kind.phases().iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled("  ›  ", Style::default().fg(Color::DarkGray)));
            }
            let (marker, color) = if *phase == Phase::Connect
                && !self.connection_observed
                && self.source_exit.is_some_and(|code| code != 0)
            {
                ("!", Color::Yellow)
            } else if *phase == self.phase {
                ("●", ACCENT)
            } else if self.phase_done(*phase) {
                ("✓", Color::Green)
            } else if self.source_exit.is_some_and(|code| code != 0)
                && matches!(phase, Phase::Inventory | Phase::Retrieve)
            {
                ("!", Color::Yellow)
            } else {
                ("○", Color::DarkGray)
            };
            let label = if short {
                &phase.label()[..1]
            } else {
                phase.label()
            };
            spans.push(Span::styled(
                format!("{marker} {label}"),
                Style::default().fg(color),
            ));
        }
        Line::from(spans)
    }

    fn phase_done(&self, phase: Phase) -> bool {
        match phase {
            Phase::Connect => self.kind.has_source() && self.connection_observed,
            Phase::Inventory => {
                self.source_exit == Some(0)
                    || self.phase == Phase::Retrieve && self.inventory.complete == Some(true)
            }
            Phase::Retrieve => self.source_exit == Some(0),
            _ => false,
        }
    }

    fn spinner(&self) -> char {
        ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][self.spinner % 10]
    }

    fn focus(&self) -> (String, String, String, Option<(usize, usize)>, bool) {
        if let Some(destination) = &self.destination {
            let name = if destination.name.is_empty() {
                "Destination"
            } else {
                &destination.name
            };
            let scope = if destination.total_destinations > 0 {
                format!(
                    "{name} · destination {} of {}",
                    destination.index, destination.total_destinations
                )
            } else {
                name.to_owned()
            };
            let Some(event) = destination.event else {
                return if self.kind == OperationKind::Reconcile {
                    (
                        scope,
                        "Verifying selected existing original…".into(),
                        "Waiting for the verification result".into(),
                        None,
                        false,
                    )
                } else {
                    (
                        scope,
                        "Preparing destination…".into(),
                        "Local document total is not known yet".into(),
                        None,
                        false,
                    )
                };
            };
            let action = match event.phase {
                DeliveryPhase::Started => "Preparing local documents",
                DeliveryPhase::Uploading => "Uploading",
                DeliveryPhase::Waiting => "Waiting for processing",
                DeliveryPhase::Verifying => "Verifying destination content",
                DeliveryPhase::Reconciling => "Reconciling previous upload",
                DeliveryPhase::Finished if event.processed < event.total => {
                    "Destination stopped before all items were processed"
                }
                DeliveryPhase::Finished if event.attention > 0 => {
                    "Destination finished with items requiring attention"
                }
                DeliveryPhase::Finished => "Destination items processed",
            };
            let note = if destination.audit {
                "Verification of this destination's local documents"
            } else {
                "Progress for this destination's local documents"
            };
            return (
                scope,
                action.into(),
                note.into(),
                Some((event.processed, event.total)),
                event.attention > 0
                    || event.phase == DeliveryPhase::Finished && event.processed < event.total,
            );
        }
        match self.phase {
            Phase::Connect if self.source_exit.is_some_and(|code| code != 0) => (
                "Source connection".into(),
                if self.source_exit == Some(3) {
                    "Source connection failed · existing local results preserved"
                } else {
                    "Source operation stopped before inventory · local results preserved"
                }
                .into(),
                "No completed source connection was observed".into(),
                None,
                true,
            ),
            Phase::Inspect if self.inventory.batch > 0 => {
                let label = if self.inventory.label.is_empty() {
                    "Selected metadata"
                } else {
                    &self.inventory.label
                };
                let message = if self.inventory.total.is_some() {
                    format!("{} documents observed", self.inventory.found)
                } else if self.inventory.page > 0 {
                    format!(
                        "Reading page {} · {} documents observed",
                        self.inventory.page, self.inventory.found
                    )
                } else {
                    "Reading selected metadata…".into()
                };
                (
                    label.into(),
                    message,
                    "Metadata listing · mailbox total is not provided".into(),
                    None,
                    self.inventory.complete == Some(false),
                )
            }
            Phase::Inventory => {
                let label = if self.inventory.label.is_empty() {
                    "Selected source"
                } else {
                    &self.inventory.label
                };
                let scope = if self.inventory.batch > 0 {
                    format!("{label} · inventory {}", self.inventory.batch)
                } else {
                    label.into()
                };
                let message = if self.inventory.page > 0 {
                    format!(
                        "Scanning page {} · {} documents found so far",
                        self.inventory.page, self.inventory.found
                    )
                } else {
                    "Scanning documents…".into()
                };
                (
                    scope,
                    message,
                    "Total unknown until this inventory finishes".into(),
                    None,
                    false,
                )
            }
            Phase::Retrieve => {
                let label = if self.inventory.label.is_empty() {
                    "Selected source"
                } else {
                    &self.inventory.label
                };
                let scope = format!("{label} · inventory {}", self.inventory.batch.max(1));
                let message = match (self.source_exit, self.inventory.current) {
                    (Some(0), _) if self.kind == OperationKind::RetrieveAndDeliver => {
                        "Source retrieval finished · preparing delivery".into()
                    }
                    (Some(0), _) => "Source retrieval finished".into(),
                    (Some(_), _) => {
                        "Source retrieval needs attention · completed results preserved".into()
                    }
                    (None, Some(index)) => format!("Checking item {index}"),
                    _ => "Retrieving observed documents…".into(),
                };
                let note = if self.inventory.complete == Some(false) {
                    "Incomplete inventory · progress covers only observed documents"
                } else {
                    "Progress for this inventory · mailbox total is not provided"
                };
                (
                    scope,
                    message,
                    note.into(),
                    self.inventory
                        .total
                        .map(|total| (self.inventory.processed, total)),
                    self.inventory.complete == Some(false)
                        || self.source_exit.is_some_and(|code| code != 0),
                )
            }
            Phase::Diagnose => {
                let message = self
                    .diagnostics
                    .last()
                    .map(|(component, _)| format!("Checking {component}"))
                    .unwrap_or_else(|| "Checking local configuration and access…".into());
                (
                    "Doctor".into(),
                    message,
                    "Checks report observed results; total checks is not provided".into(),
                    None,
                    self.diagnostic_overflow
                        || self.diagnostics.iter().any(|(_, status)| {
                            matches!(status, DiagnosticStatus::Warning | DiagnosticStatus::Error)
                        }),
                )
            }
            phase => (
                phase.label().into(),
                match phase {
                    Phase::Connect => "Connecting to the selected source…",
                    Phase::Deliver => "Preparing delivery…",
                    Phase::Verify => "Preparing destination verification…",
                    Phase::Inspect => "Inspecting selected local state…",
                    Phase::Reconcile => "Verifying the selected existing original…",
                    _ => "Starting…",
                }
                .into(),
                "Waiting for observed progress".into(),
                None,
                false,
            ),
        }
    }

    fn source_lines(&self) -> Vec<Line<'static>> {
        if !self.source_seen {
            return vec![Line::styled(
                "Waiting for retrieval results",
                Style::default().fg(Color::DarkGray),
            )];
        }
        let counts = &self.source;
        vec![
            metric_pair(
                "New PDF",
                counts.new_pdf,
                "Reused PDF",
                counts.reused_pdf,
                Color::Green,
            ),
            metric_pair(
                "Other formats",
                counts.other,
                "Pending",
                counts.pending,
                Color::Yellow,
            ),
            metric_pair(
                "Failed",
                counts.failed,
                "Deferred",
                counts.deferred,
                Color::Yellow,
            ),
        ]
    }

    fn destination_lines(&self) -> Vec<Line<'static>> {
        let Some(destination) = &self.destination else {
            return vec![Line::styled(
                "Waiting for destination",
                Style::default().fg(Color::DarkGray),
            )];
        };
        let Some(event) = destination.event else {
            return vec![
                Line::from(destination.name.clone()),
                Line::styled(
                    "Waiting for local inventory",
                    Style::default().fg(Color::DarkGray),
                ),
            ];
        };
        vec![
            Line::from(vec![
                Span::styled(
                    format!("{}", event.delivered),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" confirmed"),
            ]),
            Line::from(vec![
                Span::styled(
                    format!("{}", event.attention),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" need attention"),
            ]),
            Line::styled(
                format!("{} / {} processed", event.processed, event.total),
                Style::default().fg(Color::Gray),
            ),
        ]
    }

    fn render_focus(&self, frame: &mut Frame, area: Rect, bordered: bool) {
        let (scope, message, note, gauge, attention) = self.focus();
        let color = if attention { Color::Yellow } else { ACCENT };
        let block = if bordered {
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {scope} "))
                .border_style(Style::default().fg(color))
        } else {
            Block::default()
        };
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{} ", self.spinner()), Style::default().fg(color)),
                Span::styled(
                    message,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            rows[0],
        );
        if let Some((processed, total)) = gauge {
            // This is strictly the current observed inventory/destination, not
            // a mailbox percentage. A result never synthesizes completion.
            if total == 0 {
                frame.render_widget(
                    Paragraph::new("0 / 0 processed · no documents in this inventory")
                        .style(Style::default().fg(Color::Gray)),
                    rows[1],
                );
            } else {
                frame.render_widget(
                    Gauge::default()
                        .ratio((processed.min(total) as f64 / total as f64).clamp(0.0, 1.0))
                        .label(format!("{processed} / {total} processed"))
                        .gauge_style(Style::default().fg(color).bg(Color::DarkGray)),
                    rows[1],
                );
            }
        } else {
            frame.render_widget(
                Paragraph::new("Working · total unknown").style(Style::default().fg(Color::Gray)),
                rows[1],
            );
        }
        frame.render_widget(
            Paragraph::new(note).style(Style::default().fg(if attention {
                Color::Yellow
            } else {
                Color::DarkGray
            })),
            rows[2],
        );
    }

    fn render_counters(&self, frame: &mut Frame, area: Rect) {
        let areas = if self.kind.has_source() && self.kind.has_destination() {
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area)
        } else {
            Layout::horizontal([Constraint::Percentage(100)]).split(area)
        };
        let panel = |title: &'static str, lines: Vec<Line<'static>>| {
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
        };
        if self.kind.has_source() {
            frame.render_widget(panel(" Source results ", self.source_lines()), areas[0]);
        }
        if self.kind.has_destination() {
            frame.render_widget(
                panel(" Destination results ", self.destination_lines()),
                areas[areas.len() - 1],
            );
        }
        if self.kind == OperationKind::Doctor {
            frame.render_widget(
                panel(" Diagnostic results ", self.diagnostic_lines()),
                areas[0],
            );
        }
    }

    fn diagnostic_lines(&self) -> Vec<Line<'static>> {
        let count = |expected| {
            self.diagnostics
                .iter()
                .filter(|(_, status)| *status == expected)
                .count()
        };
        vec![
            metric_pair(
                "OK",
                count(DiagnosticStatus::Ok),
                "Warning",
                count(DiagnosticStatus::Warning),
                Color::Green,
            ),
            metric_pair(
                "Error",
                count(DiagnosticStatus::Error),
                "Not checked",
                count(DiagnosticStatus::NotChecked),
                Color::Yellow,
            ),
            Line::styled(
                if self.diagnostic_overflow {
                    "Counts limited to 128 observed checks · see the Doctor report"
                } else {
                    "Observed checks · no completion percentage"
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]
    }

    fn render_history(&self, frame: &mut Frame, area: Rect, bordered: bool) {
        let block = if bordered {
            Block::default()
                .borders(Borders::ALL)
                .title(if self.show_details {
                    " Details · recent 100 events "
                } else {
                    " Recent activity "
                })
                .border_style(Style::default().fg(Color::DarkGray))
        } else {
            Block::default()
        };
        let inner = block.inner(area);
        let visible =
            usize::from(inner.height).min(if self.show_details { HISTORY_LIMIT } else { 3 });
        let end = self.history.len().saturating_sub(if self.show_details {
            self.scroll.min(self.history.len())
        } else {
            0
        });
        let start = end.saturating_sub(visible);
        let mut lines: Vec<_> = self
            .history
            .iter()
            .skip(start)
            .take(end - start)
            .map(|line| Line::styled(line.as_str(), Style::default().fg(Color::Gray)))
            .collect();
        if lines.is_empty() {
            lines.push(Line::styled(
                "Waiting for activity…",
                Style::default().fg(Color::DarkGray),
            ));
        }
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        if area.is_empty() {
            return;
        }
        let compact = area.width < 56 || area.height < 18;
        let body = if compact {
            area
        } else {
            area.inner(ratatui::layout::Margin::new(1, 1))
        };
        if compact {
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(body);
            frame.render_widget(
                Paragraph::new(Line::styled(
                    self.title.as_str(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
                rows[0],
            );
            frame.render_widget(Paragraph::new(self.phase_line(area.width < 40)), rows[1]);
            self.render_focus(frame, rows[2], false);
            if self.show_details {
                self.render_history(frame, rows[3], false);
            } else {
                let mut lines = Vec::new();
                if self.kind.has_source() {
                    lines.push(Line::styled("Source results", Style::default().fg(ACCENT)));
                    lines.extend(self.source_lines());
                }
                if self.kind.has_destination() {
                    lines.push(Line::styled(
                        "Destination results",
                        Style::default().fg(ACCENT),
                    ));
                    lines.extend(self.destination_lines());
                }
                if self.kind == OperationKind::Doctor {
                    lines.push(Line::styled(
                        "Diagnostic results",
                        Style::default().fg(ACCENT),
                    ));
                    lines.extend(self.diagnostic_lines());
                }
                if !self.history.is_empty() {
                    lines.push(Line::styled("Recent activity", Style::default().fg(ACCENT)));
                    lines.extend(
                        self.history
                            .iter()
                            .skip(self.history.len().saturating_sub(3))
                            .map(|line| {
                                Line::styled(line.as_str(), Style::default().fg(Color::Gray))
                            }),
                    );
                }
                frame.render_widget(Paragraph::new(lines), rows[3]);
            }
            frame.render_widget(
                Paragraph::new("d details · q/Esc stop")
                    .style(Style::default().fg(Color::DarkGray)),
                rows[4],
            );
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(body);
        frame.render_widget(heading("ePost", &self.title), rows[0]);
        frame.render_widget(Paragraph::new(self.phase_line(false)), rows[1]);
        if self.show_details {
            self.render_history(frame, rows[2], true);
        } else {
            let has_counters = self.kind.has_source()
                || self.kind.has_destination()
                || self.kind == OperationKind::Doctor;
            let panels = Layout::vertical([
                Constraint::Length(5),
                Constraint::Length(if has_counters { 5 } else { 0 }),
                Constraint::Min(0),
            ])
            .split(rows[2]);
            self.render_focus(frame, panels[0], true);
            if has_counters {
                self.render_counters(frame, panels[1]);
            }
            self.render_history(frame, panels[2], true);
        }
        let footer = vec![
            Line::from(vec![
                Span::styled(
                    if self.show_details {
                        " d  Dashboard "
                    } else {
                        " d  Details "
                    },
                    Style::default().fg(Color::Black).bg(ACCENT),
                ),
                Span::raw("  q/Esc/Ctrl-C Stop after current request"),
            ]),
            Line::styled(
                if self.show_details {
                    "↑/↓ scroll · End latest · history is bounded to 100 events"
                } else {
                    "Counts reflect observed work; each gauge covers its current inventory or destination"
                },
                Style::default().fg(Color::DarkGray),
            ),
        ];
        frame.render_widget(Paragraph::new(footer), rows[3]);
    }
}

fn source_processed(counts: &RetrievalCounts) -> usize {
    [
        counts.new_pdf,
        counts.reused_pdf,
        counts.other,
        counts.pending,
        counts.failed,
        counts.deferred,
    ]
    .into_iter()
    .fold(0usize, usize::saturating_add)
}

fn metric_pair(
    first: &str,
    first_count: usize,
    second: &str,
    second_count: usize,
    color: Color,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{first_count}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {first}    ")),
        Span::styled(
            format!("{second_count}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {second}")),
    ])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    Setup,
    CheckInbox,
    BrowseInbox,
    BrowseArchive,
    Deliver,
    Problems,
    Doctor,
    Destinations,
    Settings,
    Quit,
}

struct Home {
    profile: String,
    destinations: String,
    actions: Vec<(MenuAction, &'static str, &'static str)>,
    selected: usize,
}

impl Home {
    fn new(profile: Option<&str>, destinations: &[String], setup_needed: bool) -> Self {
        let mut actions = Vec::new();
        if setup_needed {
            actions.push((
                MenuAction::Setup,
                "Set up a profile",
                "Sign in, explicitly choose a mailbox, and save your preferences",
            ));
        } else {
            actions.extend([
                (
                    MenuAction::CheckInbox,
                    "Check inbox",
                    "Retrieve new documents using the active profile",
                ),
                (
                    MenuAction::BrowseInbox,
                    "Browse inbox",
                    "Read the inbox listing and choose documents",
                ),
                (
                    MenuAction::BrowseArchive,
                    "Browse archive",
                    "Browse the archive root and folders",
                ),
                (
                    MenuAction::Deliver,
                    "Deliver saved PDFs",
                    "Send verified local PDFs to a configured destination",
                ),
                (
                    MenuAction::Problems,
                    "Items needing attention",
                    "Inspect saved status and delivery issues",
                ),
            ]);
        }
        actions.extend([
            (
                MenuAction::Doctor,
                "Check configuration",
                "Inspect local configuration and prerequisites",
            ),
            (
                MenuAction::Destinations,
                "Destinations",
                "Manage document delivery destinations",
            ),
            (
                MenuAction::Settings,
                "Profiles and settings",
                "Review profiles or start profile setup",
            ),
            (MenuAction::Quit, "Quit", "Return to the terminal"),
        ]);
        let destinations = if destinations.is_empty() {
            "Local documents".into()
        } else {
            destinations
                .iter()
                .take(6)
                .map(|name| display(name, 60))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Self {
            profile: profile
                .map(|value| display(value, 120))
                .unwrap_or_else(|| "No profile selected".into()),
            destinations,
            actions,
            selected: 0,
        }
    }

    fn event(&mut self, event: Event) -> Option<MenuAction> {
        let Event::Key(key) = event else {
            return None;
        };
        if !pressed(key) {
            return None;
        }
        if stop_key(key) {
            return Some(MenuAction::Quit);
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % self.actions.len()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = (self.selected + self.actions.len() - 1) % self.actions.len()
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.actions.len() - 1,
            KeyCode::Enter => return Some(self.actions[self.selected].0),
            _ => {}
        }
        None
    }

    fn render(&self, frame: &mut Frame) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
        frame.render_widget(
            heading(
                "ePost",
                "Inbox and archive retrieval · ePost operations are read-only",
            ),
            areas[0],
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("Profile: {}", self.profile)),
                Line::from(format!("Destinations: {}", self.destinations)),
            ])
            .block(Block::default().borders(Borders::BOTTOM)),
            areas[1],
        );
        let items: Vec<_> = self
            .actions
            .iter()
            .map(|(_, label, _)| ListItem::new(*label))
            .collect();
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Choose an action "),
            );
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, areas[2], &mut state);
        frame.render_widget(
            Paragraph::new(self.actions[self.selected].2)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(Color::Gray)),
            areas[3],
        );
        frame.render_widget(
            Paragraph::new("↑/↓ or j/k select · Enter open · q/Esc quit")
                .style(Style::default().fg(Color::DarkGray)),
            areas[4],
        );
    }
}

pub(crate) fn home(
    profile: Option<&str>,
    destinations: &[String],
    setup_needed: bool,
) -> Result<MenuAction> {
    let mut terminal = TerminalGuard::new()?;
    let mut state = Home::new(profile, destinations, setup_needed);
    terminal.draw(|frame| state.render(frame))?;
    loop {
        if event::poll(Duration::from_millis(100))? {
            let event = event::read()?;
            if let Some(action) = state.event(event) {
                return Ok(action);
            }
            terminal.draw(|frame| state.render(frame))?;
        }
    }
}

/// The caller supplies already-redacted summaries, never credentials, provider
/// response bodies or document contents. Display sanitization is not redaction.
#[derive(Clone, Debug)]
pub(crate) struct ResultView {
    pub(crate) title: String,
    pub(crate) status: ResultStatus,
    pub(crate) sections: Vec<ResultSection>,
    pub(crate) issues: Vec<ResultIssue>,
    pub(crate) details: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultStatus {
    Success,
    Attention,
    Error,
}

impl ResultStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Success => "Completed",
            Self::Attention => "Needs attention",
            Self::Error => "Unable to complete",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Success => Color::Green,
            Self::Attention => Color::Yellow,
            Self::Error => Color::Red,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResultSection {
    pub(crate) title: String,
    pub(crate) lines: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResultIssue {
    pub(crate) heading: String,
    pub(crate) explanation: String,
    pub(crate) next_step: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResultAction {
    Back,
    Retry,
    Doctor,
    Quit,
}

struct ResultScreen {
    view: ResultView,
    sections_omitted: usize,
    issues_omitted: usize,
    details_omitted: usize,
    show_details: bool,
    scroll: usize,
    max_scroll: usize,
    page_height: usize,
}

impl ResultScreen {
    fn new(view: &ResultView) -> Self {
        Self {
            view: ResultView {
                title: result_text(&view.title, 120),
                status: view.status,
                sections: view
                    .sections
                    .iter()
                    .take(RESULT_SECTION_LIMIT)
                    .map(|section| {
                        let mut lines: Vec<_> = section
                            .lines
                            .iter()
                            .take(RESULT_LINE_LIMIT)
                            .map(|line| result_text(line, RESULT_TEXT_LIMIT))
                            .collect();
                        if section.lines.len() > RESULT_LINE_LIMIT {
                            lines.push("Additional rows omitted from this view.".into());
                        }
                        ResultSection {
                            title: result_text(&section.title, 120),
                            lines,
                        }
                    })
                    .collect(),
                issues: view
                    .issues
                    .iter()
                    .take(RESULT_ISSUE_LIMIT)
                    .map(|issue| ResultIssue {
                        heading: result_text(&issue.heading, 120),
                        explanation: result_text(&issue.explanation, RESULT_TEXT_LIMIT),
                        next_step: result_text(&issue.next_step, RESULT_TEXT_LIMIT),
                    })
                    .collect(),
                details: view
                    .details
                    .iter()
                    .take(RESULT_DETAIL_LIMIT)
                    .map(|line| result_text(line, RESULT_TEXT_LIMIT))
                    .collect(),
            },
            sections_omitted: view.sections.len().saturating_sub(RESULT_SECTION_LIMIT),
            issues_omitted: view.issues.len().saturating_sub(RESULT_ISSUE_LIMIT),
            details_omitted: view.details.len().saturating_sub(RESULT_DETAIL_LIMIT),
            show_details: false,
            scroll: 0,
            max_scroll: 0,
            page_height: 1,
        }
    }

    fn event(&mut self, event: Event) -> Option<ResultAction> {
        let Event::Key(key) = event else {
            return None;
        };
        if !pressed(key) {
            return None;
        }
        if cancel_key(key) {
            return Some(ResultAction::Quit);
        }
        // Only plain keys invoke actions. In particular, a modified key or
        // pasted text must not accidentally trigger a retry.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Enter => return Some(ResultAction::Back),
            KeyCode::Char('r' | 'R') => return Some(ResultAction::Retry),
            KeyCode::Char('g' | 'G') => return Some(ResultAction::Doctor),
            KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Some(ResultAction::Quit),
            KeyCode::Char('d' | 'D') => {
                self.show_details = !self.show_details;
                self.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1).min(self.max_scroll)
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_add(self.page_height)
                    .min(self.max_scroll)
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.page_height),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = self.max_scroll,
            _ => {}
        }
        None
    }

    fn lines(&self, width: u16, cards: bool) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let mut lines = Vec::new();
        let normal = Style::default().fg(Color::Gray);
        if self.show_details {
            let mut details: Vec<_> = self
                .view
                .details
                .iter()
                .map(|text| (text.as_str(), normal))
                .collect();
            if details.is_empty() {
                details.push(("No technical details were provided.", normal));
            }
            if self.details_omitted > 0 {
                details.push((
                    "Additional technical details omitted from this view.",
                    normal,
                ));
            }
            result_card(
                &mut lines,
                "Technical details",
                &details,
                width,
                cards,
                ACCENT,
            );
            return lines;
        }
        for section in &self.view.sections {
            let contents: Vec<_> = section
                .lines
                .iter()
                .map(|text| (text.as_str(), Style::default().fg(Color::White)))
                .collect();
            result_card(&mut lines, &section.title, &contents, width, cards, ACCENT);
        }
        if self.sections_omitted > 0 {
            result_card(
                &mut lines,
                "Additional results",
                &[("More result sections were omitted from this view.", normal)],
                width,
                cards,
                ACCENT,
            );
        }
        if !self.view.issues.is_empty() {
            lines.push(Line::styled(
                "NEXT STEPS",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            for issue in &self.view.issues {
                let next = if issue.next_step.is_empty() {
                    "No suggested next step was provided.".to_owned()
                } else {
                    format!("Next: {}", issue.next_step)
                };
                result_card(
                    &mut lines,
                    &issue.heading,
                    &[
                        (issue.explanation.as_str(), normal),
                        (&next, Style::default().fg(Color::White)),
                    ],
                    width,
                    cards,
                    Color::Yellow,
                );
            }
        }
        if self.issues_omitted > 0 {
            result_card(
                &mut lines,
                "Additional issues",
                &[("More issues were omitted from this view.", normal)],
                width,
                cards,
                Color::Yellow,
            );
        }
        if lines.is_empty() {
            result_card(
                &mut lines,
                "Results",
                &[("No additional result details were provided.", normal)],
                width,
                cards,
                ACCENT,
            );
        }
        lines
    }

    fn render(&mut self, frame: &mut Frame) {
        let compact = frame.area().width < 40 || frame.area().height < 12;
        let areas = if compact {
            Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(frame.area())
        } else {
            Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .margin(1)
            .split(frame.area())
        };
        let status = self.view.status;
        if compact {
            frame.render_widget(
                Paragraph::new(status.label()).style(
                    Style::default()
                        .fg(status.color())
                        .add_modifier(Modifier::BOLD),
                ),
                areas[0],
            );
            frame.render_widget(Paragraph::new(self.view.title.as_str()), areas[1]);
        } else {
            frame.render_widget(heading("ePost", &self.view.title), areas[0]);
            let message = match status {
                ResultStatus::Success => "Review the results below.",
                ResultStatus::Attention | ResultStatus::Error => "Review the next steps below.",
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        status.label(),
                        Style::default()
                            .fg(status.color())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" · {message}")),
                ]))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Outcome ")
                        .border_style(Style::default().fg(status.color())),
                ),
                areas[1],
            );
        }
        let lines = self.lines(areas[2].width, !compact);
        let total = lines.len();
        self.page_height = usize::from(areas[2].height).max(1);
        self.max_scroll = total.saturating_sub(self.page_height);
        self.scroll = self.scroll.min(self.max_scroll);
        // Skip owned lines rather than converting a potentially large scroll
        // offset to u16. The content and all vector dimensions are bounded.
        frame.render_widget(
            Paragraph::new(lines.into_iter().skip(self.scroll).collect::<Vec<_>>()),
            areas[2],
        );
        let detail_hint = if self.show_details {
            "d summary"
        } else {
            "d details"
        };
        let footer = if compact {
            vec![Line::from(
                "Enter back · r retry · g Doctor · q quit · d details",
            )]
        } else {
            vec![
                Line::from(vec![
                    Span::styled(
                        " Enter  Back to menu ",
                        Style::default().fg(Color::Black).bg(ACCENT),
                    ),
                    Span::raw("  r Retry   g Doctor   q/Esc Quit"),
                ]),
                Line::styled(
                    format!(
                        "↑/↓ scroll · PgUp/PgDn page · {detail_hint}     {}–{} of {total}",
                        self.scroll.saturating_add(1).min(total),
                        self.scroll.saturating_add(self.page_height).min(total),
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        };
        frame.render_widget(Paragraph::new(footer), areas[3]);
    }
}

/// Bounds both the displayed text and the work spent on an untrusted string.
/// Whitespace becomes a space; terminal controls and bidi controls are removed.
fn result_text(text: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let mut text = text.chars();
    let mut output = String::new();
    let mut length = 0;
    for character in text.by_ref().take(limit.saturating_mul(8)) {
        let character = match character {
            '\n' | '\r' | '\t' => ' ',
            character if visible_character(character) => character,
            _ => continue,
        };
        output.push(character);
        length += 1;
        if length == limit {
            break;
        }
    }
    if text.next().is_some() {
        if length == limit {
            output.pop();
        }
        output.push('…');
    }
    output
}

fn result_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_width = 0;
    for word in text.split_whitespace() {
        let word_width = Line::from(word).width();
        let separator = usize::from(!line.is_empty());
        if line_width + separator + word_width <= width {
            if separator != 0 {
                line.push(' ');
            }
            line.push_str(word);
            line_width += separator + word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        let span = Span::raw(word);
        for grapheme in span.styled_graphemes(Style::default()) {
            let grapheme_width = Line::from(grapheme.symbol).width();
            if line_width + grapheme_width > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                line_width = 0;
            }
            if grapheme_width > width {
                line.push('�');
                line_width += 1;
            } else {
                line.push_str(grapheme.symbol);
                line_width += grapheme_width;
            }
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn result_card(
    output: &mut Vec<Line<'static>>,
    title: &str,
    contents: &[(&str, Style)],
    width: usize,
    bordered: bool,
    accent: Color,
) {
    let content_width = if bordered {
        width.saturating_sub(4)
    } else {
        width
    };
    let title = result_wrap(title, content_width)
        .into_iter()
        .next()
        .unwrap_or_default();
    let title_style = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let border_style = Style::default().fg(Color::DarkGray);
    if bordered {
        let fill = width.saturating_sub(Line::from(title.as_str()).width() + 4);
        output.push(Line::from(vec![
            Span::styled("┌ ", border_style),
            Span::styled(title, title_style),
            Span::styled(format!(" {}┐", "─".repeat(fill)), border_style),
        ]));
    } else {
        output.push(Line::styled(title, title_style));
    }
    for (text, style) in contents {
        for line in result_wrap(text, content_width) {
            if bordered {
                let fill = content_width.saturating_sub(Line::from(line.as_str()).width());
                output.push(Line::from(vec![
                    Span::styled("│ ", border_style),
                    Span::styled(line, *style),
                    Span::styled(format!("{} │", " ".repeat(fill)), border_style),
                ]));
            } else {
                output.push(Line::styled(line, *style));
            }
        }
    }
    if bordered {
        output.push(Line::styled(
            format!("└{}┘", "─".repeat(width.saturating_sub(2))),
            border_style,
        ));
    }
    output.push(Line::default());
}

fn result_loop(
    view: &ResultView,
    mut draw: impl FnMut(&mut ResultScreen) -> io::Result<()>,
    mut events: impl FnMut() -> io::Result<Event>,
) -> io::Result<ResultAction> {
    let mut state = ResultScreen::new(view);
    draw(&mut state)?;
    loop {
        if let Some(action) = state.event(events()?) {
            return Ok(action);
        }
        draw(&mut state)?;
    }
}

/// Shows a completed operation. It only returns a chosen action; retry and
/// Doctor workflows remain the caller's responsibility after the guard drops.
pub(crate) fn result(view: &ResultView) -> Result<ResultAction> {
    let mut terminal = TerminalGuard::new()?;
    Ok(result_loop(
        view,
        |state| terminal.draw(|frame| state.render(frame)),
        event::read,
    )?)
}

/// Existing onboarding and browse flows keep their policy/validation logic.
/// Only prompt display and input change. Answers never enter the history.
#[derive(Default)]
pub(crate) struct TuiPrompts {
    history: VecDeque<String>,
}

impl TuiPrompts {
    pub(crate) fn new() -> Result<Self> {
        if !available() {
            return Err(Error::Config("open a terminal for guided setup and browse"));
        }
        Ok(Self::default())
    }

    /// Secret mode accepts a literal `q`; only Esc/Ctrl-C/Ctrl-D cancel.
    pub(crate) fn prompt(&mut self, prompt: &str, secret: bool) -> Result<Option<String>> {
        let mut terminal = TerminalGuard::new()?;
        let mut input = InputBuffer::default();
        loop {
            terminal.draw(|frame| self.render_input(frame, prompt, &input, secret))?;
            let mut event = event::read()?;
            match input.event(&mut event, secret) {
                InputAction::Continue => {}
                InputAction::Cancelled => return Ok(None),
                InputAction::Accepted(answer) => return Ok(Some(answer)),
            }
        }
    }

    fn render_input(&self, frame: &mut Frame, prompt: &str, input: &InputBuffer, secret: bool) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
        frame.render_widget(
            heading(
                "ePost",
                if secret {
                    "Credential entry · hidden input"
                } else {
                    "Guided setup and browsing"
                },
            ),
            areas[0],
        );
        let visible = usize::from(areas[1].height.saturating_sub(2));
        let lines: Vec<Line<'_>> = self
            .history
            .iter()
            .skip(self.history.len().saturating_sub(visible))
            .map(|line| Line::from(line.as_str()))
            .collect();
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Details ")),
            areas[1],
        );
        let width = usize::from(areas[2].width.saturating_sub(5));
        let value = if secret {
            "•".repeat(input.text.chars().count().min(width))
        } else {
            let tail: String = input.text.chars().rev().take(width / 2).collect();
            tail.chars().rev().collect()
        };
        frame.render_widget(
            Paragraph::new(format!("{value}▏")).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", display(prompt, 160)))
                    .border_style(Style::default().fg(ACCENT)),
            ),
            areas[2],
        );
        let hint = if input.limited {
            "Input limit reached (8192 bytes) · Backspace to edit"
        } else if secret {
            "Enter accept · Esc/Ctrl-C/Ctrl-D cancel · password stays hidden"
        } else {
            "Enter accept · Esc/Ctrl-C/Ctrl-D or q cancel · Ctrl-U clear"
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(if input.limited {
                Color::Yellow
            } else {
                Color::DarkGray
            })),
            areas[3],
        );
    }

    fn add_line(&mut self, text: &str) {
        for line in text.lines() {
            if self.history.len() == HISTORY_LIMIT {
                self.history.pop_front();
            }
            self.history.push_back(display(line, 512));
        }
    }
}

struct InputBuffer {
    text: Zeroizing<String>,
    limited: bool,
}

impl Default for InputBuffer {
    fn default() -> Self {
        // Reserve once so typing cannot leave previous secret allocations
        // behind during a String reallocation. Zeroizing clears spare capacity.
        Self {
            text: Zeroizing::new(String::with_capacity(INPUT_LIMIT)),
            limited: false,
        }
    }
}

enum InputAction {
    Continue,
    Cancelled,
    Accepted(String),
}

impl InputBuffer {
    fn event(&mut self, event: &mut Event, secret: bool) -> InputAction {
        match event {
            Event::Key(key) if pressed(*key) => {
                if cancel_key(*key) {
                    self.text.zeroize();
                    return InputAction::Cancelled;
                }
                match key.code {
                    KeyCode::Enter => {
                        if self.limited {
                            return InputAction::Continue;
                        }
                        if !secret && self.text.trim().eq_ignore_ascii_case("q") {
                            return InputAction::Cancelled;
                        }
                        let answer = if secret {
                            std::mem::take(&mut *self.text)
                        } else {
                            self.text.trim().to_owned()
                        };
                        return InputAction::Accepted(answer);
                    }
                    KeyCode::Backspace => {
                        self.text.pop();
                        self.limited = false;
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.text.zeroize();
                        self.limited = false;
                    }
                    KeyCode::Char(character)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.push(character)
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                for character in text.chars() {
                    self.push(character);
                }
                text.zeroize();
            }
            _ => {}
        }
        InputAction::Continue
    }

    fn push(&mut self, character: char) {
        if !visible_character(character) {
            return;
        }
        if self.text.len() + character.len_utf8() <= INPUT_LIMIT {
            self.text.push(character);
        } else {
            self.limited = true;
        }
    }
}

impl super::interactive::Prompts for TuiPrompts {
    fn input(&mut self, prompt: &str) -> Result<Option<String>> {
        self.prompt(prompt, false)
    }

    fn credentials(&mut self, cli: &super::Cli) -> Result<(String, Secret)> {
        super::password_with_prompt(cli, |secret| {
            self.prompt(
                if secret {
                    "ePost/KLARA password (not SwissID):"
                } else {
                    "ePost account email:"
                },
                secret,
            )
        })
    }

    fn line(&mut self, text: &str) -> Result<()> {
        self.add_line(text);
        // Restore before the lasting text so finishing setup/browse does not
        // silently discard its final confirmation on the alternate screen.
        for line in text.lines() {
            writeln!(io::stderr().lock(), "{}", display(line, 512))?;
        }
        Ok(())
    }

    fn supports_keychain(&self) -> bool {
        epost_cli::keychain::supported()
    }

    fn persist_profile(
        &mut self,
        cli: &super::Cli,
        name: &str,
        profile: &epost_cli::config::Profile,
        previous: Option<&epost_cli::config::Profile>,
        secret: Option<&Secret>,
    ) -> Result<()> {
        super::persist_profile(cli, name, profile, previous, secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn screen(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(draw).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn result_fixture(status: ResultStatus) -> ResultView {
        ResultView {
            title: "Synthetic inbox check".into(),
            status,
            sections: vec![ResultSection {
                title: "Retrieval".into(),
                lines: vec![
                    "Documents found: 7".into(),
                    "New PDFs saved: 4".into(),
                    "Already saved: 2".into(),
                ],
            }],
            issues: vec![ResultIssue {
                heading: "One item requires acceptance".into(),
                explanation: "This item could not be retrieved before acceptance.".into(),
                next_step: "Review the item in ePost, then retry when ready.".into(),
            }],
            details: vec!["Synthetic redacted diagnostic code: acceptance_required".into()],
        }
    }

    fn result_key(state: &mut ResultScreen, code: KeyCode) -> Option<ResultAction> {
        state.event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    #[test]
    fn result_cards_show_counts_issues_next_steps_and_explicit_actions() {
        let mut state = ResultScreen::new(&result_fixture(ResultStatus::Attention));
        let rendered = screen(90, 30, |frame| state.render(frame));
        for text in [
            "Synthetic inbox check",
            "Needs attention",
            "Documents found: 7",
            "New PDFs saved: 4",
            "NEXT STEPS",
            "One item requires acceptance",
            "Review the item in ePost",
            "Back to menu",
            "r Retry",
            "g Doctor",
            "q/Esc Quit",
        ] {
            assert!(rendered.contains(text), "missing result text: {text}");
        }
        assert!(rendered.contains('┌'));
        assert!(!rendered.contains("acceptance_required"));
        assert!(!rendered.contains("Healthy"));
    }

    #[test]
    fn result_outcomes_are_explicit_and_never_invent_coverage() {
        for (status, label) in [
            (ResultStatus::Success, "Completed"),
            (ResultStatus::Attention, "Needs attention"),
            (ResultStatus::Error, "Unable to complete"),
        ] {
            let mut view = result_fixture(status);
            view.issues.clear();
            let mut state = ResultScreen::new(&view);
            let rendered = screen(80, 24, |frame| state.render(frame));
            assert!(rendered.contains(label));
            assert!(!rendered.contains("complete mailbox"));
            assert!(!rendered.contains("Healthy"));
        }
    }

    #[test]
    fn result_actions_require_plain_pressed_keys_and_never_auto_retry() {
        for status in [
            ResultStatus::Success,
            ResultStatus::Attention,
            ResultStatus::Error,
        ] {
            for (key, action) in [
                (KeyCode::Enter, ResultAction::Back),
                (KeyCode::Char('r'), ResultAction::Retry),
                (KeyCode::Char('g'), ResultAction::Doctor),
                (KeyCode::Char('q'), ResultAction::Quit),
                (KeyCode::Esc, ResultAction::Quit),
            ] {
                let mut state = ResultScreen::new(&result_fixture(status));
                assert_eq!(state.event(Event::Resize(80, 24)), None);
                assert_eq!(state.event(Event::Paste("r".into())), None);
                assert_eq!(result_key(&mut state, KeyCode::Char('x')), None);
                assert_eq!(result_key(&mut state, key), Some(action));
            }
        }
        let mut state = ResultScreen::new(&result_fixture(ResultStatus::Attention));
        for modifiers in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            assert_eq!(
                state.event(Event::Key(KeyEvent::new(KeyCode::Char('r'), modifiers))),
                None
            );
        }
        let mut release = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(state.event(Event::Key(release)), None);
        assert_eq!(
            state.event(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
            Some(ResultAction::Quit)
        );
    }

    #[test]
    fn result_details_toggle_is_local_and_resets_scroll() {
        let mut state = ResultScreen::new(&result_fixture(ResultStatus::Attention));
        let summary = screen(80, 24, |frame| state.render(frame));
        assert!(!summary.contains("Synthetic redacted diagnostic"));
        state.scroll = 10;
        assert_eq!(result_key(&mut state, KeyCode::Char('d')), None);
        assert!(state.show_details);
        assert_eq!(state.scroll, 0);
        let details = screen(80, 24, |frame| state.render(frame));
        assert!(details.contains("Technical details"));
        assert!(details.contains("Synthetic redacted diagnostic code"));
        assert!(!details.contains("Documents found"));
        assert!(details.contains("d summary"));
        assert_eq!(result_key(&mut state, KeyCode::Char('d')), None);
        assert!(!state.show_details);
        let summary_again = screen(80, 24, |frame| state.render(frame));
        assert!(summary_again.contains("Documents found"));
        assert!(!summary_again.contains("acceptance_required"));
    }

    #[test]
    fn result_scroll_is_bounded_paged_and_clamped_after_resize() {
        let mut view = result_fixture(ResultStatus::Attention);
        view.sections[0].lines = (0..32)
            .map(|index| format!("Synthetic result row {index:02}"))
            .collect();
        let mut state = ResultScreen::new(&view);
        let first = screen(70, 16, |frame| state.render(frame));
        assert!(first.contains("Synthetic result row 00"));
        assert!(!first.contains("Synthetic result row 31"));
        let page_height = state.page_height;
        assert_eq!(result_key(&mut state, KeyCode::PageDown), None);
        assert_eq!(state.scroll, page_height);
        result_key(&mut state, KeyCode::End);
        assert_eq!(state.scroll, state.max_scroll);
        let last = screen(70, 16, |frame| state.render(frame));
        assert!(last.contains("Review the item in ePost"));
        for _ in 0..200 {
            result_key(&mut state, KeyCode::Down);
        }
        assert_eq!(state.scroll, state.max_scroll);
        screen(100, 80, |frame| state.render(frame));
        assert_eq!(state.scroll, 0);
        result_key(&mut state, KeyCode::PageUp);
        result_key(&mut state, KeyCode::Home);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn result_input_is_sanitized_and_all_dimensions_are_bounded() {
        let unsafe_text = format!("Synthetic\n\r\t\x1b\u{202e}\u{2066}{}", "Ä🔐".repeat(800));
        let view = ResultView {
            title: unsafe_text.clone(),
            status: ResultStatus::Error,
            sections: (0..40)
                .map(|_| ResultSection {
                    title: unsafe_text.clone(),
                    lines: vec![unsafe_text.clone(); 40],
                })
                .collect(),
            issues: (0..40)
                .map(|_| ResultIssue {
                    heading: unsafe_text.clone(),
                    explanation: unsafe_text.clone(),
                    next_step: unsafe_text.clone(),
                })
                .collect(),
            details: vec![unsafe_text; 200],
        };
        let mut state = ResultScreen::new(&view);
        assert_eq!(state.view.sections.len(), RESULT_SECTION_LIMIT);
        assert_eq!(state.view.issues.len(), RESULT_ISSUE_LIMIT);
        assert_eq!(state.view.details.len(), RESULT_DETAIL_LIMIT);
        assert_eq!(state.sections_omitted, 28);
        assert_eq!(state.issues_omitted, 28);
        assert_eq!(state.details_omitted, 100);
        assert!(state.view.title.chars().count() <= 120);
        let mut fields = vec![state.view.title.as_str()];
        for section in &state.view.sections {
            assert!(section.lines.len() <= RESULT_LINE_LIMIT + 1);
            assert!(section.title.chars().count() <= 120);
            fields.push(section.title.as_str());
            fields.extend(section.lines.iter().map(String::as_str));
        }
        for issue in &state.view.issues {
            assert!(issue.heading.chars().count() <= 120);
            fields.extend([
                issue.heading.as_str(),
                issue.explanation.as_str(),
                issue.next_step.as_str(),
            ]);
        }
        fields.extend(state.view.details.iter().map(String::as_str));
        for field in fields {
            assert!(field.chars().count() <= RESULT_TEXT_LIMIT);
            assert!(field.chars().all(visible_character));
            assert!(!field.contains('\u{202e}'));
            assert!(!field.contains('\u{2066}'));
        }
        assert!(result_text(&"\x1b".repeat(100_000), 20).chars().count() <= 20);
        assert_eq!(result_text("abc", 0), "");
        for details in [false, true] {
            state.show_details = details;
            let rendered = screen(70, 20, |frame| state.render(frame));
            assert!(!rendered.contains('\x1b'));
            assert!(!rendered.contains('\u{202e}'));
        }
    }

    #[test]
    fn result_wrap_preserves_words_unicode_and_card_width() {
        assert_eq!(result_wrap("short words stay", 11), ["short words", "stay"]);
        assert_eq!(result_wrap("abcdef", 3), ["abc", "def"]);
        assert_eq!(result_wrap("Ä🔐Ä🔐", 3), ["Ä🔐", "Ä🔐"]);
        for width in [1, 2, 5, 40, 80] {
            for line in result_wrap("🔐 synthetic Ä̈ diagnostic", width) {
                assert!(Line::from(line.as_str()).width() <= width);
            }
        }
        let mut state = ResultScreen::new(&result_fixture(ResultStatus::Attention));
        for (width, height) in [(0, 0), (1, 1), (5, 2), (20, 8), (40, 12), (80, 24)] {
            screen(width, height, |frame| state.render(frame));
            result_key(&mut state, KeyCode::Char('d'));
            screen(width, height, |frame| state.render(frame));
            result_key(&mut state, KeyCode::End);
            screen(width, height, |frame| state.render(frame));
        }
    }

    #[test]
    fn result_loop_returns_only_the_selected_action_and_propagates_display_errors() {
        let view = result_fixture(ResultStatus::Error);
        let mut events = VecDeque::from([
            Event::Resize(80, 24),
            Event::Paste("r".into()),
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        let mut draws = 0;
        let action = result_loop(
            &view,
            |state| {
                draws += 1;
                screen(80, 24, |frame| state.render(frame));
                Ok(())
            },
            || {
                Ok(events
                    .pop_front()
                    .expect("bounded synthetic event sequence"))
            },
        )
        .unwrap();
        assert_eq!(action, ResultAction::Back);
        assert_eq!(draws, 4);
        let mut reads = 0;
        assert!(
            result_loop(
                &view,
                |_| Err(io::Error::other("synthetic draw failure")),
                || {
                    reads += 1;
                    Ok(Event::Key(KeyEvent::new(
                        KeyCode::Char('r'),
                        KeyModifiers::NONE,
                    )))
                }
            )
            .is_err()
        );
        assert_eq!(reads, 0);
        assert!(
            result_loop(
                &view,
                |_| Ok(()),
                || Err(io::Error::other("synthetic event failure"))
            )
            .is_err()
        );
    }

    #[test]
    fn menu_displays_configuration_without_inventing_mailbox_health() {
        let state = Home::new(
            Some("synthetic-profile"),
            &["synthetic-paperless".into()],
            false,
        );
        let rendered = screen(90, 24, |frame| state.render(frame));
        for text in [
            "synthetic-profile",
            "synthetic-paperless",
            "Check inbox",
            "Browse archive",
            "read-only",
        ] {
            assert!(rendered.contains(text));
        }
        assert!(!rendered.contains("unread"));
        assert!(!rendered.contains("Healthy"));
    }

    #[test]
    fn terminal_selection_keeps_pipes_and_dumb_terminals_plain() {
        let rich = Some(std::ffi::OsStr::new("xterm-256color"));
        assert!(supported_terminal(true, true, true, rich));
        assert!(supported_terminal(true, true, true, None));
        assert!(!supported_terminal(
            true,
            true,
            true,
            Some(std::ffi::OsStr::new("dumb"))
        ));
        for (input, output, diagnostics) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            assert!(!supported_terminal(input, output, diagnostics, rich));
        }
    }

    #[test]
    fn setup_is_explicit_and_menu_keyboard_selection_is_closed() {
        let mut state = Home::new(None, &[], true);
        assert_eq!(state.actions[0].0, MenuAction::Setup);
        assert!(
            !state
                .actions
                .iter()
                .any(|item| item.0 == MenuAction::CheckInbox)
        );
        assert_eq!(state.event(Event::Resize(12, 6)), None);
        assert_eq!(
            state.event(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            Some(MenuAction::Setup)
        );
        state.event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        assert_eq!(state.actions[state.selected].0, MenuAction::Quit);
        assert_eq!(
            state.event(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            ))),
            Some(MenuAction::Quit)
        );
    }

    fn source_update(state: &mut Activity, event: progress::Event, counts: RetrievalCounts) {
        state.update(progress::Update {
            line: "Synthetic retrieval activity".into(),
            telemetry: Some(Telemetry::Retrieval { event, counts }),
        });
    }

    fn destination_start(
        state: &mut Activity,
        name: &str,
        index: usize,
        total: usize,
        audit: bool,
    ) {
        state.update(progress::Update {
            line: "Synthetic destination activity".into(),
            telemetry: Some(Telemetry::DestinationStarted {
                name: name.into(),
                index,
                total,
                audit,
            }),
        });
    }

    fn destination_update(
        state: &mut Activity,
        phase: DeliveryPhase,
        processed: usize,
        total: usize,
    ) {
        state.update(progress::Update {
            line: "Synthetic delivery activity".into(),
            telemetry: Some(Telemetry::Delivery(DeliveryEvent {
                phase,
                processed,
                total,
                delivered: processed,
                attention: 0,
            })),
        });
    }

    #[test]
    fn progress_inventory_keeps_unknown_total_and_never_parses_log_counts() {
        let mut state = Activity::new("Synthetic inbox check", OperationKind::RetrieveAndDeliver);
        state.line("Checking document 99 of 100…");
        assert!(!state.source_seen);
        assert!(state.inventory.total.is_none());
        source_update(
            &mut state,
            progress::Event::InventoryStarted {
                label: "inbox".into(),
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut state,
            progress::Event::Page {
                page: 2,
                documents: 7,
            },
            RetrievalCounts::default(),
        );
        let rendered = screen(90, 24, |frame| state.render(frame));
        for text in [
            "Connect",
            "Inventory",
            "Retrieve",
            "Deliver",
            "Scanning page 2",
            "7 documents found so far",
            "total unknown",
            "Source results",
            "Destination results",
        ] {
            assert!(rendered.contains(text), "missing progress text: {text}");
        }
        assert!(!rendered.contains('%'));
        assert_eq!(state.focus().3, None);
        assert_eq!(state.inventory.processed, 0);
    }

    #[test]
    fn source_gauge_counts_completed_snapshots_and_resets_for_each_inventory() {
        let mut state = Activity::new("Synthetic source check", OperationKind::Retrieve);
        source_update(
            &mut state,
            progress::Event::InventoryStarted {
                label: "inbox".into(),
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut state,
            progress::Event::InventoryComplete {
                documents: 8,
                complete: true,
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut state,
            progress::Event::DocumentStarted { index: 1, total: 8 },
            RetrievalCounts::default(),
        );
        assert_eq!(state.focus().3, Some((0, 8)));
        let counts = RetrievalCounts {
            new_pdf: 1,
            reused_pdf: 2,
            other: 1,
            pending: 1,
            failed: 0,
            deferred: 1,
        };
        source_update(
            &mut state,
            progress::Event::DocumentComplete {
                status: "already_saved".into(),
                media_kind: Some("pdf".into()),
            },
            counts,
        );
        assert_eq!(state.focus().3, Some((6, 8)));
        // A subsequent authoritative snapshot recovers missed completions;
        // the next attempted index still cannot count the in-flight document.
        source_update(
            &mut state,
            progress::Event::DocumentStarted { index: 8, total: 8 },
            RetrievalCounts {
                new_pdf: 2,
                reused_pdf: 2,
                other: 1,
                pending: 1,
                failed: 0,
                deferred: 1,
            },
        );
        assert_eq!(state.focus().3, Some((7, 8)));
        source_update(
            &mut state,
            progress::Event::InventoryStarted {
                label: "archive folder".into(),
            },
            RetrievalCounts {
                new_pdf: 2,
                reused_pdf: 2,
                other: 1,
                pending: 1,
                failed: 0,
                deferred: 1,
            },
        );
        assert_eq!(state.inventory.batch, 2);
        assert_eq!(state.inventory.processed, 0);
        assert!(state.inventory.total.is_none());
        source_update(
            &mut state,
            progress::Event::InventoryComplete {
                documents: 3,
                complete: true,
            },
            RetrievalCounts {
                new_pdf: 2,
                reused_pdf: 2,
                other: 1,
                pending: 1,
                failed: 0,
                deferred: 1,
            },
        );
        source_update(
            &mut state,
            progress::Event::DocumentComplete {
                status: "saved_pdf".into(),
                media_kind: Some("pdf".into()),
            },
            RetrievalCounts {
                new_pdf: 3,
                reused_pdf: 2,
                other: 1,
                pending: 1,
                failed: 0,
                deferred: 1,
            },
        );
        assert_eq!(state.focus().3, Some((1, 3)));
        assert_eq!(state.source.new_pdf, 3);
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(rendered.contains("archive folder · inventory 2"));
        assert!(rendered.contains("1 / 3 processed"));
        assert!(!rendered.contains('%'));
    }

    #[test]
    fn source_finished_is_separate_from_delivery_and_each_destination_resets_its_gauge() {
        let mut state = Activity::new(
            "Synthetic check and delivery",
            OperationKind::RetrieveAndDeliver,
        );
        source_update(
            &mut state,
            progress::Event::InventoryStarted {
                label: "inbox".into(),
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut state,
            progress::Event::InventoryComplete {
                documents: 4,
                complete: true,
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut state,
            progress::Event::Finished { exit_code: 0 },
            RetrievalCounts {
                new_pdf: 3,
                reused_pdf: 1,
                ..Default::default()
            },
        );
        assert_eq!(state.phase, Phase::Retrieve);
        assert!(state.focus().1.contains("preparing delivery"));
        assert_eq!(state.focus().3, Some((4, 4)));
        destination_start(&mut state, "synthetic-folder", 1, 2, false);
        assert_eq!(state.phase, Phase::Deliver);
        assert_eq!(state.focus().3, None);
        destination_update(&mut state, DeliveryPhase::Uploading, 1, 4);
        assert_eq!(state.focus().3, Some((1, 4)));
        assert_eq!(state.source.new_pdf, 3);
        destination_update(&mut state, DeliveryPhase::Finished, 4, 4);
        destination_start(&mut state, "synthetic-paperless", 2, 2, false);
        assert_eq!(state.focus().3, None);
        assert!(state.destination.as_ref().unwrap().event.is_none());
        destination_update(&mut state, DeliveryPhase::Verifying, 0, 3);
        assert_eq!(state.focus().3, Some((0, 3)));
        let rendered = screen(90, 24, |frame| state.render(frame));
        for text in [
            "synthetic-paperless",
            "destination 2 of 2",
            "0 / 3 processed",
            "Verifying destination content",
            "3 New PDF",
            "1 Reused PDF",
            "0 confirmed",
        ] {
            assert!(rendered.contains(text), "missing destination text: {text}");
        }
    }

    #[test]
    fn source_authentication_failure_never_becomes_connected_during_local_delivery() {
        let mut state = Activity::new(
            "Synthetic source failure",
            OperationKind::RetrieveAndDeliver,
        );
        source_update(
            &mut state,
            progress::Event::Authenticating,
            RetrievalCounts::default(),
        );
        assert!(!state.phase_done(Phase::Connect));
        source_update(
            &mut state,
            progress::Event::Finished { exit_code: 3 },
            RetrievalCounts::default(),
        );
        assert!(!state.phase_done(Phase::Connect));
        assert!(state.focus().1.contains("connection failed"));
        destination_start(&mut state, "synthetic-local-destination", 1, 1, false);
        destination_update(&mut state, DeliveryPhase::Uploading, 1, 2);
        assert!(!state.phase_done(Phase::Connect));
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(rendered.contains("! Connect"));
        assert!(!rendered.contains("✓ Connect"));
        source_update(
            &mut state,
            progress::Event::InventoryStarted {
                label: "inbox".into(),
            },
            RetrievalCounts::default(),
        );
        assert!(state.phase_done(Phase::Connect));
        source_update(
            &mut state,
            progress::Event::Authenticating,
            RetrievalCounts::default(),
        );
        assert!(!state.phase_done(Phase::Connect));
    }

    #[test]
    fn partial_and_stopped_operations_never_force_progress_to_complete() {
        let mut state = Activity::new("Synthetic partial source", OperationKind::Retrieve);
        source_update(
            &mut state,
            progress::Event::InventoryStarted {
                label: "inbox".into(),
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut state,
            progress::Event::InventoryComplete {
                documents: 5,
                complete: false,
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut state,
            progress::Event::Finished { exit_code: 2 },
            RetrievalCounts {
                pending: 1,
                ..Default::default()
            },
        );
        assert_eq!(state.focus().3, Some((1, 5)));
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(rendered.contains("Incomplete inventory"));
        assert!(rendered.contains("1 / 5 processed"));
        assert!(rendered.contains("needs attention"));
        assert!(!state.phase_done(Phase::Retrieve));
        assert!(!rendered.contains("100%"));
        let mut state = Activity::new("Synthetic stopped delivery", OperationKind::Deliver);
        destination_start(&mut state, "synthetic-target", 1, 1, false);
        destination_update(&mut state, DeliveryPhase::Finished, 1, 5);
        assert_eq!(state.focus().3, Some((1, 5)));
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(rendered.contains("stopped before all items"));
        assert!(rendered.contains("1 / 5 processed"));
        assert!(!rendered.contains("100%"));
    }

    #[test]
    fn progress_phases_and_counters_match_each_command() {
        for (kind, label) in [
            (OperationKind::Deliver, "Deliver"),
            (OperationKind::Verify, "Verify"),
            (OperationKind::Doctor, "Checks"),
            (OperationKind::Inspect, "Inspect"),
            (OperationKind::Reconcile, "Reconcile"),
        ] {
            let state = Activity::new("Synthetic command", kind);
            assert_eq!(kind.phases().len(), 1);
            let rendered = screen(90, 24, |frame| state.render(frame));
            assert!(rendered.contains(label));
            assert!(!rendered.contains("Source results"));
            assert!(!rendered.contains("Mailbox"));
        }
        let mut inspect = Activity::new("Synthetic metadata", OperationKind::Inspect);
        source_update(
            &mut inspect,
            progress::Event::Authenticating,
            RetrievalCounts::default(),
        );
        assert_eq!(inspect.phase, Phase::Inspect);
        source_update(
            &mut inspect,
            progress::Event::InventoryStarted {
                label: "synthetic metadata listing".into(),
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut inspect,
            progress::Event::Page {
                page: 1,
                documents: 4,
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut inspect,
            progress::Event::InventoryComplete {
                documents: 4,
                complete: true,
            },
            RetrievalCounts::default(),
        );
        assert_eq!(inspect.phase, Phase::Inspect);
        let rendered = screen(90, 24, |frame| inspect.render(frame));
        assert!(rendered.contains("4 documents observed"));
        assert!(rendered.contains("Metadata listing"));
        assert!(!rendered.contains("Retrieving"));
        assert!(!rendered.contains("Source results"));
        let mut reconcile = Activity::new("Synthetic recovery", OperationKind::Reconcile);
        destination_start(&mut reconcile, "synthetic-target", 1, 1, true);
        assert_eq!(reconcile.phase, Phase::Reconcile);
        assert!(reconcile.focus().1.contains("selected existing original"));
        let mut state = Activity::new("Synthetic verification", OperationKind::Verify);
        destination_start(&mut state, "synthetic-target", 1, 1, true);
        destination_update(&mut state, DeliveryPhase::Verifying, 2, 4);
        assert_eq!(state.phase, Phase::Verify);
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(rendered.contains("Verification of this destination"));
        assert!(rendered.contains("2 / 4 processed"));
        let mut state = Activity::new("Synthetic Doctor", OperationKind::Doctor);
        for status in [DiagnosticStatus::Warning, DiagnosticStatus::Ok] {
            state.update(progress::Update {
                line: "Synthetic check".into(),
                telemetry: Some(Telemetry::Diagnostic {
                    component: "local configuration".into(),
                    status,
                }),
            });
        }
        assert_eq!(state.diagnostics.len(), 1);
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(rendered.contains("1 OK"));
        assert!(rendered.contains("0 Warning"));
        let compact = screen(48, 14, |frame| state.render(frame));
        assert!(compact.contains("Diagnostic results"));
        assert!(compact.contains("1 OK"));
        assert!(compact.contains("0 Warning"));
        assert!(compact.contains("Recent activity"));
        assert!(compact.contains("Synthetic check"));
    }

    #[test]
    fn progress_details_are_bounded_sanitized_and_separate_from_recent_activity() {
        let mut state = Activity::new("Synthetic\n\x1b\u{202e} title", OperationKind::Retrieve);
        for index in 0..1000 {
            state.line(&format!("Synthetic event {index}"));
        }
        assert_eq!(state.history.len(), HISTORY_LIMIT);
        assert!(!state.history.iter().any(|line| line == "Synthetic event 0"));
        state.line(&format!("provider\n\x1b\u{202e}{}", "x".repeat(4000)));
        assert!(state.history.back().unwrap().chars().count() <= 512);
        assert!(state.history.back().unwrap().chars().all(visible_character));
        state.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.scroll, 0);
        state.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        assert!(!state.show_details);
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(!rendered.contains("Synthetic event 950"));
        state.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(state.show_details);
        for _ in 0..50 {
            state.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        let rendered = screen(90, 24, |frame| state.render(frame));
        assert!(rendered.contains("Details · recent 100 events"));
        assert!(rendered.contains("Synthetic event 950"));
        assert!(!rendered.contains('\u{202e}'));
        state.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(state.scroll, 0);
        state.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(!state.show_details);
        source_update(
            &mut state,
            progress::Event::InventoryStarted {
                label: format!("synthetic\n\x1b\u{202e}{}", "x".repeat(1000)),
            },
            RetrievalCounts::default(),
        );
        assert!(state.inventory.label.chars().count() <= 80);
        assert!(state.inventory.label.chars().all(visible_character));
        destination_start(
            &mut state,
            &format!("synthetic\n\x1b\u{202e}{}", "x".repeat(1000)),
            1,
            1,
            false,
        );
        assert!(state.destination.as_ref().unwrap().name.chars().count() <= 80);
        assert!(
            state
                .destination
                .as_ref()
                .unwrap()
                .name
                .chars()
                .all(visible_character)
        );
    }

    #[test]
    fn diagnostic_counter_storage_is_bounded_and_reports_omissions() {
        let mut state = Activity::new("Synthetic Doctor", OperationKind::Doctor);
        for index in 0..140 {
            state.update(progress::Update {
                line: "Synthetic check".into(),
                telemetry: Some(Telemetry::Diagnostic {
                    component: format!("synthetic-component-{index}"),
                    status: DiagnosticStatus::Ok,
                }),
            });
        }
        assert_eq!(state.diagnostics.len(), 128);
        assert!(state.diagnostic_overflow);
        assert!(state.focus().4);
        let rendered = screen(100, 24, |frame| state.render(frame));
        assert!(rendered.contains("Counts limited to 128 observed checks"));
        assert!(!rendered.contains("140 OK"));
    }

    #[test]
    fn tiny_resized_screens_render_without_panicking() {
        let state = Home::new(None, &[], true);
        let mut activity = Activity::new("synthetic work", OperationKind::RetrieveAndDeliver);
        source_update(
            &mut activity,
            progress::Event::InventoryStarted {
                label: "synthetic empty inventory".into(),
            },
            RetrievalCounts::default(),
        );
        source_update(
            &mut activity,
            progress::Event::InventoryComplete {
                documents: 0,
                complete: true,
            },
            RetrievalCounts::default(),
        );
        assert_eq!(activity.focus().3, Some((0, 0)));
        let prompts = TuiPrompts::default();
        for (width, height) in [(0, 0), (0, 4), (4, 0), (1, 1), (5, 2), (20, 8), (80, 24)] {
            screen(width, height, |frame| state.render(frame));
            activity.show_details = false;
            screen(width, height, |frame| activity.render(frame));
            activity.show_details = true;
            screen(width, height, |frame| activity.render(frame));
            screen(width, height, |frame| {
                prompts.render_input(frame, "Password", &InputBuffer::default(), true)
            });
        }
    }

    #[test]
    fn secret_rendering_and_history_never_contain_the_entered_answer() {
        let mut prompts = TuiPrompts::default();
        prompts.add_line("Choose your mailbox.\x1b\u{202e}");
        let mut input = InputBuffer::default();
        for character in "synthetic-secret-Ä🔐".chars() {
            input.push(character);
        }
        let rendered = screen(80, 14, |frame| {
            prompts.render_input(frame, "Password", &input, true)
        });
        assert!(rendered.contains('•'));
        assert!(!rendered.contains("synthetic-secret"));
        assert!(
            !prompts
                .history
                .iter()
                .any(|line| line.contains("synthetic-secret"))
        );
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\u{202e}'));
    }

    #[test]
    fn unicode_input_is_byte_bounded_and_controls_cannot_be_pasted() {
        let mut input = InputBuffer::default();
        for character in "Ä🔐\n\r\x1b\u{202e}\u{2066}".chars() {
            input.push(character);
        }
        assert_eq!(input.text.as_str(), "Ä🔐");
        for _ in 0..9000 {
            input.push('ä');
        }
        assert!(input.text.len() <= INPUT_LIMIT);
        assert!(input.limited);
        assert!(input.text.is_char_boundary(input.text.len()));
        input.text.zeroize();
        assert!(input.text.is_empty());
    }

    #[test]
    fn synthetic_input_events_edit_cancel_and_accept_secrets_without_echoing() {
        let mut input = InputBuffer::default();
        let mut paste = Event::Paste("Ä🔐\n\x1b\u{202e}".into());
        assert!(matches!(
            input.event(&mut paste, true),
            InputAction::Continue
        ));
        assert_eq!(input.text.as_str(), "Ä🔐");
        assert!(matches!(paste, Event::Paste(ref text) if text.is_empty()));
        input.event(
            &mut Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            true,
        );
        assert_eq!(input.text.as_str(), "Ä");
        input.event(
            &mut Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            true,
        );
        assert!(input.text.is_empty());
        input.push('q');
        let answer = input.event(
            &mut Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            true,
        );
        assert!(matches!(answer, InputAction::Accepted(ref text) if text == "q"));
        assert!(input.text.is_empty());
        input.push('q');
        assert!(matches!(
            input.event(
                &mut Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                false
            ),
            InputAction::Cancelled
        ));
        for key in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        ] {
            input.push('x');
            assert!(matches!(
                input.event(&mut Event::Key(key), true),
                InputAction::Cancelled
            ));
            assert!(input.text.is_empty());
        }
    }

    #[test]
    fn oversized_paste_cannot_be_silently_accepted_as_a_truncated_password() {
        let mut input = InputBuffer::default();
        input.event(&mut Event::Paste("x".repeat(INPUT_LIMIT + 1)), true);
        assert!(input.limited);
        assert!(matches!(
            input.event(
                &mut Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                true
            ),
            InputAction::Continue
        ));
        input.event(
            &mut Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            true,
        );
        assert!(matches!(
            input.event(
                &mut Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                true
            ),
            InputAction::Accepted(_)
        ));
    }

    #[tokio::test]
    async fn stop_input_and_display_error_leave_the_same_future_available_to_finish() {
        for display_error in [false, true] {
            let mut future = std::pin::pin!(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Ok(42)
            });
            let outcome = operation_loop(
                "Synthetic upload",
                OperationKind::Deliver,
                future.as_mut(),
                None,
                |_| {
                    if display_error {
                        Err(io::Error::other("synthetic terminal failure"))
                    } else {
                        Ok(())
                    }
                },
                || {
                    Ok(vec![Event::Key(KeyEvent::new(
                        KeyCode::Esc,
                        KeyModifiers::NONE,
                    ))])
                },
            )
            .await;
            assert!(matches!(
                outcome,
                LiveOutcome::StopRequested | LiveOutcome::DisplayUnavailable
            ));
            assert_eq!(future.await.unwrap(), 42);
        }
    }

    #[tokio::test]
    async fn progress_loop_returns_the_actual_operation_result() {
        let mut future = std::pin::pin!(async { Err::<(), _>(Error::Auth) });
        let outcome = operation_loop(
            "Synthetic sign-in",
            OperationKind::Retrieve,
            future.as_mut(),
            None,
            |_| Ok(()),
            || Ok(Vec::new()),
        )
        .await;
        assert!(matches!(outcome, LiveOutcome::Completed(Err(Error::Auth))));
    }
}
