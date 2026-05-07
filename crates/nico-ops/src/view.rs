use chrono::{DateTime, Local};
use nico_common::output::Status;
use nico_common::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tui_big_text::{BigText, PixelSize};

use nico_doctor::baseline::Delta;

use crate::app::{App, Layout as AppLayout};
use crate::events::Overlay;
use crate::model::{
    CorrelateState, CorrelateStatus, Finding, LayerSnapshot, PopoverEvent, PopoverSeverity,
    Quadrant, SourceError, overall_verdict,
};
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
    "s         spotlight (incident-only) view",
    "a / Esc   show all (return from spotlight)",
    "y         copy focused next-command (spotlight)",
    "o         open focused link (spotlight)",
    "c         quick-correlate popover (workflow Finding)",
    "Esc       close overlay",
    "?         this help",
    "q / ^C    quit",
];

/// Top-level render. The host loop calls this once per dirty frame.
///
/// Takes `&mut App` so the renderer can publish the rendered scorecard
/// rectangles back to the reducer for click hit-testing — the regions are
/// only known here and have to round-trip to the reducer somehow.
pub fn render(app: &mut App, theme: &Theme, frame: &mut Frame) {
    match app.layout() {
        AppLayout::A => render_layout_a(app, theme, frame),
        AppLayout::B => render_layout_b(app, theme, frame),
        AppLayout::Spotlight => render_spotlight(app, theme, frame),
    }
}

fn render_layout_a(app: &mut App, theme: &Theme, frame: &mut Frame) {
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
    let regions = render_grid(app, theme, frame, chunks[1]);
    app.set_card_regions(regions);
    render_drill(app, theme, frame, chunks[2]);
    render_hint_bar(app, theme, frame, chunks[3]);

    match app.overlay() {
        Overlay::Detail => render_detail_overlay(app, theme, frame, area),
        Overlay::Help => render_help_overlay(theme, frame, area),
        Overlay::Correlate => render_correlate_overlay(app, theme, frame, area),
        Overlay::None => {}
    }
}

fn render_spotlight(app: &mut App, theme: &Theme, frame: &mut Frame) {
    let area = frame.area();
    // Layout: big-text headline, vertical incident cards, green footer,
    // hint bar.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(SPOTLIGHT_HEADER_HEIGHT),
            Constraint::Min(SPOTLIGHT_CARD_HEIGHT),
            Constraint::Length(1), // green footer
            Constraint::Length(1), // hint bar
        ])
        .split(area);

    render_spotlight_header(app, theme, frame, chunks[0]);
    render_spotlight_cards(app, theme, frame, chunks[1]);
    render_spotlight_green_footer(app, theme, frame, chunks[2]);
    render_spotlight_hint_bar(app, theme, frame, chunks[3]);

    // Layout C still honors the help overlay (`?`) so the operator can
    // see all keybinds, including the Spotlight-only ones.
    match app.overlay() {
        Overlay::Help => render_help_overlay(theme, frame, area),
        Overlay::Correlate => render_correlate_overlay(app, theme, frame, area),
        _ => {}
    }
}

/// Approximate row height of the tui-big-text headline at
/// `PixelSize::Quadrant`. A glyph is 8 pixels tall and Quadrant maps two
/// pixels to one cell, so the rendered headline is 4 rows; we add 1 row
/// of padding above/below so it doesn't crowd the cards.
const SPOTLIGHT_HEADER_HEIGHT: u16 = 5;

/// Per-card row height in Layout C: title + evidence + dim next-cmd +
/// action row + 2 border rows.
const SPOTLIGHT_CARD_HEIGHT: u16 = 6;

const SPOTLIGHT_ACTION_LINE: &str = "[y] copy   [o] open   [c] correlate   s/a/Esc: show all";

fn render_spotlight_header(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let verdict = overall_verdict(app.snapshots());
    let word = verdict_word(&verdict);
    let color = theme_color(theme, &verdict);
    let big = BigText::builder()
        .pixel_size(PixelSize::Quadrant)
        .alignment(Alignment::Center)
        .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .lines(vec![Line::from(word.to_string())])
        .build();
    frame.render_widget(big, area);
}

fn render_spotlight_cards(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let cards = app.spotlight_cards();
    if cards.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" no incidents ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let line = Line::from(Span::styled(
            "All layers are green. Press s / a / Esc to return to the show-all view.",
            Style::default().fg(theme.muted),
        ));
        frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), inner);
        return;
    }

    let row_constraints: Vec<Constraint> =
        std::iter::repeat_n(Constraint::Length(SPOTLIGHT_CARD_HEIGHT), cards.len()).collect();
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);
    for (i, card) in cards.iter().enumerate() {
        if i >= row_areas.len() {
            break;
        }
        render_spotlight_card(app, i, card, theme, frame, row_areas[i]);
    }
}

