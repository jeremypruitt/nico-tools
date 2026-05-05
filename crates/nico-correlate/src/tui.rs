use std::io;
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
use crate::source::StateEntry;
use crate::diagnosis::Diagnosis;

pub struct TuiContext {
    pub mode: OutputMode,
}

/// Data collected by the correlate pass, passed into the TUI rendering layer.
#[allow(dead_code)]
pub struct CorrelateOutput {
    pub id: String,
    pub id_type: String,
    pub events: Vec<CorrelateEvent>,
    pub state: Vec<StateEntry>,
    pub diagnosis: Option<Diagnosis>,
    pub restricted: Vec<String>,
    pub unavailable: Vec<String>,
    pub exit_code: i32,
}

/// Install a panic hook that restores the terminal before printing the panic message.
/// Must be called before entering raw mode so a crash does not leave the operator's
/// terminal broken.
pub fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        hook(info);
    }));
}

struct TuiState {
    list_state: ListState,
    event_count: usize,
}

impl TuiState {
    fn new(event_count: usize) -> Self {
        Self { list_state: ListState::default(), event_count }
    }

    fn selected(&self) -> Option<usize> {
        self.list_state.selected()
    }

    fn select_first(&mut self) {
        if self.event_count > 0 {
            self.list_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if self.event_count > 0 {
            self.list_state.select(Some(self.event_count - 1));
        }
    }

    fn select_prev(&mut self) {
        let next = match self.list_state.selected() {
            None | Some(0) => 0,
            Some(n) => n - 1,
        };
        self.list_state.select(Some(next));
    }

    fn select_next(&mut self) {
        if self.event_count == 0 {
            return;
        }
        let next = match self.list_state.selected() {
            None => 0,
            Some(n) => (n + 1).min(self.event_count - 1),
        };
        self.list_state.select(Some(next));
    }

    fn select_prev_page(&mut self, page: usize) {
        let next = match self.list_state.selected() {
            None => 0,
            Some(n) => n.saturating_sub(page),
        };
        self.list_state.select(Some(next));
    }

    fn select_next_page(&mut self, page: usize) {
        if self.event_count == 0 {
            return;
        }
        let next = match self.list_state.selected() {
            None => 0,
            Some(n) => (n + page).min(self.event_count - 1),
        };
        self.list_state.select(Some(next));
    }
}

pub fn run_tui(output: CorrelateOutput, ctx: TuiContext) -> i32 {
    let mut stdout = io::stdout();
    enable_raw_mode().expect("enable raw mode");
    execute!(stdout, EnterAlternateScreen).expect("enter alternate screen");

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("create terminal");
    let mut state = TuiState::new(output.events.len());

    let code = event_loop(&mut terminal, &output, &ctx, &mut state);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    code
}

const PAGE_SIZE: usize = 10;

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    output: &CorrelateOutput,
    ctx: &TuiContext,
    state: &mut TuiState,
) -> i32 {
    loop {
        terminal.draw(|f| render(f, output, ctx, state)).expect("draw");

        if event::poll(std::time::Duration::from_millis(100)).expect("poll")
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
    }
    output.exit_code
}

/// Map event severity to a ratatui `Style`. Returns default (no color) when color is off.
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

/// ASCII border set: `+` corners, `-` top/bottom, `|` sides.
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

fn render(frame: &mut Frame, output: &CorrelateOutput, ctx: &TuiContext, state: &mut TuiState) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(rows[0]);

    render_timeline(frame, output, ctx, state, panes[0]);
    render_detail(frame, output, ctx, state, panes[1]);
    render_status_bar(frame, output, ctx, rows[1]);
}

