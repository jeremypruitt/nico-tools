//! HBN config-drift correlation — joins desired-vs-applied for the two
//! version axes (`managed_host_config_version`,
//! `instance_network_config_version`) of a DPU/machine, computes the age
//! of any drift, and surfaces probe alerts whose `in_alert_since` falls
//! in the drift window.
//!
//! Read-only, side-effect-free apart from the [`DriftClient`] trait —
//! the seam over the Postgres/forgedb data source. The pure assembly
//! layer (`assemble_drift_rows`) is fully unit-testable without I/O.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Default freshness window for the `last_seen_at` field before a drift
/// row is flagged stale (matches the per-DPU HBN verdict default).
pub const DEFAULT_FRESHNESS_THRESHOLD: Duration = Duration::from_secs(90);

/// Snapshot of HBN drift state for a single machine: per-axis drift,
/// most recent observation timestamp, and the relevant probe alerts.
#[derive(Debug, Clone)]
pub struct DriftSnapshot {
    pub machine_id: String,
    pub managed_host: AxisDrift,
    pub instance_network: AxisDrift,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub alerts: Vec<HealthAlert>,
}

/// Applied vs desired version for one axis, plus the timestamp at which
/// drift was first observed (`None` = currently in sync).
#[derive(Debug, Clone)]
pub struct AxisDrift {
    pub applied: String,
    pub desired: String,
    pub first_observed_drift_at: Option<DateTime<Utc>>,
}

/// A probe alert from the health-report stream. Used to overlay the
/// drift window with concurrent operational signal (e.g.
/// `PostConfigCheckWait`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthAlert {
    pub name: String,
    pub in_alert_since: DateTime<Utc>,
}

/// Read-only seam over the drift data layer. Real impl queries forgedb;
/// tests inject a mock. `Ok(None)` means "no row found for this machine".
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
    pub age: Option<chrono::Duration>,
    pub overlapping_alerts: Vec<HealthAlert>,
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
/// - `age` = `now − first_observed_drift_at` (when present) for drifting
///   axes, else `None`
/// - `overlapping_alerts` for a drifting axis includes any alert whose
///   `in_alert_since` falls in `[drift_start, now]` (inclusive)
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
        row_for_axis(
            AXIS_MANAGED_HOST,
            &snapshot.managed_host,
            now,
            &snapshot.alerts,
            stale,
        ),
        row_for_axis(
            AXIS_INSTANCE_NETWORK,
            &snapshot.instance_network,
            now,
            &snapshot.alerts,
            stale,
        ),
    ]
}