fn render_spotlight_card(
    app: &App,
    idx: usize,
    snap: &LayerSnapshot,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    let focused = idx == app.spotlight_focus();
    let pip_color = theme_color(theme, &snap.status);
    let title = if focused {
        format!(" ▶ {} ", snap.name)
    } else {
        format!("   {} ", snap.name)
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![
            Span::styled(
                pip_glyph(&snap.status).to_string(),
                Style::default().fg(pip_color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(title),
        ]));
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

    let evidence_line = Line::from(Span::raw(snap.evidence.clone()));
    let next_cmd = snap.findings.iter().find_map(|f| f.next_command.clone());
    let next_line = match next_cmd {
        Some(cmd) => Line::from(Span::styled(
            format!("next: {cmd}"),
            Style::default().fg(theme.muted),
        )),
        None => Line::from(Span::styled(
            "next: (no command suggested)",
            Style::default().fg(theme.muted),
        )),
    };
    let action_line = Line::from(Span::styled(
        SPOTLIGHT_ACTION_LINE.to_string(),
        Style::default().fg(theme.overlay_key),
    ));
    let lines = vec![evidence_line, next_line, action_line];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn render_spotlight_green_footer(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let names = app.spotlight_green_layer_names();
    if names.is_empty() {
        return;
    }
    let mut spans: Vec<Span> = Vec::new();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled("● ", Style::default().fg(theme.ok)));
        spans.push(Span::styled(name.clone(), Style::default().fg(theme.muted)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_spotlight_hint_bar(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    if let Some(t) = app.toast() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} ", t.message),
                Style::default()
                    .fg(theme.warn)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ))),
            area,
        );
        return;
    }
    let line = Line::from(Span::styled(
        " spotlight  R:refresh  ?:help  [y] copy  [o] open  [c] correlate  s/a/Esc: show all  q:quit "
            .to_string(),
        Style::default().fg(theme.muted),
    ));
    frame.render_widget(Paragraph::new(line), area);
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

fn render_grid(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) -> Vec<Rect> {
    let snapshots = app.snapshots();
    if snapshots.is_empty() {
        let block = Block::default().borders(Borders::ALL).title(" layers ");
        frame.render_widget(block, area);
        return Vec::new();
    }

    let cols = grid_cols_for_width(area.width);
    let rows = snapshots.len().div_ceil(cols);

    let row_constraints: Vec<Constraint> =
        std::iter::repeat_n(Constraint::Length(SCORECARD_ROW_HEIGHT), rows).collect();
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);

    let mut regions: Vec<Rect> = vec![Rect::default(); snapshots.len()];
    for r in 0..rows {
        let col_constraints: Vec<Constraint> =
            std::iter::repeat_n(Constraint::Ratio(1, cols as u32), cols).collect();
        let col_areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints)
            .split(row_areas[r]);
        for c in 0..cols {
            let idx = r * cols + c;
            if idx >= snapshots.len() {
                break;
            }
            let cell = if c < col_areas.len() {
                col_areas[c]
            } else {
                continue;
            };
            regions[idx] = cell;
            render_scorecard(app, idx, theme, frame, cell);
        }
    }
    regions
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
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.drill_scroll(), 0)),
        inner,
    );
}

fn render_hint_bar(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    if let Some(t) = app.toast() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {} ", t.message),
                Style::default()
                    .fg(theme.warn)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ))),
            area,
        );
        return;
    }
    let mouse = if app.mouse_capture() { "on" } else { "off" };
    let mut spans: Vec<Span> = vec![Span::styled(
        format!(
            " R:refresh  Space:pause  hjkl/arrows:focus  Enter:detail  s:spotlight  M:mouse({mouse})  ?:help  q:quit "
        ),
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
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.overlay_scroll(), 0)),
        inner.inner(Margin {
            horizontal: 1,
            vertical: 0,
        }),
    );
}

/// Quick-correlate popover (issue #157). Centered ~80%×70% modal; title
/// shows the workflow ID and a throbber while collecting; body renders
/// the Source-attributed Timeline. Failed Sources surface as inline
/// `source_error` rows so the operator can see *why* a Source dropped
/// out without leaving the dashboard.
fn render_correlate_overlay(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let inner_area = centered(area, 80, 70);
    let Some(state) = app.correlate_state() else {
        return;
    };
    let title = correlate_title(app, state);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().bg(theme.overlay_bg).fg(theme.overlay_fg));
    let inner = block.inner(inner_area);
    frame.render_widget(Clear, inner_area);
    frame.render_widget(block, inner_area);

    let body = inner.inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    let lines = correlate_body_lines(state, theme);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

fn correlate_title(app: &App, state: &CorrelateState) -> String {
    let throb = app.throbber_glyph();
    let (loading_marker, suffix) = match state.status {
        CorrelateStatus::Loading if !throb.is_empty() => (format!(" {throb}"), " collecting…"),
        CorrelateStatus::Loading => (String::new(), " collecting…"),
        CorrelateStatus::Loaded { .. } => (String::new(), ""),
    };
    format!(
        " correlate — {}{}{} ",
        state.workflow_id, loading_marker, suffix
    )
}

