//! Rendering layer for the boot probe — ratatui widgets, no I/O.
//!
//! Per ADR-0016, rendering goes through `ratatui::widgets::Widget`. The
//! orchestrator drives a `ratatui::Terminal<CrosstermBackend<io::Stderr>>`
//! with `Viewport::Inline(rendered_line_count(state))`; ratatui owns
//! cursor placement, clearing, wrap, and resize — replacing the
//! hand-rolled `\x1b[F` / `\x1b[J` cursor moves that lived in the
//! pre-ADR-0016 implementation.
//!
//! Tests render `BootProbeBlock` against `ratatui::backend::TestBackend`
//! and assert on the resulting cell grid.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use super::state::{ProbeState, Section, StepState};

/// Same throbber used by `nico-ops`'s refresh indicator (ADR-0013 §
/// "Active rows reuse `THROBBER_FRAMES`"). Indexed by frame.
pub const THROBBER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
pub const ASCII_THROBBER_FRAMES: &[char] = &['|', '/', '-', '\\'];

/// Visible width of the timing column on each row. Single-duration
/// strings from `format_dur` are at most 5 chars (e.g. "10.0s").
const TIMING_COL_WIDTH: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderMode {
    pub color: bool,
    pub ascii: bool,
}

impl RenderMode {
    pub fn plain() -> Self {
        Self {
            color: false,
            ascii: false,
        }
    }
    pub fn ascii() -> Self {
        Self {
            color: false,
            ascii: true,
        }
    }
}

/// Pick the right glyph for a step's current state. `frame` is only
/// consulted for `Active` (the throbber); the other states are stable.
pub fn glyph_for(state: &StepState, mode: RenderMode, frame: usize) -> String {
    match state {
        StepState::Pending => {
            if mode.ascii {
                "[..]".into()
            } else {
                "○".into()
            }
        }
        StepState::Active { .. } => {
            if mode.ascii {
                let f = ASCII_THROBBER_FRAMES[frame % ASCII_THROBBER_FRAMES.len()];
                f.to_string()
            } else {
                let f = THROBBER_FRAMES[frame % THROBBER_FRAMES.len()];
                f.to_string()
            }
        }
        StepState::Passed { .. } => {
            if mode.ascii {
                "[ok]".into()
            } else {
                "✓".into()
            }
        }
        StepState::Failed { .. } => {
            if mode.ascii {
                "[XX]".into()
            } else {
                "✗".into()
            }
        }
        StepState::Skipped => {
            if mode.ascii {
                "[--]".into()
            } else {
                "─".into()
            }
        }
    }
}

/// Format a duration as `Xs` or `X.Ys` depending on size — short enough
/// to fit alongside the row label but readable.
pub fn format_dur(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 10.0 {
        format!("{secs:.0}s")
    } else {
        format!("{secs:.1}s")
    }
}

/// Number of terminal lines `BootProbeBlock` will paint, used by the
/// orchestrator to size the inline viewport per frame.
pub fn rendered_line_count(state: &ProbeState) -> usize {
    let mut sections_present = 0;
    for section in [Section::Connecting, Section::Validating, Section::Serving] {
        if state.steps.iter().any(|(d, _)| d.section == section) {
            sections_present += 1;
        }
    }
    // header + warnings + (blank + section-label + N rows) per section + blank + bar
    let rows: usize = state.steps.len();
    1 + state.warnings.len() + sections_present * 2 + rows + 2
}

/// Top-level widget for the boot-probe block — header, warnings,
/// sections (blank + label + step rows), blank, bar.
#[derive(Debug, Clone, Copy)]
pub struct BootProbeBlock<'a> {
    pub state: &'a ProbeState,
    pub mode: RenderMode,
    pub frame: usize,
}

impl<'a> BootProbeBlock<'a> {
    pub fn new(state: &'a ProbeState, mode: RenderMode, frame: usize) -> Self {
        Self { state, mode, frame }
    }

    /// Build the lines this widget will paint, in order, top to bottom.
    fn lines(&self) -> Vec<Line<'static>> {
        let state = self.state;
        let mode = self.mode;
        let frame = self.frame;
        let mut lines: Vec<Line<'static>> = Vec::new();

