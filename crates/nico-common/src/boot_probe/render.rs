//! Rendering layer for the boot probe — pure functions, no I/O.
//!
//! The renderer takes a `ProbeState` and a `RenderMode` and returns a
//! `String` (the multi-line block, ready to be written to stderr by the
//! orchestrator). Tests assert against the returned string so we can
//! verify glyph swaps, ASCII fallback, color toggling, and bar rendering
//! without any terminal I/O.

use std::time::Duration;

use owo_colors::OwoColorize;

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

/// Render the multi-line boot probe block — header + sections + bar.
/// `frame` rotates the active throbber. Color is suppressed when
/// `mode.color = false`.
pub fn render_block(state: &ProbeState, mode: RenderMode, frame: usize) -> String {
    let mut out = String::new();

    // Header
    let header_glyph = if mode.ascii { "[..]" } else { "◐" };
    let type_segment = match &state.deployment_type {
        Some(name) => format!("  ·  type: {} ({})", name, state.deployment_type_source),
        None => "  ·  type: auto".to_string(),
    };
    let header = format!(
        "  {header_glyph} booting nico  ·  reach: {} ({}){type_segment}",
        state.reach_mode, state.reach_source,
    );
    if mode.color {
        out.push_str(&header.bold().to_string());
    } else {
        out.push_str(&header);
    }
    out.push('\n');

    // Override-conflict warnings (PRD-001 slice 5). One line per key
    // immediately under the header, before the first section.
    for warning in &state.warnings {
        let line = format!("  {warning}");
        if mode.color {
            out.push_str(&line.bright_yellow().to_string());
        } else {
            out.push_str(&line);
        }
        out.push('\n');
    }

    // Sections, in fixed topological order. We only render sections
    // that actually have steps assigned to them.
    for section in [Section::Connecting, Section::Validating, Section::Serving] {
        let rows: Vec<_> = state
            .steps
            .iter()
            .filter(|(d, _)| d.section == section)
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push('\n');
        let sec_label = format!("    {}", section.label());
        if mode.color {
            out.push_str(&sec_label.dimmed().to_string());
        } else {
            out.push_str(&sec_label);
        }
        out.push('\n');

        for (def, st) in rows {
            let glyph = glyph_for(st, mode, frame);
            let label = &def.label;
            let timing = match st {
                StepState::Pending | StepState::Skipped => "-.-s".to_string(),
                StepState::Active { elapsed } => format_dur(*elapsed),
                StepState::Passed { elapsed } => format_dur(*elapsed),
                StepState::Failed { elapsed, .. } => format_dur(*elapsed),
            };

            // Per-row format from ADR-0013:
            // - Pending: dim everything; show budget faintly
            // - Active: bright label, dim budget
            // - Passed: dim label, bright glyph
            // - Failed: bright label in red
            // - Skipped: dim everything
            let (label_styled, glyph_styled, timing_styled) = if !mode.color {
                (label.clone(), glyph, timing.clone())
            } else {
                match st {
                    StepState::Pending | StepState::Skipped => (
                        label.dimmed().to_string(),
                        glyph.dimmed().to_string(),
                        timing.dimmed().to_string(),
                    ),
                    StepState::Active { .. } => (
                        label.clone(),
                        glyph.bright_cyan().to_string(),
                        timing.dimmed().to_string(),
                    ),
                    StepState::Passed { .. } => (
                        label.dimmed().to_string(),
                        glyph.bright_green().to_string(),
                        timing.dimmed().to_string(),
                    ),
                    StepState::Failed { .. } => (
                        label.bright_red().to_string(),
                        glyph.bright_red().to_string(),
                        timing.dimmed().to_string(),
                    ),
                }
            };

            // Pad the timing column to a fixed *visible* width so labels
            // line up regardless of ANSI escape codes on the styled
            // strings. `format_dur` outputs at most 5 chars in practice
            // (e.g. "10.0s"), so 5 is the steady-state column width.
            let timing_pad = " ".repeat(TIMING_COL_WIDTH.saturating_sub(timing.chars().count()));

            out.push_str(&format!(
                "      {glyph_styled}  {timing_styled}{timing_pad} {label_styled}\n",
            ));
        }
    }

    // Bar
    out.push('\n');
    out.push_str(&render_bar(state, mode));
    out.push('\n');

    out
}

pub fn render_bar(state: &ProbeState, mode: RenderMode) -> String {
    let total = state.total_count();
    let done = state.completed_count();

    let (filled_ch, empty_ch) = if mode.ascii { ('=', '-') } else { ('▰', '▱') };

    let mut bar = String::with_capacity(total + 8);
    bar.push_str("  ");
    if mode.ascii {
        bar.push('[');
    }
    // Each chit reflects its own step's state — no global cascade.
    for (_, st) in &state.steps {
        let ch = match st {
            StepState::Pending => empty_ch,
            _ => filled_ch,
        };
        let s = ch.to_string();
        let styled = if !mode.color {
            s
        } else {
            match st {
                StepState::Passed { .. } => s.bright_green().to_string(),
                StepState::Failed { .. } => s.bright_red().to_string(),
                StepState::Active { .. } => s.bright_cyan().to_string(),
                StepState::Skipped | StepState::Pending => s.dimmed().to_string(),
            }
        };
        bar.push_str(&styled);
    }
    if mode.ascii {
        bar.push(']');
    }
    bar.push_str(&format!("  {done} / {total} checks"));
    bar
}

