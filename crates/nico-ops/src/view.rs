use chrono::{DateTime, Local};
use crossterm::event::KeyCode;
use nico_common::output::Status;
use nico_common::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use tui_big_text::{BigText, PixelSize};

use nico_doctor::baseline::Delta;

use crate::app::{App, Layout as AppLayout};
use crate::events::Overlay;
use crate::layout_solver::{FocusState, SolverInput, solve};
use crate::model::{
    CorrelateState, Finding, LogLine, PopoverEvent, PopoverSeverity, SourceError, SourceStatus,
    overall_verdict,
};
use crate::popup::{Popup, PopupSize};
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
    "↑↓/hjk    move focus  (←→ for left/right; `l` reserved for logs)",
    "j/k/wheel scroll logs (when logs panel is zoomed/focused)",
    "Enter     open detail",
    "s         spotlight (incident-only) view",
    "a / Esc   show all (return from spotlight)",
    "y         copy focused next-command (spotlight)",
    "o         open focused link (spotlight)",
    "c         correlate mini-dashboard popup for focused entity",
    "l         logs modal overlay (any view; `l`/Esc to dismiss)",
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
        AppLayout::Scorecard => render_scorecard_layout(app, theme, frame),
        AppLayout::Spotlight => render_spotlight(app, theme, frame),
    }
}

fn render_scorecard_layout(app: &mut App, theme: &Theme, frame: &mut Frame) {
    let area = frame.area();
    let focus = if app.focused().is_some_and(|s| s.name == "logs") {
        FocusState::Logs
    } else {
        FocusState::Grid
    };
    let plan = solve(SolverInput {
        viewport: area,
        view: AppLayout::Scorecard,
        focus,
        logs_overlay_open: matches!(app.overlay(), Overlay::Logs),
    });

    render_header(app, theme, frame, plan.header);
    let regions = render_grid(app, theme, frame, plan.body);
    app.set_card_regions(regions);
    render_drill(app, theme, frame, plan.drill);
    render_legend_bar(theme, frame, plan.legend_bar);
    render_hint_bar(app, theme, frame, plan.hint_bar);

    match app.overlay() {
        Overlay::Detail => render_detail_overlay(app, theme, frame, area),
        Overlay::Help => render_help_overlay(theme, frame, area),
        Overlay::Correlate => render_correlate_overlay(app, theme, frame, area),
        Overlay::CorrelateFullscreen => render_correlate_fullscreen(app, theme, frame, area),
        Overlay::CorrelateChooser => render_correlate_chooser_overlay(app, theme, frame, area),
        Overlay::Logs => render_logs_overlay(app, theme, frame, plan.logs_overlay.unwrap_or(area)),
        Overlay::None => {}
    }
}

fn render_spotlight(app: &mut App, theme: &Theme, frame: &mut Frame) {
    let area = frame.area();
    // PRD-006 Slice 5 (#371): redesigned vertical stack —
    //   big-text headline, severity-grouped findings list, green footer,
    //   persistent action row, severity legend, hint/toast bar.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(SPOTLIGHT_HEADER_HEIGHT),
            Constraint::Min(0),    // findings body fills the rest
            Constraint::Length(1), // green footer
            Constraint::Length(1), // persistent action row
            Constraint::Length(1), // severity legend
            Constraint::Length(1), // hint / toast bar
        ])
        .split(area);

    render_spotlight_header(app, theme, frame, chunks[0]);
    render_spotlight_findings(app, theme, frame, chunks[1]);
    render_spotlight_green_footer(app, theme, frame, chunks[2]);
    render_spotlight_action_row(theme, frame, chunks[3]);
    render_legend_bar(theme, frame, chunks[4]);
    render_spotlight_hint_bar(app, theme, frame, chunks[5]);

    // Spotlight still honors the help overlay (`?`) so the operator can
    // see all keybinds, including the Spotlight-only ones. PRD-006 Slice
    // 2 (#368) adds the logs overlay reachable here via `l`.
    match app.overlay() {
        Overlay::Help => render_help_overlay(theme, frame, area),
        Overlay::Correlate => render_correlate_overlay(app, theme, frame, area),
        Overlay::CorrelateFullscreen => render_correlate_fullscreen(app, theme, frame, area),
        Overlay::CorrelateChooser => render_correlate_chooser_overlay(app, theme, frame, area),
        Overlay::Logs => {
            let plan = solve(SolverInput {
                viewport: area,
                view: AppLayout::Spotlight,
                focus: FocusState::Grid,
                logs_overlay_open: true,
            });
            render_logs_overlay(app, theme, frame, plan.logs_overlay.unwrap_or(area));
        }
        _ => {}
    }
}

