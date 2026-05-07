use nico_common::output::Status;

/// A single warning/failure line attached to a Layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub status: Status,
    pub message: String,
    pub next_command: Option<String>,
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
}
