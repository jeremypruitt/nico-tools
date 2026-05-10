//! HBN (Host-Based Networking) verdict — single-DPU health check.
//!
//! This is the tracer-bullet data layer for `nico doctor hbn <dpu-id>`
//! (issue #205). It defines the trait we query for a DPU's most recent
//! `DpuNetworkStatus` plus its desired-config peer, and the pure check
//! assembly that turns that snapshot into a [`Vec<Check>`] for the
//! existing headline-vs-detail renderer.
//!
//! Intentionally read-only and side-effect-free apart from the trait —
//! everything else is data-in / checks-out and unit-testable without
//! touching Postgres.

use std::cmp::Ordering;
use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};
use crate::verdicts::{hbn_verdict, AxisSummary};

/// Minimum HBN version required by NVUE-managed config flows.
pub const NVUE_MINIMUM_HBN_VERSION: &str = "2.0.0-doca2.5.0";

/// Minimum HBN version required by FMDS-managed flows. Below this we
/// emit a *warning* (informational) — operators may still want to plan
/// the upgrade even if the cluster is functionally OK.
pub const FMDS_MINIMUM_HBN_VERSION: &str = "1.5.0-doca2.2.0";

/// Default freshness window for the most recent `DpuNetworkStatus`
/// before the per-DPU verdict goes Unknown.
pub const DEFAULT_FRESHNESS_THRESHOLD: Duration = Duration::from_secs(90);

/// A snapshot of HBN-relevant data for a single DPU. Combines fields
/// from the producer-side `machines.network_status_observation` JSON
/// (applied state) and `machines.network_config` JSON (desired state)
/// on the same row, joined with `instances.network_config_version` for
/// the desired instance-network axis (PRD-002).
///
/// `network_config_error`, when set, is the agent's explicit error
/// message and surfaces as the verdict headline — more actionable than
/// "versions disagree" because it tells the operator *why*. `bgp_alerts`
/// is narrowed to BGP-typed alerts only; other categories live in the
/// `dpu_health` layer.
#[derive(Debug, Clone)]
pub struct HbnSnapshot {
    pub dpu_id: String,
    pub hbn_version: String,
    pub applied_managed_host_config_version: String,
    pub desired_managed_host_config_version: String,
    pub applied_instance_network_config_version: String,
    pub desired_instance_network_config_version: String,
    pub network_config_error: Option<String>,
    pub bgp_alerts: Vec<String>,
    pub quarantine_state: Option<String>,
    pub last_seen_at: DateTime<Utc>,
}

/// Read-only seam over the HBN data layer (forgedb + Postgres). The
/// real impl queries `dpu_network_status` and the matching
/// desired-config row; tests inject mocks. Returning `Ok(None)` means
/// "no row found for this DPU" (the no-recent-status acceptance case).
#[async_trait]
pub trait HbnClient: Send + Sync {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HbnSnapshot>>;

    /// Fetch the most recent `DpuNetworkStatus` + desired-config peer
    /// for every DPU in forgedb. Powers the `nico ops hbn` per-DPU panel
    /// (issue #209). Returns an empty vec when the schema is absent so
    /// the panel renders an empty table instead of erroring on dev
    /// clusters; populated otherwise.
    async fn fetch_all_snapshots(&self) -> Result<Vec<HbnSnapshot>>;
}

/// Default sqlx-backed [`HbnClient`]. Reads producer-side state from
/// the `machines` row (PRD-002): `network_status_observation` JSON for
/// applied state plus `network_config_error` and BGP-typed alerts,
/// `network_config` JSON for desired state including `quarantine_state`,
/// `inventory` for the running HBN component version, and
/// `instances.network_config_version` (joined on `instances.machine_id`)
/// for the desired instance axis. Schema-probes `machines` and degrades
/// to `Ok(None)` / empty vec when absent so dev clusters that don't have
/// the table render "no recent status" instead of panicking.
pub struct SqlxHbnClient {
    pool: sqlx::PgPool,
}

