use chrono::{DateTime, Local};
use nico_common::output::Status;
use nico_common::theme::Theme;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use nico_doctor::baseline::Delta;

use crate::app::App;
use crate::events::Overlay;
use crate::model::{Finding, overall_verdict};
use crate::widgets::{breadcrumb_verdicts, sparkline_for_layer};

/// How many recent verdicts the header breadcrumb shows. The ring buffer
/// caps at `RING_CAPACITY` (32); the breadcrumb shows a smaller window so
/// it stays glanceable at narrow terminal widths.
pub const BREADCRUMB_CAP: usize = 10;

/// Glyph used for each entry in the header breadcrumb. Color carries the
/// verdict — flat shape keeps the strip scannable at any width.
pub const BREADCRUMB_GLYPH: &str = "■";

/// Maximum number of data points rendered in a per-scorecard sparkline.
/// Capped so very wide terminals don't paint a visually dense strip; the
/// renderer also clamps to the available cell width.
pub const SPARKLINE_MAX: usize = 24;

pub const HELP_LINES: &[&str] = &[
    "R         refresh",
    "Space     pause / resume auto-refresh",
    "↑↓←→/hjkl move focus",
    "Enter     open detail",
    "Esc       close overlay",
    "?         this help",
    "q / ^C    quit",
];

/// Top-level render. The host loop calls this once per dirty frame.
pub fn render(app: &App, theme: &Theme, frame: &mut Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(7),    // body grid
            Constraint::Min(5),    // drill panel
            Constraint::Length(1), // hint bar
        ])
        .split(area);

    render_header(app, theme, frame, chunks[0]);
    render_grid(app, theme, frame, chunks[1]);
    render_drill(app, theme, frame, chunks[2]);
    render_hint_bar(app, theme, frame, chunks[3]);

    match app.overlay() {
        Overlay::Detail => render_detail_overlay(app, theme, frame, area),
        Overlay::Help => render_help_overlay(theme, frame, area),
        Overlay::None => {}
    }
}

fn render_header(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let snapshots = app.snapshots();
    let verdict = overall_verdict(snapshots);

    let mut spans: Vec<Span> = Vec::new();
    if snapshots.is_empty() {
        spans.push(Span::styled("loading…", Style::default().fg(theme.muted)));
    } else {
        for s in snapshots {
            spans.push(Span::styled(
                pip_glyph(&s.status).to_string(),
                Style::default().fg(theme_color(theme, &s.status)),
            ));
            spans.push(Span::raw(" "));
        }
    }
    spans.push(Span::raw("    "));
    spans.push(Span::styled(
        verdict_word(&verdict).to_string(),
        Style::default()
            .fg(theme_color(theme, &verdict))
            .add_modifier(Modifier::BOLD),
    ));

    let crumbs = breadcrumb_verdicts(app.history(), BREADCRUMB_CAP);
    if !crumbs.is_empty() {
        spans.push(Span::raw("    "));
        for v in &crumbs {
            spans.push(Span::styled(
                BREADCRUMB_GLYPH.to_string(),
                Style::default().fg(theme_color(theme, v)),
            ));
        }
    }

    let timestamp = match (app.last_refreshed(), app.refreshing()) {
        (_, true) => "refreshing…".to_string(),
        (Some(t), false) => format!("refreshed {}", format_time(&t)),
        (None, false) => "—".to_string(),
    };

    let throbber = app.throbber_glyph();
    let title_right = if throbber.is_empty() {
        format!(" {timestamp} ")
    } else {
        format!(" {timestamp}  {throbber} ")
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" nico ops ")
        .title_top(Line::from(title_right).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn render_grid(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let snapshots = app.snapshots();
    if snapshots.is_empty() {
        let block = Block::default().borders(Borders::ALL).title(" layers ");
        frame.render_widget(block, area);
        return;
    }

    let cols = grid_cols_for_width(area.width);
    let rows = snapshots.len().div_ceil(cols);

    let row_constraints: Vec<Constraint> =
        std::iter::repeat_n(Constraint::Length(SCORECARD_ROW_HEIGHT), rows).collect();
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    for r in 0..rows {
        let col_constraints: Vec<Constraint> =
            std::iter::repeat_n(Constraint::Ratio(1, cols as u32), cols).collect();
        let col_areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints)
            .split(row_areas[r]);
        for c in 0..cols {
            let idx = r * 3 + c;
            if idx >= snapshots.len() {
                break;
            }
            let cell = if c < col_areas.len() {
                col_areas[c]
            } else {
                continue;
            };
            render_scorecard(app, idx, theme, frame, cell);
        }
    }
}

/// Row height for one scorecard: top border + evidence line + sparkline +
/// bottom border = 4. The sparkline shares the inner area with the
/// evidence block; the renderer reserves the bottom row for it.
const SCORECARD_ROW_HEIGHT: u16 = 5;

fn render_scorecard(app: &App, idx: usize, theme: &Theme, frame: &mut Frame, area: Rect) {
    let snapshots = app.snapshots();
    let snap = &snapshots[idx];
    let focused = idx == app.focus();
    let badge = delta_badge(app.deltas().get(&snap.name));
    let mut title_spans: Vec<Span> = Vec::with_capacity(4);
    title_spans.push(Span::raw(if focused {
        format!("▶ {} ", snap.name)
    } else {
        format!(" {} ", snap.name)
    }));
    if let Some((label, palette)) = badge {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            format!(" {label} "),
            Style::default()
                .fg(theme_color(theme, &palette))
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
        title_spans.push(Span::raw(" "));
    }
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(title_spans));
    if focused {
        block = block.border_style(
            Style::default()
                .fg(theme.overlay_key)
                .add_modifier(Modifier::BOLD),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let mut pip_style = Style::default().fg(theme_color(theme, &snap.status));
    if app.pulse_active(&snap.name) {
        pip_style = pip_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    let line = Line::from(vec![
        Span::styled(format!("{} ", pip_glyph(&snap.status)), pip_style),
        Span::raw(snap.evidence.clone()),
    ]);
    frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), chunks[0]);

    let spark_width = (chunks[1].width as usize).min(SPARKLINE_MAX);
    let sparkline = sparkline_for_layer(app.history(), &snap.name, spark_width);
    if !sparkline.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                sparkline,
                Style::default().fg(theme_color(theme, &snap.status)),
            ))),
            chunks[1],
        );
    }
}