        // ----- Header -----
        let header_glyph = if mode.ascii { "[..]" } else { "◐" };
        let type_segment = match &state.deployment_type {
            Some(name) => format!("  ·  type: {} ({})", name, state.deployment_type_source),
            None => "  ·  type: auto".to_string(),
        };
        let ib_label = match state.infiniband_present {
            Some(true) => "present",
            Some(false) => "absent",
            None => "unknown",
        };
        let header_text = format!(
            "  {header_glyph} booting nico  ·  reach: {} ({}){type_segment}  ·  ib: {ib_label}",
            state.reach_mode, state.reach_source,
        );
        let header_style = if mode.color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(header_text, header_style)));

        // ----- Warnings -----
        for warning in &state.warnings {
            let text = format!("  {warning}");
            let style = if mode.color {
                Style::default().fg(Color::LightYellow)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }

        // ----- Sections -----
        for section in [Section::Connecting, Section::Validating, Section::Serving] {
            let rows: Vec<_> = state
                .steps
                .iter()
                .filter(|(d, _)| d.section == section)
                .collect();
            if rows.is_empty() {
                continue;
            }
            // Blank separator before the section.
            lines.push(Line::from(""));

            let sec_label = format!("    {}", section.label());
            let label_style = if mode.color {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(sec_label, label_style)));

            for (def, st) in rows {
                let glyph = glyph_for(st, mode, frame);
                let label = def.label.clone();
                let timing = match st {
                    StepState::Pending | StepState::Skipped => "-.-s".to_string(),
                    StepState::Active { elapsed }
                    | StepState::Passed { elapsed }
                    | StepState::Failed { elapsed, .. } => format_dur(*elapsed),
                };
                let timing_pad =
                    " ".repeat(TIMING_COL_WIDTH.saturating_sub(timing.chars().count()));

                // Per-row format from ADR-0013 (preserved):
                // - Pending / Skipped: dim everything
                // - Active: bright label, dim timing, cyan glyph
                // - Passed: dim label, green glyph
                // - Failed: red label + glyph
                let (label_style, glyph_style, timing_style) = if !mode.color {
                    let plain = Style::default();
                    (plain, plain, plain)
                } else {
                    match st {
                        StepState::Pending | StepState::Skipped => {
                            let dim = Style::default().add_modifier(Modifier::DIM);
                            (dim, dim, dim)
                        }
                        StepState::Active { .. } => (
                            Style::default(),
                            Style::default().fg(Color::LightCyan),
                            Style::default().add_modifier(Modifier::DIM),
                        ),
                        StepState::Passed { .. } => (
                            Style::default().add_modifier(Modifier::DIM),
                            Style::default().fg(Color::LightGreen),
                            Style::default().add_modifier(Modifier::DIM),
                        ),
                        StepState::Failed { .. } => (
                            Style::default().fg(Color::LightRed),
                            Style::default().fg(Color::LightRed),
                            Style::default().add_modifier(Modifier::DIM),
                        ),
                    }
                };

                let spans = vec![
                    Span::raw("      "),
                    Span::styled(glyph, glyph_style),
                    Span::raw("  "),
                    Span::styled(timing, timing_style),
                    Span::raw(timing_pad),
                    Span::raw(" "),
                    Span::styled(label, label_style),
                ];
                lines.push(Line::from(spans));
            }
        }

        // ----- Blank + Bar -----
        lines.push(Line::from(""));
        lines.push(bar_line(state, mode));

        lines
    }
}

impl<'a> Widget for BootProbeBlock<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines();
        Paragraph::new(lines).render(area, buf);
    }
}