/// Approximate row height of the tui-big-text headline at
/// `PixelSize::Quadrant`. A glyph is 8 pixels tall and Quadrant maps two
/// pixels to one cell, so the rendered headline is 4 rows; we add 1 row
/// of padding above/below so it doesn't crowd the cards.
const SPOTLIGHT_HEADER_HEIGHT: u16 = 5;

/// PRD-006 Slice 5 (#371): persistent action row text. Renders once,
/// at the bottom of the view, above the legend / hint bars. Replaces
/// the pre-Slice-5 per-card action line.
const SPOTLIGHT_ACTION_LINE: &str = "  [y] copy   [o] open   [Enter] drill   [a] all   [esc] back";

/// PRD-007 stub toast surfaced when the operator presses `[Enter]` from
/// Spotlight. The drill-down primitive itself lands in PRD-007; this
/// slice ships the visual contract (the persistent `[Enter] drill` cue
/// in the action row) so PRD-007 has a stable entry point to plug into.
pub const SPOTLIGHT_DRILL_STUB_TOAST: &str = "Drill-down coming in PRD-007";

/// Group header glyphs + labels for the severity-grouped findings list.
fn group_header(theme: &Theme, status: &Status) -> Line<'static> {
    let label = match status {
        Status::Fail => "FAIL",
        Status::Warn => "WARN",
        Status::Unknown => "UNKNOWN",
        _ => "OTHER",
    };
    Line::from(vec![
        Span::styled(
            pip_glyph(status).to_string(),
            Style::default()
                .fg(theme_color(theme, status))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            label,
            Style::default()
                .fg(theme_color(theme, status))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

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

fn render_spotlight_findings(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let cards = app.spotlight_cards();
    if cards.is_empty() {
        // Empty-state placeholder is an inner widget — title only, no
        // border (issue #370 border-rule pass).
        let block = theme.plain_block().title(" no incidents ");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let line = Line::from(Span::styled(
            "All layers are green. Press s / a / Esc to return to the show-all view.",
            Style::default().fg(theme.muted),
        ));
        frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), inner);
        return;
    }

    // Build a flat line list: severity group header, then per-card
    // headline + indented detail. Focus-aware styling (focus accent for
    // the focused row, dim for others) is applied per finding row.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let focused_idx = app.spotlight_focus();
    let mut prev_status: Option<Status> = None;
    for (i, snap) in cards.iter().enumerate() {
        if prev_status.as_ref() != Some(&snap.status) {
            if i != 0 {
                // Blank separator between groups.
                lines.push(Line::from(""));
            }
            lines.push(group_header(theme, &snap.status));
            prev_status = Some(snap.status.clone());
        }
        let focused = i == focused_idx;
        let headline_style = if focused {
            Style::default()
                .fg(theme.overlay_key)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.muted)
        };
        let detail_style = if focused {
            Style::default().fg(theme.overlay_key)
        } else {
            Style::default().fg(theme.muted)
        };
        let cursor = if focused { "▶ " } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(cursor, headline_style),
            Span::styled(
                pip_glyph(&snap.status).to_string(),
                Style::default().fg(theme_color(theme, &snap.status)),
            ),
            Span::raw(" "),
            Span::styled(snap.name.clone(), headline_style),
            Span::raw("  "),
            Span::styled(snap.evidence.clone(), headline_style),
        ]));
        let detail = snap
            .findings
            .iter()
            .map(|f| f.message.clone())
            .next()
            .unwrap_or_else(|| "(no detail)".to_string());
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(detail, detail_style),
        ]));
        if let Some(cmd) = snap.findings.iter().find_map(|f| f.next_command.clone()) {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("next: {cmd}"), Style::default().fg(theme.muted)),
            ]));
        }
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_spotlight_action_row(theme: &Theme, frame: &mut Frame, area: Rect) {
    let line = Line::from(Span::styled(
        SPOTLIGHT_ACTION_LINE.to_string(),
        Style::default().fg(theme.overlay_key),
    ));
    frame.render_widget(Paragraph::new(line), area);
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

    // Header is an inner widget within the Scorecard view — title yes,
    // border no (issue #370 border-rule pass).
    let block = theme
        .plain_block()
        .title(" nico ops ")
        .title_top(Line::from(title_right).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(Line::from(spans)), inner);
}

