//! `ib_verdict()` — pure reduction of an [`IbSnapshot`] to an
//! [`AxisSummary`]. Encodes the precedence ladder PRD-004 specifies:
//! `Fail` (any port with empty `fabric_id` or `lid == 0xffff`) >
//! `Warn` (UFM unobservable, stale observation, or any IB-typed
//! `HealthReport` alert) > `Ok`. `Unknown` when the DPU has no
//! observation at all.
//!
//! The precedence is "sticky": once a Fail trigger appears in any port
//! the verdict is Fail regardless of how many warns are also present —
//! the rollup at slices 4 + 5 picks the most severe headline per
//! axis. Warn triggers compose by category but only one Warn message
//! is emitted (the first hit) so the headline stays short; the layer
//! renderer puts the per-port detail underneath for the operator.

use std::time::Duration;

use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::infiniband::{IbSnapshot, LID_UNASSIGNED};
use crate::verdicts::AxisSummary;

/// The axis name shared across every ib-verdict caller. Equal to the
/// [`crate::layer::Layer::name`] returned by
/// [`crate::layers::infiniband::InfinibandLayer`] so a rollup can join
/// the verdict back to its source layer.
pub const AXIS: &str = "infiniband";

/// Reduce an [`IbSnapshot`] to a single [`AxisSummary`]. `now` is the
/// caller's clock so the function stays pure.
pub fn ib_verdict(
    snapshot: &IbSnapshot,
    now: DateTime<Utc>,
    stale_threshold: Duration,
) -> AxisSummary {
    let dpu_id = &snapshot.dpu_id;

    if snapshot.observed_at.is_none() && snapshot.ports.is_empty() {
        return AxisSummary {
            axis: AXIS,
            status: Status::Unknown,
            message: format!(
                "dpu {dpu_id} infiniband: no recent infiniband_status_observation"
            ),
            next_command: Some(format!(
                "nico correlate {dpu_id} # last activity for this DPU"
            )),
        };
    }

    if let Some(reason) = fail_reason(snapshot) {
        return AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!("dpu {dpu_id} infiniband: {reason}"),
            next_command: Some(format!(
                "nico doctor infiniband {dpu_id} # inspect per-port detail"
            )),
        };
    }

    if let Some(reason) = warn_reason(snapshot, now, stale_threshold) {
        return AxisSummary {
            axis: AXIS,
            status: Status::Warn,
            message: format!("dpu {dpu_id} infiniband: {reason}"),
            next_command: Some(format!(
                "nico doctor infiniband {dpu_id} # inspect per-port detail"
            )),
        };
    }

    let port_count = snapshot.ports.len();
    AxisSummary {
        axis: AXIS,
        status: Status::Ok,
        message: format!("dpu {dpu_id} infiniband healthy: {port_count} port(s) active"),
        next_command: None,
    }
}

fn fail_reason(snapshot: &IbSnapshot) -> Option<String> {
    let unassigned_lid = snapshot
        .ports
        .iter()
        .filter(|p| p.lid == LID_UNASSIGNED)
        .count();
    let empty_fabric = snapshot
        .ports
        .iter()
        .filter(|p| p.fabric_id.is_empty())
        .count();

    match (empty_fabric, unassigned_lid) {
        (0, 0) => None,
        (n, 0) => Some(format!("{n} port(s) with no fabric_id")),
        (0, n) => Some(format!("{n} port(s) with unassigned lid (0xffff)")),
        (f, l) => Some(format!(
            "{f} port(s) with no fabric_id, {l} with unassigned lid"
        )),
    }
}