/// SQL columns shared by the per-DPU and fleet queries. Extracted as a
/// constant so the schema choice (which producer-side JSON paths the
/// layer reads) is pinned by a unit test, even though we can't exercise
/// the full query without a live postgres.
pub(crate) const FETCH_SNAPSHOT_COLS: &str = "\
    m.id, \
    COALESCE(m.network_status_observation->>'network_config_version', ''), \
    COALESCE(m.network_status_observation->'instance_network_observation'->>'config_version', ''), \
    m.network_config_version, \
    COALESCE(i.network_config_version, ''), \
    NULLIF(m.network_status_observation->>'network_config_error', ''), \
    m.network_config->'quarantine_state'->>'mode', \
    (m.network_status_observation->>'observed_at')::timestamptz, \
    (SELECT c->>'version' FROM jsonb_array_elements(COALESCE(m.inventory->'components', '[]'::jsonb)) c \
       WHERE c->>'name' = 'hbn' LIMIT 1), \
    m.dpu_agent_health_report";

pub(crate) const FETCH_SNAPSHOT_FROM: &str = "\
    FROM machines m \
    LEFT JOIN instances i ON i.machine_id = m.id";

impl SqlxHbnClient {
    pub fn new(url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .map_err(|e| anyhow::anyhow!("invalid postgres URL: {e}"))?;
        Ok(Self { pool })
    }
}

type SnapshotRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    DateTime<Utc>,
    Option<String>,
    Option<serde_json::Value>,
);

fn row_to_snapshot(r: SnapshotRow) -> HbnSnapshot {
    HbnSnapshot {
        dpu_id: r.0,
        applied_managed_host_config_version: r.1,
        applied_instance_network_config_version: r.2,
        desired_managed_host_config_version: r.3,
        desired_instance_network_config_version: r.4,
        network_config_error: r.5,
        quarantine_state: r.6,
        last_seen_at: r.7,
        hbn_version: r.8.unwrap_or_default(),
        bgp_alerts: parse_bgp_alerts(r.9.as_ref()),
    }
}

async fn machines_table_exists(pool: &sqlx::PgPool) -> Result<bool> {
    let exists: (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_name = 'machines')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("hbn schema probe failed: {e}"))?;
    Ok(exists.0)
}

#[async_trait]
impl HbnClient for SqlxHbnClient {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HbnSnapshot>> {
        if !machines_table_exists(&self.pool).await? {
            return Ok(None);
        }

        let sql = format!(
            "SELECT {FETCH_SNAPSHOT_COLS} {FETCH_SNAPSHOT_FROM} \
             WHERE m.id = $1 AND m.network_status_observation IS NOT NULL \
             LIMIT 1"
        );
        let row: Option<SnapshotRow> = sqlx::query_as(&sql)
            .bind(dpu_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("hbn snapshot query failed: {e}"))?;

        Ok(row.map(row_to_snapshot))
    }

    async fn fetch_all_snapshots(&self) -> Result<Vec<HbnSnapshot>> {
        if !machines_table_exists(&self.pool).await? {
            return Ok(Vec::new());
        }

        let sql = format!(
            "SELECT {FETCH_SNAPSHOT_COLS} {FETCH_SNAPSHOT_FROM} \
             WHERE m.network_status_observation IS NOT NULL"
        );
        let rows: Vec<SnapshotRow> = sqlx::query_as(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("hbn fleet query failed: {e}"))?;

        Ok(rows.into_iter().map(row_to_snapshot).collect())
    }
}

/// Extract BGP-typed alert strings from the `dpu_agent_health_report`
/// JSON blob. The agent's `HealthReport.alerts` array carries a
/// `category` discriminator; only `Bgp` entries surface in the `hbn`
/// layer (PRD-002). Other categories are owned by `dpu_health`.
///
/// Each alert is rendered as `"{id}"` (or `"{id}: {message}"` when the
/// agent attached a message). Tolerates the column being NULL or having
/// no `alerts` array.
pub fn parse_bgp_alerts(blob: Option<&serde_json::Value>) -> Vec<String> {
    let Some(v) = blob else { return Vec::new() };
    let Some(arr) = v.get("alerts").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter(|entry| {
            entry
                .get("category")
                .and_then(|c| c.as_str())
                .map(|c| c.eq_ignore_ascii_case("bgp"))
                .unwrap_or(false)
        })
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            let message = entry.get("message").and_then(|m| m.as_str()).unwrap_or("");
            if message.is_empty() {
                Some(id.to_owned())
            } else {
                Some(format!("{id}: {message}"))
            }
        })
        .collect()
}

