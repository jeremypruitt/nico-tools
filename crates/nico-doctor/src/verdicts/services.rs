//! `services_verdict()` — pure reduction of a [`ServicesSnapshot`] to
//! an [`AxisSummary`]. Mirrors the per-service classification ladder
//! [`crate::dpu_services::assemble_checks`] uses to filter detail rows,
//! but rolls the per-service buckets up into a single `M/N ready`
//! headline with the appropriate severity.
//!
//! **Rollup priority** (most severe wins; first match in this list
//! becomes the headline status):
//!
//! 1. `Fail` — would not fire today; reserved for future hard failures.
//! 2. `Warn` — at least one service is `Failed` / `Error` (or any
//!    unrecognised state) — those are the buckets the layer flags as
//!    bad. `Pending` / `Unknown` services count toward `Warn` only when
//!    the `extension_service_observation` is older than
//!    `stale_threshold` (the agent's "we're stuck" signal).
//! 3. `Ok` — every service is `Running`, `Terminating`/`Terminated`
//!    (lifecycle), `Pending`/`Unknown` with a fresh observation, or
//!    flagged `removed`.
//!
//! The headline message is `services: M/N ready` (M = `Running` count,
//! N = total services) with parenthetical degraded counts when applicable
//! — `services: 4/4 ready`, `services: 3/4 ready (1 degraded)`. Empty
//! inventories collapse to `services: ok`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::dpu_services::{ServicesSnapshot, ServiceStatus};
use crate::verdicts::AxisSummary;

/// The axis name shared across every services-verdict caller. Equal to
/// the [`crate::layer::Layer::name`] returned by
/// [`crate::layers::dpu_services::DpuServicesLayer`] so a rollup can
/// join the verdict back to its source layer.
pub const AXIS: &str = "dpu_services";

/// Reduce a [`ServicesSnapshot`] to a single [`AxisSummary`]. `now` is
/// the caller's clock so the function stays pure; `stale_threshold`
/// promotes `Pending` / `Unknown` services from silent-transient to
/// `Warn` once the observation is older than the threshold (mirrors
/// [`crate::dpu_services::assemble_checks`]).
pub fn services_verdict(
    snapshot: &ServicesSnapshot,
    now: DateTime<Utc>,
    stale_threshold: Duration,
) -> AxisSummary {
    let dpu_id = &snapshot.dpu_id;

    if snapshot.services.is_empty() {
        return AxisSummary {
            axis: AXIS,
            status: Status::Ok,
            message: format!("dpu {dpu_id} services: ok"),
            next_command: None,
        };
    }

    let observation_is_stale = match snapshot.observed_at {
        Some(ts) => (now - ts).to_std().unwrap_or(Duration::ZERO) > stale_threshold,
        None => false,
    };

    let total = snapshot.services.len();
    let mut ready = 0usize;
    let mut degraded = 0usize;
    for svc in &snapshot.services {
        match classify(svc, observation_is_stale) {
            Bucket::Ready => ready += 1,
            Bucket::Degraded => degraded += 1,
            Bucket::Silent => {}
        }
    }

    if degraded > 0 {
        AxisSummary {
            axis: AXIS,
            status: Status::Warn,
            message: format!(
                "dpu {dpu_id} services: {ready}/{total} ready ({degraded} degraded)"
            ),
            next_command: Some(format!("nico doctor dpu_services {dpu_id}")),
        }
    } else {
        AxisSummary {
            axis: AXIS,
            status: Status::Ok,
            message: format!("dpu {dpu_id} services: {ready}/{total} ready"),
            next_command: None,
        }
    }
}

/// Per-service contribution to the rollup. `Ready` counts toward the
/// `M` in `M/N ready`; `Degraded` flips the headline severity to
/// `Warn`; `Silent` (lifecycle / removed / fresh-transient) is excluded
/// from both counts so the headline doesn't pretend a `Terminating`
/// service is healthy traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Ready,
    Degraded,
    Silent,
}

