use std::io;
use std::collections::HashMap;
use std::sync::mpsc;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use nico_common::output::OutputMode;
use crate::event::{Event as CorrelateEvent, Severity};
use crate::source::{SourceKind, StateEntry, SourceResult};
use crate::diagnosis::{diagnose, Diagnosis};
use crate::timeline::filter_timeline;

pub struct TuiContext {
    pub mode: OutputMode,
}

/// Static configuration passed to the TUI at launch time.
pub struct TuiConfig {
    pub id: String,
    /// Names of sources that will be attempted (send a TuiUpdate for each).
    pub source_names: Vec<&'static str>,
    /// Names of sources that were skipped entirely (shown as ○).
    pub restricted: Vec<String>,
}

/// Message sent from a source-fetch task to the TUI event loop.
pub enum TuiUpdate {
    SourceDone {
        name: String,
        result: SourceResult,
    },
}

/// Per-source loading state — drives the four-state bottom-bar indicators.
#[derive(Clone, PartialEq, Eq)]
pub enum SourceState {
    Fetching,    // ⟳  in-flight
    Available,   // ●  SourceResult::Output
    Errored,     // ✗  SourceResult::Unavailable
    Unavailable, // ○  restricted / skipped
}

/// Install a panic hook that restores the terminal before printing the panic message.
pub fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));
}

// ─── Event identity for cursor tracking ──────────────────────────────────────

#[derive(Clone, PartialEq)]
struct EventKey {
    ts: chrono::DateTime<chrono::Utc>,
    source: String,
    kind: String,
}

fn event_key(e: &CorrelateEvent) -> EventKey {
    EventKey { ts: e.ts, source: e.source.clone(), kind: e.kind.clone() }
}

// ─── Live incremental state ───────────────────────────────────────────────────

struct IncrementalState {
    events: Vec<CorrelateEvent>,
    source_states: HashMap<String, SourceState>,
    state_entries: Vec<StateEntry>,
    diagnosis: Option<Diagnosis>,
    done_count: usize,
    total_sources: usize,
    has_unavailable: bool,

    // Cursor — pinned to the selected event by identity, not row index.
    selected_key: Option<EventKey>,
    list_state: ListState,
}

impl IncrementalState {
    fn new(config: &TuiConfig) -> Self {
        let mut source_states = HashMap::new();
        for &name in &config.source_names {
            source_states.insert(name.to_string(), SourceState::Fetching);
        }
        for name in &config.restricted {
            source_states.insert(name.clone(), SourceState::Unavailable);
        }
        Self {
            events: vec![],
            source_states,
            state_entries: vec![],
            diagnosis: None,
            done_count: 0,
            total_sources: config.source_names.len(),
            has_unavailable: false,
            selected_key: None,
            list_state: ListState::default(),
        }
    }

    fn apply_update(&mut self, update: TuiUpdate) {
        let TuiUpdate::SourceDone { name, result } = update;
        self.done_count += 1;
        match result {
            SourceResult::Output(output) => {
                self.source_states.insert(name, SourceState::Available);
                self.events.extend(output.events);
                self.events.sort_unstable_by_key(|e| e.ts);
                self.state_entries.extend(output.state);
            }
            SourceResult::Unavailable(_) => {
                self.source_states.insert(name, SourceState::Errored);
                self.has_unavailable = true;
            }
        }
        if self.all_done() {
            // Apply the same timeline filter used by the non-TUI path.
            let filtered = filter_timeline(std::mem::take(&mut self.events), 5, 10);
            self.events = filtered;
            self.diagnosis = diagnose(&self.events, &self.state_entries);
        }
        self.sync_cursor();
    }

    fn all_done(&self) -> bool {
        self.done_count >= self.total_sources
    }

    /// True while the timeline is empty and at least one source is still fetching.
    fn show_placeholder(&self) -> bool {
        self.events.is_empty() && !self.all_done()
    }

    fn exit_code(&self) -> i32 {
        if self.events.is_empty() {
            return 1;
        }
        if self.has_unavailable { 2 } else { 0 }
    }

    fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    /// Re-pin the list cursor to the previously selected event after a merge.
    fn sync_cursor(&mut self) {
        if let Some(ref key) = self.selected_key.clone() {
            let new_idx = self.events.iter().position(|e| {
                e.ts == key.ts && e.source == key.source && e.kind == key.kind
            });
            self.list_state.select(new_idx);
        }
    }

    fn select_at(&mut self, idx: usize) {
        if let Some(e) = self.events.get(idx) {
            self.selected_key = Some(event_key(e));
            self.list_state.select(Some(idx));
        }
    }

