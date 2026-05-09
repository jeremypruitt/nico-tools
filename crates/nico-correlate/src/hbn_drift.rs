//! HBN config-drift correlation — compares applied (`network_status_observation`)
//! vs desired (`network_config`) for the two version axes
//! (`managed_host_config_version`, `instance_network_config_version`)
//! on a single `machines` row.
//!
//! Read-only, side-effect-free apart from the [`DriftClient`] trait —
//! the seam over the Postgres data source. The pure assembly layer
//! (`assemble_drift_rows`) is fully unit-testable without I/O.
//!
//! See PRD-002 (`docs/prds/002-dpu-layer-rewrite.md`) for the schema
//! mapping. Drift detection reports applied vs desired on the same
//! machine row; the "first observed drift at" timestamp capability has
//! been dropped (its only data source — a per-DPU history table —
//! does not exist in the producer-side schema).

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Default freshness window for the `last_seen_at` field before a drift
/// row is flagged stale (matches the per-DPU HBN verdict default).
pub const DEFAULT_FRESHNESS_THRESHOLD: Duration = Duration::from_secs(90);

/// Snapshot of HBN drift state for a single machine: per-axis drift
/// plus the most recent `network_status_observation->>'observed_at'`.
#[derive(Debug, Clone)]
pub struct DriftSnapshot {
    pub machine_id: String,
    pub managed_host: AxisDrift,
    pub instance_network: AxisDrift,
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// Applied vs desired version for one axis. Drift exists iff
/// `applied != desired` — no first-observed-drift timestamp because the
/// producer-side schema has no history table to derive it from.
#[derive(Debug, Clone)]
pub struct AxisDrift {
    pub applied: String,
    pub desired: String,
}

/// Read-only seam over the drift data layer. Real impl reads the
/// `machines` row's JSON columns; tests inject a mock. `Ok(None)`
/// means "no `machines` row found for this id".
#[async_trait]
pub trait DriftClient: Send + Sync {
    async fn fetch_drift(&self, machine_id: &str) -> Result<Option<DriftSnapshot>>;
}

/// Single row in the rendered correlation table — one per axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftRow {
    pub axis: &'static str,
    pub applied: String,
    pub desired: String,
    pub status: AxisStatus,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxisStatus {
    NoDrift,
    Drifting,
}

/// Axis name used for the `managed_host_config_version` column.
pub const AXIS_MANAGED_HOST: &str = "managed_host_config";
/// Axis name used for the `instance_network_config_version` column.
pub const AXIS_INSTANCE_NETWORK: &str = "instance_network_config";

/// Pure assembly: drift snapshot → table rows.
///
/// Rules:
/// - one row per axis, in fixed order: managed_host, instance_network
/// - `Drifting` iff `applied != desired`
/// - `stale = true` when `last_seen_at` is older than `freshness_threshold`
pub fn assemble_drift_rows(
    snapshot: &DriftSnapshot,
    now: DateTime<Utc>,
    freshness_threshold: Duration,
) -> Vec<DriftRow> {
    let stale = match snapshot.last_seen_at {
        Some(seen) => match (now - seen).to_std() {
            Ok(age) => age > freshness_threshold,
            Err(_) => false,
        },
        None => false,
    };

    vec![
        row_for_axis(AXIS_MANAGED_HOST, &snapshot.managed_host, stale),
        row_for_axis(AXIS_INSTANCE_NETWORK, &snapshot.instance_network, stale),
    ]
}

fn row_for_axis(axis: &'static str, drift: &AxisDrift, stale: bool) -> DriftRow {
    let status = if drift.applied == drift.desired {
        AxisStatus::NoDrift
    } else {
        AxisStatus::Drifting
    };
    DriftRow {
        axis,
        applied: drift.applied.clone(),
        desired: drift.desired.clone(),
        status,
        stale,
    }
}

/// Render the rows + machine header as a compact text table.
///
/// Output style mirrors the postgres/redfish state blocks in
/// `nico correlate <id>` so operators see consistent output across
/// the two correlate flows.
pub fn render_drift_text(
    machine_id: &str,
    rows: &[DriftRow],
    now: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("HBN config drift for machine {machine_id}:\n"));
    out.push_str("  axis                       status     applied              desired\n");
    for row in rows {
        let status = match row.status {
            AxisStatus::NoDrift => "no-drift",
            AxisStatus::Drifting => "drift",
        };
        out.push_str(&format!(
            "  {axis:<26} {status:<10} {applied:<20} {desired}\n",
            axis = row.axis,
            status = status,
            applied = row.applied,
            desired = row.desired,
        ));
    }
    if let Some(seen) = last_seen_at {
        let age = format_age(now - seen);
        let stale_marker = if rows.iter().any(|r| r.stale) {
            " (stale)"
        } else {
            ""
        };
        out.push_str(&format!(
            "\nLast observed: {} ({} ago){}\n",
            seen.to_rfc3339(),
            age,
            stale_marker
        ));
    }
    out
}