fn render_grid(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) -> Vec<Rect> {
    let snapshots = app.snapshots();
    if snapshots.is_empty() {
        // Empty-grid placeholder is an inner widget; the per-cell
        // Scorecard frames below carry the only borders.
        let block = theme.plain_block().title(" layers ");
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
    // Per-cell Scorecard frame is an outermost container — keep border.
    let mut block = theme.container_block().title(Line::from(title_spans));
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
    if app.focused().is_some_and(|s| s.name == "logs") {
        render_logs_panel(app.log_lines(), app.logs_scroll(), theme, frame, area);
        return;
    }

    let title = match app.focused() {
        Some(s) => format!(" findings — {} ", s.name),
        None => " findings ".to_string(),
    };
    // Drill panel is an inner widget within the Scorecard view — title
    // only, no border (issue #370 border-rule pass).
    let block = theme.plain_block().title(title);
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

/// Render the snapshot logs panel — error log lines from the most recent
/// refresh round. Used by the scorecard drill panel when the `logs`
/// layer is focused. (Mission Control's `Logs` quadrant used to be the
/// other consumer; it was removed in PRD-006 slice 1, issue #367.) The
/// renderer is the sole cap on visible row count: it shows up to
/// `inner.height` rows from
/// `lines[clamped..]` and the title carries the `start–end of total`
/// range. `scroll` is clamped on use so a post-refresh dataset shrink
/// can't render past the end. Empty `lines` yields a "no errors" empty
/// state. Issue #158, ADR-0014.
fn render_logs_panel(lines: &[LogLine], scroll: u16, theme: &Theme, frame: &mut Frame, area: Rect) {
    // Logs overlay is an outermost frame (per ADR-0008 amendment).
    let block = theme.container_block();
    let inner = block.inner(area);
    let total = lines.len();
    let max_offset = total.saturating_sub(inner.height as usize);
    let clamped = (scroll as usize).min(max_offset);
    let visible = total.saturating_sub(clamped).min(inner.height as usize);
    let title = if total == 0 {
        " logs ".to_string()
    } else {
        let start = clamped + 1;
        let end = clamped + visible;
        format!(" logs — {start}–{end} of {total} ")
    };
    frame.render_widget(block.title(title), area);
    if inner.height == 0 {
        return;
    }

    if lines.is_empty() {
        let empty = Line::from(Span::styled("no errors", Style::default().fg(theme.muted)));
        frame.render_widget(Paragraph::new(empty), inner);
        return;
    }

    let body: Vec<Line> = lines
        .iter()
        .skip(clamped)
        .take(visible)
        .map(|l| log_line_spans(l, theme, inner.width))
        .collect();
    frame.render_widget(Paragraph::new(body), inner);
}

/// PRD-006 Slice 2 (#368): logs modal overlay. Renders the snapshot
/// `log_lines` inside the popup primitive centered at the rect
/// returned by the layout solver. Operator dismisses with `Esc` / `l`
/// / `q`. Empty `log_lines` yields a "no errors" empty state so the
/// modal still says something.
fn render_logs_overlay(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let lines = app.log_lines();
    let total = lines.len();
    let title = if total == 0 {
        " logs ".to_string()
    } else {
        format!(" logs — {total} ")
    };
    let body: Vec<Line<'static>> = if lines.is_empty() {
        vec![Line::from(Span::styled(
            "no errors",
            Style::default().fg(theme.muted),
        ))]
    } else {
        lines
            .iter()
            .map(|l| log_line_spans(l, theme, area.width))
            .collect()
    };
    let popup = Popup {
        title,
        body,
        size_pct: PopupSize {
            width_pct: 100,
            height_pct: 100,
        },
        dismiss_keys: vec![KeyCode::Esc, KeyCode::Char('l'), KeyCode::Char('q')],
        body_margin: Margin {
            horizontal: 1,
            vertical: 0,
        },
        scroll: app.logs_scroll(),
    };
    popup.render(theme, frame, area);
}

/// Format one `LogLine` as `HH:MM:SS · pod · glyph · message`. Truncates
/// the message so the whole row fits in `width` cells (best-effort — uses
/// char count, not display width). The message is always truncated, never
/// the timestamp/pod columns, so the alignment stays scannable.
fn log_line_spans(line: &LogLine, theme: &Theme, width: u16) -> Line<'static> {
    let ts = line.ts.format("%H:%M:%S").to_string();
    let pod = line.pod.clone();
    let glyph = pip_glyph(&line.level).to_string();
    let prefix_len = ts.chars().count() + 3 + pod.chars().count() + 3 + glyph.chars().count() + 1;
    let budget = (width as usize).saturating_sub(prefix_len);
    let msg = truncate_message(&line.message, budget);
    let level_style = Style::default().fg(theme_color(theme, &line.level));
    Line::from(vec![
        Span::styled(ts, Style::default().fg(theme.muted)),
        Span::raw(" · "),
        Span::raw(pod),
        Span::raw(" · "),
        Span::styled(glyph, level_style.add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::raw(msg),
    ])
}

fn truncate_message(s: &str, budget: usize) -> String {
    if budget <= 1 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= budget {
        return s.to_string();
    }
    let mut out: String = s.chars().take(budget.saturating_sub(1)).collect();
    out.push('…');
    out
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
            " R:refresh  Space:pause  hjk/arrows:focus  Enter:detail  s:spotlight  l:logs  M:mouse({mouse})  ?:help  q:quit "
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
    let title = match app.focused() {
        Some(s) => format!(" detail — {} ", s.name),
        None => " detail ".to_string(),
    };
    let body = match app.focused() {
        Some(s) if !s.findings.is_empty() => finding_lines(&s.findings, theme, true),
        Some(_) => vec![Line::from(Span::styled(
            "no findings",
            Style::default().fg(theme.muted),
        ))],
        None => vec![],
    };
    Popup {
        title,
        body,
        size_pct: PopupSize {
            width_pct: 80,
            height_pct: 80,
        },
        dismiss_keys: vec![KeyCode::Esc, KeyCode::Enter],
        body_margin: Margin {
            horizontal: 1,
            vertical: 0,
        },
        scroll: app.overlay_scroll(),
    }
    .render(theme, frame, area);
}

/// Quick-correlate popover (issue #157). Centered ~80%×70% modal; title
/// shows the workflow ID and a throbber while collecting; body renders
/// the Source-attributed Timeline. Failed Sources surface as inline
/// `source_error` rows so the operator can see *why* a Source dropped
/// out without leaving the dashboard.
fn render_correlate_overlay(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let Some(state) = app.correlate_state() else {
        return;
    };
    Popup {
        title: correlate_title(app, state),
        body: correlate_body_lines(state, theme),
        size_pct: PopupSize {
            width_pct: 80,
            height_pct: 70,
        },
        dismiss_keys: vec![KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('Q')],
        body_margin: Margin {
            horizontal: 1,
            vertical: 0,
        },
        scroll: 0,
    }
    .render(theme, frame, area);
}

/// PRD-007 Slice 4 (#377): full-screen correlate view. Same data as the
/// condensed popup; expanded to fill the viewport so the operator can
/// read a long timeline without scrolling. Esc collapses back to the
/// condensed popup (preserving the in-flight stream); q tears the entire
/// overlay down.
fn render_correlate_fullscreen(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let Some(state) = app.correlate_state() else {
        return;
    };
    Popup {
        title: correlate_fullscreen_title(app, state),
        body: correlate_fullscreen_body_lines(state, theme),
        // 100%×100% — same Popup primitive, but stretched to fill the
        // whole viewport so the chrome stays consistent with the
        // condensed popup the operator just expanded out of.
        size_pct: PopupSize {
            width_pct: 100,
            height_pct: 100,
        },
        dismiss_keys: vec![KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('Q')],
        body_margin: Margin {
            horizontal: 1,
            vertical: 0,
        },
        scroll: 0,
    }
    .render(theme, frame, area);
}

fn correlate_fullscreen_title(app: &App, state: &CorrelateState) -> String {
    let throb = app.throbber_glyph();
    let (loading_marker, suffix) = if state.is_loading() {
        if throb.is_empty() {
            (String::new(), " collecting…")
        } else {
            (format!(" {throb}"), " collecting…")
        }
    } else {
        (String::new(), "")
    };
    format!(
        " correlate (full) — {}{}{} ",
        state.entity.id, loading_marker, suffix
    )
}

/// Same body composition as the condensed popup, but the action row at
/// the bottom is rebound to match the fullscreen-only dismiss contract
/// (`[esc] back`, `[q] close`).
fn correlate_fullscreen_body_lines(state: &CorrelateState, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(diag) = &state.diagnosis {
        out.extend(diagnosis_banner_lines(diag, theme));
    }
    if !state.sources.is_empty() {
        out.push(source_dots_line(state, theme));
        out.push(Line::from(""));
    }
    if state.events.is_empty() && state.source_errors.is_empty() {
        let msg = if state.is_loading() {
            "loading timeline…".to_string()
        } else {
            format!("No events in the last 1h for {}", state.entity.id)
        };
        out.push(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
    } else {
        for e in &state.events {
            out.push(format_popover_event(e, theme));
        }
        for se in &state.source_errors {
            out.push(format_source_error(se, theme));
        }
    }
    out.push(Line::from(""));
    out.push(Line::from(vec![
        Span::styled(
            "[esc] back   ".to_string(),
            Style::default().fg(theme.overlay_key),
        ),
        Span::styled(
            "[q] close".to_string(),
            Style::default().fg(theme.overlay_key),
        ),
    ]));
    out
}

/// PRD-007 Slice 1 (#372): multi-match chooser popup. Centered ~50%×40%
/// modal that lists every entity the extraction primitive returned;
/// arrow keys / `j` / `k` move focus, `Enter` opens the correlate popup
/// for the highlighted entity, `Esc` cancels.
fn render_correlate_chooser_overlay(app: &App, theme: &Theme, frame: &mut Frame, area: Rect) {
    let Some(state) = app.chooser_state() else {
        return;
    };
    Popup {
        title: " drill into ".to_string(),
        body: chooser_body_lines(state, theme),
        size_pct: PopupSize {
            width_pct: 50,
            height_pct: 40,
        },
        dismiss_keys: vec![KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char('Q')],
        body_margin: Margin {
            horizontal: 1,
            vertical: 0,
        },
        scroll: 0,
    }
    .render(theme, frame, area);
}

fn chooser_body_lines(state: &crate::app::ChooserState, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(Span::styled(
        format!(
            "{} candidates — Enter selects, Esc cancels",
            state.entities.len()
        ),
        Style::default().fg(theme.muted),
    )));
    out.push(Line::from(""));
    for (i, entity) in state.entities.iter().enumerate() {
        let focused = i == state.focus;
        let marker = if focused { "▶ " } else { "  " };
        let style = if focused {
            Style::default()
                .fg(theme.overlay_key)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.overlay_fg)
        };
        out.push(Line::from(vec![
            Span::styled(marker.to_string(), style),
            Span::styled(entity.id.clone(), style),
            Span::styled(
                format!("  ({})", entity.id_type.cli_name()),
                Style::default().fg(theme.muted),
            ),
        ]));
    }
    out
}

