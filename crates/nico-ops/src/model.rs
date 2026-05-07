use chrono::{DateTime, Utc};
use nico_common::output::Status;

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

/// The full state the correlate popover needs: which workflow it's for,
/// and the in-flight / loaded payload.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrelateState {
    pub workflow_id: String,
    pub status: CorrelateStatus,
}

/// Heuristic: pull a workflow ID out of a `Finding.message` produced by
/// the workflows Layer. Matches the first whitespace-separated token that
/// starts with `wf-` or `hp-` (the prefixes recognized by
/// `nico_correlate::id::detect_id_type`). Returns `None` when no token
/// matches — used to decide whether `[c]` should open the popover.
pub fn workflow_id_from_finding(f: &Finding) -> Option<String> {
    f.message
        .split_whitespace()
        .find(|tok| tok.starts_with("wf-") || tok.starts_with("hp-"))
        .map(|s| s.to_string())
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
    fn workflow_id_extracts_wf_prefix_token() {
        let f = finding("stuck_workflow: wf-001 (HostProvisioning): 47m running");
        assert_eq!(workflow_id_from_finding(&f).as_deref(), Some("wf-001"));
    }

    #[test]
    fn workflow_id_extracts_hp_prefix_token() {
        let f = finding("failed_workflow: hp-7f3a (HostDecommission): failed");
        assert_eq!(workflow_id_from_finding(&f).as_deref(), Some("hp-7f3a"));
    }

    #[test]
    fn workflow_id_returns_none_when_no_prefixed_token() {
        let f = finding("0 stuck, 0 failed");
        assert_eq!(workflow_id_from_finding(&f), None);
    }
}