    fn select_first(&mut self) {
        if !self.events.is_empty() {
            self.select_at(0);
        }
    }

    fn select_last(&mut self) {
        if !self.events.is_empty() {
            let idx = self.events.len() - 1;
            self.select_at(idx);
        }
    }

    fn select_prev(&mut self) {
        let n = match self.list_state.selected() {
            None | Some(0) => 0,
            Some(n) => n - 1,
        };
        self.select_at(n);
    }

    fn select_next(&mut self) {
        if self.events.is_empty() {
            return;
        }
        let n = match self.list_state.selected() {
            None => 0,
            Some(n) => (n + 1).min(self.events.len() - 1),
        };
        self.select_at(n);
    }

    fn select_prev_page(&mut self, page: usize) {
        let n = match self.list_state.selected() {
            None => 0,
            Some(n) => n.saturating_sub(page),
        };
        self.select_at(n);
    }

    fn select_next_page(&mut self, page: usize) {
        if self.events.is_empty() {
            return;
        }
        let n = match self.list_state.selected() {
            None => 0,
            Some(n) => (n + page).min(self.events.len() - 1),
        };
        self.select_at(n);
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────────

/// Run the incremental TUI.  The caller sends one `TuiUpdate::SourceDone` per
/// source over `rx` as each source resolves.  The TUI renders immediately and
/// populates the timeline as results arrive.
pub fn run_tui_incremental(
    config: TuiConfig,
    rx: mpsc::Receiver<TuiUpdate>,
    ctx: TuiContext,
) -> i32 {
    let mut stdout = io::stdout();
    enable_raw_mode().expect("enable raw mode");
    execute!(stdout, EnterAlternateScreen).expect("enter alternate screen");

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("create terminal");
    let mut state = IncrementalState::new(&config);

    let code = event_loop(&mut terminal, &config, &ctx, &mut state, rx);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    code
}

const PAGE_SIZE: usize = 10;

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    config: &TuiConfig,
    ctx: &TuiContext,
    state: &mut IncrementalState,
    rx: mpsc::Receiver<TuiUpdate>,
) -> i32 {
    loop {
        terminal.draw(|f| render(f, config, ctx, state)).expect("draw");

        // Poll crossterm events with a short timeout so source updates feel
        // nearly instantaneous without burning the CPU.
        if event::poll(std::time::Duration::from_millis(50)).expect("poll")
            && let Ok(CrosstermEvent::Key(key)) = event::read()
        {
            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Up => state.select_prev(),
                KeyCode::Down => state.select_next(),
                KeyCode::PageUp => state.select_prev_page(PAGE_SIZE),
                KeyCode::PageDown => state.select_next_page(PAGE_SIZE),
                KeyCode::Char('g') => state.select_first(),
                KeyCode::Char('G') | KeyCode::End => state.select_last(),
                _ => {}
            }
        }

        // Drain all pending source updates before the next frame.
        while let Ok(update) = rx.try_recv() {
            state.apply_update(update);
        }
    }
    state.exit_code()
}

// ─── Rendering ────────────────────────────────────────────────────────────────

fn severity_style(severity: &Severity, use_color: bool) -> Style {
    if !use_color {
        return Style::default();
    }
    match severity {
        Severity::Info => Style::default().fg(Color::Green),
        Severity::Warning => Style::default().fg(Color::Yellow),
        Severity::Error => Style::default().fg(Color::Red),
    }
}

const ASCII_BORDER: symbols::border::Set = symbols::border::Set {
    top_left: "+",
    top_right: "+",
    bottom_left: "+",
    bottom_right: "+",
    vertical_left: "|",
    vertical_right: "|",
    horizontal_top: "-",
    horizontal_bottom: "-",
};

fn make_block(title: &str, ascii: bool) -> Block<'_> {
    let b = Block::default().title(title).borders(Borders::ALL);
    if ascii { b.border_set(ASCII_BORDER) } else { b }
}

fn render(
    frame: &mut Frame,
    config: &TuiConfig,
    ctx: &TuiContext,
    state: &mut IncrementalState,
) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(rows[0]);

    render_timeline(frame, ctx, state, panes[0]);
    render_detail(frame, ctx, state, panes[1]);
    render_status_bar(frame, ctx, state, config, rows[1]);
}

