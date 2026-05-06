use std::io;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use chrono::{DateTime, Local};
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
use nico_common::output::{OutputMode, Status};
use crate::layer::LayerResult;

pub struct TuiContext {
    pub mode: OutputMode,
}

pub struct TuiConfig {
    pub refresh_interval: Duration,
    pub layer_names: Vec<&'static str>,
}

pub enum TuiUpdate {
    LayerDone { name: String, result: LayerResult },
    RunComplete,
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

// ─── Dashboard state ──────────────────────────────────────────────────────────

struct LayerRow {
    name: String,
    result: Option<LayerResult>,
    fetching: bool,
}

struct DashState {
    layers: Vec<LayerRow>,
    list_state: ListState,
    last_refresh: Option<DateTime<Local>>,
    next_refresh: Option<Instant>,
    refresh_interval: Duration,
    help_open: bool,
    detail_open: bool,
    running: bool,
}

impl DashState {
    fn new(config: &TuiConfig) -> Self {
        let layers = config.layer_names.iter().map(|&name| LayerRow {
            name: name.to_string(),
            result: None,
            fetching: true,
        }).collect();
        Self {
            layers,
            list_state: ListState::default(),
            last_refresh: None,
            next_refresh: None,
            refresh_interval: config.refresh_interval,
            help_open: false,
            detail_open: false,
            running: true,
        }
    }

    fn apply_layer_done(&mut self, name: &str, result: LayerResult) {
        if let Some(row) = self.layers.iter_mut().find(|r| r.name == name) {
            row.result = Some(result);
            row.fetching = false;
        }
    }

    fn apply_run_complete(&mut self) {
        self.running = false;
        self.last_refresh = Some(Local::now());
        self.next_refresh = Some(Instant::now() + self.refresh_interval);
        for row in &mut self.layers {
            row.fetching = false;
        }
    }

    fn start_new_run(&mut self) {
        self.running = true;
        self.next_refresh = None;
        for row in &mut self.layers {
            row.fetching = true;
        }
    }

    fn selected_layer(&self) -> Option<&LayerRow> {
        self.list_state.selected().and_then(|i| self.layers.get(i))
    }

    fn select_prev(&mut self) {
        let n = self.layers.len();
        if n == 0 { return; }
        let next = match self.list_state.selected() {
            None | Some(0) => 0,
            Some(i) => i - 1,
        };
        self.list_state.select(Some(next));
    }

    fn select_next(&mut self) {
        let n = self.layers.len();
        if n == 0 { return; }
        let next = match self.list_state.selected() {
            None => 0,
            Some(i) => (i + 1).min(n - 1),
        };
        self.list_state.select(Some(next));
    }

    fn countdown_secs(&self) -> Option<u64> {
        self.next_refresh.map(|t| {
            let now = Instant::now();
            if now >= t { 0 } else { (t - now).as_secs() }
        })
    }