/// One row in the per-DPU HBN panel (`nico ops hbn`).
///
/// Pure aggregation of an [`HbnSnapshot`] into the columns the renderer
/// displays — applied/desired versions per axis, drift booleans, an
/// overall row status, and a drift-age proxy used by the renderer for the
/// `DRIFT` column. Layout selection (Option A wide vs. Option B narrow)
/// happens in `nico-ops::hbn_panel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbnRow {
    pub machine_id: String,
    pub hbn_version: String,
    pub managed_host_applied: String,
    pub managed_host_desired: String,
    pub instance_network_applied: String,
    pub instance_network_desired: String,
    pub managed_host_drift: bool,
    pub instance_network_drift: bool,
    pub quarantine_state: Option<String>,
    pub status: HbnRowStatus,
    /// Lower-bound drift age: `now - last_seen_at` when either axis is
    /// drifting. Zero when both axes are aligned. The DPU has been in
    /// the observed applied state since at least `last_seen_at`.
    pub drift_age: Duration,
}

/// Per-row status used to drive sortability and the Option B `STATUS`
/// column. Precedence: `Quarantined` > `Unhealthy` > `Drift` > `Healthy`.
/// `Unhealthy` now reflects an explicit `network_config_error` from the
/// agent (PRD-002): a producer-side error is more actionable than
/// "versions disagree" because it tells the operator *why*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HbnRowStatus {
    Healthy,
    Drift,
    Unhealthy,
    Quarantined,
}

/// Aggregate one [`HbnSnapshot`] into a displayable [`HbnRow`].
///
/// `now` is provided by the caller so the function stays pure (no clock
/// reads). Drift age is `now - last_seen_at` clamped at zero — and zero
/// when neither axis is drifting.
pub fn aggregate_row(snap: &HbnSnapshot, now: DateTime<Utc>) -> HbnRow {
    let managed_host_drift =
        snap.applied_managed_host_config_version != snap.desired_managed_host_config_version;
    let instance_network_drift = snap.applied_instance_network_config_version
        != snap.desired_instance_network_config_version;

    let drift_age = if managed_host_drift || instance_network_drift {
        (now - snap.last_seen_at).to_std().unwrap_or(Duration::ZERO)
    } else {
        Duration::ZERO
    };

    let status = if snap.quarantine_state.is_some() {
        HbnRowStatus::Quarantined
    } else if snap.network_config_error.is_some() {
        HbnRowStatus::Unhealthy
    } else if managed_host_drift || instance_network_drift {
        HbnRowStatus::Drift
    } else {
        HbnRowStatus::Healthy
    };

    HbnRow {
        machine_id: snap.dpu_id.clone(),
        hbn_version: snap.hbn_version.clone(),
        managed_host_applied: snap.applied_managed_host_config_version.clone(),
        managed_host_desired: snap.desired_managed_host_config_version.clone(),
        instance_network_applied: snap.applied_instance_network_config_version.clone(),
        instance_network_desired: snap.desired_instance_network_config_version.clone(),
        managed_host_drift,
        instance_network_drift,
        quarantine_state: snap.quarantine_state.clone(),
        status,
        drift_age,
    }
}

/// Compare two HBN version strings (e.g. `"2.0.0-doca2.5.0"`).
///
/// Splits on `-doca` and compares each side as a dot-delimited tuple of
/// integers, padding shorter tuples with zeros. Non-numeric suffixes
/// fall back to lexicographic comparison.
pub fn compare_hbn_versions(lhs: &str, rhs: &str) -> Ordering {
    let split = |s: &str| -> (Vec<u64>, Vec<u64>) {
        let (head, tail) = match s.split_once("-doca") {
            Some((h, t)) => (h, t),
            None => (s, ""),
        };
        let parse = |s: &str| -> Vec<u64> {
            if s.is_empty() {
                return vec![];
            }
            s.split('.')
                .map(|p| p.parse::<u64>().unwrap_or(0))
                .collect()
        };
        (parse(head), parse(tail))
    };

    let cmp_tuples = |a: &[u64], b: &[u64]| -> Ordering {
        let n = a.len().max(b.len());
        for i in 0..n {
            let av = a.get(i).copied().unwrap_or(0);
            let bv = b.get(i).copied().unwrap_or(0);
            match av.cmp(&bv) {
                Ordering::Equal => continue,
                non_eq => return non_eq,
            }
        }
        Ordering::Equal
    };

    let (lh, lt) = split(lhs);
    let (rh, rt) = split(rhs);
    match cmp_tuples(&lh, &rh) {
        Ordering::Equal => cmp_tuples(&lt, &rt),
        non_eq => non_eq,
    }
}