fn correlate_body_lines(state: &CorrelateState, theme: &Theme) -> Vec<Line<'static>> {
    match &state.status {
        CorrelateStatus::Loading => vec![Line::from(Span::styled(
            "loading timeline…".to_string(),
            Style::default().fg(theme.muted),
        ))],
        CorrelateStatus::Loaded {
            events,
            source_errors,
        } => {
            if events.is_empty() && source_errors.is_empty() {
                return vec![Line::from(Span::styled(
                    "(no events found for this workflow)".to_string(),
                    Style::default().fg(theme.muted),
                ))];
            }
            let mut out: Vec<Line<'static>> =
                Vec::with_capacity(events.len() + source_errors.len());
            for e in events {
                out.push(format_popover_event(e, theme));
            }
            for se in source_errors {
                out.push(format_source_error(se, theme));
            }
            out
        }
    }
}

fn format_popover_event(e: &PopoverEvent, theme: &Theme) -> Line<'static> {
    let color = popover_color(theme, e.severity);
    let ts = e.ts.format("%H:%M:%S").to_string();
    let mut spans = vec![
        Span::styled(format!("{ts}  "), Style::default().fg(theme.muted)),
        Span::styled(format!("{}  ", e.source), Style::default().fg(color)),
        Span::styled(e.kind.clone(), Style::default().fg(color)),
    ];
    if !e.message.is_empty() {
        spans.push(Span::raw(format!("  {}", e.message)));
    }
    Line::from(spans)
}

fn format_source_error(se: &SourceError, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("          ".to_string(), Style::default().fg(theme.muted)),
        Span::styled(format!("{}  ", se.name), Style::default().fg(theme.error)),
        Span::styled("source_error".to_string(), Style::default().fg(theme.error)),
        Span::raw(format!("  {}", se.reason)),
    ])
}

fn popover_color(theme: &Theme, severity: PopoverSeverity) -> ratatui::style::Color {
    match severity {
        PopoverSeverity::Info => theme.muted,
        PopoverSeverity::Warning => theme.warn,
        PopoverSeverity::Error => theme.error,
    }
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

// ── Layout B (Mission Control, issue #155) ────────────────────────────

/// Inner cell rows needed to fit a Quadrant-pixel-size big-text glyph
/// (4 inner rows). Add 2 for the surrounding block borders to get the
/// outer header height. Below this we degrade to an ASCII verdict line.
const BIG_TEXT_INNER_ROWS: u16 = 4;
const BIG_TEXT_HEADER_HEIGHT: u16 = BIG_TEXT_INNER_ROWS + 2;
const ASCII_HEADER_HEIGHT: u16 = 3;

fn render_layout_b(app: &App, theme: &Theme, frame: &mut Frame) {
    let area = frame.area();
    let header_height = if area.height >= BIG_TEXT_HEADER_HEIGHT + 8 {
        BIG_TEXT_HEADER_HEIGHT
    } else {
        ASCII_HEADER_HEIGHT
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(area);

    render_layout_b_header(app, theme, frame, chunks[0]);
    if app.b_zoomed() {
        render_layout_b_zoomed(app, theme, frame, chunks[1]);
    } else {
        render_layout_b_grid(app, theme, frame, chunks[1]);
    }
    render_layout_b_hint_bar(app, theme, frame, chunks[2]);

    if app.overlay() == Overlay::Help {
        // Layout B does not use the Detail overlay; Enter zooms instead.
        render_help_overlay(theme, frame, area);
    }
}

fn render_layout_b_header(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let snapshots = app.snapshots();
    let verdict = overall_verdict(snapshots);
    let word = verdict_word(&verdict);
    let color = theme_color(theme, &verdict);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" mission control ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height >= BIG_TEXT_INNER_ROWS {
        let big = BigText::builder()
            .pixel_size(PixelSize::Quadrant)
            .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
            .lines(vec![Line::from(word)])
            .build();
        frame.render_widget(big, inner);
    } else {
        // ASCII fallback when the terminal is too short for tui-big-text.
        let line = Line::from(Span::styled(
            word.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), inner);
    }
}

fn render_layout_b_grid(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
        .split(area);

    for row in 0..2 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
                Constraint::Ratio(1, 3),
            ])
            .split(rows[row]);
        for col in 0..3 {
            let idx = row * 3 + col;
            let q = Quadrant::ALL[idx];
            let focused = idx == app.b_focus();
            render_quadrant(app, q, focused, theme, frame, cols[col]);
        }
    }
}

fn render_layout_b_zoomed(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    render_quadrant(app, app.focused_quadrant(), true, theme, frame, area);
}

fn render_quadrant(
    app: &App,
    quadrant: Quadrant,
    focused: bool,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    let title_text = if focused {
        format!(" ▶ {} ", quadrant.title())
    } else {
        format!(" {} ", quadrant.title())
    };
    let mut block = Block::default()
        .borders(Borders::ALL)
        .title(title_text);
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

    match quadrant {
        Quadrant::Activity => render_activity_body(app, theme, frame, inner),
        _ => render_layer_body(app, quadrant, theme, frame, inner),
    }
}