    fn should_refresh(&self) -> bool {
        if self.running { return false; }
        self.next_refresh.map(|t| Instant::now() >= t).unwrap_or(false)
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn run_tui(
    config: TuiConfig,
    rx: mpsc::Receiver<TuiUpdate>,
    ctx: TuiContext,
    trigger_refresh: Box<dyn Fn() + Send>,
) -> i32 {
    let mut stdout = io::stdout();
    enable_raw_mode().expect("enable raw mode");
    execute!(stdout, EnterAlternateScreen).expect("enter alternate screen");

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("create terminal");
    let mut state = DashState::new(&config);

    let code = event_loop(&mut terminal, &ctx, &mut state, rx, &trigger_refresh);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    code
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    ctx: &TuiContext,
    state: &mut DashState,
    rx: mpsc::Receiver<TuiUpdate>,
    trigger_refresh: &dyn Fn(),
) -> i32 {
    loop {
        terminal.draw(|f| render(f, ctx, state)).expect("draw");

        if state.should_refresh() {
            state.start_new_run();
            trigger_refresh();
        }

        if event::poll(Duration::from_millis(100)).expect("poll") {
            if let Ok(CrosstermEvent::Key(key)) = event::read() {
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
                } else {
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Char('?') => state.help_open = true,
                        KeyCode::Up => state.select_prev(),
                        KeyCode::Down => state.select_next(),
                        KeyCode::Enter => {
                            if state.list_state.selected().is_some() {
                                state.detail_open = true;
                            }
                        }
                        KeyCode::Char('r') => {
                            if !state.running {
                                state.start_new_run();
                                trigger_refresh();
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        while let Ok(update) = rx.try_recv() {
            match update {
                TuiUpdate::LayerDone { name, result } => {
                    state.apply_layer_done(&name, result);
                }
                TuiUpdate::RunComplete => {
                    state.apply_run_complete();
                }
            }
        }
    }
    0
}

// ─── Rendering ────────────────────────────────────────────────────────────────

const NARROW_THRESHOLD: u16 = 100;

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

fn status_color(status: &Status, use_color: bool) -> Style {
    if !use_color { return Style::default(); }
    match status {
        Status::Ok => Style::default().fg(Color::Green),
        Status::Warn => Style::default().fg(Color::Yellow),
        Status::Fail => Style::default().fg(Color::Red),
        Status::Unknown | Status::Skipped => Style::default().fg(Color::DarkGray),
    }
}

fn render(frame: &mut Frame, ctx: &TuiContext, state: &mut DashState) {
    let area = frame.area();
    let narrow = area.width < NARROW_THRESHOLD;

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    if narrow {
        render_layers(frame, ctx, state, rows[0], true);
    } else {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(rows[0]);
        render_layers(frame, ctx, state, panes[0], false);
        render_findings(frame, ctx, state, panes[1]);
    }

    render_bottom_bar(frame, ctx, state, rows[1]);

    if state.detail_open {
        render_detail_overlay(frame, ctx, state, area);
    } else if state.help_open {
        render_help_overlay(frame, ctx, area);
    }
}

fn render_layers(
    frame: &mut Frame,
    ctx: &TuiContext,
    state: &mut DashState,
    area: Rect,
    narrow: bool,
) {
    let ascii = ctx.mode.ascii;
    let use_color = ctx.mode.color;
    let mode = &ctx.mode;
    let selector = if ascii { ">" } else { "▶" };

    let narrow_hint = if narrow { " (Enter for detail)" } else { "" };
    let title = format!(" Layers{} ", narrow_hint);
    let block = make_block(&title, ascii);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let selected = state.list_state.selected();

    let items: Vec<ListItem> = state.layers.iter().enumerate().map(|(i, row)| {
        let prefix = if selected == Some(i) { selector } else { " " };
        let (icon, style) = if row.fetching {
            let fetch = if ascii { "~" } else { "⟳" };
            let s = if use_color {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            (fetch.to_string(), s)
        } else {
            match &row.result {
                Some(r) => {
                    let icon_str = r.status.icon(mode).to_string();
                    let s = status_color(&r.status, use_color);
                    (icon_str, s)
                }
                None => {
                    let skip = if ascii { "." } else { "·" };
                    (skip.to_string(), Style::default().fg(Color::DarkGray))
                }
            }
        };

        let mut row_style = style;
        if selected == Some(i) {
            row_style = row_style.add_modifier(Modifier::REVERSED);
        }

        let text = format!("{prefix} {:<8} {}", icon, row.name);
        ListItem::new(Line::from(Span::styled(text, row_style)))
    }).collect();

    let list = List::new(items);
    frame.render_stateful_widget(list, inner, &mut state.list_state);
}

fn render_findings(frame: &mut Frame, ctx: &TuiContext, state: &DashState, area: Rect) {
    let ascii = ctx.mode.ascii;
    let use_color = ctx.mode.color;
    let mode = &ctx.mode;

    let block = make_block(" Findings ", ascii);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().add_modifier(Modifier::DIM);

    let widget = match state.selected_layer() {
        None => {
            let hint = if ascii {
                "up/down to select a layer"
            } else {
                "↑↓ to select a layer"
            };
            Paragraph::new(Line::from(Span::styled(hint, dim)))
        }
        Some(row) if row.fetching => {
            Paragraph::new(Line::from(Span::styled("running…", dim)))
        }
        Some(row) => {
            match &row.result {
                None => Paragraph::new(Line::from(Span::styled("no data", dim))),
                Some(result) if result.checks.is_empty() => {
                    let msg = match result.status {
                        Status::Skipped => "(skipped)",
                        _ => "(no checks)",
                    };
                    Paragraph::new(Line::from(Span::styled(msg, dim)))
                }
                Some(result) => {
                    let lines: Vec<Line> = result.checks.iter().map(|check| {
                        let icon = check.status.icon(mode);
                        let style = status_color(&check.status, use_color);
                        Line::from(vec![
                            Span::styled(format!("{icon} "), style),
                            Span::raw(format!("{}: {}", check.name, check.value)),
                        ])
                    }).collect();
                    Paragraph::new(lines).wrap(Wrap { trim: false })
                }
            }
        }
    };
    frame.render_widget(widget, inner);
}

fn render_detail_overlay(frame: &mut Frame, ctx: &TuiContext, state: &DashState, area: Rect) {
    let ascii = ctx.mode.ascii;
    let use_color = ctx.mode.color;
    let mode = &ctx.mode;

    let title = " Findings  \u{2014}  q / Esc to close ";
    frame.render_widget(Clear, area);
    let block = make_block(title, ascii);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().add_modifier(Modifier::DIM);

    let widget = match state.selected_layer() {
        None => Paragraph::new(Line::from(Span::styled("no layer selected", dim))),
        Some(row) if row.fetching => {
            Paragraph::new(Line::from(Span::styled("running…", dim)))
        }
        Some(row) => {
            match &row.result {
                None => Paragraph::new(Line::from(Span::styled("no data", dim))),
                Some(result) if result.checks.is_empty() => {
                    let msg = match result.status {
                        Status::Skipped => "(skipped)",
                        _ => "(no checks)",
                    };
                    Paragraph::new(Line::from(Span::styled(msg, dim)))
                }
                Some(result) => {
                    let mut lines: Vec<Line> = vec![
                        Line::from(vec![
                            Span::styled("layer:  ", dim),
                            Span::raw(row.name.clone()),
                        ]),
                        Line::from(vec![
                            Span::styled("status: ", dim),
                            Span::styled(
                                result.status.icon(mode).to_string(),
                                status_color(&result.status, use_color),
                            ),
                        ]),
                        Line::from(""),
                    ];
                    for check in &result.checks {
                        let icon = check.status.icon(mode);
                        let style = status_color(&check.status, use_color);
                        lines.push(Line::from(vec![
                            Span::styled(format!("{icon} "), style),
                            Span::raw(format!("{}: {}", check.name, check.value)),
                        ]));
                        if let Some(cmd) = &check.next_command {
                            lines.push(Line::from(vec![
                                Span::styled("  → ", dim),
                                Span::raw(cmd.clone()),
                            ]));
                        }
                    }
                    Paragraph::new(lines).wrap(Wrap { trim: false })
                }
            }
        }
    };
    frame.render_widget(widget, inner);
}

fn render_bottom_bar(frame: &mut Frame, ctx: &TuiContext, state: &DashState, area: Rect) {
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
            Constraint::Fill(1),
            Constraint::Length(22),
        ])
        .split(inner);

    // Left: last-refresh timestamp
    let left = if let Some(ts) = state.last_refresh {
        format!("last: {}", ts.format("%H:%M:%S"))
    } else if state.running {
        "running…".to_string()
    } else {
        String::new()
    };
    frame.render_widget(Paragraph::new(left), cols[0]);

    // Centre: next-refresh countdown
    let center_text = if state.running {
        String::new()
    } else {
        match state.countdown_secs() {
            Some(secs) => format!("next in {secs}s"),
            None => String::new(),
        }
    };
    frame.render_widget(
        Paragraph::new(center_text).alignment(Alignment::Center),
        cols[1],
    );

    // Right: key hints; running indicator when checks are in-flight
    let hint_line: Line = if state.running {
        let running_style = if use_color {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Line::from(vec![
            Span::styled("RUNNING", running_style),
            Span::raw("  q:quit"),
        ])
    } else {
        Line::from("?:help  r:refresh  q:quit")
    };
    frame.render_widget(
        Paragraph::new(hint_line).alignment(Alignment::Right),
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
    let overlay_rect = center_rect(62, 14, area);

    frame.render_widget(Clear, overlay_rect);

    let block = make_block(" Keybindings  \u{2014}  ? or Esc to close ", ascii);
    let inner = block.inner(overlay_rect);
    frame.render_widget(block, overlay_rect);

    let dim = Style::default().add_modifier(Modifier::DIM);
    let rows: &[(&str, &str)] = &[
        ("\u{2191} / \u{2193}", "Move layer selection"),
        ("Enter",        "Open full-screen Findings overlay"),
        ("r",            "Force immediate refresh"),
        ("?",            "Toggle this help overlay"),
        ("Escape",       "Dismiss overlay"),
        ("q / Ctrl-C",   "Exit"),
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
    use crate::layer::Check;

    fn test_config() -> TuiConfig {
        TuiConfig {
            refresh_interval: Duration::from_secs(30),
            layer_names: vec!["cluster", "logs", "workflows", "health", "grpc", "postgres"],
        }
    }

    fn test_ctx() -> TuiContext {
        TuiContext { mode: OutputMode { color: false, ascii: false } }
    }

    fn ok_result(name: &'static str) -> LayerResult {
        LayerResult {
            name,
            status: Status::Ok,
            checks: vec![
                Check { name: "pods_ready", status: Status::Ok, value: "2/2".into(), next_command: None },
            ],
            duration_ms: 0,
        }
    }

    fn row_str(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().chars().next().unwrap_or(' ')).unwrap_or(' '))
            .collect()
    }

    #[test]
    fn wide_layout_has_layers_and_findings_panes() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = test_ctx();
        let mut state = DashState::new(&config);

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        assert!(row0.contains("Layers"), "Layers pane title missing: {row0}");
        assert!(row0.contains("Findings"), "Findings pane title missing: {row0}");
    }

    #[test]
    fn narrow_layout_shows_enter_hint() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = test_ctx();
        let mut state = DashState::new(&config);

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 80);
        assert!(row0.contains("Enter for detail"), "Narrow hint missing: {row0}");
        // No second pane border — only one pane title on row 0 in narrow mode.
        let pane_titles: Vec<&str> = ["Layers", "Findings"].iter()
            .filter(|&&t| row0.contains(t))
            .copied().collect();
        assert_eq!(pane_titles.len(), 1, "Expected exactly one pane title in narrow mode, got: {pane_titles:?}: {row0}");
    }

    #[test]
    fn layer_rows_visible() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = test_ctx();
        let mut state = DashState::new(&config);

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let body: String = (1..21).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(body.contains("cluster"), "cluster row missing: {body}");
        assert!(body.contains("logs"), "logs row missing: {body}");
        assert!(body.contains("postgres"), "postgres row missing: {body}");
    }

    #[test]
    fn no_selection_shows_hint_in_findings() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = test_ctx();
        let mut state = DashState::new(&config);

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let body: String = (1..21).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(body.contains("to select a layer"), "Selection hint missing: {body}");
    }

    #[test]
    fn selection_shows_findings() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = test_ctx();
        let mut state = DashState::new(&config);
        state.apply_layer_done("cluster", ok_result("cluster"));
        state.list_state.select(Some(0));

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let body: String = (1..21).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(body.contains("pods_ready"), "Check name missing: {body}");
    }

