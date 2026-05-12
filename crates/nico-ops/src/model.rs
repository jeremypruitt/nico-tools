use chrono::{DateTime, Utc};
use nico_common::output::Status;
use nico_correlate::id::{IdType, detect_id_type};

/// A single warning/failure line attached to a Layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub status: Status,
    pub message: String,
    pub next_command: Option<String>,
    /// Optional deep-link a Layer can attach so the Spotlight `[o]` action
    /// has a sensible URL to hand to the system browser (e.g. a Grafana
    /// dashboard or Temporal Web UI workflow page). `None` means the
    /// `[o]` action raises a toast instead.
    pub link: Option<String>,
}

/// PRD-007: a typed pointer at one entity (workflow / host / DPU / request)
/// the correlate drill-down popup is opened against. Constructed from
/// `Finding.message` text via [`extract_entity_from_finding`] using the
/// shared [`detect_id_type`] vocabulary so doctor / correlate / ops agree
/// on what an entity ID looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub id: String,
    pub id_type: IdType,
}

/// Severity of a single timeline entry inside the quick-correlate popover.
/// Mirrors `nico_correlate::event::Severity` but is decoupled so the
/// renderer never has to depend on the correlate crate's internal types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopoverSeverity {
    Info,
    Warning,
    Error,
}

/// One row in the quick-correlate Timeline. Source-attributed; severity
/// drives the color in the renderer. `kind` carries the event type
/// (`WorkflowExecutionStarted`, `provision_fail`, `source_error`, …).
#[derive(Debug, Clone, PartialEq)]
pub struct PopoverEvent {
    pub ts: DateTime<Utc>,
    pub source: String,
    pub kind: String,
    pub message: String,
    pub severity: PopoverSeverity,
}

/// Either the correlate run is still in flight (`Loading`) or it has
/// landed and the renderer has events + per-source error lines to show.
#[derive(Debug, Clone, PartialEq)]
pub enum CorrelateStatus {
    Loading,
    Loaded {
        events: Vec<PopoverEvent>,
        source_errors: Vec<SourceError>,
    },
}

/// One Source that errored during the correlate run. Rendered inline as
/// a synthetic `source_error` event (mirrors ADR-007's tail-mode behavior).
#[derive(Debug, Clone, PartialEq)]
pub struct SourceError {
    pub name: String,
    pub reason: String,
}

/// PRD-007: condensed mirror of [`nico_correlate::diagnosis::Diagnosis`]
/// for the correlate mini-dashboard popup banner. Drops the `activity`
/// field (popup banner is two lines: pattern + error_signature). Mirroring
/// — rather than re-exporting — keeps the renderer free of correlate's
/// internal types, same pattern as [`PopoverSeverity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopoverDiagnosis {
    pub pattern: String,
    pub error_signature: String,
    pub next_commands: Vec<String>,
}

/// The full state the correlate popup needs: which entity it's for, the
/// in-flight / loaded payload, and the optional Diagnosis banner.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrelateState {
    pub entity: EntityRef,
    pub status: CorrelateStatus,
    pub diagnosis: Option<PopoverDiagnosis>,
}

/// PRD-007: pull the first entity (DPU / workflow / host / request) out
/// of a `Finding.message`. Tokens are split on whitespace and trimmed of
/// surrounding non-id punctuation before being run through
/// [`detect_id_type`]. Returns `None` when no token matches — drives the
/// "no entity found in this row" toast.
pub fn extract_entity_from_finding(f: &Finding) -> Option<EntityRef> {
    f.message.split_whitespace().find_map(|raw| {
        let tok = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_');
        detect_id_type(tok).map(|id_type| EntityRef {
            id: tok.to_string(),
            id_type,
        })
    })
}

/// One line in the snapshot logs panel. `ts` is the snapshot fetch time
/// (no per-entry timestamps are preserved by the shared `LogSource` API
/// today — that's a follow-up). `level` drives the glyph + color.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub ts: DateTime<Utc>,
    pub pod: String,
    pub level: Status,
    pub message: String,
}

/// Heuristic level inference from raw log text. Matches the same
/// substring discipline used by `nico_doctor::log_source::is_error_line`
/// so the operator's mental model stays consistent.
pub fn log_level_from_text(s: &str) -> Status {
    let l = s.to_lowercase();
    if l.contains("panic") || l.contains("fatal") {
        Status::Fail
    } else if l.contains("error") || l.contains("warn") {
        Status::Warn
    } else {
        Status::Unknown
    }
}

/// What a single Layer scorecard shows: its aggregate status, a one-line
/// evidence summary, and the underlying findings used by the drill panel
/// and the detail overlay. `duration_ms` carries the layer's reported
/// runtime so the ring buffer can record per-layer durations without a
/// second pass over the raw `LayerResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct LayerSnapshot {
    pub name: String,
    pub status: Status,
    pub evidence: String,
    pub findings: Vec<Finding>,
    pub duration_ms: u64,
}

