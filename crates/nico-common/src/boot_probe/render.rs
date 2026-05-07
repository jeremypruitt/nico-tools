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
    let header = format!(
        "  {header_glyph} booting nico  ·  reach: {} ({})",
        state.reach_mode, state.reach_source,
    );
    if mode.color {
        out.push_str(&header.bold().to_string());
    } else {
        out.push_str(&header);
    }
    out.push('\n');

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
                StepState::Pending => format_dur(def.budget),
                StepState::Active { elapsed } => {
                    format!("{} / {}", format_dur(*elapsed), format_dur(def.budget))
                }
                StepState::Passed { elapsed } => format_dur(*elapsed),
                StepState::Failed { elapsed, .. } => format_dur(*elapsed),
                StepState::Skipped => format_dur(def.budget),
            };

            // Per-row format from ADR-0013:
            // - Pending: dim everything; show budget faintly
            // - Active: bright label, dim budget
            // - Passed: dim label, bright glyph
            // - Failed: bright label in red
            // - Skipped: dim everything
            let (label_styled, glyph_styled, timing_styled) = if !mode.color {
                (label.clone(), glyph, timing)
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

            out.push_str(&format!(
                "      {glyph_styled}  {label_styled:<32} {timing_styled}\n",
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
    let any_failed = state.any_failed();
    let all_passed = state.all_passed();

    let (filled_ch, empty_ch) = if mode.ascii { ('=', '-') } else { ('▰', '▱') };

    let mut bar = String::with_capacity(total + 8);
    bar.push_str("  ");
    if mode.ascii {
        bar.push('[');
    }
    for i in 0..total {
        let ch = if i < done { filled_ch } else { empty_ch };
        let s = ch.to_string();
        let styled = if !mode.color {
            s
        } else if any_failed {
            s.bright_red().to_string()
        } else if all_passed {
            s.bright_green().to_string()
        } else if i < done {
            s.bright_cyan().to_string()
        } else {
            s.dimmed().to_string()
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
    // header + (blank + section-label + N rows) per section + blank + bar
    let rows: usize = state.steps.len();
    1 + sections_present * 2 + rows + 2
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
    fn render_block_pending_row_shows_budget() {
        let s = three_step_state();
        let out = render_block(&s, RenderMode::plain(), 0);
        // Pending rows should display the budget so users can sum the
        // worst-case wait at a glance.
        assert!(out.contains("5.0s"), "expected budget '5.0s' in:\n{out}");
    }

    #[test]
    fn render_block_active_row_shows_elapsed_slash_budget() {
        let mut s = three_step_state();
        s.set_state(
            StepId::ReachApiServer,
            StepState::Active {
                elapsed: Duration::from_millis(400),
            },
        );
        let out = render_block(&s, RenderMode::plain(), 0);
        assert!(
            out.contains("0.4s / 5.0s"),
            "expected 'elapsed / budget' format, got:\n{out}"
        );
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