    #[test]
    fn bottom_bar_shows_running_when_in_flight() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = test_ctx();
        let mut state = DashState::new(&config);

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar: String = (21..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(bar.contains("RUNNING"), "RUNNING indicator missing: {bar}");
    }

    #[test]
    fn bottom_bar_shows_last_refresh_and_countdown_after_run() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = test_ctx();
        let mut state = DashState::new(&config);
        state.apply_run_complete();

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let bar: String = (21..24).map(|y| row_str(&buf, y, 120)).collect::<Vec<_>>().join(" ");
        assert!(bar.contains("last:"), "last-refresh timestamp missing: {bar}");
        assert!(bar.contains("next in"), "next-refresh countdown missing: {bar}");
        assert!(bar.contains("r:refresh"), "r:refresh hint missing: {bar}");
    }

    #[test]
    fn ascii_mode_uses_ascii_borders() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let config = test_config();
        let ctx = TuiContext { mode: OutputMode { color: false, ascii: true } };
        let mut state = DashState::new(&config);

        terminal.draw(|f| render(f, &ctx, &mut state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let row0 = row_str(&buf, 0, 120);
        assert!(row0.contains('-'), "Expected ASCII '-' border: {row0}");
        assert!(!row0.contains('─'), "No box-drawing chars in ASCII mode: {row0}");
    }

    #[test]
    fn select_nav_wraps_at_bounds() {
        let config = test_config();
        let mut state = DashState::new(&config);

        state.select_prev();
        assert_eq!(state.list_state.selected(), Some(0));

        for _ in 0..20 {
            state.select_next();
        }
        assert_eq!(state.list_state.selected(), Some(5));
    }

    #[test]
    fn should_refresh_false_while_running() {
        let config = test_config();
        let mut state = DashState::new(&config);
        state.next_refresh = Some(Instant::now() - Duration::from_secs(1));
        assert!(!state.should_refresh(), "should not refresh while running=true");
    }

    #[test]
    fn should_refresh_true_when_timer_elapsed() {
        let config = test_config();
        let mut state = DashState::new(&config);
        state.apply_run_complete();
        state.next_refresh = Some(Instant::now() - Duration::from_secs(1));
        assert!(state.should_refresh());
    }
}