fn render_timeline(
    frame: &mut Frame,
    output: &CorrelateOutput,
    ctx: &TuiContext,
    state: &mut TuiState,
    area: Rect,
) {
    let ascii = ctx.mode.ascii;
    let use_color = ctx.mode.color;
    let selector = if ascii { ">" } else { "▶" };
    let selected = state.selected();

    let title = format!(" Timeline ({} events) ", output.events.len());
    let block = make_block(&title, ascii);

    let items: Vec<ListItem> = output.events.iter().enumerate().map(|(i, e)| {
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
    output: &CorrelateOutput,
    ctx: &TuiContext,
    state: &TuiState,
    area: Rect,
) {
    let ascii = ctx.mode.ascii;
    let block = make_block(" Event detail ", ascii);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let widget = match state.selected().and_then(|i| output.events.get(i)) {
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
            if let Some(cmd) = output.diagnosis.as_ref().and_then(|d| d.next_commands.first()) {
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
    output: &CorrelateOutput,
    ctx: &TuiContext,
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

    // Left: diagnosis label
    let diag = output.diagnosis.as_ref()
        .map(|d| format!("Diagnosis: {}", d.pattern))
        .unwrap_or_default();
    frame.render_widget(Paragraph::new(diag), cols[0]);

    // Centre: per-source availability indicators
    const ALL_SOURCES: &[&str] = &["temporal", "postgres", "k8s", "loki", "redfish"];
    let avail_dot = if ascii { "*" } else { "●" };
    let skip_dot = if ascii { "-" } else { "○" };
    let err_dot = if ascii { "x" } else { "✗" };

    let spans: Vec<Span> = ALL_SOURCES.iter().flat_map(|&src| {
        let (dot, color) = if output.restricted.iter().any(|r| r == src) {
            (skip_dot, Color::DarkGray)
        } else if output.unavailable.iter().any(|u| u == src) {
            (err_dot, Color::Red)
        } else {
            (avail_dot, Color::Green)
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

    // Right: help/quit hint, right-aligned
    frame.render_widget(
        Paragraph::new("?:help  q:quit").alignment(Alignment::Right),
        cols[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

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

    fn sample_output() -> CorrelateOutput {
        CorrelateOutput {
            id: "wf-abc123".into(),
            id_type: "workflow".into(),
            events: vec![
                make_event(1_746_450_063, "temporal", "started", Severity::Info, "workflow started"),
                make_event(1_746_450_069, "temporal", "activity_failed", Severity::Error, "ProvisionnActivity attempt 3/3"),
                make_event(1_746_450_071, "k8s", "warning", Severity::Warning, "pod restarted"),
            ],
            state: vec![],
            diagnosis: Some(Diagnosis {
                pattern: "activity_retry_exhaustion".into(),
                activity: "ProvisionnActivity".into(),
                error_signature: "Redfish 503".into(),
                next_commands: vec!["tctl workflow describe --workflow-id wf-abc123".into()],
            }),
            restricted: vec![],
            unavailable: vec!["loki".into(), "redfish".into()],
            exit_code: 1,
        }
    }

    fn row_str(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buf.cell((x, y)).map(|c| c.symbol().chars().next().unwrap_or(' ')).unwrap_or(' ')).collect()
    }

    #[test]
    fn two_pane_layout_snapshot_120x24() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let output = sample_output();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = TuiState::new(output.events.len());
        state.select_first();

        terminal.draw(|f| render(f, &output, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        // Row 0: outer borders of the two panes including their titles
        let row0 = row_str(&buf, 0, 120);
        assert!(row0.contains("Timeline"), "Timeline title missing: {row0}");
        assert!(row0.contains("Event detail"), "Event detail title missing: {row0}");

        // The timeline list should show the first event in the list area (rows 1–20)
        let has_temporal = (1..21).any(|y| row_str(&buf, y, 120).contains("temporal"));
        assert!(has_temporal, "Expected 'temporal' in timeline rows");

        // Detail pane should show selected event info
        let has_source_label = (1..21).any(|y| row_str(&buf, y, 120).contains("source:"));
        assert!(has_source_label, "Expected 'source:' label in detail pane");

        // Bottom bar: rows 21–23
        let bar_rows: String = (21..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(bar_rows.contains("q:quit"), "Expected 'q:quit' in bottom bar: {bar_rows}");
        assert!(bar_rows.contains("Diagnosis:"), "Expected 'Diagnosis:' in bottom bar: {bar_rows}");
    }

    #[test]
    fn no_selection_shows_hint() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let output = sample_output();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = TuiState::new(output.events.len());
        // No select_first() — state has no selection

        terminal.draw(|f| render(f, &output, &ctx, &mut state)).unwrap();

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
        let output = sample_output();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true } };
        let mut state = TuiState::new(output.events.len());

        terminal.draw(|f| render(f, &output, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        // ASCII mode: border should use '-' not '─'
        assert!(row0.contains('-'), "Expected ASCII '-' border in row 0: {row0}");
        assert!(!row0.contains('─'), "Expected no box-drawing '─' in ASCII mode: {row0}");
    }

    #[test]
    fn bottom_bar_source_indicators_present() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let output = sample_output();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = TuiState::new(output.events.len());

        terminal.draw(|f| render(f, &output, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar = row_str(&buf, 22, 120);
        assert!(bar.contains("temporal"), "Expected 'temporal' in status bar: {bar}");
        assert!(bar.contains("postgres"), "Expected 'postgres' in status bar: {bar}");
    }

    #[test]
    fn selection_navigation() {
        let mut state = TuiState::new(5);
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

    #[test]
    fn scaffold_renders_without_tty() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let output = CorrelateOutput {
            id: "test-workflow-id".into(),
            id_type: "workflow".into(),
            events: vec![],
            state: vec![],
            diagnosis: None,
            restricted: vec![],
            unavailable: vec![],
            exit_code: 1,
        };
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: false } };
        let mut state = TuiState::new(0);
        terminal.draw(|f| render(f, &output, &ctx, &mut state)).unwrap();
    }

    #[test]
    fn panic_hook_restores_terminal() {
        install_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("tui scaffold test panic"));
    }
}
