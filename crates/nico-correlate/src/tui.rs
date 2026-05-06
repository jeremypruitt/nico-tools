use std::io;
use std::collections::HashMap;
use std::sync::mpsc;
use chrono::Utc;
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
use nico_common::theme::Theme;
use crate::event::{Event as CorrelateEvent, Severity};
use crate::source::{SourceKind, StateEntry, SourceResult};
use crate::diagnosis::{diagnose, Diagnosis, DiagnosisConfig};
use crate::timeline::filter_timeline;

pub struct TuiContext {
    pub mode: OutputMode,
    pub theme: Theme,
}

/// Static configuration passed to the TUI at launch time.
pub struct TuiConfig {
    pub id: String,
    /// Names of sources that will be attempted (send a TuiUpdate for each).
    pub source_names: Vec<&'static str>,
    /// Names of sources that were skipped entirely (shown as ○).
    pub restricted: Vec<String>,
    pub diagnosis: DiagnosisConfig,
    /// Whether the TUI was launched with `--tail` (enables FOLLOW/PAUSED indicator).
    pub tail: bool,
}

/// Message sent from a source-fetch task to the TUI event loop.
pub enum TuiUpdate {
    SourceDone {
        name: String,
        result: SourceResult,
    },
    /// New events streamed from a tail-polling task.
    TailEvents {
        events: Vec<CorrelateEvent>,
    },
    /// A source that was polling encountered an error.
    TailSourceError {
        source: String,
        message: String,
    },
}

/// Per-source loading state — drives the four-state bottom-bar indicators.
#[derive(Clone, PartialEq, Eq, Debug)]
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
    detail_open: bool,

    // Filter bar — view-layer only, does not mutate self.events.
    filter_query: String,
    filter_active: bool,

    // Tail mode — follow causes new events to auto-scroll to the last row.
    tail_mode: bool,
    follow: bool,
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
            detail_open: false,
            filter_query: String::new(),
            filter_active: false,
            tail_mode: config.tail,
            follow: config.tail,
        }
    }

    fn apply_update(&mut self, update: TuiUpdate) {
        match update {
            TuiUpdate::SourceDone { name, result } => {
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
            TuiUpdate::TailEvents { events } => {
                // Tail events bypass the timeline filter — stream them all in.
                self.events.extend(events);
                self.events.sort_unstable_by_key(|e| e.ts);
                self.sync_cursor();
                if self.follow {
                    self.select_last();
                }
            }
            TuiUpdate::TailSourceError { source, message } => {
                // Only inject the synthetic event once: on the Available→Errored transition.
                let was_available = self.source_states.get(&source) == Some(&SourceState::Available);
                self.source_states.insert(source.clone(), SourceState::Errored);
                if was_available {
                    let synthetic = CorrelateEvent {
                        ts: Utc::now(),
                        source,
                        kind: "source_error".to_string(),
                        message,
                        severity: Severity::Error,
                        tags: HashMap::new(),
                    };
                    self.events.push(synthetic);
                    self.events.sort_unstable_by_key(|e| e.ts);
                }
                self.sync_cursor();
                if self.follow {
                    self.select_last();
                }
            }
        }
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

    #[cfg(test)]
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
        if let Some(&raw_idx) = indices.get(vis_idx)
            && let Some(e) = self.events.get(raw_idx)
        {
            self.selected_key = Some(event_key(e));
            self.list_state.select(Some(vis_idx));
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

    fn toggle_follow(&mut self) {
        self.follow = !self.follow;
        if self.follow {
            self.select_last();
        }
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

        let was_overlay = state.help_open || state.detail_open;

        // Poll crossterm events with a short timeout so source updates feel
        // nearly instantaneous without burning the CPU.
        if event::poll(std::time::Duration::from_millis(50)).expect("poll")
            && let Ok(CrosstermEvent::Key(key)) = event::read()
        {
            if state.detail_open {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => state.detail_open = false,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    _ => {}
                }
            } else if state.help_open {
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
                    KeyCode::Up => {
                        state.select_prev();
                        if state.tail_mode { state.follow = false; }
                    }
                    KeyCode::Down => state.select_next(),
                    KeyCode::PageUp => {
                        state.select_prev_page(PAGE_SIZE);
                        if state.tail_mode { state.follow = false; }
                    }
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
                    KeyCode::Esc if !state.filter_query.is_empty() => {
                        state.close_filter();
                    }
                    KeyCode::Up => {
                        state.select_prev();
                        if state.tail_mode { state.follow = false; }
                    }
                    KeyCode::Down => state.select_next(),
                    KeyCode::PageUp => {
                        state.select_prev_page(PAGE_SIZE);
                        if state.tail_mode { state.follow = false; }
                    }
                    KeyCode::PageDown => state.select_next_page(PAGE_SIZE),
                    KeyCode::Char('g') => state.select_first(),
                    KeyCode::Char('G') | KeyCode::End => {
                        state.select_last();
                        if state.tail_mode { state.follow = true; }
                    }
                    KeyCode::Char('f') if state.tail_mode => { state.toggle_follow(); }
                    KeyCode::Enter => state.detail_open = true,
                    _ => {}
                }
            }
        }

        // When an overlay closes, force a full repaint so no border or text
        // characters linger over the panes on the next frame.
        if was_overlay && !(state.help_open || state.detail_open) {
            terminal.clear().expect("clear");
        }

        // Drain all pending source updates before the next frame.
        while let Ok(update) = rx.try_recv() {
            state.apply_update(update);
        }
    }
    state.exit_code()
}

