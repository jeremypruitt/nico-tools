//! DPU client certificate verdict — early-warning signal for the
//! cert-rotation failure mode where a routine config change triggers a
//! random tenant outage minutes later because the dpu-agent's client
//! cert silently expired.
//!
//! Reads `client_certificate_expiry` (i64 unix epoch secs) from
//! `machines.network_status_observation` JSON — the producer-side
//! storage. See PRD-002 (`docs/prds/002-dpu-layer-rewrite.md`) for the
//! schema mapping. Reports days-to-expiry against a configurable
//! warning threshold.
//!
//! Four mutually-exclusive verdicts: `expired` > `expiring-soon` >
//! `healthy`, plus `no-recent-status` for the case where the JSON
//! column is absent or has no `client_certificate_expiry` field for
//! this machine row.
//!
//! Since PRD-003 Slice 1 (#305) the verdict precedence lives in the
//! shared [`crate::verdicts::cert_verdict`] primitive (returning an
//! [`crate::verdicts::AxisSummary`] that downstream holistic rollups
//! consume); [`assemble_checks`] here is the per-layer renderer that
//! turns that summary into a headline `Check` plus cert-specific
//! detail rows (absolute expiry, threshold echo). The
//! [`DpuCertClient`] trait remains the I/O seam.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};
use crate::verdicts::{cert_verdict, AxisSummary};

/// Default warning window: warn when the cert expires within this
/// duration. 30 days matches the issue's suggested default and gives
/// the operator a comfortable rotation runway.
pub const DEFAULT_WARN_THRESHOLD: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// All the data the verdict needs, fetched as one snapshot so the
/// assessment is pure. `client_certificate_expiry == None` means the
/// data layer found no `machines` row for this DPU, or its
/// `network_status_observation` JSON had no `client_certificate_expiry`
/// field.
#[derive(Debug, Clone)]
pub struct CertSnapshot {
    pub dpu_id: String,
    pub client_certificate_expiry: Option<DateTime<Utc>>,
}

/// Read-only seam over the cert data layer
/// (`machines.network_status_observation` JSON). The real impl issues
/// the canonical query against forgedb and degrades soft when the
/// schema is absent (returns `None`); tests inject mocks.
#[async_trait]
pub trait DpuCertClient: Send + Sync {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<CertSnapshot>;
}

/// Default sqlx-backed [`DpuCertClient`].
///
/// Reads the `client_certificate_expiry` field (i64 unix epoch secs)
/// out of `machines.network_status_observation` JSON. Schema-probes the
/// `machines` table first and degrades to `no-recent-status` when
/// absent (e.g. dev cluster without forgedb).
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
             WHERE table_name = 'machines')",
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

        let row: Option<(Option<i64>,)> = sqlx::query_as(
            "SELECT (network_status_observation->>'client_certificate_expiry')::bigint \
             FROM machines \
             WHERE id = $1 \
             LIMIT 1",
        )
        .bind(dpu_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("dpu_cert query failed: {e}"))?;

        Ok(CertSnapshot {
            dpu_id: dpu_id.to_string(),
            client_certificate_expiry: row
                .and_then(|(secs,)| secs)
                .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0)),
        })
    }
}

/// Render the cert axis as a headline `Check` (sourced from
/// [`cert_verdict`]) followed by cert-specific detail rows: the
/// absolute expiry timestamp and a threshold echo. The detail rows
/// give the operator raw data the punchy headline elides; the rollup
/// layers (PRD-003 slices 5 + 6) consume only the headline.
///
/// JSON ordering — issue #305 acceptance criteria: headline first
/// (`kind: "headline"`), then detail (`kind: "detail"`).
pub fn assemble_checks(
    snapshot: &CertSnapshot,
    now: DateTime<Utc>,
    warn_threshold: Duration,
) -> Vec<Check> {
    let summary = cert_verdict(snapshot, now, warn_threshold);
    let mut checks = vec![headline_from(&summary)];

    if let Some(expiry) = snapshot.client_certificate_expiry {
        checks.push(Check {
            name: "expiry",
            status: Status::Ok,
            value: expiry.to_rfc3339(),
            next_command: None,
            kind: CheckKind::Detail,
        });
        checks.push(Check {
            name: "warn-threshold",
            status: Status::Ok,
            value: format_days(warn_threshold),
            next_command: None,
            kind: CheckKind::Detail,
        });
    }

    checks
}

fn headline_from(summary: &AxisSummary) -> Check {
    Check {
        name: summary.axis,
        status: summary.status.clone(),
        value: summary.message.clone(),
        next_command: summary.next_command.clone(),
        kind: CheckKind::Headline,
    }
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

    fn snap_with_expiry_in(days: i64) -> CertSnapshot {
        CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: Some(Utc::now() + chrono::Duration::days(days)),
        }
    }

    // Verdict precedence + per-variant content live in
    // [`crate::verdicts::cert`] tests. Tests here cover the layer
    // renderer: headline-vs-detail ordering, detail-row population,
    // and data-layer error surfacing.

    #[test]
    fn assemble_checks_emits_headline_then_expiry_then_threshold() {
        let snap = snap_with_expiry_in(180);
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);

        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].value.contains("healthy"));

        assert_eq!(checks[1].kind, CheckKind::Detail);
        assert_eq!(checks[1].name, "expiry");
        // RFC3339 timestamp — bare presence of `T` between date+time
        // is enough to confirm the format without anchoring on the
        // exact wall-clock value.
        assert!(checks[1].value.contains('T'), "expiry: {}", checks[1].value);

        assert_eq!(checks[2].kind, CheckKind::Detail);
        assert_eq!(checks[2].name, "warn-threshold");
        assert_eq!(checks[2].value, "30d");
    }

    #[test]
    fn assemble_checks_omits_detail_rows_when_no_expiry_to_anchor_them() {
        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: None,
        };
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_WARN_THRESHOLD);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Unknown);
    }

    #[test]
    fn assemble_error_checks_surfaces_underlying_error() {
        let checks = assemble_error_checks("dpu-42", "postgres unreachable");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
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
