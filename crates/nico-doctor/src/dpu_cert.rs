//! DPU client certificate verdict — early-warning signal for the
//! cert-rotation failure mode where a routine config change triggers a
//! random tenant outage minutes later because the dpu-agent's client
//! cert silently expired.
//!
//! Reads `client_certificate_expiry_unix_epoch_secs` from the most
//! recent `DpuNetworkStatus` row and reports days-to-expiry against a
//! configurable warning threshold.
//!
//! Four mutually-exclusive verdicts: `expired` > `expiring-soon` >
//! `healthy`, plus `no-recent-status` for the case where forgedb has
//! never observed a status row for this DPU. Pure `assess()` over a
//! small [`CertSnapshot`] keeps the logic testable without touching
//! Postgres; the [`DpuCertClient`] trait is the seam.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};

/// Default warning window: warn when the cert expires within this
/// duration. 30 days matches the issue's suggested default and gives
/// the operator a comfortable rotation runway.
pub const DEFAULT_WARN_THRESHOLD: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// All the data the verdict needs, fetched as one snapshot so the
/// assessment is pure. `client_certificate_expiry == None` means the
/// data layer found no recent `DpuNetworkStatus` row for this DPU.
#[derive(Debug, Clone)]
pub struct CertSnapshot {
    pub dpu_id: String,
    pub client_certificate_expiry: Option<DateTime<Utc>>,
}

/// Read-only seam over the cert data layer (`DpuNetworkStatus`). The
/// real impl issues the canonical query against forgedb and degrades
/// soft when the schema is absent (returns `None`); tests inject mocks.
#[async_trait]
pub trait DpuCertClient: Send + Sync {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<CertSnapshot>;
}

/// Default sqlx-backed [`DpuCertClient`].
///
/// Follows the same schema-probe-and-degrade pattern as
/// [`crate::hbn::SqlxHbnClient`]: when `dpu_network_status` is absent
/// (carbide drift, #213), every DPU reports `no-recent-status` rather
/// than crashing.
pub struct SqlxDpuCertClient {
    pool: sqlx::PgPool,
}

impl SqlxDpuCertClient {
    pub fn new(url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .map_err(|e| anyhow::anyhow!("invalid postgres URL: {e}"))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl DpuCertClient for SqlxDpuCertClient {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<CertSnapshot> {
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'dpu_network_status')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("dpu_cert schema probe failed: {e}"))?;

        if !exists.0 {
            return Ok(CertSnapshot {
                dpu_id: dpu_id.to_string(),
                client_certificate_expiry: None,
            });
        }

        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT s.client_certificate_expiry_unix_epoch_secs \
             FROM dpu_network_status s \
             WHERE s.dpu_id = $1 \
             ORDER BY s.last_seen_at DESC \
             LIMIT 1",
        )
        .bind(dpu_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("dpu_cert query failed: {e}"))?;

        Ok(CertSnapshot {
            dpu_id: dpu_id.to_string(),
            client_certificate_expiry: row.and_then(|(secs,)| DateTime::<Utc>::from_timestamp(secs, 0)),
        })
    }
}

/// The four possible verdicts. `time_to_expiry` is included on the
/// healthy / expiring-soon variants so the operator sees the actual
/// number, not just a category. `expired_for` shows how long the cert
/// has been past its expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertVerdict {
    NoRecentStatus,
    Expired { expired_for: Duration },
    ExpiringSoon { time_to_expiry: Duration, threshold: Duration },
    Healthy { time_to_expiry: Duration },
}

/// Run the precedence ladder over a snapshot. `now` is the caller's
/// clock so this stays pure.
pub fn assess(
    snapshot: &CertSnapshot,
    now: DateTime<Utc>,
    warn_threshold: Duration,
) -> CertVerdict {
    let Some(expiry) = snapshot.client_certificate_expiry else {
        return CertVerdict::NoRecentStatus;
    };
    let delta = expiry - now;
    if delta.num_seconds() <= 0 {
        let expired_for = (-delta).to_std().unwrap_or(Duration::ZERO);
        return CertVerdict::Expired { expired_for };
    }
    let time_to_expiry = delta.to_std().unwrap_or(Duration::ZERO);
    if time_to_expiry <= warn_threshold {
        CertVerdict::ExpiringSoon {
            time_to_expiry,
            threshold: warn_threshold,
        }
    } else {
        CertVerdict::Healthy { time_to_expiry }
    }
}

