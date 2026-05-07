//! Non-TTY degradation: per-event log lines instead of an animated
//! block. ADR-0013 § "Non-TTY (piped stderr, CI logs)" requires one
//! `nico: <step>: ok (Xs)` / `: failed: <msg>` / `: timed out after Xs`
//! / `: skipped` line per state transition.

use std::time::Duration;

use super::render::format_dur;
use super::state::{StepDef, StepState};

/// Render the log line for a single state transition. Returns `None`
/// for the `Pending` state (no log needed before the step starts) and
/// for `Active` (the start log line is emitted by `start_line`).
pub fn transition_line(def: &StepDef, state: &StepState) -> Option<String> {
    match state {
        StepState::Pending | StepState::Active { .. } => None,
        StepState::Passed { elapsed } => {
            Some(format!("nico: {}: ok ({})", def.label, format_dur(*elapsed)))
        }
        StepState::Failed {
            elapsed: _,
            message,
            timed_out,
            next_command: _,
        } => {
            if *timed_out {
                Some(format!("nico: {}: {}", def.label, message))
            } else {
                Some(format!("nico: {}: failed: {}", def.label, message))
            }
        }
        StepState::Skipped => Some(format!("nico: {}: skipped", def.label)),
    }
}

/// Final one-line success receipt printed above the TUI on a successful
/// boot probe. ADR-0013 example: `nico: cluster ready (9 checks · 1.6s)`.
pub fn success_receipt(check_count: usize, total_elapsed: Duration) -> String {
    format!(
        "nico: cluster ready ({check_count} checks · {})",
        format_dur(total_elapsed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot_probe::state::{Section, StepId};

    fn def(id: StepId, label: &str) -> StepDef {
        StepDef {
            id,
            label: label.into(),
            section: Section::Validating,
            budget: Duration::from_secs(1),
        }
    }

    #[test]
    fn passed_emits_ok_line_with_elapsed() {
        let d = def(StepId::Credentials, "credentials");
        let s = StepState::Passed {
            elapsed: Duration::from_millis(400),
        };
        assert_eq!(
            transition_line(&d, &s).unwrap(),
            "nico: credentials: ok (0.4s)"
        );
    }

    #[test]
    fn failed_emits_failed_line_with_message() {
        let d = def(StepId::Rbac, "list-pods permission");
        let s = StepState::Failed {
            elapsed: Duration::from_millis(200),
            message: "denied".into(),
            timed_out: false,
            next_command: "kubectl auth can-i ...".into(),
        };
        assert_eq!(
            transition_line(&d, &s).unwrap(),
            "nico: list-pods permission: failed: denied"
        );
    }

    #[test]
    fn timed_out_emits_timed_out_line() {
        let d = def(StepId::Credentials, "credentials");
        let s = StepState::Failed {
            elapsed: Duration::from_secs(5),
            message: "timed out after 5s".into(),
            timed_out: true,
            next_command: "kubectl auth whoami".into(),
        };
        let line = transition_line(&d, &s).unwrap();
        assert_eq!(line, "nico: credentials: timed out after 5s");
        assert!(!line.contains("failed:"), "should not double-prefix: {line}");
    }

    #[test]
    fn skipped_emits_skipped_line() {
        let d = def(StepId::ReachPostgres, "reach postgres");
        let s = StepState::Skipped;
        assert_eq!(
            transition_line(&d, &s).unwrap(),
            "nico: reach postgres: skipped"
        );
    }

    #[test]
    fn pending_and_active_emit_nothing() {
        let d = def(StepId::Credentials, "credentials");
        assert!(transition_line(&d, &StepState::Pending).is_none());
        assert!(
            transition_line(
                &d,
                &StepState::Active {
                    elapsed: Duration::from_millis(10)
                }
            )
            .is_none()
        );
    }

    #[test]
    fn success_receipt_matches_adr_format() {
        let line = success_receipt(9, Duration::from_millis(1600));
        assert_eq!(line, "nico: cluster ready (9 checks · 1.6s)");
    }
}
