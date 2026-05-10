//! `hbn_verdict()` — pure reduction of an [`HbnSnapshot`] to an
//! [`AxisSummary`]. Collapses the per-signal headlines that the `hbn`
//! layer used to emit (drift / network-config-error / BGP alerts /
//! freshness / version-below-minimum) into a single rolled-up verdict;
//! the per-signal breakdown moves to the layer's detail rows.
//!
//! **Rollup priority** (most severe wins; first match in this list
//! becomes the headline):
//!
//! 1. `Fail` — `network_config_error` set on the agent's observation.
//!    The agent's explicit error is more actionable than any derived
//!    signal because it tells the operator *why*.
//! 2. `Fail` — `hbn_version` parses and is below
//!    [`NVUE_MINIMUM_HBN_VERSION`]. NVUE is a hard requirement, so a
//!    below-minimum component is a config-apply blocker, not a soft
//!    informational signal. Absent / malformed versions degrade
//!    gracefully (no version-fail; the rest of the ladder still
//!    contributes).
//! 3. `Fail` — applied managed-host or instance-network config drifts
//!    from desired. Surfaced together because operators triage them
//!    the same way (drift → look at the version axis below).
//! 4. `Fail` — BGP alerts present (count surfaced in the message).
//! 5. `Fail` — `quarantine_state` is set (operator-requested isolation).
//! 6. `Warn` — `last_seen_at` older than the freshness threshold.
//! 7. `Ok` — none of the above.
//!
//! Status precedence is sticky: once a Fail trigger appears the verdict
//! is Fail. Detail rows in the layer renderer preserve every individual
//! signal so nothing is dropped from the JSON output — the verdict is a
//! summary, not a substitute for the per-signal breakdown.

use std::cmp::Ordering;
use std::time::Duration;

use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::hbn::{compare_hbn_versions, HbnSnapshot, NVUE_MINIMUM_HBN_VERSION};
use crate::verdicts::AxisSummary;

/// The axis name shared across every hbn-verdict caller. Equal to the
/// [`crate::layer::Layer::name`] returned by
/// [`crate::layers::hbn::HbnLayer`] so a rollup can join the verdict
/// back to its source layer.
pub const AXIS: &str = "hbn";

/// Reduce an [`HbnSnapshot`] to a single [`AxisSummary`]. `now` is the
/// caller's clock so the function stays pure.
pub fn hbn_verdict(
    snapshot: &HbnSnapshot,
    now: DateTime<Utc>,
    freshness_threshold: Duration,
) -> AxisSummary {
    let dpu_id = &snapshot.dpu_id;

    if let Some(err) = snapshot.network_config_error.as_deref().filter(|s| !s.is_empty()) {
        return AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!("hbn: network_config_error on dpu {dpu_id}: {err}"),
            next_command: Some(format!(
                "nico correlate {dpu_id} # trace the failed config apply"
            )),
        };
    }

    if version_below_minimum(&snapshot.hbn_version) {
        return AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!(
                "hbn: version below minimum on dpu {dpu_id} ({} < {})",
                snapshot.hbn_version, NVUE_MINIMUM_HBN_VERSION
            ),
            next_command: Some(format!("plan hbn upgrade for dpu {dpu_id}")),
        };
    }

    let managed_drift = snapshot.applied_managed_host_config_version
        != snapshot.desired_managed_host_config_version;
    let instance_drift = snapshot.applied_instance_network_config_version
        != snapshot.desired_instance_network_config_version;
    if managed_drift || instance_drift {
        let axis_label = if managed_drift && instance_drift {
            "managed-host + instance-network"
        } else if managed_drift {
            "managed-host"
        } else {
            "instance-network"
        };
        return AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!("hbn: drift on {axis_label} for dpu {dpu_id}"),
            next_command: Some(format!(
                "nico doctor hbn {dpu_id} # inspect drift detail rows"
            )),
        };
    }

    if !snapshot.bgp_alerts.is_empty() {
        let n = snapshot.bgp_alerts.len();
        return AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!("hbn: {n} BGP alert(s) on dpu {dpu_id}"),
            next_command: Some(format!(
                "ssh dpu-{dpu_id} 'nv show vrf default router bgp summary'"
            )),
        };
    }

    if let Some(state) = snapshot.quarantine_state.as_deref().filter(|s| !s.is_empty()) {
        return AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!("hbn: quarantined on dpu {dpu_id}: {state}"),
            next_command: Some(format!(
                "nico correlate {dpu_id} # see why this DPU was quarantined"
            )),
        };
    }

    let age = (now - snapshot.last_seen_at).to_std().unwrap_or(Duration::ZERO);
    if age > freshness_threshold {
        return AxisSummary {
            axis: AXIS,
            status: Status::Warn,
            message: format!(
                "hbn: stale on dpu {dpu_id} (last update {}s ago, threshold {}s)",
                age.as_secs(),
                freshness_threshold.as_secs()
            ),
            next_command: Some(format!(
                "nico correlate {dpu_id} # check DPU agent connectivity"
            )),
        };
    }

    AxisSummary {
        axis: AXIS,
        status: Status::Ok,
        message: format!("hbn: ok on dpu {dpu_id}"),
        next_command: None,
    }
}