/// Assemble the per-DPU HBN check list from a snapshot.
///
/// Produces exactly one `Headline` (sourced from
/// [`crate::verdicts::hbn_verdict`]) followed by one `Detail` per signal
/// in the order the issue lists them. Pure — no I/O, fully
/// unit-testable.
///
/// Since PRD-003 Slice 3 (#307) the headline is the rolled-up verdict
/// instead of one headline-per-signal. Every signal that previously
/// surfaced as its own headline (drift / network-config-error / BGP
/// alerts / freshness / version-below-minimum) is preserved as a detail
/// row so nothing is dropped from the JSON output. JSON ordering:
/// headline first (`kind: "headline"`), then detail rows.
pub fn assemble_checks(
    snapshot: &HbnSnapshot,
    now: DateTime<Utc>,
    freshness_threshold: Duration,
) -> Vec<Check> {
    let mut details: Vec<Check> = Vec::new();

    // 1. Producer-side network_config_error — when present, this is the
    //    most actionable signal and pins the headline (PRD-002).
    if let Some(err) = snapshot.network_config_error.as_deref().filter(|s| !s.is_empty()) {
        details.push(Check {
            name: "network_config_error",
            status: Status::Fail,
            value: format!("network_config_error: {err}"),
            next_command: Some(format!(
                "nico correlate {} # trace the failed config apply",
                snapshot.dpu_id
            )),
            kind: CheckKind::Detail,
        });
    }

    // 2. HBN version >= NVUE minimum (hard requirement)
    let nvue_ok =
        compare_hbn_versions(&snapshot.hbn_version, NVUE_MINIMUM_HBN_VERSION) != Ordering::Less;
    details.push(if nvue_ok {
        Check {
            name: "version_nvue",
            status: Status::Ok,
            value: format!(
                "hbn {} >= nvue minimum {}",
                snapshot.hbn_version, NVUE_MINIMUM_HBN_VERSION
            ),
            next_command: None,
            kind: CheckKind::Detail,
        }
    } else {
        Check {
            name: "version_nvue",
            status: Status::Fail,
            value: format!(
                "hbn {} below nvue minimum {}",
                snapshot.hbn_version, NVUE_MINIMUM_HBN_VERSION
            ),
            next_command: Some("plan hbn upgrade for this DPU".to_string()),
            kind: CheckKind::Detail,
        }
    });

    // 3. HBN version >= FMDS minimum (informational; warn if below)
    let fmds_ok =
        compare_hbn_versions(&snapshot.hbn_version, FMDS_MINIMUM_HBN_VERSION) != Ordering::Less;
    details.push(if fmds_ok {
        Check {
            name: "version_fmds",
            status: Status::Ok,
            value: format!(
                "hbn {} >= fmds minimum {}",
                snapshot.hbn_version, FMDS_MINIMUM_HBN_VERSION
            ),
            next_command: None,
            kind: CheckKind::Detail,
        }
    } else {
        Check {
            name: "version_fmds",
            status: Status::Warn,
            value: format!(
                "hbn {} below fmds minimum {} (informational)",
                snapshot.hbn_version, FMDS_MINIMUM_HBN_VERSION
            ),
            next_command: None,
            kind: CheckKind::Detail,
        }
    });

    // 4. Applied managed_host_config_version matches desired
    details.push(version_match_check(
        "managed_host_config",
        &snapshot.applied_managed_host_config_version,
        &snapshot.desired_managed_host_config_version,
    ));

    // 5. Applied instance_network_config_version matches desired
    details.push(version_match_check(
        "instance_network_config",
        &snapshot.applied_instance_network_config_version,
        &snapshot.desired_instance_network_config_version,
    ));

    // 6. BGP peer state — alerts list empty == healthy
    details.push(if snapshot.bgp_alerts.is_empty() {
        Check {
            name: "bgp",
            status: Status::Ok,
            value: "bgp peers healthy".to_string(),
            next_command: None,
            kind: CheckKind::Detail,
        }
    } else {
        Check {
            name: "bgp",
            status: Status::Fail,
            value: format!(
                "bgp alerts: {}",
                snapshot.bgp_alerts.join(", ")
            ),
            next_command: Some(format!(
                "ssh dpu-{} 'nv show vrf default router bgp summary'",
                snapshot.dpu_id
            )),
            kind: CheckKind::Detail,
        }
    });

    // 7. Quarantine state must be unset
    details.push(match snapshot.quarantine_state.as_deref() {
        None | Some("") => Check {
            name: "quarantine",
            status: Status::Ok,
            value: "not quarantined".to_string(),
            next_command: None,
            kind: CheckKind::Detail,
        },
        Some(state) => Check {
            name: "quarantine",
            status: Status::Fail,
            value: format!("quarantined: {state}"),
            next_command: Some(format!(
                "nico correlate {} # see why this DPU was quarantined",
                snapshot.dpu_id
            )),
            kind: CheckKind::Detail,
        },
    });

    // 8. Last-seen freshness
    let age = (now - snapshot.last_seen_at)
        .to_std()
        .unwrap_or(Duration::ZERO);
    details.push(if age <= freshness_threshold {
        Check {
            name: "last_seen",
            status: Status::Ok,
            value: format!("last seen {}s ago", age.as_secs()),
            next_command: None,
            kind: CheckKind::Detail,
        }
    } else {
        Check {
            name: "last_seen",
            status: Status::Warn,
            value: format!(
                "last seen {}s ago (threshold {}s)",
                age.as_secs(),
                freshness_threshold.as_secs()
            ),
            next_command: Some(format!(
                "nico correlate {} # check DPU agent connectivity",
                snapshot.dpu_id
            )),
            kind: CheckKind::Detail,
        }
    });

    let summary = hbn_verdict(snapshot, now, freshness_threshold);
    let mut out = Vec::with_capacity(details.len() + 1);
    out.push(headline_from(&summary));
    out.extend(details);
    out
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

/// Build the "no recent status row" check list — used when the data
/// layer returns `Ok(None)` for a DPU (no `machines` row, or the row's
/// `network_status_observation` JSON column is NULL). Renders as a
/// single `Unknown` headline; no detail bullets (we have nothing to say
/// about state we never received).
pub fn assemble_no_status_checks(dpu_id: &str) -> Vec<Check> {
    vec![Check {
        name: "hbn",
        status: Status::Unknown,
        value: format!("no recent network_status_observation for dpu {dpu_id}"),
        next_command: Some(format!(
            "nico correlate {dpu_id} # last activity for this DPU"
        )),
        kind: CheckKind::Headline,
    }]
}

/// Build the check list for a data-layer error (Postgres unreachable,
/// query errored, etc.). Single Unknown headline so the verdict
/// surfaces the underlying error message verbatim.
pub fn assemble_error_checks(dpu_id: &str, err: &str) -> Vec<Check> {
    vec![Check {
        name: "hbn",
        status: Status::Unknown,
        value: format!("hbn data layer error for dpu {dpu_id}: {err}"),
        next_command: Some("check forgedb / postgres connectivity".to_string()),
        kind: CheckKind::Headline,
    }]
}

fn version_match_check(name: &'static str, applied: &str, desired: &str) -> Check {
    if applied == desired {
        Check {
            name,
            status: Status::Ok,
            value: format!("{name} applied={applied}"),
            next_command: None,
            kind: CheckKind::Detail,
        }
    } else {
        Check {
            name,
            status: Status::Fail,
            value: format!("{name} drift: applied={applied} desired={desired}"),
            next_command: Some(format!(
                "select * from {name}_version where applied <> desired"
            )),
            kind: CheckKind::Detail,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

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

    // ── aggregate_row: per-DPU display columns ───────────────────────────

    #[test]
    fn aggregate_row_healthy_snapshot_yields_healthy_status_no_drift() {
        let snap = snap_healthy();
        let row = aggregate_row(&snap, snap.last_seen_at);
        assert_eq!(row.machine_id, "dpu-42");
        assert_eq!(row.hbn_version, "2.0.0-doca2.5.0");
        assert_eq!(row.managed_host_applied, "v17");
        assert_eq!(row.managed_host_desired, "v17");
        assert_eq!(row.instance_network_applied, "v9");
        assert_eq!(row.instance_network_desired, "v9");
        assert!(!row.managed_host_drift);
        assert!(!row.instance_network_drift);
        assert!(row.quarantine_state.is_none());
        assert_eq!(row.status, HbnRowStatus::Healthy);
    }

    #[test]
    fn aggregate_row_managed_host_drift_marks_only_that_axis() {
        let mut snap = snap_healthy();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        let row = aggregate_row(&snap, snap.last_seen_at);
        assert!(row.managed_host_drift);
        assert!(!row.instance_network_drift);
        assert_eq!(row.status, HbnRowStatus::Drift);
    }

    #[test]
    fn aggregate_row_quarantined_snapshot_status_overrides_drift() {
        let mut snap = snap_healthy();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        let row = aggregate_row(&snap, snap.last_seen_at);
        assert_eq!(row.quarantine_state.as_deref(), Some("BlockAllTraffic"));
        assert_eq!(row.status, HbnRowStatus::Quarantined);
    }

    #[test]
    fn aggregate_row_network_config_error_marks_unhealthy() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("apply failed: bgp peer down".into());
        let row = aggregate_row(&snap, snap.last_seen_at);
        assert_eq!(row.status, HbnRowStatus::Unhealthy);
    }

    #[test]
    fn aggregate_row_quarantine_overrides_network_config_error() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("apply failed".into());
        snap.quarantine_state = Some("BlockAllTraffic".into());
        let row = aggregate_row(&snap, snap.last_seen_at);
        // Quarantine still wins (precedence: Quarantined > Unhealthy).
        assert_eq!(row.status, HbnRowStatus::Quarantined);
    }

    #[test]
    fn aggregate_row_drift_age_uses_last_seen_when_drifting() {
        let mut snap = snap_healthy();
        snap.applied_instance_network_config_version = "v8".into();
        snap.desired_instance_network_config_version = "v9".into();
        snap.last_seen_at = Utc::now() - chrono::Duration::seconds(240);
        let row = aggregate_row(&snap, Utc::now());
        // Drift age = now - last_seen_at when drifting
        let secs = row.drift_age.as_secs();
        assert!((235..=245).contains(&secs), "drift_age was {secs}s");
    }

    #[test]
    fn aggregate_row_drift_age_zero_when_not_drifting() {
        let snap = snap_healthy();
        let row = aggregate_row(&snap, Utc::now() + chrono::Duration::seconds(120));
        assert_eq!(row.drift_age, std::time::Duration::ZERO);
    }

    // ── compare_hbn_versions ──────────────────────────────────────────────

    #[test]
    fn compare_versions_equal_returns_equal() {
        assert_eq!(
            compare_hbn_versions("2.0.0-doca2.5.0", "2.0.0-doca2.5.0"),
            Ordering::Equal,
        );
    }

    #[test]
    fn compare_versions_higher_hbn_wins_over_lower() {
        assert_eq!(
            compare_hbn_versions("2.1.0-doca2.5.0", "2.0.0-doca2.5.0"),
            Ordering::Greater,
        );
    }

    #[test]
    fn compare_versions_doca_suffix_breaks_tie() {
        assert_eq!(
            compare_hbn_versions("2.0.0-doca2.6.0", "2.0.0-doca2.5.0"),
            Ordering::Greater,
        );
    }

    #[test]
    fn compare_versions_below_nvue_minimum_is_less() {
        assert_eq!(
            compare_hbn_versions("1.5.0-doca2.2.0", NVUE_MINIMUM_HBN_VERSION),
            Ordering::Less,
        );
    }

    // ── assemble_checks: all-healthy ──────────────────────────────────────

    #[test]
    fn all_healthy_yields_ok_headline_and_only_ok_details() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .expect("headline check");
        assert_eq!(headline.status, Status::Ok);
        assert!(
            headline.value.contains("dpu-42") && headline.value.contains("ok"),
            "headline: {}",
            headline.value
        );
        assert!(
            checks
                .iter()
                .filter(|c| c.kind == CheckKind::Detail)
                .all(|c| c.status == Status::Ok),
            "expected all details Ok, got {:?}",
            checks
                .iter()
                .filter(|c| c.kind == CheckKind::Detail)
                .map(|c| (c.name, c.status.clone()))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn all_healthy_emits_exactly_one_headline() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            checks.iter().filter(|c| c.kind == CheckKind::Headline).count(),
            1
        );
    }

    // ── assemble_checks: version-stale (below nvue minimum) ───────────────

    #[test]
    fn hbn_version_below_nvue_minimum_fails_with_detail() {
        let mut snap = snap_healthy();
        snap.hbn_version = "1.9.0-doca2.4.0".into();
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks.iter().find(|c| c.kind == CheckKind::Headline).unwrap();
        assert_eq!(headline.status, Status::Fail);
        let nvue_detail = checks.iter().find(|c| c.name == "version_nvue").unwrap();
        assert_eq!(nvue_detail.status, Status::Fail);
        assert!(
            nvue_detail.value.contains("1.9.0-doca2.4.0"),
            "value: {}",
            nvue_detail.value
        );
        assert!(nvue_detail.next_command.is_some());
    }

    #[test]
    fn hbn_version_below_fmds_minimum_warns_only() {
        let mut snap = snap_healthy();
        snap.hbn_version = "1.4.0-doca2.1.0".into();
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let fmds_detail = checks.iter().find(|c| c.name == "version_fmds").unwrap();
        assert_eq!(fmds_detail.status, Status::Warn);
        assert!(fmds_detail.value.contains("informational"));
    }

    // ── assemble_checks: quarantined ─────────────────────────────────────

    #[test]
    fn quarantined_dpu_fails_with_correlate_hint() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("manual".into());
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks.iter().find(|c| c.kind == CheckKind::Headline).unwrap();
        assert_eq!(headline.status, Status::Fail);
        let q = checks.iter().find(|c| c.name == "quarantine").unwrap();
        assert_eq!(q.status, Status::Fail);
        assert!(q.value.contains("manual"));
        assert!(q.next_command.as_deref().unwrap().contains("nico correlate"));
    }

    // ── assemble_no_status_checks: no recent status ──────────────────────

    #[test]
    fn no_recent_status_yields_single_unknown_headline_only() {
        let checks = assemble_no_status_checks("dpu-42");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("dpu-42"));
        assert!(checks[0].value.contains("no recent"));
    }

    // ── assemble_checks: single-version-drift ────────────────────────────

    #[test]
    fn single_managed_host_config_drift_fails_only_that_detail() {
        let mut snap = snap_healthy();
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks.iter().find(|c| c.kind == CheckKind::Headline).unwrap();
        assert_eq!(headline.status, Status::Fail);

        let drift = checks
            .iter()
            .find(|c| c.name == "managed_host_config")
            .unwrap();
        assert_eq!(drift.status, Status::Fail);
        assert!(drift.value.contains("v16"));
        assert!(drift.value.contains("v17"));

        // Other version-match check stays Ok.
        let other = checks
            .iter()
            .find(|c| c.name == "instance_network_config")
            .unwrap();
        assert_eq!(other.status, Status::Ok);
    }

    // ── assemble_checks: stale last_seen ─────────────────────────────────

    #[test]
    fn stale_last_seen_warns_with_threshold_in_value() {
        let mut snap = snap_healthy();
        snap.last_seen_at = Utc::now() - chrono::Duration::seconds(180);
        let checks = assemble_checks(&snap, Utc::now(), Duration::from_secs(90));

        let last = checks.iter().find(|c| c.name == "last_seen").unwrap();
        assert_eq!(last.status, Status::Warn);
        assert!(last.value.contains("90s"), "value: {}", last.value);
    }

    // ── headline is the rolled-up verdict (PRD-003 slice 3) ──────────────
    //
    // The headline is now a single rolled-up verdict instead of a list
    // of failing-detail names. Drift outranks quarantine in the verdict
    // precedence ladder, so when both signals are present the headline
    // surfaces the drift axis. Every signal still appears as a detail
    // row — verified separately above and in the layer tests.
    #[test]
    fn unhealthy_headline_is_rolled_up_verdict_not_a_list_of_signal_names() {
        let mut snap = snap_healthy();
        snap.applied_instance_network_config_version = "v8".into();
        snap.desired_instance_network_config_version = "v9".into();
        snap.quarantine_state = Some("auto".into());
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks.iter().find(|c| c.kind == CheckKind::Headline).unwrap();
        assert_eq!(headline.status, Status::Fail);
        assert!(headline.value.contains("drift"));
        assert!(headline.value.contains("instance-network"));

        // Quarantine still surfaces as a detail row.
        let q = checks.iter().find(|c| c.name == "quarantine").unwrap();
        assert_eq!(q.status, Status::Fail);
        assert!(q.value.contains("auto"));
    }

    // ── network_config_error: top-line headline (PRD-002) ─────────────────

    #[test]
    fn network_config_error_pins_headline_to_fail_with_error_text() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("nvue apply failed: peer 192.0.2.1 unreachable".into());
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(headline.status, Status::Fail);
        assert!(
            headline.value.contains("network_config_error"),
            "headline: {}",
            headline.value
        );
        assert!(
            headline.value.contains("nvue apply failed"),
            "headline: {}",
            headline.value
        );
        assert!(headline.next_command.is_some());
    }

    #[test]
    fn network_config_error_emits_a_detail_line() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("apply timeout".into());
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let detail = checks
            .iter()
            .find(|c| c.name == "network_config_error")
            .expect("network_config_error detail");
        assert_eq!(detail.kind, CheckKind::Detail);
        assert_eq!(detail.status, Status::Fail);
        assert!(detail.value.contains("apply timeout"));
    }

    #[test]
    fn empty_network_config_error_string_is_treated_as_none() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some(String::new());
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(headline.status, Status::Ok);
        assert!(
            !checks.iter().any(|c| c.name == "network_config_error"),
            "no detail expected when error is empty string"
        );
    }

    #[test]
    fn network_config_error_takes_precedence_over_drift_in_headline() {
        let mut snap = snap_healthy();
        snap.applied_instance_network_config_version = "v8".into();
        snap.desired_instance_network_config_version = "v9".into();
        snap.network_config_error = Some("explicit agent error".into());
        let checks = assemble_checks(&snap, snap.last_seen_at, DEFAULT_FRESHNESS_THRESHOLD);

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert!(
            headline.value.contains("explicit agent error"),
            "headline must surface the error verbatim, got: {}",
            headline.value
        );
    }

    // ── BGP alerts: only category-Bgp entries surface here ────────────────

    #[test]
    fn parse_bgp_alerts_filters_by_category_case_insensitive() {
        let v = serde_json::json!({
            "alerts": [
                {"id": "BgpPeerDown", "category": "Bgp", "message": "peer 192.0.2.1"},
                {"id": "DhcpStale", "category": "Dhcp"},
                {"id": "BgpRoutesMissing", "category": "bgp"},
                {"id": "PostConfigCheckWait", "category": "Health"},
            ]
        });
        let out = parse_bgp_alerts(Some(&v));
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("BgpPeerDown") && out[0].contains("peer 192.0.2.1"));
        assert!(out[1].contains("BgpRoutesMissing"));
    }

    #[test]
    fn parse_bgp_alerts_returns_empty_when_blob_absent() {
        assert!(parse_bgp_alerts(None).is_empty());
    }

    #[test]
    fn parse_bgp_alerts_returns_empty_when_alerts_array_missing() {
        let v = serde_json::json!({"version": "1.2.3"});
        assert!(parse_bgp_alerts(Some(&v)).is_empty());
    }

    #[test]
    fn parse_bgp_alerts_skips_entries_without_id() {
        let v = serde_json::json!({
            "alerts": [
                {"category": "Bgp"},
                {"id": "BgpOk", "category": "Bgp"},
            ]
        });
        let out = parse_bgp_alerts(Some(&v));
        assert_eq!(out, vec!["BgpOk"]);
    }

    // ── SQL: producer-side machines columns only (PRD-002) ────────────────

    /// Pin the schema choice — the tracer SQL must read producer JSON
    /// from `machines` / `instances`, never the (non-existent) old
    /// `dpu_network_status` / `dpu_desired_network_config` tables.
    #[test]
    fn fetch_snapshot_sql_targets_producer_side_machines_columns() {
        let cols = FETCH_SNAPSHOT_COLS;
        let from = FETCH_SNAPSHOT_FROM;
        let combined = format!("{cols} {from}");

        assert!(
            !combined.contains("dpu_network_status"),
            "old table dpu_network_status still referenced: {combined}"
        );
        assert!(
            !combined.contains("dpu_desired_network_config"),
            "old table dpu_desired_network_config still referenced: {combined}"
        );
        assert!(
            from.contains("FROM machines"),
            "missing machines table: {from}"
        );
        assert!(
            from.contains("LEFT JOIN instances"),
            "missing instances join for desired instance version: {from}"
        );
        assert!(
            cols.contains("network_status_observation"),
            "missing applied-side observation JSON: {cols}"
        );
        assert!(
            cols.contains("network_config_error"),
            "missing network_config_error column: {cols}"
        );
        assert!(
            cols.contains("network_config->'quarantine_state'"),
            "quarantine must be read desired-side from network_config: {cols}"
        );
        assert!(
            cols.contains("inventory"),
            "hbn version must be read from inventory components: {cols}"
        );
    }
}
