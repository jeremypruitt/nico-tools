//! `cert_verdict()` — pure reduction of a [`CertSnapshot`] to an
//! [`AxisSummary`]. Mirrors the precedence ladder previously inlined
//! in [`crate::dpu_cert::assess`] + [`crate::dpu_cert::assemble_checks`]:
//! `expired` > `expiring-soon` > `healthy`, plus `no-recent-status`
//! when the snapshot has no expiry field.

use std::time::Duration;

use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::dpu_cert::CertSnapshot;
use crate::verdicts::AxisSummary;

/// The axis name shared across every cert-verdict caller. Equal to
/// the [`crate::layer::Layer::name`] returned by [`crate::layers::dpu_cert::DpuCertLayer`]
/// so a rollup can join the verdict back to its source layer.
pub const AXIS: &str = "dpu_cert";

/// Reduce a [`CertSnapshot`] to a single [`AxisSummary`]. `now` is the
/// caller's clock so the function stays pure.
pub fn cert_verdict(
    snapshot: &CertSnapshot,
    now: DateTime<Utc>,
    warn_threshold: Duration,
) -> AxisSummary {
    let dpu_id = &snapshot.dpu_id;

    let Some(expiry) = snapshot.client_certificate_expiry else {
        return AxisSummary {
            axis: AXIS,
            status: Status::Unknown,
            message: format!("dpu {dpu_id} cert: no recent network_status_observation to check"),
            next_command: Some(format!(
                "nico correlate {dpu_id} # last activity for this DPU"
            )),
        };
    };

    let delta = expiry - now;
    if delta.num_seconds() <= 0 {
        let expired_for = (-delta).to_std().unwrap_or(Duration::ZERO);
        return AxisSummary {
            axis: AXIS,
            status: Status::Fail,
            message: format!(
                "dpu {dpu_id} cert expired {} ago",
                format_days(expired_for)
            ),
            next_command: Some(format!("rotate dpu-agent client cert for {dpu_id}")),
        };
    }

    let time_to_expiry = delta.to_std().unwrap_or(Duration::ZERO);
    if time_to_expiry <= warn_threshold {
        AxisSummary {
            axis: AXIS,
            status: Status::Warn,
            message: format!(
                "dpu {dpu_id} cert expires in {} (threshold {})",
                format_days(time_to_expiry),
                format_days(warn_threshold)
            ),
            next_command: Some(format!("plan dpu-agent cert rotation for {dpu_id}")),
        }
    } else {
        AxisSummary {
            axis: AXIS,
            status: Status::Ok,
            message: format!(
                "dpu {dpu_id} cert healthy: expires in {}",
                format_days(time_to_expiry)
            ),
            next_command: None,
        }
    }
}

fn format_days(d: Duration) -> String {
    let days = d.as_secs() / 86_400;
    if days > 0 {
        format!("{days}d")
    } else {
        let hours = d.as_secs() / 3_600;
        if hours > 0 {
            format!("{hours}h")
        } else {
            format!("{}s", d.as_secs())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpu_cert::DEFAULT_WARN_THRESHOLD;

    fn snap_with_expiry_in(days: i64) -> CertSnapshot {
        CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: Some(Utc::now() + chrono::Duration::days(days)),
        }
    }

    #[test]
    fn healthy_snapshot_yields_ok_axis_summary_with_axis_name() {
        let snap = snap_with_expiry_in(180);
        let v = cert_verdict(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);
        assert_eq!(v.axis, "dpu_cert");
        assert_eq!(v.status, Status::Ok);
        assert!(v.message.contains("dpu-42"));
        assert!(v.message.contains("healthy"));
        assert!(v.next_command.is_none());
    }

    #[test]
    fn expiring_soon_snapshot_yields_warn_with_threshold_and_rotation_hint() {
        let snap = snap_with_expiry_in(15);
        let v = cert_verdict(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);
        assert_eq!(v.status, Status::Warn);
        assert!(v.message.contains("expires in"));
        assert!(v.message.contains("30d"), "threshold echoed: {}", v.message);
        assert!(
            v.next_command
                .as_deref()
                .unwrap()
                .contains("rotation"),
            "next_command: {:?}",
            v.next_command,
        );
    }

    #[test]
    fn expired_snapshot_yields_fail_with_age_and_rotate_hint() {
        let snap = snap_with_expiry_in(-3);
        let v = cert_verdict(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);
        assert_eq!(v.status, Status::Fail);
        assert!(v.message.contains("expired"));
        assert!(
            v.next_command.as_deref().unwrap().contains("rotate"),
            "next_command: {:?}",
            v.next_command,
        );
    }

    #[test]
    fn missing_expiry_yields_unknown_no_recent_status_with_correlate_hint() {
        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: None,
        };
        let v = cert_verdict(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);
        assert_eq!(v.status, Status::Unknown);
        assert!(v.message.contains("no recent"));
        assert!(v
            .next_command
            .as_deref()
            .unwrap()
            .contains("nico correlate"));
    }

    // ── precedence ────────────────────────────────────────────────────────

    #[test]
    fn expired_beats_within_warn_window_at_one_second_past_expiry() {
        let now = Utc::now();
        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: Some(now - chrono::Duration::seconds(1)),
        };
        let v = cert_verdict(&snap, now, DEFAULT_WARN_THRESHOLD);
        assert_eq!(v.status, Status::Fail, "expected Fail, got {v:?}");
    }

    #[test]
    fn exactly_at_threshold_counts_as_expiring_soon_warn() {
        let now = Utc::now();
        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: Some(now + chrono::Duration::days(30)),
        };
        let v = cert_verdict(&snap, now, DEFAULT_WARN_THRESHOLD);
        assert_eq!(v.status, Status::Warn, "expected Warn, got {v:?}");
    }

    #[test]
    fn custom_threshold_widens_warn_window() {
        // 100 days remaining; default 30d ⇒ Ok. Custom 200d ⇒ Warn.
        let now = Utc::now();
        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: Some(now + chrono::Duration::days(100)),
        };
        let default = cert_verdict(&snap, now, DEFAULT_WARN_THRESHOLD);
        assert_eq!(default.status, Status::Ok);
        let widened = cert_verdict(&snap, now, Duration::from_secs(200 * 86_400));
        assert_eq!(widened.status, Status::Warn);
    }
}