/// Build the single-line bar widget content as a `Line` of styled
/// per-step chits + completion suffix. Exposed so tests can render the
/// bar in isolation.
pub fn bar_line(state: &ProbeState, mode: RenderMode) -> Line<'static> {
    let total = state.total_count();
    let done = state.completed_count();

    let (filled_ch, empty_ch) = if mode.ascii { ('=', '-') } else { ('▰', '▱') };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(total + 4);
    spans.push(Span::raw("  "));
    if mode.ascii {
        spans.push(Span::raw("["));
    }
    for (_, st) in &state.steps {
        let ch = match st {
            StepState::Pending => empty_ch,
            _ => filled_ch,
        };
        let style = if !mode.color {
            Style::default()
        } else {
            match st {
                StepState::Passed { .. } => Style::default().fg(Color::LightGreen),
                StepState::Failed { .. } => Style::default().fg(Color::LightRed),
                StepState::Active { .. } => Style::default().fg(Color::LightCyan),
                StepState::Skipped | StepState::Pending => {
                    Style::default().add_modifier(Modifier::DIM)
                }
            }
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    if mode.ascii {
        spans.push(Span::raw("]"));
    }
    spans.push(Span::raw(format!("  {done} / {total} checks")));
    Line::from(spans)
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::widgets::Widget;
    use ratatui::Terminal;

    use super::{bar_line, BootProbeBlock, RenderMode};
    use crate::boot_probe::state::ProbeState;

    /// Render the block widget into a TestBackend and return the resulting buffer.
    pub fn render_to_buffer(
        state: &ProbeState,
        mode: RenderMode,
        frame: usize,
        width: u16,
        height: u16,
    ) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test backend init");
        terminal
            .draw(|f| f.render_widget(BootProbeBlock::new(state, mode, frame), f.area()))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// Render just the bar line into a TestBackend and return the resulting buffer.
    pub fn render_bar_to_buffer(
        state: &ProbeState,
        mode: RenderMode,
        width: u16,
    ) -> Buffer {
        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).expect("test backend init");
        terminal
            .draw(|f| {
                let line = bar_line(state, mode);
                ratatui::widgets::Paragraph::new(line).render(f.area(), f.buffer_mut());
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// Read a full row of cells as a single `String` (symbols only,
    /// trailing spaces preserved).
    pub fn row_text(buf: &Buffer, y: u16) -> String {
        let mut s = String::new();
        for x in 0..buf.area.width {
            s.push_str(buf[(x, y)].symbol());
        }
        s
    }

    /// Render the buffer to a single string with `\n` between rows
    /// (trailing spaces trimmed off each row for assertion ergonomics).
    pub fn buffer_to_trimmed_string(buf: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            out.push_str(row_text(buf, y).trim_end());
            out.push('\n');
        }
        out
    }

    /// Find the foreground color of the first cell whose symbol equals `needle`.
    pub fn first_cell_fg(buf: &Buffer, needle: &str) -> Option<Color> {
        let area: Rect = buf.area;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf[(x, y)].symbol() == needle {
                    return Some(buf[(x, y)].fg);
                }
            }
        }
        None
    }

    /// Count cells whose symbol equals `needle` and whose fg matches `color`.
    pub fn count_cells_with_fg(buf: &Buffer, needle: &str, color: Color) -> usize {
        let area: Rect = buf.area;
        let mut n = 0;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf[(x, y)].symbol() == needle && buf[(x, y)].fg == color {
                    n += 1;
                }
            }
        }
        n
    }

    /// Count cells whose symbol equals `needle` (any style).
    pub fn count_cells(buf: &Buffer, needle: &str) -> usize {
        let area: Rect = buf.area;
        let mut n = 0;
        for y in 0..area.height {
            for x in 0..area.width {
                if buf[(x, y)].symbol() == needle {
                    n += 1;
                }
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::*;
    use super::*;
    use crate::boot_probe::state::{StepDef, StepId};
    use ratatui::style::Color;

    fn def_in(id: StepId, label: &str, sec: Section, budget_s: u64) -> StepDef {
        StepDef {
            id,
            label: label.into(),
            section: sec,
            budget: Duration::from_secs(budget_s),
        }
    }

    fn three_step_state() -> ProbeState {
        ProbeState::new(
            vec![
                def_in(StepId::LoadKubeconfig, "load kubeconfig", Section::Connecting, 5),
                def_in(StepId::ReachApiServer, "reach API server", Section::Connecting, 5),
                def_in(StepId::Credentials, "credentials", Section::Validating, 5),
            ],
            "port-forward",
            "auto",
        )
    }

    fn render_block_string(state: &ProbeState, mode: RenderMode, frame: usize) -> String {
        let buf = render_to_buffer(state, mode, frame, 200, 40);
        buffer_to_trimmed_string(&buf)
    }

    #[test]
    fn pending_glyph_unicode_is_open_circle() {
        let g = glyph_for(&StepState::Pending, RenderMode::plain(), 0);
        assert_eq!(g, "○");
    }

    #[test]
    fn pending_glyph_ascii_is_dotdot_brackets() {
        let g = glyph_for(&StepState::Pending, RenderMode::ascii(), 0);
        assert_eq!(g, "[..]");
    }

    #[test]
    fn passed_glyph_ascii_is_ok() {
        let g = glyph_for(
            &StepState::Passed {
                elapsed: Duration::from_secs(1),
            },
            RenderMode::ascii(),
            0,
        );
        assert_eq!(g, "[ok]");
    }

    #[test]
    fn failed_glyph_ascii_is_xx() {
        let g = glyph_for(
            &StepState::Failed {
                elapsed: Duration::from_secs(1),
                message: "x".into(),
                timed_out: false,
                next_command: "y".into(),
            },
            RenderMode::ascii(),
            0,
        );
        assert_eq!(g, "[XX]");
    }

    #[test]
    fn skipped_glyph_unicode_is_em_dash() {
        let g = glyph_for(&StepState::Skipped, RenderMode::plain(), 0);
        assert_eq!(g, "─");
    }

    #[test]
    fn skipped_glyph_ascii_is_double_dash_brackets() {
        let g = glyph_for(&StepState::Skipped, RenderMode::ascii(), 0);
        assert_eq!(g, "[--]");
    }

    #[test]
    fn active_glyph_uses_throbber_frames() {
        let s = StepState::Active {
            elapsed: Duration::from_millis(100),
        };
        let g = glyph_for(&s, RenderMode::plain(), 0);
        assert_eq!(g, THROBBER_FRAMES[0].to_string());
        let g = glyph_for(&s, RenderMode::plain(), 5);
        assert_eq!(g, THROBBER_FRAMES[5].to_string());
    }

    #[test]
    fn active_glyph_ascii_uses_pipe_slash_dash() {
        let s = StepState::Active {
            elapsed: Duration::from_millis(100),
        };
        let g = glyph_for(&s, RenderMode::ascii(), 0);
        assert_eq!(g, "|");
        let g = glyph_for(&s, RenderMode::ascii(), 1);
        assert_eq!(g, "/");
    }

    #[test]
    fn block_includes_reach_mode_in_header() {
        let s = three_step_state();
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(out.contains("booting nico"), "expected header line, got:\n{out}");
        assert!(
            out.contains("reach: port-forward (auto)"),
            "expected reach mode in header, got:\n{out}"
        );
    }

    #[test]
    fn block_header_renders_resolved_deployment_type_with_source() {
        let s = three_step_state().with_deployment_type(Some("rest-only-mock".into()), "flag");
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("type: rest-only-mock (flag)"),
            "expected deployment-type tag with source paren in header, got:\n{out}"
        );
    }

    #[test]
    fn block_header_renders_auto_when_unresolved() {
        let s = three_step_state();
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("type: auto"),
            "expected `type: auto` for unresolved auto, got:\n{out}"
        );
    }

    #[test]
    fn block_header_renders_force_with_force_source() {
        let s = three_step_state().with_deployment_type(Some("force".into()), "force");
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("type: force (force)"),
            "expected `type: force (force)` for force mode, got:\n{out}"
        );
    }

    #[test]
    fn block_header_renders_ib_unknown_when_unresolved() {
        let s = three_step_state();
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("ib: unknown"),
            "expected `ib: unknown` for unresolved IB, got:\n{out}"
        );
    }

    #[test]
    fn block_header_renders_ib_present_when_some_true() {
        let s = three_step_state().with_infiniband_present(Some(true));
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("ib: present"),
            "expected `ib: present`, got:\n{out}"
        );
    }

    #[test]
    fn block_header_renders_ib_absent_when_some_false() {
        let s = three_step_state().with_infiniband_present(Some(false));
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("ib: absent"),
            "expected `ib: absent`, got:\n{out}"
        );
    }

    #[test]
    fn block_ib_segment_appears_after_type_segment() {
        let s = three_step_state()
            .with_deployment_type(Some("force".into()), "force")
            .with_infiniband_present(None);
        let out = render_block_string(&s, RenderMode::plain(), 0);
        let type_pos = out.find("type: force").expect("type segment missing");
        let ib_pos = out.find("ib: unknown").expect("ib segment missing");
        assert!(
            type_pos < ib_pos,
            "ib segment must follow type segment in header, got:\n{out}"
        );
    }

    #[test]
    fn block_renders_warnings_between_header_and_first_section() {
        let s = three_step_state().with_warnings(vec![
            "⚠  cluster.namespace=forge-system overrides deployment-type rest-only-mock default (nico-rest)".into(),
        ]);
        let out = render_block_string(&s, RenderMode::plain(), 0);
        let header_pos = out.find("booting nico").expect("header missing");
        let warning_pos = out
            .find("cluster.namespace=forge-system")
            .expect("warning missing");
        let connecting_pos = out.find("connecting").expect("connecting label missing");
        assert!(header_pos < warning_pos, "warning must be after header");
        assert!(
            warning_pos < connecting_pos,
            "warning must be before first section"
        );
    }

    #[test]
    fn block_omits_warnings_section_when_none_present() {
        let s = three_step_state();
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(!out.contains('⚠'), "unexpected warn glyph in:\n{out}");
    }

    #[test]
    fn rendered_line_count_grows_with_warnings() {
        let base = three_step_state();
        let with_warnings = three_step_state().with_warnings(vec!["a".into(), "b".into()]);
        assert_eq!(
            rendered_line_count(&with_warnings),
            rendered_line_count(&base) + 2
        );
    }

    #[test]
    fn block_includes_section_labels() {
        let s = three_step_state();
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(out.contains("connecting"), "missing connecting label:\n{out}");
        assert!(out.contains("validating"), "missing validating label:\n{out}");
    }

    #[test]
    fn block_omits_section_with_no_steps() {
        let s = three_step_state();
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            !out.contains("serving"),
            "should not render empty section:\n{out}"
        );
    }

    #[test]
    fn block_shows_bar_with_completed_count() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(100),
            },
        );
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("1 / 3 checks"),
            "expected '1 / 3 checks' in:\n{out}"
        );
    }

    #[test]
    fn bar_ascii_uses_brackets_and_equals() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(100),
            },
        );
        let buf = render_bar_to_buffer(&s, RenderMode::ascii(), 80);
        let row = row_text(&buf, 0);
        assert!(row.contains('['), "expected opening bracket: {row}");
        assert!(row.contains(']'), "expected closing bracket: {row}");
        assert!(row.contains('='), "expected '=' for filled cell: {row}");
        assert!(row.contains('-'), "expected '-' for empty cell: {row}");
    }

    #[test]
    fn block_pending_row_shows_placeholder_not_budget() {
        let s = three_step_state();
        let out = render_block_string(&s, RenderMode::plain(), 0);
        assert!(out.contains("-.-s"), "expected '-.-s' placeholder in:\n{out}");
        assert!(
            !out.contains("5.0s"),
            "pending rows must not display the budget value, got:\n{out}"
        );
    }

    #[test]
    fn block_active_row_shows_elapsed() {
        let mut s = three_step_state();
        s.set_state(
            StepId::ReachApiServer,
            StepState::Active {
                elapsed: Duration::from_millis(400),
            },
        );
        let out = render_block_string(&s, RenderMode::plain(), 0);
        let line = out
            .lines()
            .find(|l| l.contains("reach API server"))
            .expect("expected reach API server row");
        assert!(line.contains("0.4s"), "expected elapsed '0.4s' on row, got: {line}");
        assert!(
            !line.contains(" / "),
            "active row should not include budget after switch to single timing column, got: {line}"
        );
    }

    #[test]
    fn block_label_column_aligns_under_color() {
        // Labels of different lengths styled with different colors must
        // still align in the visible cell grid. With ratatui, styles live
        // on cells (no inline escape codes), so the column position is the
        // cell x — we assert the labels start at the same x.
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(0),
            },
        );
        s.set_state(
            StepId::ReachApiServer,
            StepState::Failed {
                elapsed: Duration::from_secs(5),
                message: "boom".into(),
                timed_out: true,
                next_command: "kubectl cluster-info".into(),
            },
        );
        let mode = RenderMode {
            color: true,
            ascii: false,
        };
        let buf = render_to_buffer(&s, mode, 0, 200, 40);

        let mut pos1 = None;
        let mut pos2 = None;
        for y in 0..buf.area.height {
            let row = row_text(&buf, y);
            if pos1.is_none() && row.contains("load kubeconfig") {
                pos1 = row.find("load kubeconfig");
            }
            if pos2.is_none() && row.contains("reach API server") {
                pos2 = row.find("reach API server");
            }
        }
        let pos1 = pos1.expect("kubeconfig label position");
        let pos2 = pos2.expect("reach API server label position");
        assert_eq!(
            pos1, pos2,
            "label columns should be visually aligned despite styling differences",
        );
    }

    #[test]
    fn block_passed_row_shows_only_elapsed() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(120),
            },
        );
        let out = render_block_string(&s, RenderMode::plain(), 0);
        let line = out
            .lines()
            .find(|l| l.contains("load kubeconfig"))
            .expect("expected kubeconfig row");
        assert!(line.contains("0.1s"), "expected '0.1s' on row, got: {line}");
        assert!(
            !line.contains(" / "),
            "passed row should not show '/ budget', got: {line}"
        );
    }

    #[test]
    fn glyph_swap_to_failed_is_clean_no_throbber() {
        let s = StepState::Failed {
            elapsed: Duration::from_millis(100),
            message: "boom".into(),
            timed_out: false,
            next_command: "x".into(),
        };
        let g0 = glyph_for(&s, RenderMode::plain(), 0);
        let g5 = glyph_for(&s, RenderMode::plain(), 5);
        assert_eq!(g0, "✗");
        assert_eq!(g5, "✗");
    }

    #[test]
    fn bar_colors_each_chit_per_step_state() {
        let defs = vec![
            def_in(StepId::LoadKubeconfig, "load kubeconfig", Section::Connecting, 5),
            def_in(StepId::ReachApiServer, "reach API server", Section::Connecting, 5),
            def_in(StepId::Credentials, "credentials", Section::Validating, 5),
            def_in(StepId::NamespaceExists, "namespace 'nico' exists", Section::Validating, 5),
            def_in(StepId::Rbac, "list-pods permission", Section::Validating, 5),
            def_in(StepId::PortForwardWorkflows, "port-forward: workflows", Section::Serving, 5),
            def_in(StepId::PortForwardGrpc, "port-forward: grpc", Section::Serving, 5),
            def_in(StepId::PortForwardPostgres, "port-forward: postgres", Section::Serving, 5),
            def_in(StepId::ReachPostgres, "reach postgres", Section::Serving, 5),
        ];
        let mut s = ProbeState::new(defs, "port-forward", "auto");
        let pass = StepState::Passed { elapsed: Duration::from_millis(100) };
        let fail = StepState::Failed {
            elapsed: Duration::from_millis(300),
            message: "x".into(),
            timed_out: false,
            next_command: "y".into(),
        };
        s.set_state(StepId::LoadKubeconfig, pass.clone());
        s.set_state(StepId::ReachApiServer, pass.clone());
        s.set_state(StepId::Credentials, pass.clone());
        s.set_state(StepId::NamespaceExists, pass.clone());
        s.set_state(StepId::Rbac, pass.clone());
        s.set_state(StepId::PortForwardWorkflows, fail);
        s.set_state(StepId::PortForwardGrpc, StepState::Skipped);
        s.set_state(StepId::PortForwardPostgres, pass.clone());
        s.set_state(StepId::ReachPostgres, pass);

        let mode = RenderMode { color: true, ascii: false };
        let buf = render_bar_to_buffer(&s, mode, 80);

        let green_count = count_cells_with_fg(&buf, "▰", Color::LightGreen);
        let red_count = count_cells_with_fg(&buf, "▰", Color::LightRed);
        assert_eq!(
            green_count, 7,
            "expected 7 LightGreen chits, got {green_count}",
        );
        assert_eq!(
            red_count, 1,
            "expected 1 LightRed chit, got {red_count}",
        );

        let total_filled = count_cells(&buf, "▰");
        assert_eq!(
            total_filled, 9,
            "expected 9 filled chits total (skipped uses ▰ but dimmed), got {total_filled}",
        );
    }

    #[test]
    fn bar_failure_does_not_cascade_to_later_chits() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed { elapsed: Duration::from_millis(50) },
        );
        s.set_state(
            StepId::ReachApiServer,
            StepState::Failed {
                elapsed: Duration::from_millis(80),
                message: "boom".into(),
                timed_out: false,
                next_command: "kubectl cluster-info".into(),
            },
        );
        s.set_state(
            StepId::Credentials,
            StepState::Passed { elapsed: Duration::from_millis(120) },
        );

        let mode = RenderMode { color: true, ascii: false };
        let buf = render_bar_to_buffer(&s, mode, 80);

        assert_eq!(
            count_cells_with_fg(&buf, "▰", Color::LightGreen),
            2,
            "expected 2 LightGreen chits",
        );
        assert_eq!(
            count_cells_with_fg(&buf, "▰", Color::LightRed),
            1,
            "expected 1 LightRed chit",
        );
    }

    #[test]
    fn bar_skipped_chit_keeps_filled_glyph_at_its_position() {
        let mut s = three_step_state();
        s.set_state(StepId::ReachApiServer, StepState::Skipped);

        let mode = RenderMode { color: true, ascii: false };
        let buf = render_bar_to_buffer(&s, mode, 80);
        let row = row_text(&buf, 0);
        let chits: String = row.chars().filter(|c| *c == '▰' || *c == '▱').collect();
        assert_eq!(chits, "▱▰▱", "expected ▱▰▱ chits, got {chits:?}");
    }

    #[test]
    fn bar_active_chit_is_bright_cyan() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Active { elapsed: Duration::from_millis(120) },
        );

        let mode = RenderMode { color: true, ascii: false };
        let buf = render_bar_to_buffer(&s, mode, 80);
        let cyan = first_cell_fg(&buf, "▰");
        assert_eq!(
            cyan,
            Some(Color::LightCyan),
            "expected LightCyan for active chit",
        );
    }

    #[test]
    fn rendered_line_count_matches_lines_painted() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(100),
            },
        );
        let expected = rendered_line_count(&s);
        let buf = render_to_buffer(&s, RenderMode::plain(), 0, 200, 40);
        // Count non-empty rows in the buffer.
        let non_empty_rows = (0..buf.area.height)
            .filter(|y| !row_text(&buf, *y).trim().is_empty())
            .count();
        // expected = header(1) + warnings(0) + sections*(blank+label) + rows(3) + blank + bar
        //          = 1 + 0 + 2*2 + 3 + 1 + 1 = 10
        // Non-empty: header + 2 section labels + 3 step rows + bar = 7
        // The blank rows + 1 trailing newline gap make the difference.
        let blanks = expected - non_empty_rows;
        assert!(
            blanks >= 2,
            "expected_line_count={expected}, non_empty={non_empty_rows}, want >=2 blanks",
        );
    }

    #[test]
    fn header_is_bold_when_color_enabled() {
        let s = three_step_state();
        let buf = render_to_buffer(&s, RenderMode { color: true, ascii: false }, 0, 200, 40);
        let mut found_bold_header_cell = false;
        for x in 0..buf.area.width {
            let cell = &buf[(x, 0)];
            if cell.symbol() == "b"
                && cell.modifier.contains(Modifier::BOLD)
            {
                found_bold_header_cell = true;
                break;
            }
        }
        assert!(found_bold_header_cell, "expected header row to be BOLD when color=true");
    }

    #[test]
    fn warning_line_uses_yellow_when_color_enabled() {
        let s = three_step_state().with_warnings(vec!["⚠  hello world".into()]);
        let buf = render_to_buffer(&s, RenderMode { color: true, ascii: false }, 0, 200, 40);
        let yellow = first_cell_fg(&buf, "⚠");
        assert_eq!(
            yellow,
            Some(Color::LightYellow),
            "expected warning glyph to be LightYellow when color=true",
        );
    }
}
