//! Per-DPU InfiniBand fabric verdict — drill-down on
//! `machines.infiniband_status_observation` JSON for a single DPU
//! (PRD-004 / issue #312).
//!
//! `infiniband_status_observation` is a separate JSONB column from
//! `network_status_observation`, populated by core via
//! `update_infiniband_status_observation`. It carries an observation
//! timestamp, a `ufm_observable` flag (UFM = Unified Fabric Manager —
//! Mellanox's IB telemetry source), and a `ports` array. Each port row
//! exposes its full GUID (the stable IB-fabric identifier; `pf_guid`
//! deferred per PRD-004), `fabric_id`, `lid`, and `port_state`.
//!
//! Verdict precedence (per PRD-004):
//! - **Fail**: any port with `fabric_id` empty OR `lid == 0xffff`.
//! - **Warn**: UFM unobservable OR observation older than the
//!   freshness threshold OR an IB-typed `HealthReport` alert is
//!   present (`IbPortDown`, `IbCleanupPending`).
//! - **Ok**: otherwise.
//! - **Unknown**: no observation row.
//!
//! Pure `assemble_checks` over a small [`IbSnapshot`]; the
//! [`IbClient`] trait is the seam over forgedb. Tests inject mocks.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};
use crate::verdicts::{ib_verdict, AxisSummary};

/// Default freshness threshold for the IB observation. Inherits the
/// PRD-002 DHCP staleness baseline (4h) — IB fabric telemetry pushes at
/// a similar cadence to other agent reports, so a 4h gap is the
/// "agent-stopped-reporting" signal. Configurable via
/// `nico doctor infiniband --stale`.
pub const DEFAULT_OBSERVATION_STALE_THRESHOLD: Duration = Duration::from_secs(4 * 60 * 60);

/// Sentinel LID value indicating the port has not been assigned a
/// usable LID by the subnet manager. IB LIDs are 16-bit; `0xffff` is
/// the protocol-reserved "unassigned / error" indicator and `0` means
/// the port is not configured into the fabric.
pub const LID_UNASSIGNED: u32 = 0xffff;

/// One IB-typed alert from `dpu_agent_health_report`. PRD-004 slice 3
/// will carve these out of `dpu_health` and into `infiniband`; slice 2
/// (this slice) accepts them on the snapshot so the verdict precedence
/// already covers the "alert present ⇒ Warn" case downstream consumers
/// rely on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IbAlert {
    pub id: String,
    pub message: String,
    pub target: Option<String>,
    pub in_alert_since: Option<DateTime<Utc>>,
}

/// One IB port row. `guid` is the full GUID — slice 2 deliberately
/// does not surface `pf_guid` (PRD-004 deferred). `fabric_id` empty
/// and `lid == LID_UNASSIGNED` are the two Fail triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IbPort {
    pub guid: String,
    pub fabric_id: String,
    pub lid: u32,
    pub port_state: String,
}

/// All inputs the `infiniband` verdict needs for one DPU. Snapshot
/// shape is data-in / checks-out so the verdict + renderer stay pure.
#[derive(Debug, Clone)]
pub struct IbSnapshot {
    pub dpu_id: String,
    /// `infiniband_status_observation->>'observed_at'`. `None` ⇒ no
    /// observation row for this DPU; verdict yields Unknown.
    pub observed_at: Option<DateTime<Utc>>,
    /// `infiniband_status_observation->>'ufm_observable'`. `Some(false)`
    /// ⇒ UFM has lost visibility into the fabric ⇒ Warn. `None` ⇒ field
    /// absent; treat as observable.
    pub ufm_observable: Option<bool>,
    pub ports: Vec<IbPort>,
    /// IB-typed alerts from the agent's `HealthReport`. PRD-004 slice 3
    /// owns the carve-out from `dpu_health`; slice 2 accepts them here
    /// so the verdict precedence is in place when slice 3 lands.
    pub ib_alerts: Vec<IbAlert>,
}

/// Read-only seam over forgedb for the `infiniband` layer. Returning
/// `Ok(None)` means "no `machines` row for this DPU" — same gentle
/// not-found contract as `hbn` / `dpu_cert` / `dpu_health`.
#[async_trait]
pub trait IbClient: Send + Sync {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<IbSnapshot>>;
}

/// Default sqlx-backed [`IbClient`]. Reads
/// `machines.infiniband_status_observation` JSON. Schema-probes the
/// `machines` table and degrades to `Ok(None)` when absent so dev
/// clusters that haven't run the carbide schema render "no machines
/// row" instead of panicking.
pub struct SqlxIbClient {
    pool: sqlx::PgPool,
}