fn render_timeline(
    frame: &mut Frame,
    ctx: &TuiContext,
    state: &mut IncrementalState,
    area: Rect,
) {
    let ascii = ctx.mode.ascii;
    let use_color = ctx.mode.color;
    let selector = if ascii { ">" } else { "▶" };
    let selected = state.selected();

    let title = format!(" Timeline ({} events) ", state.events.len());
    let block = make_block(&title, ascii);

    if state.show_placeholder() {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let placeholder = ListItem::new(Line::from(Span::styled("waiting for sources\u{2026}", dim)));
        let list = List::new(vec![placeholder]).block(block);
        frame.render_stateful_widget(list, area, &mut state.list_state);
        return;
    }

    let items: Vec<ListItem> = state.events.iter().enumerate().map(|(i, e)| {
        let ts = e.ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let prefix = if selected == Some(i) { selector } else { " " };
        let row = format!("{prefix} {}  {}  {}", ts, e.source, e.kind);
        let mut style = severity_style(&e.severity, use_color);
        if selected == Some(i) {
            style = style.add_modifier(Modifier::REVERSED);
        }
        ListItem::new(Line::from(Span::styled(row, style)))
    }).collect();

    let list = List::new(items).block(block);
    frame.render_stateful_widget(list, area, &mut state.list_state);
}

fn render_detail(
    frame: &mut Frame,
    ctx: &TuiContext,
    state: &IncrementalState,
    area: Rect,
) {
    let ascii = ctx.mode.ascii;
    let block = make_block(" Event detail ", ascii);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let widget = match state.selected().and_then(|i| state.events.get(i)) {
        None => {
            let hint = if ascii { "up/down to select an event" } else { "↑↓ to select an event" };
            Paragraph::new(Line::from(Span::styled(hint, dim))).block(block)
        }
        Some(e) => {
            let detail = if e.message.is_empty() { e.kind.as_str() } else { e.message.as_str() };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("source:  ", dim),
                    Span::raw(e.source.clone()),
                ]),
                Line::from(vec![
                    Span::styled("kind:    ", dim),
                    Span::raw(e.kind.clone()),
                ]),
                Line::from(vec![
                    Span::styled("detail:  ", dim),
                    Span::raw(detail.to_string()),
                ]),
            ];
            if let Some(cmd) = state.diagnosis.as_ref().and_then(|d| d.next_commands.first()) {
                lines.push(Line::from(vec![
                    Span::styled("next:    ", dim),
                    Span::raw(cmd.clone()),
                ]));
            }
            Paragraph::new(lines).block(block).wrap(Wrap { trim: false })
        }
    };
    frame.render_widget(widget, area);
}

