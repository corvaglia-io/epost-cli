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
    error::{Error, Result},
    progress,
    secret::Secret,
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use zeroize::{Zeroize, Zeroizing};

const HISTORY_LIMIT: usize = 100;
const INPUT_LIMIT: usize = 8192;
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
    mut future: Pin<&mut impl Future<Output = Result<T>>>,
    capture: Option<&progress::Subscription>,
    mut draw: impl FnMut(&Activity) -> io::Result<()>,
    mut events: impl FnMut() -> io::Result<Vec<Event>>,
) -> LiveOutcome<T> {
    let mut state = Activity::new(title);
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
                    while let Some(line) = capture.try_recv() { state.line(&line); }
                }
                let Ok(events) = events() else { return LiveOutcome::DisplayUnavailable; };
                for event in events {
                    if let Event::Key(key) = event {
                        if stop_key(key) { return LiveOutcome::StopRequested; }
                        if !pressed(key) { continue; }
                        match key.code {
                            KeyCode::Up | KeyCode::PageUp => state.scroll = state.scroll.saturating_add(1),
                            KeyCode::Down | KeyCode::PageDown => state.scroll = state.scroll.saturating_sub(1),
                            KeyCode::End => state.scroll = 0,
                            _ => {},
                        }
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

struct Activity {
    title: String,
    history: VecDeque<String>,
    current: String,
    spinner: usize,
    scroll: usize,
}

impl Activity {
    fn new(title: &str) -> Self {
        Self {
            title: display(title, 120),
            history: VecDeque::new(),
            current: "Starting…".into(),
            spinner: 0,
            scroll: 0,
        }
    }

    fn line(&mut self, text: &str) {
        let text = display(text, 512);
        if text.is_empty() {
            return;
        }
        self.current.clone_from(&text);
        if self.history.len() == HISTORY_LIMIT {
            self.history.pop_front();
        }
        self.history.push_back(text);
        self.scroll = self.scroll.min(self.history.len().saturating_sub(1));
    }

    fn render(&self, frame: &mut Frame) {
        let areas = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
        frame.render_widget(heading("ePost", &self.title), areas[0]);
        let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'][self.spinner];
        frame.render_widget(
            Paragraph::new(format!("{spinner} {}", self.current))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Current phase ")
                        .border_style(Style::default().fg(ACCENT)),
                ),
            areas[1],
        );
        let visible = usize::from(areas[2].height.saturating_sub(2));
        let end = self
            .history
            .len()
            .saturating_sub(self.scroll.min(self.history.len()));
        let start = end.saturating_sub(visible);
        let lines: Vec<Line<'_>> = self
            .history
            .iter()
            .skip(start)
            .take(end - start)
            .map(|line| {
                let attention = line.contains("failed")
                    || line.contains("incomplete")
                    || line.contains("attention")
                    || line.contains("Requires acceptance");
                Line::styled(
                    line.as_str(),
                    Style::default().fg(if attention {
                        Color::Yellow
                    } else {
                        Color::Gray
                    }),
                )
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Progress · counts reflect work observed "),
            ),
            areas[2],
        );
        frame.render_widget(
            Paragraph::new("↑/↓ scroll · q/Esc/Ctrl-C stop after current request")
                .style(Style::default().fg(Color::DarkGray)),
            areas[3],
        );
    }
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

    #[test]
    fn progress_is_bounded_shows_known_counts_and_keeps_uncertainty() {
        let mut state = Activity::new("Synthetic inbox sync");
        for index in 0..1000 {
            state.line(&format!("Scanning page {index}…"));
        }
        assert_eq!(state.history.len(), HISTORY_LIMIT);
        state.line("Inventory incomplete: 63 documents found so far.");
        state.line("Checking document 1 of 63…");
        let rendered = screen(90, 18, |frame| state.render(frame));
        assert!(rendered.contains("1 of 63"));
        assert!(rendered.contains("Inventory incomplete"));
        assert!(!rendered.contains('%'));
        state.scroll = 50;
        let earlier = screen(90, 18, |frame| state.render(frame));
        assert!(earlier.contains("Scanning page"));
    }

    #[test]
    fn tiny_resized_screens_render_without_panicking() {
        let state = Home::new(None, &[], true);
        let activity = Activity::new("synthetic work");
        let prompts = TuiPrompts::default();
        for (width, height) in [(1, 1), (5, 2), (20, 8), (80, 24)] {
            screen(width, height, |frame| state.render(frame));
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
            future.as_mut(),
            None,
            |_| Ok(()),
            || Ok(Vec::new()),
        )
        .await;
        assert!(matches!(outcome, LiveOutcome::Completed(Err(Error::Auth))));
    }
}