/// SQL columns the per-DPU snapshot reads. Extracted as constants so
/// the schema choice is pinned by a unit test. `dpu_agent_health_report`
/// supplies the IB-typed alerts the layer carved out of `dpu_health` in
/// PRD-004 slice 3.
pub(crate) const FETCH_SNAPSHOT_COLS: &str = "\
    m.id, \
    (m.infiniband_status_observation->>'observed_at')::timestamptz, \
    (m.infiniband_status_observation->>'ufm_observable')::boolean, \
    m.infiniband_status_observation->'ports', \
    m.dpu_agent_health_report";

pub(crate) const FETCH_SNAPSHOT_FROM: &str = "FROM machines m";

impl SqlxIbClient {
    pub fn new(url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .map_err(|e| anyhow::anyhow!("invalid postgres URL: {e}"))?;
        Ok(Self { pool })
    }
}

async fn machines_table_exists(pool: &sqlx::PgPool) -> Result<bool> {
    let exists: (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_name = 'machines')",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| anyhow::anyhow!("infiniband schema probe failed: {e}"))?;
    Ok(exists.0)
}

#[async_trait]
impl IbClient for SqlxIbClient {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<IbSnapshot>> {
        if !machines_table_exists(&self.pool).await? {
            return Ok(None);
        }

        let sql = format!(
            "SELECT {FETCH_SNAPSHOT_COLS} {FETCH_SNAPSHOT_FROM} \
             WHERE m.id = $1 LIMIT 1"
        );
        let row: Option<(
            String,
            Option<DateTime<Utc>>,
            Option<bool>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
        )> = sqlx::query_as(&sql)
            .bind(dpu_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("infiniband snapshot query failed: {e}"))?;

        let Some((id, observed_at, ufm_observable, ports_blob, health_report)) = row else {
            return Ok(None);
        };

        Ok(Some(IbSnapshot {
            dpu_id: id,
            observed_at,
            ufm_observable,
            ports: parse_ports(ports_blob.as_ref()),
            ib_alerts: parse_ib_alerts(health_report.as_ref()),
        }))
    }
}

/// Extract [`IbPort`] rows from the `ports` JSON array. Tolerates
/// the column being NULL or non-array. Missing string fields default
/// to `""`; missing `lid` defaults to [`LID_UNASSIGNED`] so a port
/// with no LID at all surfaces the same Fail signal as one explicitly
/// set to `0xffff`.
pub fn parse_ports(blob: Option<&serde_json::Value>) -> Vec<IbPort> {
    let Some(arr) = blob.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .map(|entry| {
            let guid = entry
                .get("guid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let fabric_id = entry
                .get("fabric_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let lid = entry
                .get("lid")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .unwrap_or(LID_UNASSIGNED);
            let port_state = entry
                .get("port_state")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_owned();
            IbPort {
                guid,
                fabric_id,
                lid,
                port_state,
            }
        })
        .collect()
}

/// Extract IB-typed [`IbAlert`]s from the raw `dpu_agent_health_report`
/// JSON. Tolerates the column being NULL or having no `alerts` array.
/// Keeps only entries whose `id` starts with `Ib` — symmetric with the
/// `dpu_health` carve-out (which drops the same prefix). Together the
/// two layers partition the health-report alert stream so each alert
/// surfaces in exactly one drill-down.
pub fn parse_ib_alerts(blob: Option<&serde_json::Value>) -> Vec<IbAlert> {
    let Some(v) = blob else { return Vec::new() };
    let Some(arr) = v.get("alerts").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            if !id.starts_with("Ib") {
                return None;
            }
            let target = entry
                .get("target")
                .and_then(|t| t.as_str())
                .map(str::to_owned);
            let message = entry
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_owned();
            let in_alert_since = entry.get("in_alert_since").and_then(parse_in_alert_since);
            Some(IbAlert {
                id,
                target,
                message,
                in_alert_since,
            })
        })
        .collect()
}

fn parse_in_alert_since(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    if v.is_null() {
        return None;
    }
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc));
    }
    if let Some(n) = v.as_i64() {
        return DateTime::<Utc>::from_timestamp(n, 0);
    }
    None
}