fn render_drill(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let title = match app.focused() {
        Some(s) => format!(" findings — {} ", s.name),
        None => " findings ".to_string(),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = match app.focused() {
        Some(s) if !s.findings.is_empty() => finding_lines(&s.findings, theme, false),
        Some(_) => vec![Line::from(Span::styled(
            "no findings",
            Style::default().fg(theme.muted),
        ))],
        None => vec![Line::from(Span::styled(
            "(no layer focused)",
            Style::default().fg(theme.muted),
        ))],
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_hint_bar(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let mut spans: Vec<Span> = vec![Span::styled(
        " R:refresh  Space:pause  hjkl/arrows:focus  Enter:detail  ?:help  q:quit ",
        Style::default().fg(theme.muted),
    )];
    if app.paused() {
        spans.push(Span::styled(
            " PAUSED ",
            Style::default()
                .fg(theme.warn)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_detail_overlay(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let inner_area = centered(area, 80, 80);
    let title = match app.focused() {
        Some(s) => format!(" detail — {} ", s.name),
        None => " detail ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().bg(theme.overlay_bg).fg(theme.overlay_fg));
    let inner = block.inner(inner_area);
    frame.render_widget(Clear, inner_area);
    frame.render_widget(block, inner_area);

    let lines = match app.focused() {
        Some(s) if !s.findings.is_empty() => finding_lines(&s.findings, theme, true),
        Some(_) => vec![Line::from(Span::styled(
            "no findings",
            Style::default().fg(theme.muted),
        ))],
        None => vec![],
    };
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        inner.inner(Margin {
            horizontal: 1,
            vertical: 0,
        }),
    );
}

fn render_help_overlay(theme: &Theme, frame: &mut Frame, area: Rect) {
    let inner_area = centered(area, 60, 50);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" keybinds ")
        .style(Style::default().bg(theme.overlay_bg).fg(theme.overlay_fg));
    let inner = block.inner(inner_area);
    frame.render_widget(Clear, inner_area);
    frame.render_widget(block, inner_area);

    let lines: Vec<Line> = HELP_LINES
        .iter()
        .map(|l| {
            let (key, rest) = split_help_line(l);
            Line::from(vec![
                Span::styled(format!("{key:<10}"), Style::default().fg(theme.overlay_key)),
                Span::raw(rest.to_string()),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines),
        inner.inner(Margin {
            horizontal: 2,
            vertical: 1,
        }),
    );
}

fn finding_lines(findings: &[Finding], theme: &Theme, full: bool) -> Vec<Line<'static>> {
    let limit = if full { findings.len() } else { 3 };
    let mut out: Vec<Line<'static>> = Vec::new();
    for f in findings.iter().take(limit) {
        out.push(Line::from(vec![
            Span::styled(
                format!("{} ", pip_glyph(&f.status)),
                Style::default().fg(theme_color(theme, &f.status)),
            ),
            Span::raw(f.message.clone()),
        ]));
        if let Some(cmd) = &f.next_command {
            out.push(Line::from(Span::styled(
                format!("    next: {cmd}"),
                Style::default().fg(theme.muted),
            )));
        }
    }
    if !full && findings.len() > limit {
        out.push(Line::from(Span::styled(
            format!(
                "    +{} more — Enter for full detail",
                findings.len() - limit
            ),
            Style::default().fg(theme.muted),
        )));
    }
    out
}

/// Map a delta to a (label, palette-key) pair the scorecard title can paint.
/// Returns `None` when no badge should be rendered (i.e. `Delta::Unchanged`
/// or absent baseline).
fn delta_badge(delta: Option<&Delta>) -> Option<(&'static str, Status)> {
    match delta {
        Some(Delta::New) => Some(("NEW", Status::Fail)),
        Some(Delta::Fixed) => Some(("FIXED", Status::Ok)),
        _ => None,
    }
}

pub fn pip_glyph(status: &Status) -> &'static str {
    match status {
        Status::Ok => "●",
        Status::Warn => "▲",
        Status::Fail => "✖",
        Status::Unknown | Status::Skipped => "○",
    }
}

pub fn verdict_word(status: &Status) -> &'static str {
    match status {
        Status::Ok => "OK",
        Status::Warn => "WARN",
        Status::Fail => "FAIL",
        Status::Unknown => "UNKNOWN",
        Status::Skipped => "SKIPPED",
    }
}

fn theme_color(theme: &Theme, status: &Status) -> ratatui::style::Color {
    match status {
        Status::Ok => theme.ok,
        Status::Warn => theme.warn,
        Status::Fail => theme.error,
        Status::Unknown | Status::Skipped => theme.muted,
    }
}

fn format_time(t: &DateTime<Local>) -> String {
    t.format("%H:%M:%S").to_string()
}

fn split_help_line(line: &str) -> (&str, &str) {
    match line.find(' ') {
        Some(i) => {
            let key = &line[..i];
            let rest = line[i..].trim_start();
            (key, rest)
        }
        None => (line, ""),
    }
}

/// 3-up grid is the design (see ADR-010); reflows to 2-up below 90 cols
/// and 1-up below 60.
pub fn grid_cols_for_width(width: u16) -> usize {
    if width < 60 {
        1
    } else if width < 90 {
        2
    } else {
        3
    }
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let h = (area.width * pct_x) / 100;
    let v = (area.height * pct_y) / 100;
    let x = area.x + (area.width.saturating_sub(h)) / 2;
    let y = area.y + (area.height.saturating_sub(v)) / 2;
    Rect {
        x,
        y,
        width: h,
        height: v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, Dir};
    use crate::model::LayerSnapshot;
    use nico_common::theme::DEFAULT;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn six_layers() -> Vec<LayerSnapshot> {
        vec![
            LayerSnapshot {
                name: "cluster".into(),
                status: Status::Ok,
                evidence: "3 nodes ready".into(),
                findings: vec![],
                duration_ms: 12,
            },
            LayerSnapshot {
                name: "logs".into(),
                status: Status::Warn,
                evidence: "12 errors".into(),
                findings: vec![Finding {
                    status: Status::Warn,
                    message: "12 ERROR lines in carbide-controller".into(),
                    next_command: Some("kubectl logs -n nico carbide-controller".into()),
                }],
                duration_ms: 34,
            },
            LayerSnapshot {
                name: "workflows".into(),
                status: Status::Ok,
                evidence: "no stuck wf".into(),
                findings: vec![],
                duration_ms: 8,
            },
            LayerSnapshot {
                name: "health".into(),
                status: Status::Ok,
                evidence: "4/4 healthy".into(),
                findings: vec![],
                duration_ms: 5,
            },
            LayerSnapshot {
                name: "grpc".into(),
                status: Status::Ok,
                evidence: "reachable".into(),
                findings: vec![],
                duration_ms: 7,
            },
            LayerSnapshot {
                name: "postgres".into(),
                status: Status::Ok,
                evidence: "12ms ping".into(),
                findings: vec![],
                duration_ms: 12,
            },
        ]
    }

    fn render_to_string(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(app, &DEFAULT, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            out.push('\n');
        }
        out
    }

    fn baseline_with(pairs: &[(&str, &str)]) -> nico_doctor::baseline::Baseline {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn scorecard_renders_new_badge_when_layer_regressed_vs_baseline() {
        let mut app = App::new();
        app.set_baseline(Some(baseline_with(&[("logs", "ok")])));
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: "12 errors".into(),
            findings: vec![],
            duration_ms: 0,
        }]));
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("NEW"), "NEW badge missing:\n{s}");
    }

    #[test]
    fn scorecard_renders_fixed_badge_when_layer_recovered_vs_baseline() {
        let mut app = App::new();
        app.set_baseline(Some(baseline_with(&[("logs", "fail")])));
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Ok,
            evidence: "all clear".into(),
            findings: vec![],
            duration_ms: 0,
        }]));
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("FIXED"), "FIXED badge missing:\n{s}");
    }

    #[test]
    fn missing_baseline_renders_no_delta_badges() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&app, 120, 24);
        assert!(
            !s.contains("NEW"),
            "NEW unexpectedly present without baseline:\n{s}"
        );
        assert!(
            !s.contains("FIXED"),
            "FIXED unexpectedly present without baseline:\n{s}"
        );
    }

    #[test]
    fn unchanged_delta_renders_no_badge() {
        let mut app = App::new();
        app.set_baseline(Some(baseline_with(&[("logs", "warn")])));
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: "still warn".into(),
            findings: vec![],
            duration_ms: 0,
        }]));
        let s = render_to_string(&app, 120, 24);
        assert!(
            !s.contains("NEW"),
            "NEW unexpectedly shown for unchanged layer:\n{s}"
        );
        assert!(
            !s.contains("FIXED"),
            "FIXED unexpectedly shown for unchanged layer:\n{s}"
        );
    }

    #[test]
    fn pulsing_layer_pip_uses_reversed_modifier() {
        use std::time::{Duration, Instant};
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Ok,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }]));
        app.handle(Action::Tick(t0 + Duration::from_millis(50)));
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }]));

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&app, &DEFAULT, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let pip = pip_glyph(&Status::Warn);
        let mut found_reversed = false;
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = buf.cell((x, y)).unwrap();
                if cell.symbol() == pip && cell.modifier.contains(Modifier::REVERSED) {
                    found_reversed = true;
                    break;
                }
            }
            if found_reversed {
                break;
            }
        }
        assert!(found_reversed, "expected REVERSED modifier on pulsing pip");
    }

    #[test]
    fn settled_layer_pip_does_not_use_reversed_modifier() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }]));
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&app, &DEFAULT, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let pip = pip_glyph(&Status::Warn);
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = buf.cell((x, y)).unwrap();
                if cell.symbol() == pip {
                    assert!(
                        !cell.modifier.contains(Modifier::REVERSED),
                        "non-pulsing pip must not have REVERSED set",
                    );
                }
            }
        }
    }

    #[test]
    fn new_badge_paints_in_error_palette() {
        let mut app = App::new();
        app.set_baseline(Some(baseline_with(&[("logs", "ok")])));
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }]));
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&app, &DEFAULT, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Find the 'N' of "NEW" and check fg is theme.error.
        for y in 0..buf.area.height {
            for x in 0..buf.area.width.saturating_sub(2) {
                let n = buf.cell((x, y)).unwrap();
                let e = buf.cell((x + 1, y)).unwrap();
                let w = buf.cell((x + 2, y)).unwrap();
                if n.symbol() == "N" && e.symbol() == "E" && w.symbol() == "W" {
                    assert_eq!(n.fg, DEFAULT.error, "NEW badge fg must use theme.error");
                    return;
                }
            }
        }
        panic!("NEW badge not found in rendered output");
    }

    #[test]
    fn pip_glyphs_are_distinct_per_status() {
        assert_ne!(pip_glyph(&Status::Ok), pip_glyph(&Status::Warn));
        assert_ne!(pip_glyph(&Status::Warn), pip_glyph(&Status::Fail));
        assert_ne!(pip_glyph(&Status::Fail), pip_glyph(&Status::Ok));
    }

    #[test]
    fn verdict_word_renders_each_status() {
        assert_eq!(verdict_word(&Status::Ok), "OK");
        assert_eq!(verdict_word(&Status::Warn), "WARN");
        assert_eq!(verdict_word(&Status::Fail), "FAIL");
    }

    #[test]
    fn grid_cols_reflows_with_width() {
        assert_eq!(grid_cols_for_width(40), 1);
        assert_eq!(grid_cols_for_width(70), 2);
        assert_eq!(grid_cols_for_width(120), 3);
    }

    #[test]
    fn render_shows_title_and_all_layer_names() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&app, 120, 20);
        assert!(s.contains("nico ops"), "title missing:\n{s}");
        for name in ["cluster", "logs", "workflows", "health", "grpc", "postgres"] {
            assert!(s.contains(name), "layer {name} missing:\n{s}");
        }
    }

    #[test]
    fn render_shows_overall_verdict_word() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&app, 120, 20);
        assert!(s.contains("WARN"), "verdict missing:\n{s}");
    }

    #[test]
    fn render_marks_focused_scorecard() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right));
        let s = render_to_string(&app, 120, 20);
        // Focus marker is rendered as part of the focused scorecard's title.
        // We expect "▶ logs" but not "▶ cluster".
        assert!(s.contains("▶ logs"), "expected '▶ logs' in render:\n{s}");
        assert!(
            !s.contains("▶ cluster"),
            "did not expect '▶ cluster' (cluster is not focused):\n{s}"
        );
    }

    #[test]
    fn render_drill_shows_findings_for_focused_layer() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right));
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("findings — logs"), "drill title missing:\n{s}");
        assert!(s.contains("ERROR lines"), "finding text missing:\n{s}");
        assert!(s.contains("next:"), "next-cmd hint missing:\n{s}");
    }

    #[test]
    fn render_hint_bar_lists_keybinds() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("R:refresh"), "hint missing:\n{s}");
        assert!(s.contains("?:help"), "hint missing:\n{s}");
        assert!(s.contains("q:quit"), "hint missing:\n{s}");
    }

    #[test]
    fn render_help_overlay_shows_keybinds() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::OpenHelp);
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("keybinds"), "overlay title missing:\n{s}");
        assert!(s.contains("refresh"), "overlay body missing:\n{s}");
    }

    #[test]
    fn render_hint_bar_shows_paused_when_paused() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::TogglePause);
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("PAUSED"), "PAUSED indicator missing:\n{s}");
    }

    #[test]
    fn render_hint_bar_omits_paused_when_running() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&app, 120, 24);
        assert!(!s.contains("PAUSED"), "PAUSED unexpectedly shown:\n{s}");
    }

    #[test]
    fn render_header_shows_done_glyph_after_completion() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("✓"), "expected ✓ in header after refresh:\n{s}");
    }

    #[test]
    fn render_help_overlay_lists_pause_keybind() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::OpenHelp);
        let s = render_to_string(&app, 120, 24);
        assert!(
            s.contains("pause"),
            "help overlay should mention pause keybind:\n{s}"
        );
    }

    #[test]
    fn render_detail_overlay_shows_focused_findings() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right));
        app.handle(Action::OpenDetail);
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains("detail — logs"), "overlay title missing:\n{s}");
    }

    fn drive_runs(app: &mut App, finding_counts: &[usize]) {
        for &fc in finding_counts {
            let findings = (0..fc)
                .map(|i| Finding {
                    status: Status::Warn,
                    message: format!("warn {i}"),
                    next_command: None,
                })
                .collect();
            let mut snaps = six_layers();
            // Inject a varying finding count on the "logs" layer.
            if let Some(logs) = snaps.iter_mut().find(|s| s.name == "logs") {
                logs.findings = findings;
                logs.status = if fc > 0 { Status::Warn } else { Status::Ok };
            }
            app.handle(Action::Snapshots(snaps));
        }
    }

    #[test]
    fn scorecard_sparkline_appears_after_two_or_more_runs() {
        let mut app = App::new();
        drive_runs(&mut app, &[0, 4, 8]);
        let s = render_to_string(&app, 120, 24);
        let has_spark = s
            .chars()
            .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
        assert!(has_spark, "expected sparkline glyph in render:\n{s}");
    }

    #[test]
    fn scorecard_sparkline_blank_after_first_run_only() {
        let mut app = App::new();
        drive_runs(&mut app, &[3]);
        let s = render_to_string(&app, 120, 24);
        let has_spark = s
            .chars()
            .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
        assert!(!has_spark, "no sparkline expected for <2 runs:\n{s}");
    }

    #[test]
    fn render_does_not_panic_at_narrow_widths_with_history() {
        for (w, h) in [(40u16, 24u16), (60, 24), (90, 24)] {
            let mut app = App::new();
            drive_runs(&mut app, &[0, 4, 8, 2, 6, 1, 0, 7, 3, 5, 4, 6]);
            // No assertion beyond "renders cleanly" — this test guards against
            // panics or layout glitches on the narrow grid reflows.
            let _ = render_to_string(&app, w, h);
        }
    }

    #[test]
    fn pre_populated_ring_renders_both_sparkline_and_breadcrumb() {
        let mut app = App::new();
        // Drive enough varied runs to seed both widgets.
        drive_runs(&mut app, &[0, 4, 8, 2, 6]);
        let s = render_to_string(&app, 120, 24);
        assert!(s.contains('■'), "breadcrumb missing:\n{s}");
        let has_spark = s
            .chars()
            .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
        assert!(has_spark, "sparkline missing:\n{s}");
    }

    #[test]
    fn header_breadcrumb_renders_one_square_per_past_verdict() {
        let mut app = App::new();
        for st in [Status::Warn, Status::Ok, Status::Fail] {
            app.handle(Action::Snapshots(vec![LayerSnapshot {
                name: "logs".into(),
                status: st,
                evidence: String::new(),
                findings: vec![],
                duration_ms: 0,
            }]));
        }
        let s = render_to_string(&app, 120, 20);
        let count = s.chars().filter(|c| *c == '■').count();
        assert!(
            count >= 3,
            "expected ≥3 breadcrumb squares, found {count}:\n{s}"
        );
    }

    #[test]
    fn header_breadcrumb_absent_before_any_run() {
        let app = App::new();
        let s = render_to_string(&app, 120, 20);
        assert!(
            !s.contains('■'),
            "breadcrumb must not paint before any run:\n{s}"
        );
    }

    #[test]
    fn header_breadcrumb_caps_at_breadcrumb_cap() {
        let mut app = App::new();
        for _ in 0..(BREADCRUMB_CAP + 5) {
            app.handle(Action::Snapshots(vec![LayerSnapshot {
                name: "logs".into(),
                status: Status::Ok,
                evidence: String::new(),
                findings: vec![],
                duration_ms: 0,
            }]));
        }
        let s = render_to_string(&app, 120, 20);
        let count = s.chars().filter(|c| *c == '■').count();
        assert_eq!(
            count, BREADCRUMB_CAP,
            "breadcrumb must cap at BREADCRUMB_CAP:\n{s}"
        );
    }

    #[test]
    fn loading_header_when_no_snapshots() {
        let app = App::new();
        let s = render_to_string(&app, 120, 20);
        assert!(s.contains("loading"), "loading hint missing:\n{s}");
    }

    fn pip_color_for(theme: &Theme, status: Status) -> ratatui::style::Color {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "x".into(),
            status: status.clone(),
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }]));
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&app, theme, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let pip = pip_glyph(&status);
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = buf.cell((x, y)).unwrap();
                if cell.symbol() == pip {
                    return cell.fg;
                }
            }
        }
        panic!("pip not found");
    }

    #[test]
    fn dracula_theme_paints_pips_with_dracula_palette() {
        use nico_common::theme::DRACULA;
        let color = pip_color_for(&DRACULA, Status::Warn);
        assert_eq!(color, DRACULA.warn);
    }

    #[test]
    fn nord_theme_paints_pips_with_nord_palette() {
        use nico_common::theme::NORD;
        let color = pip_color_for(&NORD, Status::Fail);
        assert_eq!(color, NORD.error);
    }

    #[test]
    fn gruvbox_theme_paints_pips_with_gruvbox_palette() {
        use nico_common::theme::GRUVBOX;
        let color = pip_color_for(&GRUVBOX, Status::Ok);
        assert_eq!(color, GRUVBOX.ok);
    }
}