/// Output for the "no `machines` row for this id" case.
pub fn render_drift_no_data(machine_id: &str) -> String {
    format!(
        "HBN config drift: no machines row for {machine_id}.\n\
         hint: nico correlate {machine_id} to see recent activity for this id.\n"
    )
}

/// PRD-001 §"Status semantics for 'n/a in this deployment-type'": output
/// for the case where the resolved deployment-type lacks forgedb (no
/// drift signal exists). Skipped-by-design — not a fail.
pub fn render_drift_skipped(machine_id: &str, reason: &str) -> String {
    format!("HBN config drift: skipped for {machine_id} — {reason}.\n")
}

/// JSON rendering — same data shape as the text output, intended for
/// scripting consumption. Compatible with `--json`.
pub fn render_drift_json(
    machine_id: &str,
    rows: &[DriftRow],
    now: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
) -> String {
    let row_objs: Vec<_> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "axis": r.axis,
                "applied": r.applied,
                "desired": r.desired,
                "status": match r.status {
                    AxisStatus::NoDrift => "no-drift",
                    AxisStatus::Drifting => "drift",
                },
                "stale": r.stale,
            })
        })
        .collect();
    serde_json::json!({
        "machine_id": machine_id,
        "now": now.to_rfc3339(),
        "last_seen_at": last_seen_at.map(|t| t.to_rfc3339()),
        "rows": row_objs,
    })
    .to_string()
}

/// Default sqlx-backed [`DriftClient`]. Reads the applied/desired
/// version pairs out of the `machines` row's JSON columns. Best-effort:
/// schema-probes the `machines` table first and returns `Ok(None)` when
/// absent (e.g. dev clusters without forgedb).
pub struct SqlxDriftClient {
    pool: sqlx::PgPool,
}

impl SqlxDriftClient {
    pub fn new(url: &str) -> anyhow::Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .map_err(|e| anyhow::anyhow!("invalid postgres URL: {e}"))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl DriftClient for SqlxDriftClient {
    async fn fetch_drift(&self, machine_id: &str) -> Result<Option<DriftSnapshot>> {
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'machines')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("hbn_drift schema probe failed: {e}"))?;
        if !exists.0 {
            return Ok(None);
        }

        let row: Option<(
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )> = sqlx::query_as(
            "SELECT \
                network_status_observation->>'managed_host_config_version', \
                network_status_observation->>'instance_network_config_version', \
                network_config->>'managed_host_config_version', \
                network_config->>'instance_network_config_version', \
                network_status_observation->>'observed_at' \
             FROM machines \
             WHERE id = $1 \
             LIMIT 1",
        )
        .bind(machine_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("hbn_drift query failed: {e}"))?;

        let Some((applied_managed, applied_instance, desired_managed, desired_instance, observed_at)) = row else {
            return Ok(None);
        };

        let last_seen_at = observed_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));

        Ok(Some(DriftSnapshot {
            machine_id: machine_id.to_string(),
            managed_host: AxisDrift {
                applied: applied_managed.unwrap_or_default(),
                desired: desired_managed.unwrap_or_default(),
            },
            instance_network: AxisDrift {
                applied: applied_instance.unwrap_or_default(),
                desired: desired_instance.unwrap_or_default(),
            },
            last_seen_at,
        }))
    }
}