/// Render the IB axis as a headline `Check` (sourced from
/// [`ib_verdict`]) followed by IB-specific detail rows: per-port rows
/// (full GUID, fabric_id, lid, port_state) and a freshness detail when
/// the observation timestamp is present. The detail rows give the
/// operator raw data the punchy headline elides; the rollup layers
/// (PRD-004 slices 4 + 5) consume only the headline.
///
/// JSON ordering: headline first (`kind: "headline"`), then detail
/// rows.
pub fn assemble_checks(
    snapshot: &IbSnapshot,
    now: DateTime<Utc>,
    stale_threshold: Duration,
) -> Vec<Check> {
    let summary = ib_verdict(snapshot, now, stale_threshold);
    let mut checks = vec![headline_from(&summary)];

    for port in &snapshot.ports {
        checks.push(port_detail(port));
    }

    for alert in &snapshot.ib_alerts {
        checks.push(ib_alert_detail(alert));
    }

    if let Some(observed_at) = snapshot.observed_at {
        checks.push(Check {
            name: "observed_at",
            status: Status::Ok,
            value: observed_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            next_command: None,
            kind: CheckKind::Detail,
        });

        let age = (now - observed_at).to_std().unwrap_or(Duration::ZERO);
        if age > stale_threshold {
            checks.push(Check {
                name: "freshness",
                status: Status::Warn,
                value: format!(
                    "observation stale: {}s old (threshold {}s)",
                    age.as_secs(),
                    stale_threshold.as_secs()
                ),
                next_command: None,
                kind: CheckKind::Detail,
            });
        }
    }

    checks
}

fn ib_alert_detail(alert: &IbAlert) -> Check {
    let target = alert
        .target
        .as_deref()
        .map(|t| format!("[{t}] "))
        .unwrap_or_default();
    let message = if alert.message.is_empty() {
        String::new()
    } else {
        format!(": {}", alert.message)
    };
    Check {
        name: "ib_alert",
        status: Status::Warn,
        value: format!("{target}{}{message}", alert.id),
        next_command: Some(format!(
            "nico correlate {} # trace this alert",
            alert.target.as_deref().unwrap_or(alert.id.as_str())
        )),
        kind: CheckKind::Detail,
    }
}

fn port_detail(port: &IbPort) -> Check {
    let lid_str = if port.lid == LID_UNASSIGNED {
        "0xffff".to_string()
    } else {
        port.lid.to_string()
    };
    let fabric_label = if port.fabric_id.is_empty() {
        "<unassigned>".to_string()
    } else {
        port.fabric_id.clone()
    };
    let status = port_status(port);
    Check {
        name: "port",
        status,
        value: format!(
            "{guid} fabric={fabric} lid={lid} state={state}",
            guid = port.guid,
            fabric = fabric_label,
            lid = lid_str,
            state = port.port_state,
        ),
        next_command: None,
        kind: CheckKind::Detail,
    }
}

