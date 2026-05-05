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
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use nico_common::output::OutputMode;
use crate::event::{Event as CorrelateEvent, Severity};
use crate::source::{SourceKind, StateEntry, SourceResult};
use crate::diagnosis::{diagnose, Diagnosis, DiagnosisConfig};
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
    pub diagnosis: DiagnosisConfig,
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
    diag_config: DiagnosisConfig,
    done_count: usize,
    total_sources: usize,
    has_unavailable: bool,

    // Cursor — pinned to the selected event by identity, not row index.
    // list_state holds a *filtered visual index*, not a raw events index.
    selected_key: Option<EventKey>,
    list_state: ListState,

    help_open: bool,

    // Filter bar — view-layer only, does not mutate self.events.
    filter_query: String,
    filter_active: bool,
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
            diag_config: config.diagnosis.clone(),
            done_count: 0,
            total_sources: config.source_names.len(),
            has_unavailable: false,
            selected_key: None,
            list_state: ListState::default(),
            help_open: false,
            filter_query: String::new(),
            filter_active: false,
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
            self.diagnosis = diagnose(&self.events, &self.state_entries, &self.diag_config);
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

    /// Indices into `self.events` that pass the current filter query.
    /// Returns all indices when the query is empty.
    fn filtered_indices(&self) -> Vec<usize> {
        if self.filter_query.is_empty() {
            return (0..self.events.len()).collect();
        }
        let needle = self.filter_query.to_lowercase();
        self.events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.source.to_lowercase().contains(&needle)
                    || e.kind.to_lowercase().contains(&needle)
                    || e.message.to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Re-pin the list cursor to the previously selected event after a merge or
    /// filter change.  list_state is a *filtered visual index*.
    fn sync_cursor(&mut self) {
        if let Some(ref key) = self.selected_key.clone() {
            let raw_idx = self.events.iter().position(|e| {
                e.ts == key.ts && e.source == key.source && e.kind == key.kind
            });
            let visual_idx = raw_idx.and_then(|ri| {
                self.filtered_indices().iter().position(|&fi| fi == ri)
            });
            self.list_state.select(visual_idx);
        }
    }

    /// Select the event at `vis_idx` in the current filtered list.
    fn select_at(&mut self, vis_idx: usize) {
        let indices = self.filtered_indices();
        if let Some(&raw_idx) = indices.get(vis_idx) {
            if let Some(e) = self.events.get(raw_idx) {
                self.selected_key = Some(event_key(e));
                self.list_state.select(Some(vis_idx));
            }
        }
    }

    fn select_first(&mut self) {
        if !self.filtered_indices().is_empty() {
            self.select_at(0);
        }
    }

    fn select_last(&mut self) {
        let n = self.filtered_indices().len();
        if n > 0 {
            self.select_at(n - 1);
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
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        let n = match self.list_state.selected() {
            None => 0,
            Some(n) => (n + 1).min(count - 1),
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
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        let n = match self.list_state.selected() {
            None => 0,
            Some(n) => (n + page).min(count - 1),
        };
        self.select_at(n);
    }

    fn open_filter(&mut self) {
        self.filter_active = true;
    }

    fn close_filter(&mut self) {
        self.filter_active = false;
        self.filter_query.clear();
        self.sync_cursor();
    }

    fn push_filter_char(&mut self, c: char) {
        self.filter_query.push(c);
        self.sync_cursor();
    }

    fn pop_filter_char(&mut self) {
        self.filter_query.pop();
        self.sync_cursor();
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
            if state.help_open {
                match key.code {
                    KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                        state.help_open = false;
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    _ => {}
                }
            } else if state.filter_active {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Esc => state.close_filter(),
                    KeyCode::Backspace => state.pop_filter_char(),
                    KeyCode::Up => state.select_prev(),
                    KeyCode::Down => state.select_next(),
                    KeyCode::PageUp => state.select_prev_page(PAGE_SIZE),
                    KeyCode::PageDown => state.select_next_page(PAGE_SIZE),
                    KeyCode::Char(c) => state.push_filter_char(c),
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('?') => state.help_open = true,
                    KeyCode::Char('/') => state.open_filter(),
                    KeyCode::Esc => {
                        if !state.filter_query.is_empty() {
                            state.close_filter();
                        }
                    }
                    KeyCode::Up => state.select_prev(),
                    KeyCode::Down => state.select_next(),
                    KeyCode::PageUp => state.select_prev_page(PAGE_SIZE),
                    KeyCode::PageDown => state.select_next_page(PAGE_SIZE),
                    KeyCode::Char('g') => state.select_first(),
                    KeyCode::Char('G') | KeyCode::End => state.select_last(),
                    _ => {}
                }
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

    if state.help_open {
        render_help_overlay(frame, ctx, area);
    }
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

    // Pre-compute before any mutable borrow of list_state.
    let filtered = state.filtered_indices();
    let total = state.events.len();
    let selected_vis = state.list_state.selected();
    let filter_active = state.filter_active;
    let filter_query = state.filter_query.clone();
    let show_placeholder = state.show_placeholder();

    let title = if !filter_query.is_empty() {
        format!(" Timeline ({}/{}) ", filtered.len(), total)
    } else {
        format!(" Timeline ({} events) ", total)
    };

    let block = make_block(&title, ascii);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Carve out a one-line filter bar at the bottom when the bar is open or a
    // query is live (so the bar stays visible after pressing Enter to commit).
    let (list_area, bar_area) = if filter_active || !filter_query.is_empty() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);
        (chunks[0], Some(chunks[1]))
    } else {
        (inner, None)
    };

    if show_placeholder {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let placeholder = ListItem::new(Line::from(Span::styled("waiting for sources\u{2026}", dim)));
        let list = List::new(vec![placeholder]);
        frame.render_stateful_widget(list, list_area, &mut state.list_state);
    } else {
        let items: Vec<ListItem> = filtered.iter().enumerate().map(|(vis_idx, &raw_idx)| {
            let e = &state.events[raw_idx];
            let ts = e.ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let prefix = if selected_vis == Some(vis_idx) { selector } else { " " };
            let row = format!("{prefix} {}  {}  {}", ts, e.source, e.kind);
            let mut style = severity_style(&e.severity, use_color);
            if selected_vis == Some(vis_idx) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            ListItem::new(Line::from(Span::styled(row, style)))
        }).collect();

        let list = List::new(items);
        frame.render_stateful_widget(list, list_area, &mut state.list_state);
    }

    if let Some(bar) = bar_area {
        let cursor = if filter_active { "_" } else { "" };
        let bar_text = format!("/{}{}", filter_query, cursor);
        let style = if use_color {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        frame.render_widget(Paragraph::new(Span::styled(bar_text, style)), bar);
    }
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

    let filtered = state.filtered_indices();
    let filter_has_no_results = !state.filter_query.is_empty() && filtered.is_empty();

    // Translate visual index → raw event index.
    let selected_event: Option<&CorrelateEvent> = state
        .list_state
        .selected()
        .and_then(|vis| filtered.get(vis))
        .and_then(|&raw| state.events.get(raw));

    let widget = if filter_has_no_results {
        Paragraph::new(Line::from(Span::styled("No events match filter", dim))).block(block)
    } else {
        match selected_event {
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

    // Right: quit hint (shows Esc hint while filter is active)
    let hint = if state.filter_active {
        "Esc:clear  q:quit"
    } else {
        "?:help  q:quit"
    };
    frame.render_widget(
        Paragraph::new(hint).alignment(Alignment::Right),
        cols[2],
    );
}

fn center_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn render_help_overlay(frame: &mut Frame, ctx: &TuiContext, area: Rect) {
    let ascii = ctx.mode.ascii;
    let overlay_rect = center_rect(62, 16, area);

    frame.render_widget(Clear, overlay_rect);

    let block = make_block(" Keybindings  \u{2014}  ? or Esc to close ", ascii);
    let inner = block.inner(overlay_rect);
    frame.render_widget(block, overlay_rect);

    let dim = Style::default().add_modifier(Modifier::DIM);
    let rows: &[(&str, &str)] = &[
        ("\u{2191} / \u{2193}",      "Move selection"),
        ("PgUp / PgDn", "Fast scroll"),
        ("g",           "Jump to first row"),
        ("G / End",     "Jump to last row"),
        ("f",           "Toggle auto-follow (tail mode)"),
        ("/",           "Open filter bar"),
        ("Escape",      "Clear filter / dismiss overlay"),
        ("Enter",       "Open full-screen event detail"),
        ("?",           "Toggle this help overlay"),
        ("q / Ctrl-C",  "Exit"),
    ];
    let lines: Vec<Line> = rows.iter().map(|(key, action)| {
        Line::from(vec![
            Span::styled(format!("  {:<14}", key), dim),
            Span::raw(*action),
        ])
    }).collect();

    frame.render_widget(Paragraph::new(lines), inner);
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
            diagnosis: DiagnosisConfig::default(),
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
                    make_event(1_746_450_063, "temporal", "WorkflowExecutionStarted", Severity::Info, "workflow started"),
                    make_event(1_746_450_069, "temporal", "ActivityTaskScheduled", Severity::Info, "attempt 1/3"),
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

    /// Three events across two sources: temporal(0), k8s(1), temporal(2).
    /// Useful for filter tests: query "temporal" → 2 hits, query "k8s" → 1 hit.
    fn three_event_state() -> (TuiConfig, IncrementalState) {
        let config = TuiConfig {
            id: "wf-filter-test".into(),
            source_names: vec!["temporal", "k8s"],
            restricted: vec![],
            diagnosis: DiagnosisConfig::default(),
        };
        let mut state = IncrementalState::new(&config);
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![
                    make_event(1_000, "temporal", "started",  Severity::Info,  "workflow started"),
                    make_event(3_000, "temporal", "failed",   Severity::Error, "attempt 3/3"),
                ],
                state: vec![],
            }),
        });
        state.apply_update(TuiUpdate::SourceDone {
            name: "k8s".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![make_event(2_000, "k8s", "warning", Severity::Warning, "pod restarted")],
                state: vec![],
            }),
        });
        (config, state)
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
            diagnosis: DiagnosisConfig::default(),
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
            diagnosis: DiagnosisConfig::default(),
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
        // No filter active, so visual index == raw index.
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
            diagnosis: DiagnosisConfig::default(),
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
            diagnosis: DiagnosisConfig::default(),
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

    // ─── Help overlay ─────────────────────────────────────────────────────────

    #[test]
    fn help_overlay_renders_keybindings() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = sample_state(&config);
        state.help_open = true;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");

        assert!(all_rows.contains("Keybindings"), "Expected overlay title: {all_rows}");
        assert!(all_rows.contains("Move selection"), "Expected 'Move selection': {all_rows}");
        assert!(all_rows.contains("Fast scroll"), "Expected 'Fast scroll': {all_rows}");
        assert!(all_rows.contains("Jump to first row"), "Expected 'Jump to first row': {all_rows}");
        assert!(all_rows.contains("auto-follow"), "Expected 'auto-follow': {all_rows}");
        assert!(all_rows.contains("filter bar"), "Expected 'filter bar': {all_rows}");
        assert!(all_rows.contains("dismiss overlay"), "Expected 'dismiss overlay': {all_rows}");
        assert!(all_rows.contains("full-screen"), "Expected 'full-screen': {all_rows}");
        assert!(all_rows.contains("Toggle this help overlay"), "Expected help overlay entry: {all_rows}");
        assert!(all_rows.contains("q / Ctrl-C"), "Expected 'q / Ctrl-C': {all_rows}");
    }

    #[test]
    fn help_overlay_dismissed_by_question_mark() {
        let config = sample_config();
        let mut state = sample_state(&config);
        state.help_open = true;
        assert!(state.help_open);
        state.help_open = false;
        assert!(!state.help_open);
    }

    #[test]
    fn help_overlay_not_shown_by_default() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = sample_state(&config);
        // help_open defaults to false

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(!all_rows.contains("Keybindings"), "Overlay should not appear by default: {all_rows}");
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

    // ─── Filter bar ───────────────────────────────────────────────────────────

    #[test]
    fn filtered_indices_empty_query_returns_all() {
        let (_, state) = three_event_state();
        assert_eq!(state.filtered_indices(), vec![0, 1, 2]);
    }

    #[test]
    fn filtered_indices_matches_source_name_case_insensitive() {
        let (_, mut state) = three_event_state();
        state.filter_query = "TEMPORAL".into();
        let fi = state.filtered_indices();
        assert_eq!(fi.len(), 2, "Expected 2 temporal events");
        assert!(fi.iter().all(|&i| state.events[i].source == "temporal"));
    }

    #[test]
    fn filtered_indices_matches_message_text() {
        let (_, mut state) = three_event_state();
        state.filter_query = "attempt".into();
        let fi = state.filtered_indices();
        assert_eq!(fi.len(), 1, "Expected 1 event with 'attempt' in message");
        assert_eq!(state.events[fi[0]].kind, "failed");
    }

    #[test]
    fn filtered_indices_matches_kind_text() {
        let (_, mut state) = three_event_state();
        state.filter_query = "warning".into();
        let fi = state.filtered_indices();
        assert_eq!(fi.len(), 1, "Expected 1 event with kind 'warning'");
        assert_eq!(state.events[fi[0]].source, "k8s");
    }

    #[test]
    fn filter_zero_match_returns_empty() {
        let (_, mut state) = three_event_state();
        state.filter_query = "xyzzy_no_such_source".into();
        assert!(state.filtered_indices().is_empty());
    }

    #[test]
    fn cursor_identity_preserved_when_event_survives_filter() {
        let (_, mut state) = three_event_state();
        // Select the k8s event (raw index 1, visual index 1 with no filter).
        state.select_at(1);
        assert_eq!(state.selected(), Some(1));

        // Apply a filter that keeps only k8s events.
        state.filter_query = "k8s".into();
        state.sync_cursor();

        // k8s event is now at filtered visual index 0.
        assert_eq!(state.selected(), Some(0), "k8s event should be at visual 0 after filter");
        let raw = state.filtered_indices()[0];
        assert_eq!(state.events[raw].source, "k8s");
    }

    #[test]
    fn cursor_deselects_when_event_filtered_out() {
        let (_, mut state) = three_event_state();
        // Select the first temporal event (raw index 0).
        state.select_at(0);
        assert_eq!(state.selected(), Some(0));

        // Apply a filter that hides temporal events.
        state.filter_query = "k8s".into();
        state.sync_cursor();

        // temporal event is now hidden — cursor should be None.
        assert_eq!(state.selected(), None, "Cursor should deselect when event is filtered out");
    }

    #[test]
    fn close_filter_restores_cursor_to_correct_position() {
        let (_, mut state) = three_event_state();
        // Select k8s event (raw 1).
        state.select_at(1);

        // Filter to k8s → visual index 0.
        state.filter_query = "k8s".into();
        state.sync_cursor();
        assert_eq!(state.selected(), Some(0));

        // Clear filter — k8s event is back at raw 1, which is visual 1 with no filter.
        state.close_filter();
        assert_eq!(state.selected(), Some(1), "Cursor should restore to raw position after filter cleared");
        let raw_at_visual_1 = state.filtered_indices()[1];
        assert_eq!(state.events[raw_at_visual_1].source, "k8s");
    }

    #[test]
    fn navigation_in_filtered_list_stays_in_bounds() {
        let (_, mut state) = three_event_state();
        state.filter_query = "temporal".into();
        state.sync_cursor();

        // Filtered list has 2 events; select_last should land on visual 1.
        state.select_last();
        assert_eq!(state.selected(), Some(1), "select_last should clamp to filtered list length - 1");
    }

    #[test]
    fn filter_bar_visible_in_render_when_active() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (config, mut state) = three_event_state();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        state.filter_active = true;
        state.filter_query = "tem".into();
        state.sync_cursor();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(all_rows.contains("/tem"), "Expected filter bar '/tem' in render: {all_rows}");
    }

    #[test]
    fn timeline_title_shows_filtered_ratio_when_query_active() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (config, mut state) = three_event_state();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        state.filter_query = "temporal".into();
        state.filter_active = true;
        state.sync_cursor();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        assert!(row0.contains("2/3"), "Expected '2/3' ratio in title: {row0}");
    }

    #[test]
    fn timeline_title_reverts_to_events_count_after_filter_cleared() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (config, mut state) = three_event_state();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };

        // Apply filter then clear it.
        state.filter_query = "temporal".into();
        state.filter_active = true;
        state.close_filter();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        assert!(row0.contains("3 events"), "Expected '3 events' after filter cleared: {row0}");
    }

    #[test]
    fn zero_match_filter_shows_no_events_message() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (config, mut state) = three_event_state();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        state.filter_query = "xyzzy_no_such_source".into();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(
            all_rows.contains("No events match filter"),
            "Expected 'No events match filter': {all_rows}"
        );
    }

    #[test]
    fn filter_does_not_mutate_underlying_events() {
        let (_, mut state) = three_event_state();
        let event_count_before = state.events.len();

        state.filter_query = "temporal".into();
        assert_eq!(state.filtered_indices().len(), 2);

        // Underlying list unchanged.
        assert_eq!(state.events.len(), event_count_before, "filter must not mutate self.events");

        state.close_filter();
        assert_eq!(state.events.len(), event_count_before);
        assert_eq!(state.filtered_indices().len(), event_count_before);
    }

    #[test]
    fn status_bar_shows_esc_hint_while_filter_active() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (config, mut state) = three_event_state();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        state.filter_active = true;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(
            all_rows.contains("Esc:clear"),
            "Expected 'Esc:clear' hint while filter is active: {all_rows}"
        );
    }
}