fn format_age(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn axis_in_sync(version: &str) -> AxisDrift {
        AxisDrift {
            applied: version.into(),
            desired: version.into(),
        }
    }

    fn axis_drifting(applied: &str, desired: &str) -> AxisDrift {
        AxisDrift {
            applied: applied.into(),
            desired: desired.into(),
        }
    }

    fn snapshot_in_sync(now: DateTime<Utc>) -> DriftSnapshot {
        DriftSnapshot {
            machine_id: "dpu-bf3-r12u5".into(),
            managed_host: axis_in_sync("v17"),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
        }
    }

    // ─── no-drift ──────────────────────────────────────────────────────────

    #[test]
    fn no_drift_yields_two_rows_both_no_drift() {
        let now = ts(10_000);
        let snap = snapshot_in_sync(now);
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].axis, AXIS_MANAGED_HOST);
        assert_eq!(rows[0].status, AxisStatus::NoDrift);
        assert_eq!(rows[1].axis, AXIS_INSTANCE_NETWORK);
        assert_eq!(rows[1].status, AxisStatus::NoDrift);
    }

    // ─── single-axis drift ─────────────────────────────────────────────────

    #[test]
    fn single_axis_managed_host_drift_only_managed_drifts() {
        let now = ts(10_000);
        let mut snap = snapshot_in_sync(now);
        snap.managed_host = axis_drifting("v16", "v17");

        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);

        let mh = &rows[0];
        assert_eq!(mh.axis, AXIS_MANAGED_HOST);
        assert_eq!(mh.status, AxisStatus::Drifting);
        assert_eq!(mh.applied, "v16");
        assert_eq!(mh.desired, "v17");

        let inet = &rows[1];
        assert_eq!(inet.axis, AXIS_INSTANCE_NETWORK);
        assert_eq!(inet.status, AxisStatus::NoDrift);
    }

    #[test]
    fn single_axis_instance_network_drift_only_instance_drifts() {
        let now = ts(10_000);
        let mut snap = snapshot_in_sync(now);
        snap.instance_network = axis_drifting("v8", "v9");

        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(rows[0].status, AxisStatus::NoDrift);
        assert_eq!(rows[1].status, AxisStatus::Drifting);
        assert_eq!(rows[1].applied, "v8");
        assert_eq!(rows[1].desired, "v9");
    }

    // ─── both-axes drift ───────────────────────────────────────────────────

    #[test]
    fn both_axes_drift_yields_two_drifting_rows() {
        let now = ts(10_000);
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17"),
            instance_network: axis_drifting("v8", "v9"),
            last_seen_at: Some(now),
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(rows.iter().all(|r| r.status == AxisStatus::Drifting));
    }

    // ─── stale data ────────────────────────────────────────────────────────

    #[test]
    fn stale_last_seen_marks_all_rows_stale() {
        let now = ts(10_000);
        let mut snap = snapshot_in_sync(now);
        snap.last_seen_at = Some(now - chrono::Duration::seconds(180));
        let rows = assemble_drift_rows(&snap, now, Duration::from_secs(90));
        assert!(rows.iter().all(|r| r.stale));
    }

    #[test]
    fn fresh_last_seen_leaves_rows_fresh() {
        let now = ts(10_000);
        let mut snap = snapshot_in_sync(now);
        snap.last_seen_at = Some(now - chrono::Duration::seconds(30));
        let rows = assemble_drift_rows(&snap, now, Duration::from_secs(90));
        assert!(rows.iter().all(|r| !r.stale));
    }

    #[test]
    fn missing_last_seen_leaves_rows_fresh() {
        let now = ts(10_000);
        let mut snap = snapshot_in_sync(now);
        snap.last_seen_at = None;
        let rows = assemble_drift_rows(&snap, now, Duration::from_secs(90));
        assert!(rows.iter().all(|r| !r.stale));
    }

    // ─── render_drift_text ─────────────────────────────────────────────────

    #[test]
    fn render_includes_machine_id_and_axis_names() {
        let now = ts(10_000);
        let snap = snapshot_in_sync(now);
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        let out = render_drift_text("dpu-bf3-r12u5", &rows, now, snap.last_seen_at);
        assert!(out.contains("dpu-bf3-r12u5"));
        assert!(out.contains(AXIS_MANAGED_HOST));
        assert!(out.contains(AXIS_INSTANCE_NETWORK));
        assert!(out.contains("no-drift"));
    }

    #[test]
    fn render_drifting_row_shows_applied_and_desired() {
        let now = ts(10_000);
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17"),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        let out = render_drift_text("m1", &rows, now, snap.last_seen_at);
        assert!(out.contains("drift"));
        assert!(out.contains("v16"));
        assert!(out.contains("v17"));
    }

    #[test]
    fn render_drifting_row_does_not_advertise_drift_since() {
        let now = ts(10_000);
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17"),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        let out = render_drift_text("m1", &rows, now, snap.last_seen_at);
        assert!(!out.to_lowercase().contains("drift exists since"));
        assert!(!out.to_lowercase().contains("first observed"));
    }

    #[test]
    fn render_marks_stale_when_last_seen_too_old() {
        let now = ts(10_000);
        let mut snap = snapshot_in_sync(now);
        snap.last_seen_at = Some(now - chrono::Duration::seconds(300));
        let rows = assemble_drift_rows(&snap, now, Duration::from_secs(90));
        let out = render_drift_text("m1", &rows, now, snap.last_seen_at);
        assert!(out.contains("(stale)"), "missing stale marker in: {out}");
    }

    // ─── render_drift_no_data ──────────────────────────────────────────────

    #[test]
    fn no_data_render_mentions_machine_and_correlate_hint() {
        let out = render_drift_no_data("dpu-bf3-r12u5");
        assert!(out.contains("dpu-bf3-r12u5"));
        assert!(out.contains("nico correlate"));
    }

    #[test]
    fn no_data_render_does_not_reference_dpunetworkstatus() {
        let out = render_drift_no_data("dpu-bf3-r12u5");
        assert!(!out.contains("DpuNetworkStatus"));
    }

    // ─── render_drift_skipped (PRD-001 slice 7) ────────────────────────────

    #[test]
    fn render_drift_skipped_includes_machine_id_and_reason() {
        let out = render_drift_skipped("dpu-bf3-r12u5", "n/a in rest-only-mock: no forgedb");
        assert!(out.contains("dpu-bf3-r12u5"));
        assert!(out.contains("n/a in rest-only-mock: no forgedb"));
    }

    #[test]
    fn render_drift_skipped_does_not_advertise_drift_or_error() {
        let out = render_drift_skipped("m1", "n/a in rest-only-mock: no forgedb");
        // n/a-by-design must NOT look like a fail — no "error" / "fail" wording.
        assert!(!out.to_lowercase().contains("error"));
        assert!(!out.to_lowercase().contains("fail"));
        assert!(out.contains("skipped"));
    }

    // ─── render_drift_json ─────────────────────────────────────────────────

    #[test]
    fn render_drift_json_carries_per_row_status_and_versions() {
        let now = ts(10_000);
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17"),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        let out = render_drift_json("m1", &rows, now, snap.last_seen_at);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["machine_id"], "m1");
        assert_eq!(v["rows"][0]["status"], "drift");
        assert_eq!(v["rows"][0]["applied"], "v16");
        assert_eq!(v["rows"][0]["desired"], "v17");
        assert_eq!(v["rows"][1]["status"], "no-drift");
    }

    #[test]
    fn render_drift_json_omits_age_and_alerts_fields() {
        let now = ts(10_000);
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17"),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        let out = render_drift_json("m1", &rows, now, snap.last_seen_at);
        assert!(!out.contains("age_seconds"));
        assert!(!out.contains("alerts"));
    }

    // ─── DriftClient mock-injection sanity ─────────────────────────────────

    struct StubClient(Option<DriftSnapshot>);

    #[async_trait]
    impl DriftClient for StubClient {
        async fn fetch_drift(&self, _machine_id: &str) -> Result<Option<DriftSnapshot>> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn mock_client_returns_injected_snapshot() {
        let now = ts(10_000);
        let snap = snapshot_in_sync(now);
        let client = StubClient(Some(snap.clone()));
        let got = client.fetch_drift("dpu-bf3-r12u5").await.unwrap().unwrap();
        assert_eq!(got.machine_id, snap.machine_id);
        assert_eq!(got.managed_host.applied, "v17");
    }

    #[tokio::test]
    async fn mock_client_returns_none_when_no_row() {
        let client = StubClient(None);
        let got = client.fetch_drift("missing").await.unwrap();
        assert!(got.is_none());
    }
}
