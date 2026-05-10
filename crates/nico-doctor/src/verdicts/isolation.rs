//! `isolation_verdict()` — pure reduction of an [`IsolationSnapshot`]
//! to an [`AxisSummary`]. Mirrors the precedence ladder previously
//! inlined in [`crate::dpu_isolation::assess`] +
//! [`crate::dpu_isolation::assemble_checks`]:
//! `not-yet-known` > `quarantine-requested` > `lost-connection` >
//! `healthy`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::dpu_isolation::{
    assess, IsolationSnapshot, IsolationVerdict, NotYetKnownReason,
};
use crate::verdicts::AxisSummary;

/// The axis name shared across every isolation-verdict caller. Equal
/// to the [`crate::layer::Layer::name`] returned by
/// [`crate::layers::dpu_isolation::DpuIsolationLayer`] so a rollup can
/// join the verdict back to its source layer.
pub const AXIS: &str = "dpu_isolation";

/// Reduce an [`IsolationSnapshot`] to a single [`AxisSummary`]. `now`
/// is the caller's clock so the function stays pure.
pub fn isolation_verdict(
    snapshot: &IsolationSnapshot,
    now: DateTime<Utc>,
    freshness_threshold: Duration,
) -> AxisSummary {
    let machine_id = &snapshot.machine_id;
    let v = assess(snapshot, now, freshness_threshold);
    match v {
        IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::NotRegistered,
        } => AxisSummary {
            axis: AXIS,
            status: Status::Unknown,
            message: format!("dpu {machine_id} not-yet-known: no machines row"),
            next_command: Some(format!(
                "nico correlate {machine_id} # confirm the ID is correct"
            )),
        },
        IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::ScoutDiscoveryIncomplete,
        } => AxisSummary {
            axis: AXIS,
            status: Status::Unknown,
            message: format!(
                "dpu {machine_id} not-yet-known: scout discovery has not completed"
            ),
            next_command: Some(format!(
                "nico correlate {machine_id} # last scout activity"
            )),
        },
        IsolationVerdict::Quarantined { state } => AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!("dpu {machine_id} quarantine requested: {state}"),
            next_command: Some(format!(
                "nico correlate {machine_id} # see why this DPU was quarantined"
            )),
        },
        IsolationVerdict::LostConnection {
            last_seen_age,
            threshold,
        } => AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: match last_seen_age {
                Some(age) => format!(
                    "dpu {machine_id} lost-connection: last seen {}s ago (threshold {}s)",
                    age.as_secs(),
                    threshold.as_secs()
                ),
                None => format!(
                    "dpu {machine_id} lost-connection: no network_status_observation ever recorded"
                ),
            },
            next_command: Some(format!(
                "nico correlate {machine_id} # check DPU agent connectivity"
            )),
        },
        IsolationVerdict::Healthy { last_seen_age } => AxisSummary {
            axis: AXIS,
            status: Status::Ok,
            message: format!(
                "dpu {machine_id} healthy: last seen {}s ago",
                last_seen_age.as_secs()
            ),
            next_command: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpu_isolation::DEFAULT_FRESHNESS_THRESHOLD;

    fn snap_healthy() -> IsolationSnapshot {
        IsolationSnapshot {
            machine_id: "machine-42".into(),
            registered: true,
            scout_discovery_complete: true,
            quarantine_state: None,
            last_seen_at: Some(Utc::now()),
        }
    }

    #[test]
    fn healthy_snapshot_yields_ok_axis_summary_with_axis_name() {
        let snap = snap_healthy();
        let v = isolation_verdict(&snap, snap.last_seen_at.unwrap(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.axis, "dpu_isolation");
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("machine-42"));
        assert!(v.message.contains("healthy"));
        assert!(v.next_command.is_none());
    }

    #[test]
    fn quarantined_snapshot_yields_fail_with_state_and_correlate_hint() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        let v = isolation_verdict(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("quarantine requested"));
        assert!(v.message.contains("BlockAllTraffic"));
        assert!(
            v.next_command.as_deref().unwrap().contains("nico correlate"),
            "next_command: {:?}",
            v.next_command,
        );
    }

    #[test]
    fn stale_last_seen_yields_fail_with_age_and_threshold_seconds() {
        let mut snap = snap_healthy();
        let now = Utc::now();
        snap.last_seen_at = Some(now - chrono::Duration::seconds(180));
        let v = isolation_verdict(&snap, now, Duration::from_secs(90));
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("lost-connection"));
        assert!(v.message.contains("180s"));
        assert!(v.message.contains("90s"));
    }

    #[test]
    fn no_last_seen_for_registered_scouted_machine_is_lost_connection_no_age() {
        let mut snap = snap_healthy();
        snap.last_seen_at = None;
        let v = isolation_verdict(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("no network_status_observation"));
    }

    #[test]
    fn unregistered_machine_yields_unknown_no_machines_row() {
        let mut snap = snap_healthy();
        snap.registered = false;
        snap.scout_discovery_complete = false;
        snap.last_seen_at = None;
        let v = isolation_verdict(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Unknown);
        assert!(v.message.contains("not-yet-known"));
        assert!(v.message.contains("machines row"));
        assert!(v
            .next_command
            .as_deref()
            .unwrap()
            .contains("nico correlate"));
    }

    #[test]
    fn registered_but_scout_incomplete_yields_unknown_scout_branch() {
        let mut snap = snap_healthy();
        snap.scout_discovery_complete = false;
        snap.last_seen_at = None;
        let v = isolation_verdict(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Unknown);
        assert!(v.message.contains("scout"));
    }

    // ── precedence ────────────────────────────────────────────────────────

    #[test]
    fn unregistered_beats_quarantined_and_stale() {
        let mut snap = snap_healthy();
        snap.registered = false;
        snap.scout_discovery_complete = false;
        snap.quarantine_state = Some("BlockAllTraffic".into());
        snap.last_seen_at = Some(Utc::now() - chrono::Duration::hours(1));
        let v = isolation_verdict(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Unknown);
        assert!(v.message.contains("machines row"));
    }

    #[test]
    fn quarantined_beats_stale_last_seen() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        snap.last_seen_at = Some(Utc::now() - chrono::Duration::hours(1));
        let v = isolation_verdict(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("quarantine requested"));
    }

    #[test]
    fn empty_string_quarantine_state_is_treated_as_not_quarantined() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("".into());
        let v = isolation_verdict(&snap, snap.last_seen_at.unwrap(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
    }
}