/// Render the verdict as a single headline [`Check`]. The doctor
/// formatter already paints the per-status colour and the
/// next-command hint, so the verdict is self-contained: one line, no
/// detail bullets.
pub fn assemble_checks(dpu_id: &str, verdict: &CertVerdict) -> Vec<Check> {
    let (status, value, next_command) = match verdict {
        CertVerdict::NoRecentStatus => (
            Status::Unknown,
            format!("dpu {dpu_id} cert: no recent DpuNetworkStatus to check"),
            Some(format!(
                "nico correlate {dpu_id} # last activity for this DPU"
            )),
        ),
        CertVerdict::Expired { expired_for } => (
            Status::Fail,
            format!(
                "dpu {dpu_id} cert expired {} ago",
                format_days(*expired_for)
            ),
            Some(format!(
                "rotate dpu-agent client cert for {dpu_id}"
            )),
        ),
        CertVerdict::ExpiringSoon {
            time_to_expiry,
            threshold,
        } => (
            Status::Warn,
            format!(
                "dpu {dpu_id} cert expires in {} (threshold {})",
                format_days(*time_to_expiry),
                format_days(*threshold)
            ),
            Some(format!(
                "plan dpu-agent cert rotation for {dpu_id}"
            )),
        ),
        CertVerdict::Healthy { time_to_expiry } => (
            Status::Ok,
            format!(
                "dpu {dpu_id} cert healthy: expires in {}",
                format_days(*time_to_expiry)
            ),
            None,
        ),
    };
    vec![Check {
        name: "dpu_cert",
        status,
        value,
        next_command,
        kind: CheckKind::Headline,
    }]
}