fn render_layer_body(
    app: &App,
    quadrant: Quadrant,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    let lines: Vec<Line> = match find_snapshot(app.snapshots(), quadrant) {
        Some(s) => layer_body_lines(s, theme),
        None => vec![Line::from(Span::styled(
            "no data",
            Style::default().fg(theme.muted),
        ))],
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn find_snapshot(snapshots: &[LayerSnapshot], quadrant: Quadrant) -> Option<&LayerSnapshot> {
    let name = quadrant.layer_name()?;
    snapshots.iter().find(|s| s.name == name)
}

fn layer_body_lines(snap: &LayerSnapshot, theme: &Theme) -> Vec<Line<'static>> {
    let pip_style = Style::default().fg(theme_color(theme, &snap.status));
    let mut out = vec![Line::from(vec![
        Span::styled(format!("{} ", pip_glyph(&snap.status)), pip_style),
        Span::raw(snap.evidence.clone()),
    ])];
    let mut findings_added = 0;
    for f in &snap.findings {
        if findings_added >= 4 {
            break;
        }
        out.push(Line::from(vec![
            Span::styled(
                format!("{} ", pip_glyph(&f.status)),
                Style::default().fg(theme_color(theme, &f.status)),
            ),
            Span::raw(f.message.clone()),
        ]));
        findings_added += 1;
    }
    if snap.findings.len() > findings_added {
        out.push(Line::from(Span::styled(
            format!("    +{} more", snap.findings.len() - findings_added),
            Style::default().fg(theme.muted),
        )));
    }
    out
}

fn render_activity_body(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let events = app.namespace_events();
    if events.is_empty() {
        let line = Line::from(Span::styled(
            "no recent namespace events",
            Style::default().fg(theme.muted),
        ));
        frame.render_widget(Paragraph::new(line), area);
        return;
    }
    let max_rows = area.height as usize;
    let lines: Vec<Line> = events
        .iter()
        .take(max_rows)
        .map(|e| {
            let status = severity_status(e);
            let color = theme_color(theme, &status);
            Line::from(vec![
                Span::styled(
                    format!("{} ", pip_glyph(&status)),
                    Style::default().fg(color),
                ),
                Span::raw(format!("{}  ", e.ts.format("%H:%M:%S"))),
                Span::styled(
                    format!("{:<8}", e.source),
                    Style::default().fg(theme.muted),
                ),
                Span::raw(format!(" {}", e.kind)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn severity_status(event: &nico_correlate::Event) -> Status {
    match event.severity {
        nico_correlate::Severity::Info => Status::Ok,
        nico_correlate::Severity::Warning => Status::Warn,
        nico_correlate::Severity::Error => Status::Fail,
    }
}

fn render_layout_b_hint_bar(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let mut hint = String::from(" ");
    if app.b_zoomed() {
        hint.push_str("Esc:restore  ");
    } else {
        hint.push_str("Enter:zoom  ");
    }
    hint.push_str("hjkl/arrows:focus  m/Esc:back to A  R:refresh  ?:help  q:quit ");
    let mut spans: Vec<Span> = vec![Span::styled(hint, Style::default().fg(theme.muted))];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, Dir};
    use crate::model::{LayerSnapshot, PopoverEvent, PopoverSeverity, SourceError};
    use chrono::TimeZone;
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
                    link: None,
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

    fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
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
        let s = render_to_string(&mut app, 120, 24);
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
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("FIXED"), "FIXED badge missing:\n{s}");
    }

    #[test]
    fn missing_baseline_renders_no_delta_badges() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&mut app, 120, 24);
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
        let s = render_to_string(&mut app, 120, 24);
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
        terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
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
        terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
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
        terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
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
        let s = render_to_string(&mut app, 120, 20);
        assert!(s.contains("nico ops"), "title missing:\n{s}");
        for name in ["cluster", "logs", "workflows", "health", "grpc", "postgres"] {
            assert!(s.contains(name), "layer {name} missing:\n{s}");
        }
    }

    #[test]
    fn render_shows_overall_verdict_word() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&mut app, 120, 20);
        assert!(s.contains("WARN"), "verdict missing:\n{s}");
    }

    #[test]
    fn render_marks_focused_scorecard() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right));
        let s = render_to_string(&mut app, 120, 20);
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
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("findings — logs"), "drill title missing:\n{s}");
        assert!(s.contains("ERROR lines"), "finding text missing:\n{s}");
        assert!(s.contains("next:"), "next-cmd hint missing:\n{s}");
    }

    #[test]
    fn render_hint_bar_lists_keybinds() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("R:refresh"), "hint missing:\n{s}");
        assert!(s.contains("?:help"), "hint missing:\n{s}");
        assert!(s.contains("q:quit"), "hint missing:\n{s}");
    }

    #[test]
    fn render_help_overlay_shows_keybinds() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::OpenHelp);
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("keybinds"), "overlay title missing:\n{s}");
        assert!(s.contains("refresh"), "overlay body missing:\n{s}");
    }

    #[test]
    fn render_hint_bar_shows_paused_when_paused() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::TogglePause);
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("PAUSED"), "PAUSED indicator missing:\n{s}");
    }

    #[test]
    fn render_hint_bar_omits_paused_when_running() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&mut app, 120, 24);
        assert!(!s.contains("PAUSED"), "PAUSED unexpectedly shown:\n{s}");
    }

    #[test]
    fn render_header_shows_done_glyph_after_completion() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("✓"), "expected ✓ in header after refresh:\n{s}");
    }

    #[test]
    fn render_help_overlay_lists_pause_keybind() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::OpenHelp);
        let s = render_to_string(&mut app, 120, 24);
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
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("detail — logs"), "overlay title missing:\n{s}");
    }

    fn drive_runs(app: &mut App, finding_counts: &[usize]) {
        for &fc in finding_counts {
            let findings = (0..fc)
                .map(|i| Finding {
                    status: Status::Warn,
                    message: format!("warn {i}"),
                    next_command: None,
                    link: None,
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
        let s = render_to_string(&mut app, 120, 24);
        let has_spark = s
            .chars()
            .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
        assert!(has_spark, "expected sparkline glyph in render:\n{s}");
    }

    #[test]
    fn scorecard_sparkline_blank_after_first_run_only() {
        let mut app = App::new();
        drive_runs(&mut app, &[3]);
        let s = render_to_string(&mut app, 120, 24);
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
            let _ = render_to_string(&mut app, w, h);
        }
    }

    #[test]
    fn pre_populated_ring_renders_both_sparkline_and_breadcrumb() {
        let mut app = App::new();
        // Drive enough varied runs to seed both widgets.
        drive_runs(&mut app, &[0, 4, 8, 2, 6]);
        let s = render_to_string(&mut app, 120, 24);
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
        let s = render_to_string(&mut app, 120, 20);
        let count = s.chars().filter(|c| *c == '■').count();
        assert!(
            count >= 3,
            "expected ≥3 breadcrumb squares, found {count}:\n{s}"
        );
    }

    #[test]
    fn header_breadcrumb_absent_before_any_run() {
        let mut app = App::new();
        let s = render_to_string(&mut app, 120, 20);
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
        let s = render_to_string(&mut app, 120, 20);
        let count = s.chars().filter(|c| *c == '■').count();
        assert_eq!(
            count, BREADCRUMB_CAP,
            "breadcrumb must cap at BREADCRUMB_CAP:\n{s}"
        );
    }

    #[test]
    fn loading_header_when_no_snapshots() {
        let mut app = App::new();
        let s = render_to_string(&mut app, 120, 20);
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
        terminal.draw(|f| render(&mut app, theme, f)).unwrap();
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

    // ── Layout B (Mission Control, issue #155) ─────────────────────────

    fn app_in_layout_b() -> App {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::ToggleLayout);
        app
    }

    #[test]
    fn layout_b_renders_all_six_quadrant_titles() {
        let mut app = app_in_layout_b();
        let s = render_to_string(&mut app, 140, 30);
        for title in ["Cluster", "Workflows", "Services", "Postgres", "Logs", "Activity"] {
            assert!(s.contains(title), "{title} quadrant missing:\n{s}");
        }
    }

    #[test]
    fn layout_b_header_renders_verdict_word_via_big_text() {
        let mut app = app_in_layout_b();
        let s = render_to_string(&mut app, 140, 30);
        // The big-text widget emits Quadrant glyphs (▀▄█▌▐ etc.) for the
        // verdict word — assert that at least one such glyph is present.
        let has_quadrant_glyph = s
            .chars()
            .any(|c| matches!(c, '▀' | '▄' | '█' | '▌' | '▐' | '▘' | '▝' | '▖' | '▗'));
        assert!(has_quadrant_glyph, "expected tui-big-text glyphs:\n{s}");
        assert!(s.contains("mission control"), "header title missing:\n{s}");
    }

    #[test]
    fn layout_b_activity_quadrant_shows_namespace_events() {
        let mut app = app_in_layout_b();
        let now = chrono::Utc::now();
        app.handle(Action::NamespaceEvents(vec![
            nico_correlate::Event {
                ts: now,
                source: "k8s".into(),
                kind: "OOMKilled".into(),
                message: "boom".into(),
                severity: nico_correlate::Severity::Warning,
                tags: Default::default(),
            },
            nico_correlate::Event {
                ts: now,
                source: "temporal".into(),
                kind: "HostProvisioning".into(),
                message: "hp-1".into(),
                severity: nico_correlate::Severity::Info,
                tags: Default::default(),
            },
        ]));
        let s = render_to_string(&mut app, 160, 36);
        assert!(s.contains("OOMKilled"), "k8s event missing:\n{s}");
        assert!(s.contains("HostProvisioning"), "temporal event missing:\n{s}");
    }

    #[test]
    fn layout_b_activity_quadrant_empty_state_when_no_events() {
        let mut app = app_in_layout_b();
        let s = render_to_string(&mut app, 140, 30);
        assert!(
            s.contains("no recent namespace events"),
            "empty Activity hint missing:\n{s}"
        );
    }

    #[test]
    fn layout_b_focused_quadrant_marker_is_rendered() {
        let mut app = app_in_layout_b();
        let s = render_to_string(&mut app, 140, 30);
        // Default focus is index 0 → Cluster.
        assert!(s.contains("▶ Cluster"), "focus marker missing:\n{s}");
    }

    #[test]
    fn layout_b_zoomed_renders_only_focused_quadrant() {
        let mut app = app_in_layout_b();
        app.handle(Action::ZoomQuadrant);
        let s = render_to_string(&mut app, 140, 30);
        // Zoomed-in view shows the focused quadrant title; the others
        // should not appear as quadrant headers.
        assert!(s.contains("▶ Cluster"), "focused title missing:\n{s}");
        // Body content (cluster snapshot evidence) should be visible.
        assert!(
            s.contains("3 nodes ready") || s.contains("checks ok"),
            "zoomed quadrant body missing:\n{s}"
        );
        // Activity title should not appear (it's not the focused quadrant).
        assert!(
            !s.contains("Activity"),
            "non-focused quadrant should not paint while zoomed:\n{s}"
        );
    }

    #[test]
    fn layout_b_falls_back_to_ascii_verdict_at_short_height() {
        let mut app = app_in_layout_b();
        // height < 14 → header ASCII fallback path.
        let s = render_to_string(&mut app, 140, 12);
        assert!(s.contains("WARN"), "ASCII verdict word missing:\n{s}");
        let has_quadrant_glyph = s
            .chars()
            .any(|c| matches!(c, '▀' | '▄' | '█' | '▌' | '▐' | '▘' | '▝' | '▖' | '▗'));
        assert!(
            !has_quadrant_glyph,
            "tui-big-text should be skipped at short heights:\n{s}"
        );
    }

    #[test]
    fn layout_b_hint_bar_changes_with_zoom() {
        let mut app = app_in_layout_b();
        let unzoomed = render_to_string(&mut app, 140, 30);
        assert!(unzoomed.contains("Enter:zoom"), "missing zoom hint:\n{unzoomed}");
        app.handle(Action::ZoomQuadrant);
        let zoomed = render_to_string(&mut app, 140, 30);
        assert!(zoomed.contains("Esc:restore"), "missing restore hint:\n{zoomed}");
    }
    #[test]
    fn hint_bar_shows_mouse_on_by_default() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("M:mouse(on)"), "mouse hint missing:\n{s}");
    }

    #[test]
    fn hint_bar_reflects_mouse_off_after_toggle() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::ToggleMouseCapture);
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("M:mouse(off)"), "mouse hint did not flip:\n{s}");
    }

    #[test]
    fn render_publishes_card_regions_for_hit_testing() {
        // Render once to populate card_regions, then locate the "logs"
        // scorecard by scanning cells (column-counted, not byte-indexed,
        // to avoid the multi-byte pip glyphs throwing off positions) and
        // confirm a click on it focuses card #1.
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let needle: Vec<&str> = vec!["l", "o", "g", "s"];
        let mut hit: Option<(u16, u16)> = None;
        'outer: for y in 0..buf.area.height {
            for x in 0..buf.area.width.saturating_sub(needle.len() as u16) {
                if (0..needle.len() as u16)
                    .all(|i| buf.cell((x + i, y)).unwrap().symbol() == needle[i as usize])
                {
                    hit = Some((x, y));
                    break 'outer;
                }
            }
        }
        let (col, row) = hit.expect("logs scorecard title not found in render");
        app.handle(Action::Click { col, row });
        assert_eq!(
            app.focus(),
            1,
            "click on the logs scorecard at ({col}, {row}) should focus card #1"
        );
    }

    #[test]
    fn drill_scroll_offset_is_applied_to_drill_paragraph() {
        let mut app = App::new();
        let many = vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: "many".into(),
            findings: (0..10)
                .map(|i| Finding {
                    status: Status::Warn,
                    message: format!("finding number {i:02}"),
                    next_command: None,
                    link: None,
                })
                .collect(),
            duration_ms: 0,
        }];
        app.handle(Action::Snapshots(many));
        let baseline = render_to_string(&mut app, 120, 24);
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        let scrolled = render_to_string(&mut app, 120, 24);
        assert_ne!(
            baseline, scrolled,
            "drill should redraw differently when drill_scroll changes"
        );
    }

    #[test]
    fn overlay_scroll_offset_is_applied_to_detail_overlay() {
        let mut app = App::new();
        let many = vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: "many".into(),
            findings: (0..30)
                .map(|i| Finding {
                    status: Status::Warn,
                    message: format!("overlay finding {i:02}"),
                    next_command: None,
                    link: None,
                })
                .collect(),
            duration_ms: 0,
        }];
        app.handle(Action::Snapshots(many));
        app.handle(Action::OpenDetail);
        let baseline = render_to_string(&mut app, 120, 24);
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        let scrolled = render_to_string(&mut app, 120, 24);
        assert_ne!(
            baseline, scrolled,
            "detail overlay should redraw differently when overlay_scroll changes"
        );
    }

    use crate::action::ScrollDir;

    // ── Layout C / Spotlight ────────────────────────────────────────────

    fn mixed_for_spotlight() -> Vec<LayerSnapshot> {
        // 2 non-green (warn, fail), 3 green (ok, ok, skipped).
        vec![
            LayerSnapshot {
                name: "cluster".into(),
                status: Status::Ok,
                evidence: "3 nodes ready".into(),
                findings: vec![],
                duration_ms: 0,
            },
            LayerSnapshot {
                name: "logs".into(),
                status: Status::Warn,
                evidence: "12 errors".into(),
                findings: vec![Finding {
                    status: Status::Warn,
                    message: "12 ERROR lines".into(),
                    next_command: Some("kubectl logs -n nico foo".into()),
                    link: Some("https://example.com/logs".into()),
                }],
                duration_ms: 0,
            },
            LayerSnapshot {
                name: "workflows".into(),
                status: Status::Ok,
                evidence: "no stuck wf".into(),
                findings: vec![],
                duration_ms: 0,
            },
            LayerSnapshot {
                name: "grpc".into(),
                status: Status::Fail,
                evidence: "unreachable".into(),
                findings: vec![Finding {
                    status: Status::Fail,
                    message: "dial tcp: i/o timeout".into(),
                    next_command: Some("kubectl describe svc -n nico grpc".into()),
                    link: None,
                }],
                duration_ms: 0,
            },
            LayerSnapshot {
                name: "postgres".into(),
                status: Status::Skipped,
                evidence: "skipped".into(),
                findings: vec![],
                duration_ms: 0,
            },
        ]
    }

    fn enter_spotlight(app: &mut App) {
        app.handle(Action::ShowSpotlight);
    }

    #[test]
    fn spotlight_renders_big_text_headline_for_verdict() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        enter_spotlight(&mut app);
        // tui-big-text doesn't emit literal letters; instead, it paints
        // box-drawing glyphs derived from the 8x8 font. We assert that
        // the Spotlight headline area is non-empty (not just blanks) and
        // styled in the verdict colour.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut found_painted = false;
        for y in 0..SPOTLIGHT_HEADER_HEIGHT.min(buf.area.height) {
            for x in 0..buf.area.width {
                let cell = buf.cell((x, y)).unwrap();
                if cell.symbol() != " " && cell.fg == DEFAULT.error {
                    found_painted = true;
                    break;
                }
            }
        }
        assert!(found_painted, "expected painted FAIL headline in red");
    }

    #[test]
    fn spotlight_renders_one_card_per_non_green_layer() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        enter_spotlight(&mut app);
        let s = render_to_string(&mut app, 120, 30);
        // The two non-green Layers must each surface as a card title.
        assert!(s.contains("logs"), "logs card missing:\n{s}");
        assert!(s.contains("grpc"), "grpc card missing:\n{s}");
        // Their evidence + next-command lines must show through.
        assert!(s.contains("12 errors"), "logs evidence missing:\n{s}");
        assert!(s.contains("unreachable"), "grpc evidence missing:\n{s}");
        assert!(
            s.contains("next: kubectl logs"),
            "logs next-cmd missing:\n{s}"
        );
        assert!(
            s.contains("[y] copy"),
            "spotlight action keybinds missing:\n{s}"
        );
        assert!(
            s.contains("[o] open"),
            "spotlight action keybinds missing:\n{s}"
        );
        assert!(
            s.contains("[c] correlate"),
            "spotlight action keybinds missing:\n{s}"
        );
    }

    #[test]
    fn spotlight_compresses_green_layers_to_footer_line() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        enter_spotlight(&mut app);
        let s = render_to_string(&mut app, 120, 30);
        for name in ["cluster", "workflows", "postgres"] {
            assert!(
                s.contains(name),
                "green layer {name} should be in footer:\n{s}"
            );
        }
        // The green-strip pip glyph is `●`.
        assert!(s.contains("●"), "green pip missing in footer:\n{s}");
    }

    #[test]
    fn spotlight_does_not_render_layout_a_grid() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        enter_spotlight(&mut app);
        let s = render_to_string(&mut app, 120, 30);
        // Layout A's "nico ops" header title must not appear in
        // Spotlight.
        assert!(
            !s.contains("nico ops"),
            "Layout A header leaked into Spotlight:\n{s}"
        );
        // Layout A's drill panel title must not appear either.
        assert!(
            !s.contains("findings —"),
            "Layout A drill leaked into Spotlight:\n{s}"
        );
    }

    #[test]
    fn spotlight_with_no_incidents_renders_friendly_empty_state() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "cluster".into(),
            status: Status::Ok,
            evidence: "ok".into(),
            findings: vec![],
            duration_ms: 0,
        }]));
        enter_spotlight(&mut app);
        let s = render_to_string(&mut app, 120, 24);
        assert!(
            s.contains("no incidents") || s.contains("All layers are green"),
            "expected empty-state hint:\n{s}"
        );
    }

    #[test]
    fn spotlight_toast_renders_in_hint_bar() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        enter_spotlight(&mut app);
        app.handle(Action::ShowToast("clipboard unavailable".into()));
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            s.contains("clipboard unavailable"),
            "toast missing in render:\n{s}"
        );
    }

    #[test]
    fn layout_a_hint_bar_advertises_spotlight_keybind() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        let s = render_to_string(&mut app, 120, 24);
        assert!(s.contains("s:spotlight"), "spotlight hint missing:\n{s}");
    }

    #[test]
    fn help_overlay_lists_spotlight_keybinds() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        app.handle(Action::OpenHelp);
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            s.contains("spotlight"),
            "help should mention spotlight:\n{s}"
        );
        assert!(
            s.contains("show all"),
            "help should mention show-all return:\n{s}"
        );
    }

    #[test]
    fn spotlight_help_overlay_renders_on_top_of_layout_c() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        enter_spotlight(&mut app);
        app.handle(Action::OpenHelp);
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            s.contains("keybinds"),
            "help overlay missing in spotlight:\n{s}"
        );
    }

    #[test]
    fn render_in_spotlight_does_not_panic_at_narrow_widths() {
        for (w, h) in [(40u16, 24u16), (60, 24), (90, 24), (120, 30)] {
            let mut app = App::new();
            app.handle(Action::Snapshots(mixed_for_spotlight()));
            enter_spotlight(&mut app);
            let _ = render_to_string(&mut app, w, h);
        }
    }

    // ── Quick-correlate popover (issue #157) ────────────────────────────

    fn workflows_snap_with_id(id: &str) -> LayerSnapshot {
        LayerSnapshot {
            name: "workflows".into(),
            status: Status::Warn,
            evidence: "1 stuck".into(),
            findings: vec![Finding {
                status: Status::Warn,
                message: format!(
                    "stuck_workflow: {id} (HostProvisioning): 47m running, last: 47 events"
                ),
                next_command: Some(format!("temporal workflow show -w {id}")),
                link: None,
            }],
            duration_ms: 0,
        }
    }

    fn open_correlate(app: &mut App) {
        app.handle(Action::Correlate);
    }

    #[test]
    fn correlate_overlay_title_shows_workflow_id_and_throbber_while_loading() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
        open_correlate(&mut app);
        let s = render_to_string(&mut app, 120, 30);
        assert!(s.contains("correlate"), "popover title missing:\n{s}");
        assert!(s.contains("wf-001"), "workflow id missing in title:\n{s}");
        assert!(
            s.contains("collecting"),
            "throbber/collecting indicator missing:\n{s}"
        );
    }

    #[test]
    fn correlate_overlay_body_renders_loaded_timeline_events() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
        open_correlate(&mut app);
        app.handle(Action::CorrelateResults {
            workflow_id: "wf-001".into(),
            events: vec![
                PopoverEvent {
                    ts: chrono::Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 5).unwrap(),
                    source: "temporal".into(),
                    kind: "WorkflowExecutionStarted".into(),
                    message: String::new(),
                    severity: PopoverSeverity::Info,
                },
                PopoverEvent {
                    ts: chrono::Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 9).unwrap(),
                    source: "temporal".into(),
                    kind: "WorkflowExecutionFailed".into(),
                    message: "deadline exceeded".into(),
                    severity: PopoverSeverity::Error,
                },
            ],
            source_errors: vec![],
        });
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            s.contains("WorkflowExecutionStarted"),
            "first event missing:\n{s}"
        );
        assert!(
            s.contains("WorkflowExecutionFailed"),
            "second event missing:\n{s}"
        );
        assert!(
            s.contains("deadline exceeded"),
            "event message missing:\n{s}"
        );
        assert!(
            !s.contains("collecting"),
            "Loading indicator should disappear after results land:\n{s}"
        );
    }

    #[test]
    fn correlate_overlay_renders_source_errors_inline_as_source_error_rows() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
        open_correlate(&mut app);
        app.handle(Action::CorrelateResults {
            workflow_id: "wf-001".into(),
            events: vec![],
            source_errors: vec![SourceError {
                name: "loki".into(),
                reason: "LOKI_URL not set".into(),
            }],
        });
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            s.contains("source_error"),
            "synthetic source_error row missing:\n{s}"
        );
        assert!(s.contains("loki"), "failed source name missing:\n{s}");
        assert!(
            s.contains("LOKI_URL not set"),
            "failed source reason missing:\n{s}"
        );
    }

    #[test]
    fn correlate_overlay_renders_empty_state_when_no_events_and_no_source_errors() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
        open_correlate(&mut app);
        app.handle(Action::CorrelateResults {
            workflow_id: "wf-001".into(),
            events: vec![],
            source_errors: vec![],
        });
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            s.contains("no events found"),
            "empty-state hint missing:\n{s}"
        );
    }

    #[test]
    fn correlate_overlay_does_not_render_when_overlay_is_none() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            !s.contains("correlate —"),
            "popover must not render until `c` opens it:\n{s}"
        );
    }

    #[test]
    fn correlate_overlay_renders_in_spotlight_layout_too() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
        app.handle(Action::ShowSpotlight);
        open_correlate(&mut app);
        let s = render_to_string(&mut app, 120, 30);
        assert!(
            s.contains("correlate"),
            "popover should overlay Spotlight:\n{s}"
        );
        assert!(
            s.contains("wf-001"),
            "wf id missing in Spotlight overlay:\n{s}"
        );
    }
}