fn warn_reason(
    snapshot: &IbSnapshot,
    now: DateTime<Utc>,
    stale_threshold: Duration,
) -> Option<String> {
    if let Some(observed_at) = snapshot.observed_at {
        let age = (now - observed_at).to_std().unwrap_or(Duration::ZERO);
        if age > stale_threshold {
            return Some(format!(
                "observation stale ({}s old, threshold {}s)",
                age.as_secs(),
                stale_threshold.as_secs()
            ));
        }
    }

    if matches!(snapshot.ufm_observable, Some(false)) {
        return Some("UFM unobservable".to_string());
    }

    if !snapshot.ib_alerts.is_empty() {
        let n = snapshot.ib_alerts.len();
        return Some(format!("{n} IB alert(s) active"));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infiniband::{IbAlert, IbPort, DEFAULT_OBSERVATION_STALE_THRESHOLD};

    fn port_active(guid: &str, fabric: &str, lid: u32) -> IbPort {
        IbPort {
            guid: guid.into(),
            fabric_id: fabric.into(),
            lid,
            port_state: "Active".into(),
        }
    }

    fn snap_with_ports(ports: Vec<IbPort>) -> IbSnapshot {
        IbSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: Some(Utc::now()),
            ufm_observable: Some(true),
            ports,
            ib_alerts: Vec::new(),
        }
    }

    #[test]
    fn healthy_ports_yield_ok_axis_summary_with_axis_name() {
        let snap = snap_with_ports(vec![
            port_active("fe80::1", "ib-fabric-1", 7),
            port_active("fe80::2", "ib-fabric-1", 8),
        ]);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.axis, "infiniband");
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("dpu-42"));
        assert!(v.message.contains("healthy"));
        assert!(v.message.contains("2"));
        assert!(v.next_command.is_none());
    }

    #[test]
    fn empty_fabric_id_yields_fail() {
        let snap = snap_with_ports(vec![port_active("fe80::1", "", 7)]);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(
            v.message.contains("fabric_id"),
            "expected fabric_id in {:?}",
            v.message
        );
    }

    #[test]
    fn lid_unassigned_yields_fail() {
        let snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", LID_UNASSIGNED)]);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(
            v.message.contains("lid"),
            "expected lid in {:?}",
            v.message
        );
    }

    #[test]
    fn stale_observation_yields_warn() {
        let now = Utc::now();
        let mut snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", 7)]);
        // 5h old, threshold default 4h.
        snap.observed_at = Some(now - chrono::Duration::hours(5));
        let v = ib_verdict(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("stale"), "msg: {:?}", v.message);
    }

    #[test]
    fn ufm_unobservable_yields_warn() {
        let mut snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", 7)]);
        snap.ufm_observable = Some(false);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("UFM"), "msg: {:?}", v.message);
    }

    #[test]
    fn ib_alert_present_yields_warn() {
        let mut snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", 7)]);
        snap.ib_alerts.push(IbAlert {
            id: "IbPortDown".into(),
            message: "port 1 down".into(),
            target: Some("fe80::1".into()),
            in_alert_since: None,
        });
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("alert"), "msg: {:?}", v.message);
    }

    #[test]
    fn no_observation_and_no_ports_yields_unknown() {
        let snap = IbSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: None,
            ufm_observable: None,
            ports: vec![],
            ib_alerts: vec![],
        };
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Unknown);
        assert!(v.message.contains("no recent"), "msg: {:?}", v.message);
        assert!(v
            .next_command
            .as_deref()
            .unwrap()
            .contains("nico correlate"));
    }

    // ── precedence ────────────────────────────────────────────────────────

    #[test]
    fn fail_beats_warn_when_both_triggers_present() {
        // Empty fabric_id (Fail) + stale observation (Warn) ⇒ Fail.
        let now = Utc::now();
        let mut snap = snap_with_ports(vec![port_active("fe80::1", "", 7)]);
        snap.observed_at = Some(now - chrono::Duration::hours(5));
        let v = ib_verdict(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
    }

    #[test]
    fn fail_beats_ufm_unobservable() {
        let mut snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", LID_UNASSIGNED)]);
        snap.ufm_observable = Some(false);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
    }

    #[test]
    fn stale_warn_message_carries_threshold_for_operator_anchoring() {
        let now = Utc::now();
        let mut snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", 7)]);
        snap.observed_at = Some(now - chrono::Duration::hours(5));
        let v = ib_verdict(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        // 4h default threshold ⇒ 14400s in the message.
        assert!(
            v.message.contains("14400"),
            "threshold echoed: {:?}",
            v.message
        );
    }

    #[test]
    fn next_command_is_drill_down_hint_for_warn_and_fail() {
        // Warn case
        let mut snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", 7)]);
        snap.ufm_observable = Some(false);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert!(
            v.next_command
                .as_deref()
                .unwrap()
                .contains("nico doctor infiniband dpu-42"),
            "warn next_command: {:?}",
            v.next_command,
        );

        // Fail case
        let snap = snap_with_ports(vec![port_active("fe80::1", "", 7)]);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert!(
            v.next_command
                .as_deref()
                .unwrap()
                .contains("nico doctor infiniband dpu-42"),
            "fail next_command: {:?}",
            v.next_command,
        );
    }

    #[test]
    fn ok_verdict_carries_no_drill_down_hint() {
        let snap = snap_with_ports(vec![port_active("fe80::1", "ib-fabric-1", 7)]);
        let v = ib_verdict(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
        assert!(v.next_command.is_none());
    }
}