/// Number of terminal lines `render_block` will print, used by the live
/// orchestrator to know how many lines to clear before the next frame.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot_probe::state::{StepDef, StepId};

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
    fn render_block_includes_reach_mode_in_header() {
        let s = three_step_state();
        let out = render_block(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("booting nico"),
            "expected header line, got:\n{out}"
        );
        assert!(
            out.contains("reach: port-forward (auto)"),
            "expected reach mode in header, got:\n{out}"
        );
    }

    #[test]
    fn render_block_header_renders_resolved_deployment_type_with_source() {
        let s = three_step_state()
            .with_deployment_type(Some("rest-only-mock".into()), "flag");
        let out = render_block(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("type: rest-only-mock (flag)"),
            "expected deployment-type tag with source paren in header, got:\n{out}"
        );
    }

    #[test]
    fn render_block_header_renders_auto_when_unresolved() {
        // Default ProbeState (auto, unresolved) — banner shows `type: auto`.
        let s = three_step_state();
        let out = render_block(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("type: auto"),
            "expected `type: auto` for unresolved auto, got:\n{out}"
        );
    }

    #[test]
    fn render_block_header_renders_force_with_force_source() {
        let s = three_step_state()
            .with_deployment_type(Some("force".into()), "force");
        let out = render_block(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("type: force (force)"),
            "expected `type: force (force)` for force mode, got:\n{out}"
        );
    }

    #[test]
    fn render_block_renders_warnings_between_header_and_first_section() {
        let s = three_step_state().with_warnings(vec![
            "⚠  cluster.namespace=forge-system overrides deployment-type rest-only-mock default (nico-rest)".into(),
        ]);
        let out = render_block(&s, RenderMode::plain(), 0);
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
    fn render_block_omits_warnings_section_when_none_present() {
        let s = three_step_state();
        let out = render_block(&s, RenderMode::plain(), 0);
        // No "⚠" character anywhere when there are no warnings.
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
    fn render_block_includes_section_labels() {
        let s = three_step_state();
        let out = render_block(&s, RenderMode::plain(), 0);
        assert!(out.contains("connecting"), "missing connecting label:\n{out}");
        assert!(out.contains("validating"), "missing validating label:\n{out}");
    }

    #[test]
    fn render_block_omits_section_with_no_steps() {
        let s = three_step_state();
        let out = render_block(&s, RenderMode::plain(), 0);
        // No 'serving' steps in this fixture.
        assert!(
            !out.contains("serving"),
            "should not render empty section:\n{out}"
        );
    }

    #[test]
    fn render_block_shows_bar_with_completed_count() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(100),
            },
        );
        let out = render_block(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("1 / 3 checks"),
            "expected '1 / 3 checks' in:\n{out}"
        );
    }

    #[test]
    fn render_bar_ascii_uses_brackets_and_equals() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(100),
            },
        );
        let out = render_bar(&s, RenderMode::ascii());
        assert!(out.contains('['), "expected opening bracket: {out}");
        assert!(out.contains(']'), "expected closing bracket: {out}");
        assert!(out.contains('='), "expected '=' for filled cell: {out}");
        assert!(out.contains('-'), "expected '-' for empty cell: {out}");
    }

    #[test]
    fn render_block_pending_row_shows_placeholder_not_budget() {
        let s = three_step_state();
        let out = render_block(&s, RenderMode::plain(), 0);
        // Pending rows must not parrot the timeout budget — that reads
        // as a static elapsed counter that never moves. Use a clearly
        // unstarted placeholder instead.
        assert!(out.contains("-.-s"), "expected '-.-s' placeholder in:\n{out}");
        assert!(
            !out.contains("5.0s"),
            "pending rows must not display the budget value, got:\n{out}"
        );
    }

    #[test]
    fn render_block_active_row_shows_elapsed() {
        let mut s = three_step_state();
        s.set_state(
            StepId::ReachApiServer,
            StepState::Active {
                elapsed: Duration::from_millis(400),
            },
        );
        let out = render_block(&s, RenderMode::plain(), 0);
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
    fn render_block_label_column_aligns_under_color() {
        // Labels of different lengths styled with different colors must
        // still align in the *visible* output (i.e. after stripping
        // ANSI escape codes). Guards against the prior bug where
        // `:<32` padded the styled string and let escape-code length
        // shift visible columns.
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
        let out = render_block(&s, mode, 0);
        let line1 = strip_ansi(
            out.lines()
                .find(|l| l.contains("load kubeconfig"))
                .expect("expected kubeconfig row"),
        );
        let line2 = strip_ansi(
            out.lines()
                .find(|l| l.contains("reach API server"))
                .expect("expected reach API server row"),
        );
        let pos1 = line1
            .find("load kubeconfig")
            .expect("kubeconfig label position");
        let pos2 = line2
            .find("reach API server")
            .expect("reach API server label position");
        assert_eq!(
            pos1, pos2,
            "label columns should be visually aligned despite styling differences\n\
             line1: {line1}\nline2: {line2}",
        );
    }

    fn strip_ansi(s: &str) -> String {
        // Tiny CSI stripper: drops `ESC [ ... m` sequences.
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("input was valid utf-8")
    }

    #[test]
    fn render_block_passed_row_shows_only_elapsed() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(120),
            },
        );
        let out = render_block(&s, RenderMode::plain(), 0);
        // Look for the line containing the kubeconfig label.
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
        // ADR-0013: failed bullet does a clean instant glyph swap to ✗
        let s = StepState::Failed {
            elapsed: Duration::from_millis(100),
            message: "boom".into(),
            timed_out: false,
            next_command: "x".into(),
        };
        // Frame is irrelevant for failed — same glyph regardless.
        let g0 = glyph_for(&s, RenderMode::plain(), 0);
        let g5 = glyph_for(&s, RenderMode::plain(), 5);
        assert_eq!(g0, "✗");
        assert_eq!(g5, "✗");
    }

    #[test]
    fn render_bar_colors_each_chit_per_step_state() {
        // Mirror the bug-report repro: 7 passed, 1 failed, 1 skipped.
        // Each chit should be colored independently. A failure must NOT
        // cascade visually — the chits for steps that passed *after* the
        // failed step still render green.
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
        let bar = render_bar(&s, mode);

        let green_count = bar.matches("\x1b[92m").count();
        let red_count = bar.matches("\x1b[91m").count();
        assert_eq!(
            green_count, 7,
            "expected 7 bright_green chits, got {green_count}\nbar: {bar:?}",
        );
        assert_eq!(
            red_count, 1,
            "expected 1 bright_red chit, got {red_count}\nbar: {bar:?}",
        );

        // Strip ANSI to confirm all 9 chits keep the filled glyph (skipped
        // does not switch glyphs — it is just dimmed).
        let plain = strip_ansi(&bar);
        let chit_run: String = plain.chars().filter(|c| *c == '▰' || *c == '▱').collect();
        assert_eq!(
            chit_run, "▰▰▰▰▰▰▰▰▰",
            "expected 9 filled chits, got: {chit_run:?}",
        );
    }

    #[test]
    fn render_bar_failure_does_not_cascade_to_later_chits() {
        // 3 steps: passed, failed, passed. The third chit must still be
        // green — a failure on chit 2 does not turn chit 3 red.
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
        let bar = render_bar(&s, mode);

        assert_eq!(
            bar.matches("\x1b[92m").count(),
            2,
            "expected 2 green chits, got bar: {bar:?}",
        );
        assert_eq!(
            bar.matches("\x1b[91m").count(),
            1,
            "expected 1 red chit, got bar: {bar:?}",
        );
    }

    #[test]
    fn render_bar_skipped_chit_keeps_filled_glyph_at_its_position() {
        // Glyph is per-step now: only chit 2 (the skipped step) is filled
        // — chits 1 and 3 are pending and stay empty. This case
        // distinguishes the per-step rendering from the old left-to-right
        // progress fill, which would produce "▰▱▱" instead.
        let mut s = three_step_state();
        s.set_state(StepId::ReachApiServer, StepState::Skipped);
        // LoadKubeconfig and Credentials left Pending.

        let mode = RenderMode { color: true, ascii: false };
        let bar = render_bar(&s, mode);
        let plain = strip_ansi(&bar);
        let chits: String = plain.chars().filter(|c| *c == '▰' || *c == '▱').collect();
        assert_eq!(chits, "▱▰▱", "expected ▱▰▱, got {chits:?}\nbar: {bar:?}");
    }

    #[test]
    fn render_bar_active_chit_is_bright_cyan() {
        // Only chit 1 is Active; the others are Pending. Under the old
        // global-state coloring, no chit would be cyan (done=0 means all
        // chits get dimmed). Per-step coloring puts cyan on the active
        // chit specifically.
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Active { elapsed: Duration::from_millis(120) },
        );

        let mode = RenderMode { color: true, ascii: false };
        let bar = render_bar(&s, mode);
        assert!(
            bar.contains("\x1b[96m"),
            "expected bright_cyan for active chit, got: {bar:?}",
        );
    }

    #[test]
    fn rendered_line_count_matches_actual_lines_emitted() {
        let mut s = three_step_state();
        s.set_state(
            StepId::LoadKubeconfig,
            StepState::Passed {
                elapsed: Duration::from_millis(100),
            },
        );
        let out = render_block(&s, RenderMode::plain(), 0);
        let expected = rendered_line_count(&s);
        // render_block ends with a trailing newline; lines() counts
        // non-trailing-newline lines, so accept ±1.
        let actual = out.lines().count();
        assert!(
            (actual as i64 - expected as i64).abs() <= 1,
            "rendered_line_count={expected} but actual lines={actual}\nblock:\n{out}",
        );
    }
}