fn port_status(port: &IbPort) -> Status {
    if port.fabric_id.is_empty() || port.lid == LID_UNASSIGNED {
        Status::Fail
    } else {
        Status::Ok
    }
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

/// "No machines row for this DPU" headline — single Unknown line.
pub fn assemble_no_status_checks(dpu_id: &str) -> Vec<Check> {
    vec![Check {
        name: "infiniband",
        status: Status::Unknown,
        value: format!("no machines row for dpu {dpu_id}"),
        next_command: Some(format!(
            "nico correlate {dpu_id} # last activity for this DPU"
        )),
        kind: CheckKind::Headline,
    }]
}

/// Render a data-layer error as an `Unknown` headline so the verdict
/// surfaces the underlying message verbatim.
pub fn assemble_error_checks(dpu_id: &str, err: &str) -> Vec<Check> {
    vec![Check {
        name: "infiniband",
        status: Status::Unknown,
        value: format!("infiniband data layer error for {dpu_id}: {err}"),
        next_command: Some("check forgedb / postgres connectivity".to_string()),
        kind: CheckKind::Headline,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap_with_one_active_port() -> IbSnapshot {
        IbSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: Some(Utc::now()),
            ufm_observable: Some(true),
            ports: vec![IbPort {
                guid: "fe80:0000:0000:0000:0008:f104:0399:46c3".into(),
                fabric_id: "ib-fabric-1".into(),
                lid: 7,
                port_state: "Active".into(),
            }],
            ib_alerts: Vec::new(),
        }
    }

    // ── parse_ports ──────────────────────────────────────────────────────

    #[test]
    fn parse_ports_returns_empty_for_null_blob() {
        assert!(parse_ports(None).is_empty());
    }

    #[test]
    fn parse_ports_returns_empty_for_non_array_blob() {
        let blob = json!({"not": "an array"});
        assert!(parse_ports(Some(&blob)).is_empty());
    }

    #[test]
    fn parse_ports_extracts_full_guid_fabric_lid_state_per_row() {
        let blob = json!([
            {
                "guid": "fe80:0000:0000:0000:0008:f104:0399:46c3",
                "fabric_id": "ib-fabric-1",
                "lid": 7,
                "port_state": "Active"
            }
        ]);
        let ports = parse_ports(Some(&blob));
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].guid, "fe80:0000:0000:0000:0008:f104:0399:46c3");
        assert_eq!(ports[0].fabric_id, "ib-fabric-1");
        assert_eq!(ports[0].lid, 7);
        assert_eq!(ports[0].port_state, "Active");
    }

    #[test]
    fn parse_ports_defaults_missing_lid_to_unassigned_sentinel() {
        // PRD-004: a port with no `lid` field carries the same Fail
        // signal as one explicitly set to `0xffff` — the subnet manager
        // never assigned it.
        let blob = json!([{"guid": "fe80::1", "fabric_id": "f", "port_state": "Down"}]);
        let ports = parse_ports(Some(&blob));
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].lid, LID_UNASSIGNED);
    }

    #[test]
    fn parse_ports_defaults_missing_port_state_to_unknown() {
        let blob = json!([{"guid": "fe80::1", "fabric_id": "f", "lid": 1}]);
        let ports = parse_ports(Some(&blob));
        assert_eq!(ports[0].port_state, "Unknown");
    }

    // ── assemble_checks ──────────────────────────────────────────────────

    #[test]
    fn assemble_checks_emits_headline_first_then_per_port_then_observed_at() {
        let snap = snap_with_one_active_port();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);

        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Ok);

        assert_eq!(checks[1].kind, CheckKind::Detail);
        assert_eq!(checks[1].name, "port");
        assert!(checks[1].value.contains("fe80:0000:0000:0000:0008:f104:0399:46c3"));

        assert_eq!(checks[2].kind, CheckKind::Detail);
        assert_eq!(checks[2].name, "observed_at");
    }

    #[test]
    fn assemble_checks_renders_unassigned_lid_as_hex_in_port_detail() {
        let mut snap = snap_with_one_active_port();
        snap.ports[0].lid = LID_UNASSIGNED;
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        let port = checks.iter().find(|c| c.name == "port").unwrap();
        assert!(
            port.value.contains("lid=0xffff"),
            "expected hex lid in {:?}",
            port.value
        );
        // Per-port row also flips to Fail on the unassigned LID so the
        // operator can spot the offending port at a glance.
        assert_eq!(port.status, Status::Fail);
    }

    #[test]
    fn assemble_checks_renders_empty_fabric_id_as_unassigned_label() {
        let mut snap = snap_with_one_active_port();
        snap.ports[0].fabric_id = String::new();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        let port = checks.iter().find(|c| c.name == "port").unwrap();
        assert!(
            port.value.contains("<unassigned>"),
            "expected unassigned label in {:?}",
            port.value
        );
        assert_eq!(port.status, Status::Fail);
    }

    #[test]
    fn assemble_checks_surfaces_ib_alerts_as_warn_detail_rows() {
        // PRD-004 slice 3 carve-out: Ib* alerts moved out of dpu_health.
        // The infiniband layer surfaces each one as a detail row carrying
        // the probe id + target + message, with status Warn (matching
        // ib_verdict's Warn precedence path).
        let mut snap = snap_with_one_active_port();
        snap.ib_alerts = vec![
            IbAlert {
                id: "IbPortDown".into(),
                message: "port 1 down".into(),
                target: Some("fe80::1".into()),
                in_alert_since: None,
            },
            IbAlert {
                id: "IbCleanupPending".into(),
                message: "cleanup queue".into(),
                target: None,
                in_alert_since: None,
            },
        ];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);

        let alert_details: Vec<&Check> = checks
            .iter()
            .filter(|c| c.name == "ib_alert")
            .collect();
        assert_eq!(alert_details.len(), 2, "one detail per IB alert");

        for d in &alert_details {
            assert_eq!(d.kind, CheckKind::Detail);
            assert_eq!(d.status, Status::Warn);
        }
        assert!(
            alert_details
                .iter()
                .any(|d| d.value.contains("IbPortDown") && d.value.contains("port 1 down")),
            "alert detail values: {:?}",
            alert_details.iter().map(|d| &d.value).collect::<Vec<_>>(),
        );
        assert!(
            alert_details
                .iter()
                .any(|d| d.value.contains("IbCleanupPending")),
            "alert detail values: {:?}",
            alert_details.iter().map(|d| &d.value).collect::<Vec<_>>(),
        );

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(headline.status, Status::Warn);
    }

    #[test]
    fn assemble_checks_emits_freshness_detail_when_observation_stale() {
        let now = Utc::now();
        let mut snap = snap_with_one_active_port();
        snap.observed_at = Some(now - chrono::Duration::hours(5));
        let checks = assemble_checks(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        let freshness = checks
            .iter()
            .find(|c| c.name == "freshness")
            .expect("freshness detail row");
        assert_eq!(freshness.kind, CheckKind::Detail);
        assert_eq!(freshness.status, Status::Warn);
        assert!(freshness.value.contains("stale"));
    }

    #[test]
    fn assemble_checks_omits_observed_at_detail_when_no_observation() {
        let snap = IbSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: None,
            ufm_observable: None,
            ports: vec![],
            ib_alerts: vec![],
        };
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        // Only the headline (Unknown verdict) — no observed_at, no ports.
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Unknown);
    }

    // ── error rendering ──────────────────────────────────────────────────

    #[test]
    fn assemble_no_status_checks_surfaces_dpu_id_with_correlate_hint() {
        let checks = assemble_no_status_checks("dpu-42");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("dpu-42"));
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
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("postgres unreachable"));
        assert!(checks[0].value.contains("dpu-42"));
    }

    // ── schema-pin: SQL column choice ────────────────────────────────────

    #[test]
    fn fetch_snapshot_cols_pins_infiniband_status_observation_path() {
        // Pinning the SQL column choice — slice 4/5 holistic rollups
        // join on the same columns; if this changes, verify upstream
        // schema migration first.
        assert!(FETCH_SNAPSHOT_COLS.contains("infiniband_status_observation"));
        assert!(FETCH_SNAPSHOT_COLS.contains("observed_at"));
        assert!(FETCH_SNAPSHOT_COLS.contains("ufm_observable"));
        assert!(FETCH_SNAPSHOT_COLS.contains("'ports'"));
        assert_eq!(FETCH_SNAPSHOT_FROM, "FROM machines m");
    }

    #[test]
    fn fetch_snapshot_cols_pins_dpu_agent_health_report_for_ib_alerts() {
        // PRD-004 slice 3 carve-out: infiniband layer reads
        // dpu_agent_health_report so it can surface Ib* alerts that
        // dpu_health no longer renders.
        assert!(
            FETCH_SNAPSHOT_COLS.contains("dpu_agent_health_report"),
            "must read dpu_agent_health_report JSON: {FETCH_SNAPSHOT_COLS}",
        );
    }

    // ── parse_ib_alerts ──────────────────────────────────────────────────

    #[test]
    fn parse_ib_alerts_returns_empty_when_blob_absent() {
        assert!(parse_ib_alerts(None).is_empty());
    }

    #[test]
    fn parse_ib_alerts_returns_empty_when_alerts_array_missing() {
        let blob = json!({"source": "forge-dpu-agent"});
        assert!(parse_ib_alerts(Some(&blob)).is_empty());
    }

    #[test]
    fn parse_ib_alerts_keeps_only_ib_prefixed_probe_ids() {
        // Symmetric carve-out: parse_alerts in dpu_health drops Ib*;
        // here we keep ONLY Ib*. Together the two layers partition the
        // health-report alert stream.
        let blob = json!({
            "alerts": [
                {"id": "IbPortDown", "target": "fe80::1", "message": "down"},
                {"id": "IbCleanupPending", "target": null, "message": "queue"},
                {"id": "HeartbeatTimeout", "target": "dpu-42", "message": "no report"},
                {"id": "BgpPeerDown", "target": "peer1", "message": "peer"},
                {"id": "NetworkConfigError", "target": null, "message": "apply"}
            ]
        });
        let out = parse_ib_alerts(Some(&blob));
        assert_eq!(out.len(), 2);
        let ids: Vec<&str> = out.iter().map(|a| a.id.as_str()).collect();
        assert!(ids.contains(&"IbPortDown"));
        assert!(ids.contains(&"IbCleanupPending"));
        assert!(!ids.contains(&"HeartbeatTimeout"));
        assert!(!ids.contains(&"BgpPeerDown"));
        assert!(!ids.contains(&"NetworkConfigError"));
    }

    #[test]
    fn parse_ib_alerts_extracts_id_target_message() {
        let blob = json!({
            "alerts": [
                {
                    "id": "IbPortDown",
                    "target": "fe80::1",
                    "message": "port 1 down",
                    "in_alert_since": "2024-01-15T12:34:56Z"
                }
            ]
        });
        let out = parse_ib_alerts(Some(&blob));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "IbPortDown");
        assert_eq!(out[0].target.as_deref(), Some("fe80::1"));
        assert_eq!(out[0].message, "port 1 down");
        assert_eq!(
            out[0].in_alert_since,
            Some("2024-01-15T12:34:56Z".parse().unwrap()),
        );
    }
}