fn row_for_axis(
    axis: &'static str,
    drift: &AxisDrift,
    now: DateTime<Utc>,
    alerts: &[HealthAlert],
    stale: bool,
) -> DriftRow {
    if drift.applied == drift.desired {
        return DriftRow {
            axis,
            applied: drift.applied.clone(),
            desired: drift.desired.clone(),
            status: AxisStatus::NoDrift,
            age: None,
            overlapping_alerts: Vec::new(),
            stale,
        };
    }

    let (age, overlapping) = match drift.first_observed_drift_at {
        Some(start) => {
            let age = Some(now - start);
            let overlapping = alerts
                .iter()
                .filter(|a| a.in_alert_since >= start && a.in_alert_since <= now)
                .cloned()
                .collect();
            (age, overlapping)
        }
        None => (None, Vec::new()),
    };

    DriftRow {
        axis,
        applied: drift.applied.clone(),
        desired: drift.desired.clone(),
        status: AxisStatus::Drifting,
        age,
        overlapping_alerts: overlapping,
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
    out.push_str("  axis                       status     age        alerts\n");
    for row in rows {
        let status = match row.status {
            AxisStatus::NoDrift => "no-drift",
            AxisStatus::Drifting => "drift",
        };
        let age = format_age(row.age);
        let alerts = if row.overlapping_alerts.is_empty() {
            "-".to_string()
        } else {
            row.overlapping_alerts
                .iter()
                .map(|a| {
                    let alert_age = format_age(Some(now - a.in_alert_since));
                    format!("{} (in_alert_since {} ago)", a.name, alert_age)
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(
            "  {axis:<26} {status:<10} {age:<10} {alerts}\n",
            axis = row.axis,
            status = status,
            age = age,
            alerts = alerts,
        ));
    }
    if let Some(seen) = last_seen_at {
        let age = format_age(Some(now - seen));
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

/// Output for the "no row in forgedb for this machine" case.
pub fn render_drift_no_data(machine_id: &str) -> String {
    format!(
        "HBN config drift: no DpuNetworkStatus row for machine {machine_id}.\n\
         hint: nico correlate {machine_id} to see recent activity for this id.\n"
    )
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
                "status": match r.status { AxisStatus::NoDrift => "no-drift", AxisStatus::Drifting => "drift" },
                "age_seconds": r.age.map(|d| d.num_seconds()),
                "stale": r.stale,
                "alerts": r.overlapping_alerts.iter().map(|a| serde_json::json!({
                    "name": a.name,
                    "in_alert_since": a.in_alert_since.to_rfc3339(),
                    "in_alert_age_seconds": (now - a.in_alert_since).num_seconds(),
                })).collect::<Vec<_>>(),
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

/// Default sqlx-backed [`DriftClient`]. Best-effort: queries the
/// canonical forgedb tables and gracefully degrades to `Ok(None)` /
/// empty fields when the schema is absent (so dev clusters without the
/// drift-history / health-report tables don't panic).
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

    async fn table_exists(&self, name: &str) -> anyhow::Result<bool> {
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = $1)",
        )
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("schema probe failed: {e}"))?;
        Ok(exists.0)
    }
}

#[async_trait]
impl DriftClient for SqlxDriftClient {
    async fn fetch_drift(&self, machine_id: &str) -> Result<Option<DriftSnapshot>> {
        if !self.table_exists("dpu_network_status").await? {
            return Ok(None);
        }
        if !self.table_exists("dpu_desired_network_config").await? {
            return Ok(None);
        }

        let row: Option<(String, String, String, String, String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT \
                s.dpu_id, \
                s.applied_managed_host_config_version, \
                s.applied_instance_network_config_version, \
                d.managed_host_config_version, \
                d.instance_network_config_version, \
                s.last_seen_at \
             FROM dpu_network_status s \
             JOIN dpu_desired_network_config d ON d.dpu_id = s.dpu_id \
             WHERE s.dpu_id = $1 \
             ORDER BY s.last_seen_at DESC \
             LIMIT 1",
        )
        .bind(machine_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("dpu_network_status query failed: {e}"))?;

        let Some(r) = row else {
            return Ok(None);
        };
        let (
            machine_id_db,
            applied_managed,
            applied_instance,
            desired_managed,
            desired_instance,
            last_seen,
        ) = r;

        let managed_host_drift = self
            .first_drift_at(
                &machine_id_db,
                "applied_managed_host_config_version",
                &desired_managed,
            )
            .await
            .ok()
            .flatten();
        let instance_drift = self
            .first_drift_at(
                &machine_id_db,
                "applied_instance_network_config_version",
                &desired_instance,
            )
            .await
            .ok()
            .flatten();

        let alerts = self.fetch_alerts(&machine_id_db).await.unwrap_or_default();

        Ok(Some(DriftSnapshot {
            machine_id: machine_id_db,
            managed_host: AxisDrift {
                applied: applied_managed.clone(),
                desired: desired_managed.clone(),
                first_observed_drift_at: if applied_managed != desired_managed {
                    managed_host_drift
                } else {
                    None
                },
            },
            instance_network: AxisDrift {
                applied: applied_instance.clone(),
                desired: desired_instance.clone(),
                first_observed_drift_at: if applied_instance != desired_instance {
                    instance_drift
                } else {
                    None
                },
            },
            last_seen_at: Some(last_seen),
            alerts,
        }))
    }
}

impl SqlxDriftClient {
    async fn first_drift_at(
        &self,
        machine_id: &str,
        applied_col: &str,
        desired: &str,
    ) -> anyhow::Result<Option<DateTime<Utc>>> {
        if !self.table_exists("dpu_network_status_history").await? {
            return Ok(None);
        }
        // applied_col is one of two internal constants — never user input.
        let sql = format!(
            "SELECT MIN(last_seen_at) FROM dpu_network_status_history \
             WHERE dpu_id = $1 AND {applied_col} <> $2"
        );
        let row: Option<(Option<DateTime<Utc>>,)> = sqlx::query_as(&sql)
            .bind(machine_id)
            .bind(desired)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("drift-history query failed: {e}"))?;
        Ok(row.and_then(|r| r.0))
    }

    async fn fetch_alerts(&self, machine_id: &str) -> anyhow::Result<Vec<HealthAlert>> {
        if !self.table_exists("health_report").await? {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT alert_name, in_alert_since FROM health_report \
             WHERE dpu_id = $1 AND in_alert_since IS NOT NULL \
             ORDER BY in_alert_since DESC LIMIT 50",
        )
        .bind(machine_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("health_report query failed: {e}"))?;
        Ok(rows
            .into_iter()
            .map(|(name, since)| HealthAlert {
                name,
                in_alert_since: since,
            })
            .collect())
    }
}

fn format_age(d: Option<chrono::Duration>) -> String {
    match d {
        None => "-".to_string(),
        Some(d) => {
            let secs = d.num_seconds().max(0);
            if secs < 60 {
                format!("{secs}s")
            } else if secs < 3600 {
                format!("{}m{:02}s", secs / 60, secs % 60)
            } else {
                format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
            }
        }
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
            first_observed_drift_at: None,
        }
    }

    fn axis_drifting(applied: &str, desired: &str, drift_start: DateTime<Utc>) -> AxisDrift {
        AxisDrift {
            applied: applied.into(),
            desired: desired.into(),
            first_observed_drift_at: Some(drift_start),
        }
    }

    fn snapshot_in_sync(now: DateTime<Utc>) -> DriftSnapshot {
        DriftSnapshot {
            machine_id: "dpu-bf3-r12u5".into(),
            managed_host: axis_in_sync("v17"),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
            alerts: vec![],
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
        assert!(rows[0].age.is_none());
        assert!(rows[0].overlapping_alerts.is_empty());
        assert_eq!(rows[1].axis, AXIS_INSTANCE_NETWORK);
        assert_eq!(rows[1].status, AxisStatus::NoDrift);
        assert!(rows[1].age.is_none());
    }

    // ─── single-axis drift ─────────────────────────────────────────────────

    #[test]
    fn single_axis_managed_host_drift_only_managed_drifts() {
        let now = ts(10_000);
        let drift_start = ts(9_000);
        let mut snap = snapshot_in_sync(now);
        snap.managed_host = axis_drifting("v16", "v17", drift_start);

        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);

        let mh = &rows[0];
        assert_eq!(mh.axis, AXIS_MANAGED_HOST);
        assert_eq!(mh.status, AxisStatus::Drifting);
        assert_eq!(mh.applied, "v16");
        assert_eq!(mh.desired, "v17");
        assert_eq!(mh.age, Some(chrono::Duration::seconds(1_000)));

        let inet = &rows[1];
        assert_eq!(inet.axis, AXIS_INSTANCE_NETWORK);
        assert_eq!(inet.status, AxisStatus::NoDrift);
    }

    #[test]
    fn single_axis_instance_network_drift_only_instance_drifts() {
        let now = ts(10_000);
        let mut snap = snapshot_in_sync(now);
        snap.instance_network = axis_drifting("v8", "v9", ts(8_500));

        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(rows[0].status, AxisStatus::NoDrift);
        assert_eq!(rows[1].status, AxisStatus::Drifting);
        assert_eq!(rows[1].age, Some(chrono::Duration::seconds(1_500)));
    }

    // ─── both-axes drift ───────────────────────────────────────────────────

    #[test]
    fn both_axes_drift_yields_two_drifting_rows() {
        let now = ts(10_000);
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17", ts(9_400)),
            instance_network: axis_drifting("v8", "v9", ts(9_700)),
            last_seen_at: Some(now),
            alerts: vec![],
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(rows.iter().all(|r| r.status == AxisStatus::Drifting));
        assert_eq!(rows[0].age, Some(chrono::Duration::seconds(600)));
        assert_eq!(rows[1].age, Some(chrono::Duration::seconds(300)));
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

    // ─── alerts in drift window ────────────────────────────────────────────

    #[test]
    fn alert_within_drift_window_appears_on_drifting_row() {
        let now = ts(10_000);
        let drift_start = ts(9_000);
        let alert = HealthAlert {
            name: "PostConfigCheckWait".into(),
            in_alert_since: ts(9_500),
        };
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17", drift_start),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
            alerts: vec![alert.clone()],
        };

        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(rows[0].overlapping_alerts, vec![alert]);
        assert!(rows[1].overlapping_alerts.is_empty());
    }

    #[test]
    fn alert_before_drift_window_is_excluded() {
        let now = ts(10_000);
        let drift_start = ts(9_000);
        let alert_before = HealthAlert {
            name: "PostConfigCheckWait".into(),
            in_alert_since: ts(8_500),
        };
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17", drift_start),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
            alerts: vec![alert_before],
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(rows[0].overlapping_alerts.is_empty());
    }

    #[test]
    fn alert_does_not_attach_when_axis_in_sync() {
        let now = ts(10_000);
        let alert = HealthAlert {
            name: "PostConfigCheckWait".into(),
            in_alert_since: ts(9_500),
        };
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_in_sync("v17"),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
            alerts: vec![alert],
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        assert!(rows.iter().all(|r| r.overlapping_alerts.is_empty()));
    }

    // ─── render_drift_text smoke test ──────────────────────────────────────

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
    fn render_drifting_row_contains_age_and_alert() {
        let now = ts(10_000);
        let drift_start = ts(9_400);
        let alert = HealthAlert {
            name: "PostConfigCheckWait".into(),
            in_alert_since: ts(9_500),
        };
        let snap = DriftSnapshot {
            machine_id: "m1".into(),
            managed_host: axis_drifting("v16", "v17", drift_start),
            instance_network: axis_in_sync("v9"),
            last_seen_at: Some(now),
            alerts: vec![alert],
        };
        let rows = assemble_drift_rows(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        let out = render_drift_text("m1", &rows, now, snap.last_seen_at);
        assert!(out.contains("drift"));
        assert!(out.contains("10m"), "age missing in: {out}");
        assert!(out.contains("PostConfigCheckWait"));
    }

    #[test]
    fn no_data_render_mentions_machine_and_correlate_hint() {
        let out = render_drift_no_data("dpu-bf3-r12u5");
        assert!(out.contains("dpu-bf3-r12u5"));
        assert!(out.contains("nico correlate"));
    }
}