fn correlate_title(app: &App, state: &CorrelateState) -> String {
    let throb = app.throbber_glyph();
    let (loading_marker, suffix) = if state.is_loading() {
        if throb.is_empty() {
            (String::new(), " collecting…")
        } else {
            (format!(" {throb}"), " collecting…")
        }
    } else {
        (String::new(), "")
    };
    format!(
        " correlate — {}{}{} ",
        state.entity.id, loading_marker, suffix
    )
}

/// PRD-007 Slice 2 popup body layout:
///
/// 1. **Diagnosis banner** at the top — omitted if absent.
/// 2. **Source-availability dots row** — `⟳ temporal  ● postgres  ✗ loki`.
/// 3. **Scrollable Timeline** — chronologically sorted events, with
///    failed Sources surfacing inline as synthetic `source_error` rows.
/// 4. **Action row** at the bottom — `[Enter] full   [esc] close`. The
///    `Enter` handler is a stub until Slice 4.
///
/// Rendered by stitching the four sections into one `body` vec; the
/// `Popup` primitive draws it inside the centered modal frame.
fn correlate_body_lines(state: &CorrelateState, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(diag) = &state.diagnosis {
        out.extend(diagnosis_banner_lines(diag, theme));
    }
    if !state.sources.is_empty() {
        out.push(source_dots_line(state, theme));
        out.push(Line::from(""));
    }

    if state.events.is_empty() && state.source_errors.is_empty() {
        let msg = if state.is_loading() {
            "loading timeline…".to_string()
        } else {
            format!("No events in the last 1h for {}", state.entity.id)
        };
        out.push(Line::from(Span::styled(
            msg,
            Style::default().fg(theme.muted),
        )));
    } else {
        for e in &state.events {
            out.push(format_popover_event(e, theme));
        }
        for se in &state.source_errors {
            out.push(format_source_error(se, theme));
        }
    }

    out.push(Line::from(""));
    out.push(action_row_line(theme));
    out
}