/// Render a data-layer error as an `Unknown` headline so the verdict
/// surfaces the underlying message verbatim.
pub fn assemble_error_checks(dpu_id: &str, err: &str) -> Vec<Check> {
    vec![Check {
        name: "dpu_cert",
        status: Status::Unknown,
        value: format!("dpu_cert data layer error for {dpu_id}: {err}"),
        next_command: Some("check forgedb / postgres connectivity".to_string()),
        kind: CheckKind::Headline,
    }]
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

    fn snap_healthy() -> CertSnapshot {
        CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: Some(Utc::now() + chrono::Duration::days(180)),
        }
    }

    #[test]
    fn cert_well_in_the_future_yields_healthy_with_days_remaining() {
        let snap = snap_healthy();
        let verdict = assess(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);
        match verdict {
            CertVerdict::Healthy { time_to_expiry } => {
                let days = time_to_expiry.as_secs() / 86_400;
                assert!((178..=180).contains(&days), "got {days}d");
            }
            other => panic!("expected Healthy, got {other:?}"),
        }
    }

    #[test]
    fn cert_within_warn_window_yields_expiring_soon_with_threshold() {
        let mut snap = snap_healthy();
        let now = Utc::now();
        snap.client_certificate_expiry = Some(now + chrono::Duration::days(15));
        let verdict = assess(&snap, now, DEFAULT_WARN_THRESHOLD);
        match verdict {
            CertVerdict::ExpiringSoon {
                time_to_expiry,
                threshold,
            } => {
                assert_eq!(threshold, DEFAULT_WARN_THRESHOLD);
                let days = time_to_expiry.as_secs() / 86_400;
                assert!((14..=15).contains(&days), "got {days}d");
            }
            other => panic!("expected ExpiringSoon, got {other:?}"),
        }
    }

    #[test]
    fn cert_in_the_past_yields_expired_with_age() {
        let mut snap = snap_healthy();
        let now = Utc::now();
        snap.client_certificate_expiry = Some(now - chrono::Duration::days(3));
        let verdict = assess(&snap, now, DEFAULT_WARN_THRESHOLD);
        match verdict {
            CertVerdict::Expired { expired_for } => {
                let days = expired_for.as_secs() / 86_400;
                assert!((2..=3).contains(&days), "got {days}d");
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn no_status_row_yields_no_recent_status() {
        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: None,
        };
        let verdict = assess(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);
        assert_eq!(verdict, CertVerdict::NoRecentStatus);
    }

    // ── boundary: exactly at the warn threshold counts as expiring-soon
    #[test]
    fn cert_exactly_at_threshold_yields_expiring_soon() {
        let mut snap = snap_healthy();
        let now = Utc::now();
        snap.client_certificate_expiry = Some(now + chrono::Duration::days(30));
        let verdict = assess(&snap, now, DEFAULT_WARN_THRESHOLD);
        assert!(
            matches!(verdict, CertVerdict::ExpiringSoon { .. }),
            "got {verdict:?}"
        );
    }

    // ── precedence: expired beats every other state
    #[test]
    fn expired_beats_within_warn_window() {
        let mut snap = snap_healthy();
        let now = Utc::now();
        // 1 second past expiry — still expired, not expiring-soon.
        snap.client_certificate_expiry = Some(now - chrono::Duration::seconds(1));
        let verdict = assess(&snap, now, DEFAULT_WARN_THRESHOLD);
        assert!(matches!(verdict, CertVerdict::Expired { .. }), "got {verdict:?}");
    }

    // ── assemble_checks: rendering each verdict as a headline ────────────

    #[test]
    fn healthy_check_is_single_ok_headline_no_next_command() {
        let snap = snap_healthy();
        let verdict = assess(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);
        let checks = assemble_checks(&snap.dpu_id, &verdict);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].value.contains("dpu-42"));
        assert!(checks[0].value.contains("healthy"));
        assert!(checks[0].next_command.is_none());
    }

    #[test]
    fn expiring_soon_check_is_warn_with_threshold_and_rotation_hint() {
        let verdict = CertVerdict::ExpiringSoon {
            time_to_expiry: Duration::from_secs(15 * 86_400),
            threshold: DEFAULT_WARN_THRESHOLD,
        };
        let checks = assemble_checks("dpu-42", &verdict);
        assert_eq!(checks[0].status, Status::Warn);
        assert!(checks[0].value.contains("15d"));
        assert!(checks[0].value.contains("30d"));
        assert!(
            checks[0]
                .next_command
                .as_deref()
                .unwrap()
                .contains("rotation"),
            "next_command: {:?}",
            checks[0].next_command
        );
    }

    #[test]
    fn expired_check_is_fail_with_age_and_rotate_hint() {
        let verdict = CertVerdict::Expired {
            expired_for: Duration::from_secs(3 * 86_400),
        };
        let checks = assemble_checks("dpu-42", &verdict);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].value.contains("expired"));
        assert!(checks[0].value.contains("3d"));
        assert!(
            checks[0]
                .next_command
                .as_deref()
                .unwrap()
                .contains("rotate"),
        );
    }

    #[test]
    fn no_recent_status_check_is_unknown_with_correlate_hint() {
        let checks = assemble_checks("dpu-42", &CertVerdict::NoRecentStatus);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("no recent"));
        assert!(checks[0]
            .next_command
            .as_deref()
            .unwrap()
            .contains("nico correlate"));
    }

    #[test]
    fn assemble_error_checks_surfaces_underlying_error() {
        let checks = assemble_error_checks("dpu-42", "postgres unreachable");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("postgres unreachable"));
        assert!(checks[0].value.contains("dpu-42"));
    }

    // ── format_days helper ───────────────────────────────────────────────
    #[test]
    fn format_days_uses_d_suffix_for_multi_day_durations() {
        assert_eq!(format_days(Duration::from_secs(2 * 86_400)), "2d");
    }

    #[test]
    fn format_days_falls_back_to_h_under_one_day() {
        assert_eq!(format_days(Duration::from_secs(5 * 3_600)), "5h");
    }
}