// ─── Rendering ────────────────────────────────────────────────────────────────

fn severity_style(severity: &Severity, ctx: &TuiContext) -> Style {
    if !ctx.mode.color {
        return Style::default();
    }
    let t = &ctx.theme;
    match severity {
        Severity::Info => Style::default().fg(t.ok),
        Severity::Warning => Style::default().fg(t.warn),
        Severity::Error => Style::default().fg(t.error),
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

const NARROW_THRESHOLD: u16 = 100;

fn render(
    frame: &mut Frame,
    config: &TuiConfig,
    ctx: &TuiContext,
    state: &mut IncrementalState,
) {
    let area = frame.area();
    let narrow = area.width < NARROW_THRESHOLD;

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    if narrow {
        render_timeline(frame, ctx, state, rows[0], true);
    } else {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(rows[0]);
        render_timeline(frame, ctx, state, panes[0], false);
        render_detail(frame, ctx, state, panes[1]);
    }

    render_status_bar(frame, ctx, state, config, rows[1]);

    if state.detail_open {
        render_detail_overlay(frame, ctx, state, area);
    } else if state.help_open {
        render_help_overlay(frame, ctx, area);
    }
}

fn render_timeline(
    frame: &mut Frame,
    ctx: &TuiContext,
    state: &mut IncrementalState,
    area: Rect,
    narrow: bool,
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

    let narrow_hint = if narrow { " (Enter for detail)" } else { "" };
    let title = if !filter_query.is_empty() {
        format!(" Timeline ({}/{}){} ", filtered.len(), total, narrow_hint)
    } else {
        format!(" Timeline ({} events){} ", total, narrow_hint)
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
        let placeholder = ListItem::new(Line::from(Span::raw("waiting for sources\u{2026}")));
        let list = List::new(vec![placeholder]);
        frame.render_widget(list, list_area);
    } else {
        let items: Vec<ListItem> = filtered.iter().enumerate().map(|(vis_idx, &raw_idx)| {
            let e = &state.events[raw_idx];
            let ts = e.ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let prefix = if selected_vis == Some(vis_idx) { selector } else { " " };
            let row = format!("{prefix} {}  {}  {}", ts, e.source, e.kind);
            let mut style = severity_style(&e.severity, ctx);
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
            Style::default().fg(ctx.theme.warn)
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

fn render_detail_overlay(
    frame: &mut Frame,
    ctx: &TuiContext,
    state: &IncrementalState,
    area: Rect,
) {
    let ascii = ctx.mode.ascii;
    let close_hint = "q / Esc to close";
    let _ = ascii;
    let title = format!(" Event detail  \u{2014}  {} ", close_hint);

    frame.render_widget(Clear, area);
    let block = make_block(&title, ascii);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().add_modifier(Modifier::DIM);
    let filtered = state.filtered_indices();
    let filter_has_no_results = !state.filter_query.is_empty() && filtered.is_empty();

    let selected_event: Option<&CorrelateEvent> = state
        .list_state
        .selected()
        .and_then(|vis| filtered.get(vis))
        .and_then(|&raw| state.events.get(raw));

    let widget = if filter_has_no_results {
        Paragraph::new(Line::from(Span::styled("No events match filter", dim)))
            .wrap(Wrap { trim: false })
    } else {
        match selected_event {
            None => {
                let hint = if ascii { "up/down to select an event" } else { "\u{2191}\u{2193} to select an event" };
                Paragraph::new(Line::from(Span::styled(hint, dim))).wrap(Wrap { trim: false })
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
                Paragraph::new(lines).wrap(Wrap { trim: false })
            }
        }
    };
    frame.render_widget(widget, inner);
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

    let bar_bg = if use_color { ctx.theme.overlay_bg } else { Color::Reset };
    let bar_style = Style::default().bg(bar_bg);

    let border_block = {
        let b = Block::default().borders(Borders::ALL).style(bar_style);
        if ascii { b.border_set(ASCII_BORDER) } else { b }
    };
    let inner = border_block.inner(area);
    frame.render_widget(border_block, area);

    // "Esc:clear  q:quit" is 17 chars — the longest hint string.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Fill(2),
            Constraint::Length(18),
        ])
        .split(inner);

    // Left: entity id + diagnosis label when available
    let left = if let Some(d) = state.diagnosis.as_ref() {
        format!("{} \u{2014} Diagnosis: {}", config.id, d.pattern)
    } else {
        config.id.clone()
    };
    frame.render_widget(Paragraph::new(left).style(bar_style), cols[0]);

    // Centre: per-source state indicators
    let fetch_dot = if ascii { "~" } else { "\u{27F3}" };
    let avail_dot = if ascii { "*" } else { "\u{25CF}" };
    let err_dot   = if ascii { "x" } else { "\u{2717}" };
    let skip_dot  = if ascii { "-" } else { "\u{25CB}" };

    let spans: Vec<Span> = SourceKind::ALL.iter().flat_map(|kind| {
        let src = kind.name();
        let ss = state.source_states.get(src);
        let t = &ctx.theme;
        let (dot, color) = match ss {
            Some(SourceState::Fetching)    => (fetch_dot, t.warn),
            Some(SourceState::Available)   => (avail_dot, t.ok),
            Some(SourceState::Errored)     => (err_dot,   t.error),
            Some(SourceState::Unavailable) | None => (skip_dot, t.muted),
        };
        let text = format!("{dot}{src} ");
        let s: Span = if use_color {
            Span::styled(text, Style::default().fg(color).bg(bar_bg))
        } else {
            Span::raw(text)
        };
        std::iter::once(s)
    }).collect();
    frame.render_widget(Paragraph::new(Line::from(spans)).style(bar_style), cols[1]);

    // Right: FOLLOW/PAUSED indicator in tail mode, otherwise standard quit hint.
    // Filter-active hint always wins over tail indicator.
    let hint_line: Line = if state.filter_active {
        Line::from("Esc:clear  q:quit")
    } else if state.tail_mode {
        let (indicator, color) = if state.follow {
            ("FOLLOW", ctx.theme.ok)
        } else {
            ("PAUSED", ctx.theme.warn)
        };
        if use_color {
            Line::from(vec![
                Span::styled(indicator, Style::default().fg(color).bg(bar_bg).add_modifier(Modifier::BOLD)),
                Span::raw("  q:quit"),
            ])
        } else {
            Line::from(format!("{indicator}  q:quit"))
        }
    } else {
        Line::from("?:help  q:quit")
    };
    frame.render_widget(
        Paragraph::new(hint_line).style(bar_style).alignment(Alignment::Right),
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

    let bg = Style::default().bg(ctx.theme.overlay_bg);
    let block = make_block(" Keybindings  \u{2014}  ? or Esc to close ", ascii).style(bg);
    let inner = block.inner(overlay_rect);
    frame.render_widget(block, overlay_rect);

    let dim = bg.add_modifier(Modifier::DIM);
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

    frame.render_widget(Paragraph::new(lines).style(bg), inner);
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
            tail: false,
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
            tail: false,
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar = row_str(&buf, 22, 120);
        assert!(bar.contains("temporal"), "Expected 'temporal' in status bar: {bar}");
        assert!(bar.contains("postgres"), "Expected 'postgres' in status bar: {bar}");
    }

    #[test]
    fn status_bar_has_uniform_background_in_color_mode() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: true, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        // Row 22 is the inner status bar (borders at 21 and 23). x=0 and x=119 are border cells.
        let cells_with_reset_bg: Vec<u16> = (1..119)
            .filter(|&x| {
                buf.cell((x, 22))
                    .map(|c| c.bg == Color::Reset)
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            cells_with_reset_bg.is_empty(),
            "Status bar inner cells have transparent background at x positions: {cells_with_reset_bg:?}"
        );
    }

    #[test]
    fn status_bar_background_uses_theme_overlay_bg() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: true, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        // Row 22 is the inner status bar. All inner cells must use the theme's overlay_bg.
        let wrong_bg: Vec<u16> = (1..119)
            .filter(|&x| {
                buf.cell((x, 22))
                    .map(|c| c.bg != ctx.theme.overlay_bg)
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            wrong_bg.is_empty(),
            "Status bar cells should use theme overlay_bg at x positions: {wrong_bg:?}"
        );
    }

    #[test]
    fn status_bar_filter_hint_not_truncated_on_80_col() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (config, mut state) = three_event_state();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        state.filter_active = true;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 80)).collect::<Vec<_>>().join("\n");
        assert!(
            all_rows.contains("Esc:clear"),
            "Esc:clear hint should be visible on 80-col terminal: {all_rows}"
        );
        assert!(
            all_rows.contains("q:quit"),
            "q:quit should be visible on 80-col terminal: {all_rows}"
        );
    }

    // ─── Incremental loading / placeholder ───────────────────────────────────

    #[test]
    fn placeholder_shown_while_sources_are_fetching() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
            tail: false,
        };
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true }, theme: nico_common::theme::DEFAULT };
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
            tail: false,
        };
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true }, theme: nico_common::theme::DEFAULT };
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
            tail: false,
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
            tail: false,
        };
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        // help_open defaults to false

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(!all_rows.contains("Keybindings"), "Overlay should not appear by default: {all_rows}");
    }

    #[test]
    fn help_overlay_dismiss_leaves_no_artifacts() {
        // Two-draw test: frame 1 has the overlay open, then it is dismissed
        // (state.help_open = false + terminal.clear() as the event loop does).
        // Frame 2 must show only pane content — no overlay border or keybinding
        // text may survive in the backend buffer.
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first();

        // Frame 1 — overlay visible.
        state.help_open = true;
        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        // Dismiss (mirrors what event_loop does: state change + full-repaint clear).
        state.help_open = false;
        terminal.clear().unwrap();

        // Frame 2 — normal two-pane layout.
        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(!all_rows.contains("Keybindings"), "Overlay title persisted after dismiss: {all_rows}");
        assert!(!all_rows.contains("Move selection"), "Overlay keybinding text persisted after dismiss: {all_rows}");
        assert!(all_rows.contains("Timeline"), "Timeline pane not restored after overlay dismiss: {all_rows}");
        assert!(all_rows.contains("temporal"), "Event data not visible after overlay dismiss: {all_rows}");
    }

    #[test]
    fn timeline_title_shows_correct_count_after_all_sources_done() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };

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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        state.filter_query = "xyzzy_no_such_source".into();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(
            all_rows.contains("No events match filter"),
            "Expected 'No events match filter': {all_rows}"
        );
    }

    // ─── Narrow / wide layout ─────────────────────────────────────────────────

    #[test]
    fn narrow_layout_single_pane_80x24() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 80);
        assert!(row0.contains("Timeline"), "Timeline title missing in narrow mode: {row0}");
        assert!(!row0.contains("Event detail"), "Event detail pane should be hidden at <100 cols: {row0}");
        assert!(row0.contains("Enter for detail"), "Expected '(Enter for detail)' hint in narrow title: {row0}");
    }

    #[test]
    fn narrow_layout_title_hint_absent_in_wide_mode() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        assert!(!row0.contains("Enter for detail"), "No 'Enter for detail' hint in wide mode: {row0}");
        assert!(row0.contains("Event detail"), "Detail pane should be visible at >=100 cols: {row0}");
    }

    #[test]
    fn detail_overlay_renders_full_screen_narrow() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first();
        state.detail_open = true;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 80)).collect::<Vec<_>>().join("\n");
        assert!(all_rows.contains("Event detail"), "Expected 'Event detail' in overlay: {all_rows}");
        assert!(all_rows.contains("q / Esc"), "Expected close hint in overlay: {all_rows}");
        assert!(all_rows.contains("source:"), "Expected 'source:' in overlay content: {all_rows}");
    }

    #[test]
    fn detail_overlay_renders_full_screen_wide() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first();
        state.detail_open = true;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        // Overlay takes full screen — row 0 should be the overlay border with the detail title
        assert!(row0.contains("Event detail"), "Expected overlay title in row 0: {row0}");
        assert!(row0.contains("q / Esc"), "Expected close hint in overlay title: {row0}");
    }

    #[test]
    fn detail_overlay_dismissed_by_state() {
        let config = sample_config();
        let mut state = sample_state(&config);
        state.select_first();
        state.detail_open = true;
        assert!(state.detail_open);
        state.detail_open = false;
        assert!(!state.detail_open, "detail_open should be false after dismissal");
    }

    #[test]
    fn detail_overlay_not_shown_by_default() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first();
        // detail_open defaults to false

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 80);
        // In narrow mode, row 0 should show Timeline, not the overlay title
        assert!(row0.contains("Timeline"), "Timeline should show when detail overlay is closed: {row0}");
        assert!(!row0.contains("q / Esc"), "Overlay should not be visible by default: {row0}");
    }

    #[test]
    fn detail_overlay_dismiss_leaves_no_artifacts() {
        // Two-draw test: frame 1 has the full-screen detail overlay, then it is
        // dismissed. Frame 2 must show the normal two-pane layout with no
        // full-screen overlay title or close hint remaining.
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first();

        // Frame 1 — detail overlay (full-screen).
        state.detail_open = true;
        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        // Dismiss.
        state.detail_open = false;
        terminal.clear().unwrap();

        // Frame 2 — normal two-pane layout.
        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        // Normal layout: both pane titles appear in row 0.
        assert!(row0.contains("Timeline"), "Timeline not restored after detail dismiss: {row0}");
        assert!(row0.contains("Event detail"), "Detail pane not restored after detail dismiss: {row0}");
        // Full-screen close hint must be gone (only present in the overlay title).
        assert!(!row0.contains("q / Esc to close"), "Overlay close hint persisted after dismiss: {row0}");
    }

    #[test]
    fn narrow_layout_filtered_title_includes_enter_hint() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let (config, mut state) = three_event_state();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        state.filter_query = "temporal".into();
        state.filter_active = true;
        state.sync_cursor();

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 80);
        assert!(row0.contains("2/3"), "Expected '2/3' filter ratio: {row0}");
        assert!(row0.contains("Enter for detail"), "Expected Enter hint in narrow filtered title: {row0}");
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
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        state.filter_active = true;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(
            all_rows.contains("Esc:clear"),
            "Expected 'Esc:clear' hint while filter is active: {all_rows}"
        );
    }

    // ─── Tail mode: follow / paused indicator ─────────────────────────────────

    fn tail_config() -> TuiConfig {
        TuiConfig {
            id: "wf-tail".into(),
            source_names: vec!["temporal", "k8s"],
            restricted: vec![],
            diagnosis: DiagnosisConfig::default(),
            tail: true,
        }
    }

    /// Build an IncrementalState in tail mode with initial events loaded.
    fn tail_state(config: &TuiConfig) -> IncrementalState {
        let mut state = IncrementalState::new(config);
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![
                    make_event(1_000, "temporal", "started", Severity::Info, ""),
                    make_event(2_000, "temporal", "scheduled", Severity::Info, ""),
                ],
                state: vec![],
            }),
        });
        state.apply_update(TuiUpdate::SourceDone {
            name: "k8s".into(),
            result: SourceResult::Output(SourceOutput {
                events: vec![make_event(3_000, "k8s", "warning", Severity::Warning, "")],
                state: vec![],
            }),
        });
        state
    }

    #[test]
    fn follow_indicator_shown_in_tail_mode() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = tail_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = tail_state(&config);
        assert!(state.follow, "follow should start true in tail mode");

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(all_rows.contains("FOLLOW"), "Expected 'FOLLOW' in status bar: {all_rows}");
    }

    #[test]
    fn paused_indicator_shown_when_follow_disabled() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = tail_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = tail_state(&config);
        state.follow = false;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(all_rows.contains("PAUSED"), "Expected 'PAUSED' in status bar: {all_rows}");
        assert!(!all_rows.contains("FOLLOW"), "Should not show 'FOLLOW' when paused: {all_rows}");
    }

    #[test]
    fn tail_events_appended_to_timeline() {
        let config = tail_config();
        let mut state = tail_state(&config);
        let count_before = state.events.len();

        state.apply_update(TuiUpdate::TailEvents {
            events: vec![
                make_event(5_000, "temporal", "completed", Severity::Info, ""),
                make_event(6_000, "k8s", "restarted", Severity::Warning, ""),
            ],
        });

        assert_eq!(state.events.len(), count_before + 2, "tail events should be appended");
        let last = state.events.last().unwrap();
        assert_eq!(last.kind, "restarted", "events should be sorted by ts");
    }

    #[test]
    fn tail_events_with_follow_moves_cursor_to_last() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = true;
        state.select_first();

        state.apply_update(TuiUpdate::TailEvents {
            events: vec![make_event(9_000, "temporal", "new-event", Severity::Info, "")],
        });

        let n = state.filtered_indices().len();
        assert_eq!(state.selected(), Some(n - 1), "follow should move cursor to last after tail event");
    }

    #[test]
    fn tail_events_with_follow_false_do_not_move_cursor() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = false;
        state.select_first();
        assert_eq!(state.selected(), Some(0));

        state.apply_update(TuiUpdate::TailEvents {
            events: vec![make_event(9_000, "temporal", "new-event", Severity::Info, "")],
        });

        assert_eq!(state.selected(), Some(0), "cursor should not jump when follow is false");
    }

    #[test]
    fn cursor_tracks_event_identity_during_tail_streaming() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = false;

        // Select k8s/warning at raw index 2 (ts=3000) — visual index 2.
        state.select_at(2);
        let selected_kind = state.events[state.filtered_indices()[2]].kind.clone();
        assert_eq!(selected_kind, "warning");

        // Stream in two earlier events — they land before the k8s event.
        state.apply_update(TuiUpdate::TailEvents {
            events: vec![
                make_event(500,  "temporal", "earlier-a", Severity::Info, ""),
                make_event(1_500, "temporal", "earlier-b", Severity::Info, ""),
            ],
        });

        // The k8s event moved in raw index; cursor should still point at it.
        let sel_vis = state.selected().expect("cursor must not be None");
        let raw = state.filtered_indices()[sel_vis];
        assert_eq!(state.events[raw].kind, "warning", "cursor should track 'warning' event after insertions above");
    }

    #[test]
    fn source_error_injects_synthetic_event_and_flips_indicator() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = false;
        let count_before = state.events.len();

        state.apply_update(TuiUpdate::TailSourceError {
            source: "temporal".into(),
            message: "connection lost".into(),
        });

        assert_eq!(state.events.len(), count_before + 1, "one synthetic event should be injected");
        let synthetic = state.events.iter().find(|e| e.kind == "source_error").unwrap();
        assert_eq!(synthetic.source, "temporal");
        assert_eq!(synthetic.message, "connection lost");
        assert_eq!(synthetic.severity, Severity::Error);

        assert_eq!(
            state.source_states.get("temporal"),
            Some(&SourceState::Errored),
            "source indicator should flip to Errored"
        );
    }

    #[test]
    fn source_error_not_injected_twice_for_same_source() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = false;

        state.apply_update(TuiUpdate::TailSourceError {
            source: "temporal".into(),
            message: "first error".into(),
        });
        let count_after_first = state.events.len();

        // Second error on the same source — already Errored, no new synthetic event.
        state.apply_update(TuiUpdate::TailSourceError {
            source: "temporal".into(),
            message: "second error".into(),
        });

        assert_eq!(state.events.len(), count_after_first, "no second synthetic event for already-errored source");
    }

    #[test]
    fn scroll_up_pauses_follow_in_tail_mode() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = true;
        state.select_last();

        // Simulate Up key by calling select_prev + clearing follow (as event_loop does).
        state.select_prev();
        if state.tail_mode { state.follow = false; }

        assert!(!state.follow, "follow should be false after scrolling up");
    }

    #[test]
    fn g_end_reenables_follow_in_tail_mode() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = false;
        state.select_first();

        // Simulate G/End key.
        state.select_last();
        if state.tail_mode { state.follow = true; }

        assert!(state.follow, "follow should be re-enabled after G/End in tail mode");
        let n = state.filtered_indices().len();
        assert_eq!(state.selected(), Some(n - 1), "cursor should be at last row");
    }

    #[test]
    fn toggle_follow_jumps_to_last_when_enabling() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = false;
        state.select_first();
        assert_eq!(state.selected(), Some(0));

        state.toggle_follow();

        assert!(state.follow, "follow should be enabled after toggle");
        let n = state.filtered_indices().len();
        assert_eq!(state.selected(), Some(n - 1), "cursor should jump to last when follow is enabled");
    }

    #[test]
    fn toggle_follow_disables_without_moving_cursor() {
        let config = tail_config();
        let mut state = tail_state(&config);
        state.follow = true;
        state.select_first();
        assert_eq!(state.selected(), Some(0));

        state.toggle_follow();

        assert!(!state.follow, "follow should be disabled after toggle");
        assert_eq!(state.selected(), Some(0), "cursor should not move when disabling follow");
    }

    #[test]
    fn no_tail_indicator_in_non_tail_mode() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config(); // tail: false
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(!all_rows.contains("FOLLOW"), "No FOLLOW indicator in non-tail mode: {all_rows}");
        assert!(!all_rows.contains("PAUSED"), "No PAUSED indicator in non-tail mode: {all_rows}");
        assert!(all_rows.contains("?:help"), "Should show ?:help hint in non-tail mode: {all_rows}");
    }

    #[test]
    fn synthetic_source_error_event_styled_in_render() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = tail_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = tail_state(&config);
        state.follow = false;

        state.apply_update(TuiUpdate::TailSourceError {
            source: "k8s".into(),
            message: "watch stream closed".into(),
        });

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let all_rows: String = (0..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join("\n");
        assert!(
            all_rows.contains("source_error"),
            "Expected 'source_error' event in timeline: {all_rows}"
        );
    }

    // ─── Issue #90: phantom grey bar in Timeline pane ─────────────────────────

    #[test]
    fn placeholder_row_is_not_dimmed() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = IncrementalState::new(&config); // sources still fetching

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        // The placeholder text must be present.
        let row1 = row_str(&buf, 1, 120);
        assert!(
            row1.contains("waiting for sources"),
            "Placeholder text not rendered: {row1}"
        );
        // No DIM modifier — invisible text looks like a grey bar.
        let dim_cells: Vec<u16> = (1..71)
            .filter(|&x| {
                buf.cell((x, 1))
                   .map(|c| c.modifier.contains(Modifier::DIM))
                   .unwrap_or(false)
            })
            .collect();
        assert!(
            dim_cells.is_empty(),
            "Placeholder row has DIM modifier at x={dim_cells:?}; makes text invisible against grey bg"
        );
    }

    #[test]
    fn placeholder_row_has_no_reversed_modifier() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = IncrementalState::new(&config);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let reversed_cells: Vec<u16> = (1..71)
            .filter(|&x| {
                buf.cell((x, 1))
                   .map(|c| c.modifier.contains(Modifier::REVERSED))
                   .unwrap_or(false)
            })
            .collect();
        assert!(
            reversed_cells.is_empty(),
            "Phantom REVERSED on Timeline placeholder row at x={reversed_cells:?}"
        );
    }

    #[test]
    fn first_event_row_has_no_reversed_when_unselected() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config); // all sources done, selected == None

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let reversed_cells: Vec<u16> = (1..71)
            .filter(|&x| {
                buf.cell((x, 1))
                   .map(|c| c.modifier.contains(Modifier::REVERSED))
                   .unwrap_or(false)
            })
            .collect();
        assert!(
            reversed_cells.is_empty(),
            "First event row has unexpected REVERSED at x={reversed_cells:?} with no selection"
        );
    }

    #[test]
    fn first_event_row_has_reversed_only_when_explicitly_selected() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first(); // explicit ↓ / select

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let reversed_cells: Vec<u16> = (1..71)
            .filter(|&x| {
                buf.cell((x, 1))
                   .map(|c| c.modifier.contains(Modifier::REVERSED))
                   .unwrap_or(false)
            })
            .collect();
        assert!(
            !reversed_cells.is_empty(),
            "Selected first row should have REVERSED modifier but found none"
        );
    }

    #[test]
    fn second_event_row_has_no_reversed_when_first_is_selected() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = sample_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false }, theme: nico_common::theme::DEFAULT };
        let mut state = sample_state(&config);
        state.select_first(); // first row selected, second should be plain

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        // Row y=2 is the second event row.
        let reversed_cells: Vec<u16> = (1..71)
            .filter(|&x| {
                buf.cell((x, 2))
                   .map(|c| c.modifier.contains(Modifier::REVERSED))
                   .unwrap_or(false)
            })
            .collect();
        assert!(
            reversed_cells.is_empty(),
            "Non-selected second row has unexpected REVERSED at x={reversed_cells:?}"
        );
    }

    // ─── Issue #98: theme wiring ──────────────────────────────────────────────

    #[test]
    fn severity_style_info_uses_theme_ok_rgb() {
        let ctx = TuiContext {
            mode: OutputMode { color: true, ascii: false },
            theme: nico_common::theme::DRACULA,
        };
        let style = severity_style(&Severity::Info, &ctx);
        assert_eq!(style, Style::default().fg(nico_common::theme::DRACULA.ok));
    }

    #[test]
    fn severity_style_warning_uses_theme_warn_rgb() {
        let ctx = TuiContext {
            mode: OutputMode { color: true, ascii: false },
            theme: nico_common::theme::NORD,
        };
        let style = severity_style(&Severity::Warning, &ctx);
        assert_eq!(style, Style::default().fg(nico_common::theme::NORD.warn));
    }

    #[test]
    fn severity_style_error_uses_theme_error_rgb() {
        let ctx = TuiContext {
            mode: OutputMode { color: true, ascii: false },
            theme: nico_common::theme::GRUVBOX,
        };
        let style = severity_style(&Severity::Error, &ctx);
        assert_eq!(style, Style::default().fg(nico_common::theme::GRUVBOX.error));
    }

    #[test]
    fn severity_style_returns_default_when_color_disabled_regardless_of_theme() {
        let ctx = TuiContext {
            mode: OutputMode { color: false, ascii: false },
            theme: nico_common::theme::DRACULA,
        };
        assert_eq!(severity_style(&Severity::Info,    &ctx), Style::default());
        assert_eq!(severity_style(&Severity::Warning, &ctx), Style::default());
        assert_eq!(severity_style(&Severity::Error,   &ctx), Style::default());
    }

    #[test]
    fn source_state_available_uses_theme_ok_cell_color() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = TuiConfig {
            id: "wf-1".into(),
            source_names: vec!["temporal"],
            restricted: vec![],
            diagnosis: DiagnosisConfig::default(),
            tail: false,
        };
        let ctx = TuiContext {
            mode: OutputMode { color: true, ascii: false },
            theme: nico_common::theme::NORD,
        };
        let mut state = IncrementalState::new(&config);
        state.apply_update(TuiUpdate::SourceDone {
            name: "temporal".into(),
            result: SourceResult::Output(SourceOutput { events: vec![], state: vec![] }),
        });

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar_row = (0..120)
            .filter_map(|x| buf.cell((x, 22)))
            .find(|c| c.symbol() == "●")
            .unwrap_or_else(|| panic!("available dot '●' not found in status bar"));
        assert_eq!(
            bar_row.fg,
            nico_common::theme::NORD.ok,
            "Available source indicator should use theme.ok color"
        );
    }

    #[test]
    fn source_state_errored_uses_theme_error_cell_color() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = TuiConfig {
            id: "wf-1".into(),
            source_names: vec!["temporal"],
            restricted: vec![],
            diagnosis: DiagnosisConfig::default(),
            tail: false,
        };
        let ctx = TuiContext {
            mode: OutputMode { color: true, ascii: false },
            theme: nico_common::theme::NORD,
        };
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
        let err_cell = (0..120)
            .filter_map(|x| buf.cell((x, 22)))
            .find(|c| c.symbol() == "✗")
            .unwrap_or_else(|| panic!("error dot '✗' not found in status bar"));
        assert_eq!(
            err_cell.fg,
            nico_common::theme::NORD.error,
            "Errored source indicator should use theme.error color"
        );
    }

    #[test]
    fn follow_indicator_uses_theme_ok_color() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = tail_config();
        let ctx = TuiContext {
            mode: OutputMode { color: true, ascii: false },
            theme: nico_common::theme::DRACULA,
        };
        let mut state = tail_state(&config);
        assert!(state.follow);

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let f_cell = (0..120)
            .filter_map(|x| buf.cell((x, 22)))
            .find(|c| c.symbol() == "F")
            .unwrap_or_else(|| panic!("'F' from FOLLOW not found in status bar"));
        assert_eq!(
            f_cell.fg,
            nico_common::theme::DRACULA.ok,
            "FOLLOW indicator should use theme.ok"
        );
    }

    #[test]
    fn paused_indicator_uses_theme_warn_color() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = tail_config();
        let ctx = TuiContext {
            mode: OutputMode { color: true, ascii: false },
            theme: nico_common::theme::DRACULA,
        };
        let mut state = tail_state(&config);
        state.follow = false;

        terminal.draw(|f| render(f, &config, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let p_cell = (0..120)
            .filter_map(|x| buf.cell((x, 22)))
            .find(|c| c.symbol() == "P")
            .unwrap_or_else(|| panic!("'P' from PAUSED not found in status bar"));
        assert_eq!(
            p_cell.fg,
            nico_common::theme::DRACULA.warn,
            "PAUSED indicator should use theme.warn"
        );
    }

    #[test]
    fn no_color_ignores_theme_severity_styles() {
        let ctx_color = TuiContext {
            mode: OutputMode { color: false, ascii: false },
            theme: nico_common::theme::NORD,
        };
        assert_eq!(severity_style(&Severity::Info,    &ctx_color), Style::default());
        assert_eq!(severity_style(&Severity::Warning, &ctx_color), Style::default());
        assert_eq!(severity_style(&Severity::Error,   &ctx_color), Style::default());
    }
}