/// `⟳ temporal  ● postgres  ✗ loki  ⟳ k8s …` — one entry per Source the
/// runner is querying, glyph by current status. Re-rendered on every
/// `CorrelateUpdate` so dots transition `⟳ → ● / ✗` live.
fn source_dots_line(state: &CorrelateState, theme: &Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, p) in state.sources.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        let (glyph, color) = match p.status {
            SourceStatus::Pending => ("⟳", theme.muted),
            SourceStatus::Landed => ("●", theme.ok),
            SourceStatus::Failed => ("✗", theme.error),
            SourceStatus::Skipped => ("○", theme.muted),
        };
        spans.push(Span::styled(
            format!("{glyph} "),
            Style::default().fg(color),
        ));
        spans.push(Span::styled(
            p.name.clone(),
            Style::default().fg(theme.overlay_fg),
        ));
    }
    Line::from(spans)
}

fn action_row_line(theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "[Enter] full   ".to_string(),
            Style::default().fg(theme.overlay_key),
        ),
        Span::styled(
            "[esc] close".to_string(),
            Style::default().fg(theme.overlay_key),
        ),
    ])
}

/// PRD-007: two-line Diagnosis banner + blank separator at the top of
/// the popup body. Renders only when `state.diagnosis.is_some()`.
fn diagnosis_banner_lines(
    diag: &crate::model::PopoverDiagnosis,
    theme: &Theme,
) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(
                "diagnosis: ".to_string(),
                Style::default().fg(theme.overlay_key),
            ),
            Span::styled(diag.pattern.clone(), Style::default().fg(theme.warn)),
        ]),
        Line::from(vec![
            Span::styled("  ".to_string(), Style::default()),
            Span::raw(diag.error_signature.clone()),
        ]),
        Line::from(""),
    ]
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
    let body: Vec<Line<'static>> = HELP_LINES
        .iter()
        .map(|l| {
            let (key, rest) = split_help_line(l);
            Line::from(vec![
                Span::styled(format!("{key:<10}"), Style::default().fg(theme.overlay_key)),
                Span::raw(rest.to_string()),
            ])
        })
        .collect();
    Popup {
        title: " keybinds ".to_string(),
        body,
        size_pct: PopupSize {
            width_pct: 60,
            height_pct: 80,
        },
        dismiss_keys: vec![KeyCode::Esc, KeyCode::Char('?')],
        body_margin: Margin {
            horizontal: 2,
            vertical: 1,
        },
        scroll: 0,
    }
    .render(theme, frame, area);
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