/// True when `version` parses to a non-empty tuple AND compares less
/// than [`NVUE_MINIMUM_HBN_VERSION`]. Empty / malformed strings degrade
/// gracefully — they yield `false` so the verdict ladder skips the
/// version-fail rung instead of surfacing a noisy "below minimum" line
/// for inventory we couldn't parse.
fn version_below_minimum(version: &str) -> bool {
    if !is_parseable_version(version) {
        return false;
    }
    compare_hbn_versions(version, NVUE_MINIMUM_HBN_VERSION) == Ordering::Less
}

/// Heuristic: a parseable version string is non-empty and contains at
/// least one ASCII digit. Anything else (empty inventory, the literal
/// string `"unknown"`, etc.) degrades gracefully rather than risking a
/// false "below minimum" signal from `compare_hbn_versions`'s
/// zero-padding fallback.
fn is_parseable_version(version: &str) -> bool {
    !version.is_empty() && version.chars().any(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hbn::DEFAULT_FRESHNESS_THRESHOLD;

    fn snap_healthy() -> HbnSnapshot {
        HbnSnapshot {
            dpu_id: "dpu-42".into(),
            hbn_version: "2.0.0-doca2.5.0".into(),
            applied_managed_host_config_version: "v17".into(),
            desired_managed_host_config_version: "v17".into(),
            applied_instance_network_config_version: "v9".into(),
            desired_instance_network_config_version: "v9".into(),
            network_config_error: None,
            bgp_alerts: vec![],
            quarantine_state: None,
            last_seen_at: Utc::now(),
        }
    }

    // ── tracer bullet ─────────────────────────────────────────────────────

    #[test]
    fn healthy_snapshot_yields_ok_axis_summary_with_axis_name() {
        let snap = snap_healthy();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.axis, "hbn");
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("dpu-42"));
        assert!(v.message.contains("ok"));
        assert!(v.next_command.is_none());
    }

    // ── per-signal contributions ─────────────────────────────────────────

    #[test]
    fn network_config_error_yields_fail_with_error_text_in_message() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("nvue apply failed".into());
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("network_config_error"));
        assert!(v.message.contains("nvue apply failed"));
        assert!(v.next_command.as_deref().unwrap().contains("nico correlate"));
    }

    #[test]
    fn empty_network_config_error_string_is_treated_as_none() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some(String::new());
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
    }

    #[test]
    fn managed_host_drift_yields_fail_naming_the_axis() {
        let mut snap = snap_healthy();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("drift"));
        assert!(v.message.contains("managed-host"));
    }

    #[test]
    fn instance_network_drift_yields_fail_naming_the_axis() {
        let mut snap = snap_healthy();
        snap.applied_instance_network_config_version = "v8".into();
        snap.desired_instance_network_config_version = "v9".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("drift"));
        assert!(v.message.contains("instance-network"));
    }

    #[test]
    fn drift_on_both_axes_names_both_in_message() {
        let mut snap = snap_healthy();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        snap.applied_instance_network_config_version = "v8".into();
        snap.desired_instance_network_config_version = "v9".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("managed-host"));
        assert!(v.message.contains("instance-network"));
    }

    #[test]
    fn bgp_alerts_yield_fail_with_count() {
        let mut snap = snap_healthy();
        snap.bgp_alerts = vec!["BgpPeerDown".into(), "BgpRoutesMissing".into()];
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("BGP alert"));
        assert!(v.message.contains("2"));
    }

    #[test]
    fn quarantine_yields_fail_with_state_in_message() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("quarantine"));
        assert!(v.message.contains("BlockAllTraffic"));
    }

    #[test]
    fn stale_last_seen_yields_warn_with_threshold_echo() {
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.last_seen_at = now - chrono::Duration::seconds(180);
        let v = hbn_verdict(&snap, now, Duration::from_secs(90));
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("stale"));
        assert!(v.message.contains("90s"), "threshold echoed: {}", v.message);
    }

    // ── version signal ───────────────────────────────────────────────────

    #[test]
    fn version_at_minimum_passes() {
        let mut snap = snap_healthy();
        snap.hbn_version = NVUE_MINIMUM_HBN_VERSION.to_string();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
    }

    #[test]
    fn version_above_minimum_passes() {
        let mut snap = snap_healthy();
        snap.hbn_version = "2.1.0-doca2.5.0".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
    }

    #[test]
    fn version_one_below_minimum_yields_fail_with_minimum_echoed() {
        let mut snap = snap_healthy();
        snap.hbn_version = "1.9.0-doca2.4.0".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("below minimum"));
        assert!(
            v.message.contains(NVUE_MINIMUM_HBN_VERSION),
            "minimum echoed: {}",
            v.message,
        );
        assert!(v.message.contains("1.9.0-doca2.4.0"));
        assert!(v.next_command.as_deref().unwrap().contains("upgrade"));
    }

    #[test]
    fn version_string_malformed_degrades_gracefully_to_no_version_fail() {
        // A non-numeric inventory value (e.g. the agent returned "unknown")
        // must not surface as a below-minimum Fail — we don't have a
        // reliable comparison.
        let mut snap = snap_healthy();
        snap.hbn_version = "unknown".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            v.status, Status::Ok,
            "malformed version must not contribute a version-fail; got {v:?}",
        );
    }

    #[test]
    fn inventory_absent_degrades_gracefully_to_no_version_fail() {
        // When the `machines.inventory` JSON has no `doca_hbn` row the
        // SQL projection yields the empty string. The verdict must not
        // claim "below minimum" when we have nothing to compare.
        let mut snap = snap_healthy();
        snap.hbn_version = String::new();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
    }

    // ── precedence ───────────────────────────────────────────────────────

    #[test]
    fn network_config_error_beats_version_below_minimum() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("agent error".into());
        snap.hbn_version = "1.9.0-doca2.4.0".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(v.message.contains("network_config_error"));
        assert!(!v.message.contains("below minimum"));
    }

    #[test]
    fn version_below_minimum_beats_drift_in_headline() {
        let mut snap = snap_healthy();
        snap.hbn_version = "1.9.0-doca2.4.0".into();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(v.message.contains("below minimum"));
        assert!(!v.message.contains("drift"));
    }

    #[test]
    fn drift_beats_bgp_alerts_in_headline() {
        let mut snap = snap_healthy();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        snap.bgp_alerts = vec!["BgpPeerDown".into()];
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(v.message.contains("drift"));
        assert!(!v.message.contains("BGP"));
    }

    #[test]
    fn bgp_alerts_beat_quarantine_in_headline() {
        let mut snap = snap_healthy();
        snap.bgp_alerts = vec!["BgpPeerDown".into()];
        snap.quarantine_state = Some("manual".into());
        let v = hbn_verdict(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(v.message.contains("BGP"));
        assert!(!v.message.contains("quarantine"));
    }

    #[test]
    fn quarantine_beats_freshness_in_headline() {
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("manual".into());
        snap.last_seen_at = now - chrono::Duration::seconds(600);
        let v = hbn_verdict(&snap, now, Duration::from_secs(90));
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("quarantine"));
    }

    #[test]
    fn fail_beats_warn_when_both_triggers_present() {
        // managed_host drift (Fail) + stale freshness (Warn) ⇒ Fail.
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        snap.last_seen_at = now - chrono::Duration::seconds(600);
        let v = hbn_verdict(&snap, now, Duration::from_secs(90));
        assert_eq!(v.status, Status::Fail);
    }
}