fn render_status_bar(
    frame: &mut Frame,
    ctx: &TuiContext,
    state: &IncrementalState,
    config: &TuiConfig,
    area: Rect,
) {
    let ascii = ctx.mode.ascii;
    let use_color = ctx.mode.color;

    let border_block = {
        let b = Block::default().borders(Borders::ALL);
        if ascii { b.border_set(ASCII_BORDER) } else { b }
    };
    let inner = border_block.inner(area);
    frame.render_widget(border_block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Fill(2),
            Constraint::Length(14),
        ])
        .split(inner);

    // Left: entity id + diagnosis label when available
    let left = if let Some(d) = state.diagnosis.as_ref() {
        format!("{} \u{2014} Diagnosis: {}", config.id, d.pattern)
    } else {
        config.id.clone()
    };
    frame.render_widget(Paragraph::new(left), cols[0]);

    // Centre: per-source state indicators
    let fetch_dot = if ascii { "~" } else { "\u{27F3}" };
    let avail_dot = if ascii { "*" } else { "\u{25CF}" };
    let err_dot   = if ascii { "x" } else { "\u{2717}" };
    let skip_dot  = if ascii { "-" } else { "\u{25CB}" };

    let spans: Vec<Span> = SourceKind::ALL.iter().flat_map(|kind| {
        let src = kind.name();
        let ss = state.source_states.get(src);
        let (dot, color) = match ss {
            Some(SourceState::Fetching)    => (fetch_dot, Color::Yellow),
            Some(SourceState::Available)   => (avail_dot, Color::Green),
            Some(SourceState::Errored)     => (err_dot,   Color::Red),
            Some(SourceState::Unavailable) | None => (skip_dot, Color::DarkGray),
        };
        let text = format!("{dot}{src} ");
        let s: Span = if use_color {
            Span::styled(text, Style::default().fg(color))
        } else {
            Span::raw(text)
        };
        std::iter::once(s)
    }).collect();
    frame.render_widget(Paragraph::new(Line::from(spans)), cols[1]);

    // Right: quit hint
    frame.render_widget(
        Paragraph::new("?:help  q:quit").alignment(Alignment::Right),
        cols[2],
    );
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use chrono::{TimeZone, Utc};
    use crate::source::{SourceOutput, SourceUnavailable};

    fn make_event(ts_secs: i64, source: &str, kind: &str, sev: Severity, msg: &str) -> CorrelateEvent {
        CorrelateEvent {
            ts: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            source: source.to_string(),
            kind: kind.to_string(),
            message: msg.to_string(),
            severity: sev,
            tags: HashMap::new(),
        }
    }

    fn sample_config() -> TuiConfig {
        TuiConfig {
            id: "wf-abc123".into(),
            source_names: SourceKind::ALL.iter().map(|k| k.name()).collect(),
            restricted: vec![],
        }
    }

    /// Build an IncrementalState that looks like all sources have resolved with
    /// sample data — used to test rendering without the async loading path.
    fn sample_state(config: &TuiConfig) -> IncrementalState {
        let mut state = IncrementalState::new(config);
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![
                    make_event(1_746_450_063, "temporal", "started", Severity::Info, "workflow started"),
                    make_event(1_746_450_069, "temporal", "activity_failed", Severity::Error, "attempt 3/3"),
                ],
                state: vec![],
            }),
        });
        state.apply_update(TuiUpdate::SourceDone {
            name: "k8s".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![make_event(1_746_450_071, "k8s", "warning", Severity::Warning, "pod restarted")],
                state: vec![],
            }),
        });
        state.apply_update(TuiUpdate::SourceDone {
            name: "postgres".into(),
            result: SourceResult::Output(SourceOutput { events: vec![], state: vec![] }),
        });
        state.apply_update(TuiUpdate::SourceDone {
            name: "loki".into(),
            result: SourceResult::Unavailable(SourceUnavailable { name: "loki", reason: "not configured".into() }),
        });
        state.apply_update(TuiUpdate::SourceDone {
            name: "redfish".into(),
            result: SourceResult::Unavailable(SourceUnavailable { name: "redfish", reason: "not configured".into() }),
        });
        state
    }

    fn row_str(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().chars().next().unwrap_or(' ')).unwrap_or(' '))
            .collect()
    }

    // ─── Layout and rendering ─────────────────────────────────────────────────

    #[test]
    fn two_pane_layout_snapshot_120x24() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = sample_state(&config);
        state.select_first();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        assert!(row0.contains("Timeline"), "Timeline title missing: {row0}");
        assert!(row0.contains("Event detail"), "Event detail title missing: {row0}");

        let has_temporal = (1..21).any(|y| row_str(&buf, y, 120).contains("temporal"));
        assert!(has_temporal, "Expected 'temporal' in timeline rows");

        let has_source_label = (1..21).any(|y| row_str(&buf, y, 120).contains("source:"));
        assert!(has_source_label, "Expected 'source:' label in detail pane");

        let bar_rows: String = (21..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(bar_rows.contains("q:quit"), "Expected 'q:quit' in bottom bar: {bar_rows}");
        assert!(bar_rows.contains("Diagnosis:"), "Expected 'Diagnosis:' in bottom bar: {bar_rows}");
    }

    #[test]
    fn no_selection_shows_hint() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = sample_state(&config);
        // No select_first() — no selection.

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let pane_rows: String = (1..21).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(
            pane_rows.contains("to select an event"),
            "Expected selection hint in detail pane: {pane_rows}"
        );
    }

    #[test]
    fn ascii_mode_uses_ascii_borders() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true } };
        let mut state = sample_state(&config);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        assert!(row0.contains('-'), "Expected ASCII '-' border in row 0: {row0}");
        assert!(!row0.contains('─'), "Expected no box-drawing '─' in ASCII mode: {row0}");
    }

    #[test]
    fn bottom_bar_source_indicators_present() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = sample_state(&config);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar = row_str(&buf, 22, 120);
        assert!(bar.contains("temporal"), "Expected 'temporal' in status bar: {bar}");
        assert!(bar.contains("postgres"), "Expected 'postgres' in status bar: {bar}");
    }

    // ─── Incremental loading / placeholder ───────────────────────────────────

    #[test]
    fn placeholder_shown_while_sources_are_fetching() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = IncrementalState::new(&config); // no updates yet

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let timeline_rows: String = (1..21).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(
            timeline_rows.contains("waiting for sources"),
            "Expected placeholder while fetching: {timeline_rows}"
        );
    }

    #[test]
    fn placeholder_disappears_when_first_event_arrives() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = IncrementalState::new(&config);

        // Send one source with an event.
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![make_event(1_000, "temporal", "started", Severity::Info, "")],
                state: vec![],
            }),
        });

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let timeline_rows: String = (1..21).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(
            !timeline_rows.contains("waiting for sources"),
            "Placeholder should be gone after first event: {timeline_rows}"
        );
        assert!(
            timeline_rows.contains("temporal"),
            "Expected temporal event in timeline: {timeline_rows}"
        );
    }

    #[test]
    fn fetching_indicator_shown_in_bottom_bar() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = TuiConfig {
            id: "wf-1".into(),
            source_names: vec!["temporal"],
            restricted: vec![],
        };
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true } };
        let mut state = IncrementalState::new(&config); // temporal still fetching

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar = row_str(&buf, 22, 120);
        // ASCII fetching indicator is '~'
        assert!(bar.contains('~'), "Expected '~' fetching indicator: {bar}");
    }

    #[test]
    fn errored_source_shows_x_indicator() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = TuiConfig {
            id: "wf-1".into(),
            source_names: vec!["temporal"],
            restricted: vec![],
        };
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true } };
        let mut state = IncrementalState::new(&config);
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Unavailable(SourceUnavailable {
                name: "temporal",
                reason: "connection refused".into(),
            }),
        });

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar = row_str(&buf, 22, 120);
        assert!(bar.contains('x'), "Expected 'x' error indicator for temporal: {bar}");
    }

    // ─── Event-identity cursor tracking ──────────────────────────────────────

    #[test]
    fn cursor_follows_event_when_rows_inserted_above() {
        let config = sample_config();
        let mut state = IncrementalState::new(&config);

        // Seed with two events from temporal (later timestamps).
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![
                    make_event(2_000, "temporal", "ev-b", Severity::Info, ""),
                    make_event(3_000, "temporal", "ev-c", Severity::Info, ""),
                ],
                state: vec![],
            }),
        });

        // Select ev-b (index 0 at this point).
        state.select_at(0);
        assert_eq!(state.selected(), Some(0));

        // Now a second source delivers an earlier event — inserted at index 0.
        state.apply_update(TuiUpdate::SourceDone {
            name: "postgres".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![make_event(1_000, "postgres", "ev-a", Severity::Info, "")],
                state: vec![],
            }),
        });

        // ev-b was at index 0; after merge it is at index 1.  Cursor should follow.
        assert_eq!(state.selected(), Some(1), "cursor should follow ev-b to its new index");

        // The event at the new cursor position should still be ev-b.
        let selected_event = state.events.get(state.selected().unwrap()).unwrap();
        assert_eq!(selected_event.kind, "ev-b");
    }

    #[test]
    fn cursor_stays_none_when_no_selection_made() {
        let config = sample_config();
        let mut state = IncrementalState::new(&config);

        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![make_event(1_000, "temporal", "ev-a", Severity::Info, "")],
                state: vec![],
            }),
        });

        // No select_first() was called — cursor should still be None.
        assert_eq!(state.selected(), None, "unselected cursor should stay None after merge");
    }

    // ─── Selection navigation ─────────────────────────────────────────────────

    #[test]
    fn selection_navigation() {
        let config = TuiConfig {
            id: "wf-1".into(),
            source_names: vec!["temporal"],
            restricted: vec![],
        };
        let mut state = IncrementalState::new(&config);
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput {
                events: (0i64..5).map(|i| make_event(i * 100, "temporal", "ev", Severity::Info, "")).collect(),
                state: vec![],
            }),
        });

        assert_eq!(state.selected(), None);

        state.select_next();
        assert_eq!(state.selected(), Some(0));

        state.select_next();
        assert_eq!(state.selected(), Some(1));

        state.select_prev();
        assert_eq!(state.selected(), Some(0));

        state.select_prev(); // clamped at 0
        assert_eq!(state.selected(), Some(0));

        state.select_last();
        assert_eq!(state.selected(), Some(4));

        state.select_first();
        assert_eq!(state.selected(), Some(0));

        state.select_next_page(10); // clamped at last
        assert_eq!(state.selected(), Some(4));

        state.select_prev_page(10); // clamped at 0
        assert_eq!(state.selected(), Some(0));
    }

    // ─── Misc ─────────────────────────────────────────────────────────────────

    #[test]
    fn scaffold_renders_without_tty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = TuiConfig {
            id: "test-workflow-id".into(),
            source_names: vec![],
            restricted: vec![],
        };
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = IncrementalState::new(&config);
        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();
    }

    #[test]
    fn panic_hook_restores_terminal() {
        install_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("tui scaffold test panic"));
    }

    #[test]
    fn timeline_title_shows_correct_count_after_all_sources_done() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = sample_state(&config); // all 5 sources resolved

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        // sample_state sends 3 events; filter_timeline keeps all 3 (≤ 5+10).
        assert!(row0.contains("3 events"), "Expected '3 events' in title: {row0}");
    }
}