/// Renders the severity legend row into `area`. Single-row primitive used
/// by both Scorecard and Spotlight layouts (issue #370).
fn render_legend_bar(theme: &Theme, frame: &mut Frame, area: Rect) {
    let line = severity_legend_line(theme, area.width);
    frame.render_widget(Paragraph::new(line), area);
}

/// Bottom-bar severity legend (issue #370). Decodes the four dashboard
/// glyphs so operators don't have to consult `?` help. At width < 60 the
/// row collapses to glyphs-only so the legend still fits next to the hint
/// row at narrow widths; at 60+ each glyph carries its `Fail`/`Warn`/`OK`/
/// `Unknown` label. Pure read-only; no interaction. Colors come from the
/// active theme so it reads consistently across dracula/nord/gruvbox.
pub fn severity_legend_line(theme: &Theme, width: u16) -> Line<'static> {
    let glyphs_only = width < 60;
    let entries: [(Status, &str); 4] = [
        (Status::Fail, "Fail"),
        (Status::Warn, "Warn"),
        (Status::Ok, "OK"),
        (Status::Unknown, "Unknown"),
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (status, label)) in entries.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(if glyphs_only { " " } else { "  ·  " }));
        }
        spans.push(Span::styled(
            pip_glyph(status).to_string(),
            Style::default().fg(theme_color(theme, status)),
        ));
        if !glyphs_only {
            spans.push(Span::raw(format!(" {label}")));
        }
    }
    Line::from(spans)
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

// ── Mission Control (Layout B) removed in PRD-006 slice 1 (issue #367).
//
// The 2×3 quadrant grid, its tui-big-text verdict header, the Activity
// feed, the per-quadrant zoom path, and the bespoke Layout-B hint bar
// used to live below this point. The scorecard layout now carries the
// full set of operator affordances; the `m` keybinding raises a one-shot
// toast pointing operators at the two remaining views (Scorecard ↔
// Spotlight) and the logs drill. See ADR-0010 "Mission Control shrink"
// amendment.

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