/// Computes the overall verdict word across all layers in the snapshot:
/// `Fail` > `Warn` > `Unknown` > `Skipped` > `Ok`. Empty input is `Ok`.
pub fn overall_verdict(snapshots: &[LayerSnapshot]) -> Status {
    if snapshots.iter().any(|s| s.status == Status::Fail) {
        Status::Fail
    } else if snapshots.iter().any(|s| s.status == Status::Warn) {
        Status::Warn
    } else if snapshots.iter().any(|s| s.status == Status::Unknown) {
        Status::Unknown
    } else {
        Status::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(status: Status) -> LayerSnapshot {
        LayerSnapshot {
            name: "x".into(),
            status,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }
    }

    #[test]
    fn empty_snapshots_verdict_is_ok() {
        assert_eq!(overall_verdict(&[]), Status::Ok);
    }

    #[test]
    fn fail_dominates_warn() {
        let s = vec![snap(Status::Warn), snap(Status::Fail), snap(Status::Ok)];
        assert_eq!(overall_verdict(&s), Status::Fail);
    }

    #[test]
    fn warn_dominates_unknown_and_ok() {
        let s = vec![snap(Status::Unknown), snap(Status::Warn), snap(Status::Ok)];
        assert_eq!(overall_verdict(&s), Status::Warn);
    }

    #[test]
    fn unknown_dominates_ok() {
        let s = vec![snap(Status::Unknown), snap(Status::Ok)];
        assert_eq!(overall_verdict(&s), Status::Unknown);
    }

    #[test]
    fn all_ok_is_ok() {
        let s = vec![snap(Status::Ok), snap(Status::Ok)];
        assert_eq!(overall_verdict(&s), Status::Ok);
    }

    fn finding(msg: &str) -> Finding {
        Finding {
            status: Status::Warn,
            message: msg.into(),
            next_command: None,
            link: None,
        }
    }

    #[test]
    fn extract_entity_finds_wf_prefix_token_as_workflow() {
        let f = finding("stuck_workflow: wf-001 (HostProvisioning): 47m running");
        let e = extract_entity_from_finding(&f).unwrap();
        assert_eq!(e.id, "wf-001");
        assert_eq!(e.id_type, IdType::Workflow);
    }

    #[test]
    fn extract_entity_finds_hp_prefix_token_as_workflow() {
        let f = finding("failed_workflow: hp-7f3a (HostDecommission): failed");
        let e = extract_entity_from_finding(&f).unwrap();
        assert_eq!(e.id, "hp-7f3a");
        assert_eq!(e.id_type, IdType::Workflow);
    }

    #[test]
    fn extract_entity_finds_dpu_prefix_token_as_dpu() {
        // PRD-007 Slice 0 — the "DPU id in Spotlight finding row" path.
        let f = finding("dpu-r12u5 disconnected at 14:32 (link down 5m)");
        let e = extract_entity_from_finding(&f).unwrap();
        assert_eq!(e.id, "dpu-r12u5");
        assert_eq!(e.id_type, IdType::Dpu);
    }

    #[test]
    fn extract_entity_strips_surrounding_punctuation() {
        // Operators tend to bracket entity refs inline: "[dpu-r12u5]".
        let f = finding("incident: [dpu-r12u5] flapping");
        let e = extract_entity_from_finding(&f).unwrap();
        assert_eq!(e.id, "dpu-r12u5");
        assert_eq!(e.id_type, IdType::Dpu);
    }

    #[test]
    fn extract_entity_returns_none_when_no_token_matches() {
        let f = finding("0 stuck, 0 failed");
        assert_eq!(extract_entity_from_finding(&f), None);
    }

    #[test]
    fn extract_entity_picks_first_matching_token() {
        // Slice 0 takes the first match; chooser comes in Slice 1.
        let f = finding("host-r12u5 had dpu-bf3-r12u5 disconnect");
        let e = extract_entity_from_finding(&f).unwrap();
        assert_eq!(e.id, "host-r12u5");
        assert_eq!(e.id_type, IdType::Host);
    }

    #[test]
    fn log_level_from_text_promotes_panic_and_fatal_to_fail() {
        assert_eq!(log_level_from_text("PANIC: nil deref"), Status::Fail);
        assert_eq!(log_level_from_text("fatal: oom"), Status::Fail);
    }

    #[test]
    fn log_level_from_text_classifies_error_and_warn_as_warn() {
        assert_eq!(log_level_from_text("ERROR: disk full"), Status::Warn);
        assert_eq!(log_level_from_text("WARN: deprecated api"), Status::Warn);
    }

    #[test]
    fn log_level_from_text_falls_back_to_unknown() {
        assert_eq!(log_level_from_text("unhelpful trace"), Status::Unknown);
    }
}