fn classify(svc: &ServiceStatus, observation_is_stale: bool) -> Bucket {
    if svc.removed.is_some() {
        return Bucket::Silent;
    }
    match svc.overall_state.as_str() {
        "Running" => Bucket::Ready,
        "Terminating" | "Terminated" => Bucket::Silent,
        "Pending" | "Unknown" => {
            if observation_is_stale {
                Bucket::Degraded
            } else {
                Bucket::Silent
            }
        }
        _ => Bucket::Degraded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpu_services::DEFAULT_OBSERVATION_STALE_THRESHOLD;

    fn snap(services: Vec<ServiceStatus>) -> ServicesSnapshot {
        ServicesSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: Some(Utc::now()),
            services,
        }
    }

    fn svc(name: &str, state: &str) -> ServiceStatus {
        ServiceStatus {
            service_name: name.into(),
            version: "1.0.0".into(),
            overall_state: state.into(),
            message: String::new(),
            removed: None,
        }
    }

    // ── tracer bullet ────────────────────────────────────────────────────

    #[test]
    fn empty_snapshot_yields_ok_axis_summary_with_axis_name() {
        let s = snap(vec![]);
        let v = services_verdict(&s, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.axis, "dpu_services");
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("dpu-42"));
        assert!(v.message.contains("ok"));
        assert!(v.next_command.is_none());
    }

    // ── all ready ────────────────────────────────────────────────────────

    #[test]
    fn all_running_yields_ok_with_full_ready_count() {
        let s = snap(vec![
            svc("doca-bfb", "Running"),
            svc("doca-telemetry", "Running"),
            svc("doca-x", "Running"),
            svc("doca-y", "Running"),
        ]);
        let v = services_verdict(&s, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("4/4 ready"), "message: {}", v.message);
        assert!(v.next_command.is_none());
    }

    // ── partial degraded ─────────────────────────────────────────────────

    #[test]
    fn one_failed_among_ready_yields_warn_with_degraded_count_and_drilldown() {
        let s = snap(vec![
            svc("doca-bfb", "Running"),
            svc("doca-telemetry", "Failed"),
            svc("doca-x", "Running"),
            svc("doca-y", "Running"),
        ]);
        let v = services_verdict(&s, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("3/4 ready"), "message: {}", v.message);
        assert!(v.message.contains("1 degraded"), "message: {}", v.message);
        assert!(
            v.next_command
                .as_deref()
                .unwrap()
                .contains("nico doctor dpu_services dpu-42"),
            "next_command: {:?}",
            v.next_command,
        );
    }

    // ── all degraded ─────────────────────────────────────────────────────

    #[test]
    fn all_failed_yields_warn_with_zero_ready_and_full_degraded_count() {
        let s = snap(vec![
            svc("a", "Failed"),
            svc("b", "Error"),
            svc("c", "WeirdNewState"),
        ]);
        let v = services_verdict(&s, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("0/3 ready"), "message: {}", v.message);
        assert!(v.message.contains("3 degraded"), "message: {}", v.message);
    }

    // ── lifecycle / removed are silent (count toward N but not M or degraded) ──

    #[test]
    fn terminating_service_neither_ready_nor_degraded() {
        let s = snap(vec![
            svc("alive", "Running"),
            svc("going-away", "Terminating"),
        ]);
        let v = services_verdict(&s, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
        // 1 of 2 services is "Running"; the other is Terminating, which
        // is silent — counted in the total but not the ready bucket.
        assert!(v.message.contains("1/2 ready"), "message: {}", v.message);
        assert!(!v.message.contains("degraded"));
    }

    #[test]
    fn removed_service_is_silent_regardless_of_state() {
        let mut s = snap(vec![
            svc("alive", "Running"),
            svc("decommissioned", "Failed"),
        ]);
        s.services[1].removed = Some("operator decommissioned".into());
        let v = services_verdict(&s, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        // The removed service is silent even though its state is Failed.
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("1/2 ready"), "message: {}", v.message);
    }

    // ── transient + staleness ────────────────────────────────────────────

    #[test]
    fn pending_service_with_fresh_observation_is_silent() {
        let now = Utc::now();
        let mut s = snap(vec![svc("alive", "Running"), svc("starting", "Pending")]);
        s.observed_at = Some(now - chrono::Duration::seconds(30));
        let v = services_verdict(&s, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("1/2 ready"));
    }

    #[test]
    fn pending_service_with_stale_observation_counts_as_degraded() {
        let now = Utc::now();
        let mut s = snap(vec![svc("alive", "Running"), svc("stuck", "Pending")]);
        s.observed_at = Some(now - chrono::Duration::minutes(10));
        let v = services_verdict(&s, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("1/2 ready"));
        assert!(v.message.contains("1 degraded"));
    }
}
