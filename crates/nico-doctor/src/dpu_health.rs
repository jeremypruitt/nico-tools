//! Per-DPU agent health verdict — drill-down on the producer-side
//! `machines.dpu_agent_health_report` JSON for a single DPU (PRD-002 /
//! issue #262).
//!
//! Surfaces what the existing `hbn` and `dpu_cert` per-DPU drill-downs
//! deliberately leave behind:
//!
//! - All non-BGP, non-config-error alert categories from the agent's
//!   `HealthReport.alerts` array, **grouped by category**.
//! - Agent-version drift: `network_status_observation->>'agent_version'`
//!   compared against `network_status_observation->>'agent_version_superseded_at'`
//!   so the operator sees "agent X, superseded since Y" and can plan
//!   the upgrade.
//! - DHCP staleness per machine interface: `machine_interfaces.last_dhcp`
//!   older than threshold (default 4h, configurable via `--dhcp-stale`).
//!
//! Pure `assess` + `assemble_checks` over a small [`HealthSnapshot`]; the
//! [`DpuHealthClient`] trait is the seam over forgedb. Tests inject mocks.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};

/// Default DHCP staleness threshold. PRD-002 lists 4h / 24h / configurable
/// as options; 4h is the tighter signal — DHCP renewals on bf3-class
/// interfaces are typically < 1h, so > 4h almost always means "lease
/// renewal stopped landing in the controller".
pub const DEFAULT_DHCP_STALE_THRESHOLD: Duration = Duration::from_secs(4 * 60 * 60);

/// Per-layer carve-outs. `hbn` owns BGP-typed alerts and the
/// network-config-error headline (PRD-002 carve-out); `infiniband` owns
/// every `Ib*` probe id (PRD-004 slice 3 carve-out — `IbPortDown`,
/// `IbCleanupPending`, and any future `Ib*` probe). We drop both
/// families here to keep `dpu_health` focused on the agent-health,
/// drift, and DHCP signals other layers deliberately leave behind.
const HBN_OWNED_PROBE_IDS: &[&str] = &[
    "BgpPeerDown",
    "BgpRoutesMissing",
    "BgpStateInvalid",
    "NetworkConfigError",
];

/// True for any probe id owned by the `infiniband` layer. Prefix-based
/// rather than enumerated so a future `Ib*` probe id added upstream is
/// carved out automatically without a code change here.
fn is_infiniband_owned(id: &str) -> bool {
    id.starts_with("Ib")
}

/// One alert from `HealthReport.alerts`, narrowed to fields the layer
/// reads. `category` is derived from `id` (the agent's `HealthReport`
/// JSON does not carry an explicit category field — see
/// `health-report::HealthProbeId` in upstream — so we group by a
/// stable prefix-derived key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAlert {
    pub id: String,
    pub target: Option<String>,
    pub message: String,
    pub in_alert_since: Option<DateTime<Utc>>,
}

impl AgentAlert {
    /// Bucket name for grouping. Stable prefix-based mapping over the
    /// known probe IDs documented in upstream
    /// (`HealthProbeId::heartbeat_timeout`, `stale_agent_version`,
    /// `ib_port_down`, etc.); falls back to `Other` for unknowns so a
    /// new probe ID doesn't crash the renderer.
    pub fn category(&self) -> &'static str {
        match self.id.as_str() {
            "HeartbeatTimeout" | "StaleAgentVersion" | "MissingReport" | "MalformedReport" => {
                "agent"
            }
            "IbPortDown" => "ib_fabric",
            "Quarantine" | "Maintenance" => "operator",
            "SkuValidation" => "sku",
            id if id.starts_with("Bgp") => "bgp",
            id if id.starts_with("Cert") || id.contains("Certificate") => "cert",
            id if id.starts_with("Dhcp") || id.starts_with("Interface") || id.contains("Network")
            => "interface",
            _ => "other",
        }
    }
}

/// Per-interface DHCP record narrowed to the fields the staleness check
/// uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDhcp {
    pub mac_address: String,
    pub last_dhcp: Option<DateTime<Utc>>,
}

/// All inputs the `dpu_health` verdict needs for one DPU. Snapshot shape
/// is data-in / checks-out so `assemble_checks` is pure.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub dpu_id: String,
    /// `network_status_observation->>'agent_version'` (DPU agent's
    /// reported running version). `None` ⇒ agent has never reported.
    pub agent_version: Option<String>,
    /// `network_status_observation->>'agent_version_superseded_at'`. When
    /// `Some`, the agent is on a stale version — emit drift verdict that
    /// includes the superseded-since timestamp.
    pub agent_version_superseded_at: Option<DateTime<Utc>>,
    /// All alerts from the agent's `HealthReport.alerts` array. The
    /// layer filters out BGP-typed and `NetworkConfigError` IDs internally
    /// before grouping.
    pub alerts: Vec<AgentAlert>,
    /// Per-interface DHCP rows for this DPU's machine. Empty ⇒ DPU has
    /// no machine_interfaces rows (or they're not joinable on the dev
    /// schema); the staleness check stays silent.
    pub interfaces: Vec<InterfaceDhcp>,
}

/// Read-only seam over forgedb for the `dpu_health` layer. Returning
/// `Ok(None)` means "no `machines` row for this DPU" — same gentle-not-
/// found contract as `hbn` / `dpu_cert`.
#[async_trait]
pub trait DpuHealthClient: Send + Sync {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HealthSnapshot>>;
}

/// Default sqlx-backed [`DpuHealthClient`]. Reads:
///
/// - `machines.network_status_observation->>'agent_version'`
/// - `machines.network_status_observation->>'agent_version_superseded_at'`
/// - `machines.dpu_agent_health_report` (raw JSONB, parsed into [`AgentAlert`])
/// - `machine_interfaces.mac_address`, `machine_interfaces.last_dhcp` for
///   every interface row whose `machine_id` matches the DPU.
///
/// Schema-probes the `machines` table and degrades to `Ok(None)` when
/// absent so dev clusters that haven't run the carbide schema render
/// "no recent agent health report" instead of panicking.
pub struct SqlxDpuHealthClient {
    pool: sqlx::PgPool,
}

/// SQL columns the per-DPU snapshot reads. Extracted as a constant so
/// the schema choice is pinned by a unit test.
pub(crate) const FETCH_SNAPSHOT_COLS: &str = "\
    m.id, \
    m.network_status_observation->>'agent_version', \
    (m.network_status_observation->>'agent_version_superseded_at')::timestamptz, \
    m.dpu_agent_health_report";

pub(crate) const FETCH_SNAPSHOT_FROM: &str = "FROM machines m";

pub(crate) const FETCH_INTERFACES_SQL: &str = "\
    SELECT mi.mac_address::text, mi.last_dhcp \
    FROM machine_interfaces mi \
    WHERE mi.machine_id = $1";

impl SqlxDpuHealthClient {
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
    .map_err(|e| anyhow::anyhow!("dpu_health schema probe failed: {e}"))?;
    Ok(exists.0)
}

#[async_trait]
impl DpuHealthClient for SqlxDpuHealthClient {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HealthSnapshot>> {
        if !machines_table_exists(&self.pool).await? {
            return Ok(None);
        }

        let sql = format!(
            "SELECT {FETCH_SNAPSHOT_COLS} {FETCH_SNAPSHOT_FROM} \
             WHERE m.id = $1 LIMIT 1"
        );
        let row: Option<(
            String,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<serde_json::Value>,
        )> = sqlx::query_as(&sql)
            .bind(dpu_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("dpu_health snapshot query failed: {e}"))?;

        let Some((id, agent_version, superseded_at, health_report)) = row else {
            return Ok(None);
        };

        let interfaces: Vec<(String, Option<DateTime<Utc>>)> =
            sqlx::query_as(FETCH_INTERFACES_SQL)
                .bind(&id)
                .fetch_all(&self.pool)
                .await
                .map_err(|e| anyhow::anyhow!("dpu_health interfaces query failed: {e}"))?;

        Ok(Some(HealthSnapshot {
            dpu_id: id,
            agent_version,
            agent_version_superseded_at: superseded_at,
            alerts: parse_alerts(health_report.as_ref()),
            interfaces: interfaces
                .into_iter()
                .map(|(mac_address, last_dhcp)| InterfaceDhcp {
                    mac_address,
                    last_dhcp,
                })
                .collect(),
        }))
    }
}

/// Extract [`AgentAlert`]s from the raw `dpu_agent_health_report` JSON.
/// Tolerates the column being NULL or having no `alerts` array. Accepts
/// `in_alert_since` as RFC3339 string, Unix epoch seconds, or `null` —
/// matches the same tolerance the fleet `dpu` layer already grants
/// (`crate::dpu::parse_health_alerts`).
pub fn parse_alerts(blob: Option<&serde_json::Value>) -> Vec<AgentAlert> {
    let Some(v) = blob else { return Vec::new() };
    let Some(arr) = v.get("alerts").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
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
            Some(AgentAlert {
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

/// Assemble the per-DPU `dpu_health` check list from a snapshot.
///
/// Headline summarises the worst-of across the three sources of signal
/// (alerts, agent-version drift, DHCP staleness). Detail bullets are
/// emitted in this order:
///
/// 1. Agent-version drift (when `agent_version_superseded_at` is set).
/// 2. One detail per non-BGP/non-config-error alert, **grouped by
///    category** — alerts in the same category appear consecutively,
///    each prefixed with their category name.
/// 3. One detail per interface whose `last_dhcp` is older than
///    `dhcp_stale_threshold`.
///
/// Pure — no I/O, no clock reads (caller supplies `now`).
pub fn assemble_checks(
    snapshot: &HealthSnapshot,
    now: DateTime<Utc>,
    dhcp_stale_threshold: Duration,
) -> Vec<Check> {
    let mut details: Vec<Check> = Vec::new();

    if let Some(superseded_at) = snapshot.agent_version_superseded_at {
        let age = (now - superseded_at).to_std().unwrap_or(Duration::ZERO);
        let version_label = snapshot
            .agent_version
            .as_deref()
            .unwrap_or("<unknown>");
        details.push(Check {
            name: "agent_version_drift",
            status: Status::Warn,
            value: format!(
                "agent version {version_label}, superseded since {} ({}s ago)",
                superseded_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                age.as_secs()
            ),
            next_command: Some(format!(
                "plan dpu-agent upgrade for {}",
                snapshot.dpu_id
            )),
            kind: CheckKind::Detail,
        });
    }

    let mut surfaced: Vec<&AgentAlert> = snapshot
        .alerts
        .iter()
        .filter(|a| !HBN_OWNED_PROBE_IDS.contains(&a.id.as_str()))
        .filter(|a| !is_infiniband_owned(&a.id))
        .collect();
    surfaced.sort_by(|a, b| a.category().cmp(b.category()).then_with(|| a.id.cmp(&b.id)));
    for alert in surfaced {
        details.push(alert_check(alert));
    }

    for iface in &snapshot.interfaces {
        if let Some(check) = dhcp_stale_check(iface, now, dhcp_stale_threshold) {
            details.push(check);
        }
    }

    let aggregate = aggregate_status(&details);
    let headline = headline_check(snapshot, aggregate, &details);

    let mut out = Vec::with_capacity(details.len() + 1);
    out.push(headline);
    out.extend(details);
    out
}

/// "No machines row for this DPU" headline — single Unknown line.
pub fn assemble_no_status_checks(dpu_id: &str) -> Vec<Check> {
    vec![Check {
        name: "dpu_health",
        status: Status::Unknown,
        value: format!("no machines row for dpu {dpu_id}"),
        next_command: Some(format!(
            "nico correlate {dpu_id} # last activity for this DPU"
        )),
        kind: CheckKind::Headline,
    }]
}

/// Data-layer error headline (Postgres unreachable, query errored, etc.).
pub fn assemble_error_checks(dpu_id: &str, err: &str) -> Vec<Check> {
    vec![Check {
        name: "dpu_health",
        status: Status::Unknown,
        value: format!("dpu_health data layer error for {dpu_id}: {err}"),
        next_command: Some("check forgedb / postgres connectivity".to_string()),
        kind: CheckKind::Headline,
    }]
}

fn alert_check(alert: &AgentAlert) -> Check {
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
        name: "alert",
        status: Status::Fail,
        value: format!("[{}] {}{}{message}", alert.category(), target, alert.id),
        next_command: Some(format!(
            "nico correlate {} # trace this alert",
            alert.target.as_deref().unwrap_or(alert.id.as_str())
        )),
        kind: CheckKind::Detail,
    }
}

fn dhcp_stale_check(
    iface: &InterfaceDhcp,
    now: DateTime<Utc>,
    threshold: Duration,
) -> Option<Check> {
    let last = iface.last_dhcp?;
    let age = (now - last).to_std().unwrap_or(Duration::ZERO);
    if age <= threshold {
        return None;
    }
    Some(Check {
        name: "dhcp_stale",
        status: Status::Warn,
        value: format!(
            "[interface] {} last DHCP {}s ago (threshold {}s)",
            iface.mac_address,
            age.as_secs(),
            threshold.as_secs()
        ),
        next_command: Some(format!(
            "ssh dpu # check DHCP renewal for {}",
            iface.mac_address
        )),
        kind: CheckKind::Detail,
    })
}

fn aggregate_status(checks: &[Check]) -> Status {
    if checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        Status::Warn
    } else if checks.iter().any(|c| c.status == Status::Unknown) {
        Status::Unknown
    } else {
        Status::Ok
    }
}

fn headline_check(snapshot: &HealthSnapshot, aggregate: Status, details: &[Check]) -> Check {
    let value = match aggregate {
        Status::Ok => format!("dpu {} agent healthy", snapshot.dpu_id),
        Status::Skipped => format!("dpu {} agent skipped", snapshot.dpu_id),
        Status::Unknown => format!("dpu {} agent unknown", snapshot.dpu_id),
        Status::Warn | Status::Fail => {
            let bad: Vec<&str> = details
                .iter()
                .filter(|c| c.status != Status::Ok)
                .map(|c| c.name)
                .collect();
            format!(
                "dpu {} agent issues: {}",
                snapshot.dpu_id,
                bad.join(", ")
            )
        }
    };
    Check {
        name: "dpu_health",
        status: aggregate,
        value,
        next_command: None,
        kind: CheckKind::Headline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_healthy() -> HealthSnapshot {
        HealthSnapshot {
            dpu_id: "dpu-42".into(),
            agent_version: Some("2.0.0".into()),
            agent_version_superseded_at: None,
            alerts: vec![],
            interfaces: vec![],
        }
    }

    // ── parse_alerts ──────────────────────────────────────────────────────

    #[test]
    fn parse_alerts_returns_empty_when_blob_absent() {
        assert!(parse_alerts(None).is_empty());
    }

    #[test]
    fn parse_alerts_returns_empty_when_alerts_array_missing() {
        let v = serde_json::json!({"source": "forge-dpu-agent"});
        assert!(parse_alerts(Some(&v)).is_empty());
    }

    #[test]
    fn parse_alerts_extracts_id_target_message_in_alert_since() {
        let v = serde_json::json!({
            "alerts": [
                {
                    "id": "HeartbeatTimeout",
                    "target": "dpu-42",
                    "message": "no health report received",
                    "in_alert_since": "2024-01-15T12:34:56Z"
                }
            ]
        });
        let out = parse_alerts(Some(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "HeartbeatTimeout");
        assert_eq!(out[0].target.as_deref(), Some("dpu-42"));
        assert_eq!(out[0].message, "no health report received");
        assert_eq!(
            out[0].in_alert_since,
            Some("2024-01-15T12:34:56Z".parse().unwrap()),
        );
    }

    #[test]
    fn parse_alerts_skips_entries_without_id() {
        let v = serde_json::json!({
            "alerts": [
                {"target": "dpu-42"},
                {"id": "IbPortDown"}
            ]
        });
        let out = parse_alerts(Some(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "IbPortDown");
    }

    // ── category mapping ──────────────────────────────────────────────────

    #[test]
    fn category_groups_known_probe_ids_into_stable_buckets() {
        let cases = [
            ("HeartbeatTimeout", "agent"),
            ("StaleAgentVersion", "agent"),
            ("MissingReport", "agent"),
            ("IbPortDown", "ib_fabric"),
            ("Quarantine", "operator"),
            ("Maintenance", "operator"),
            ("SkuValidation", "sku"),
            ("BgpPeerDown", "bgp"),
            ("DhcpStale", "interface"),
            ("InterfaceDown", "interface"),
            ("CertExpiring", "cert"),
            ("ClientCertificateExpired", "cert"),
            ("Unknown99", "other"),
        ];
        for (id, expected) in cases {
            let alert = AgentAlert {
                id: id.into(),
                target: None,
                message: String::new(),
                in_alert_since: None,
            };
            assert_eq!(alert.category(), expected, "id={id}");
        }
    }

    // ── assemble_checks: empty ────────────────────────────────────────────

    #[test]
    fn empty_snapshot_yields_single_ok_headline_no_details() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].value.contains("dpu-42"));
        assert!(checks[0].value.contains("healthy"));
    }

    // ── alerts: BGP and config-error are HBN-owned, not surfaced here ────

    #[test]
    fn ib_typed_alerts_do_not_surface_in_dpu_health() {
        // PRD-004 slice 3 carve-out: Ib* probe IDs are owned by the
        // `infiniband` layer, mirror of the BGP→hbn carve-out PRD-002
        // already established.
        let mut snap = snap_healthy();
        snap.alerts = vec![
            AgentAlert {
                id: "IbPortDown".into(),
                target: Some("fe80::1".into()),
                message: "port 1 down".into(),
                in_alert_since: None,
            },
            AgentAlert {
                id: "IbCleanupPending".into(),
                target: None,
                message: "cleanup queue".into(),
                in_alert_since: None,
            },
            AgentAlert {
                id: "IbFutureProbeId".into(),
                target: None,
                message: "any Ib* prefix".into(),
                in_alert_since: None,
            },
        ];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        assert_eq!(checks.len(), 1, "only headline expected, Ib* filtered out");
        assert_eq!(checks[0].status, Status::Ok);
    }

    #[test]
    fn bgp_typed_alerts_do_not_surface_in_dpu_health() {
        let mut snap = snap_healthy();
        snap.alerts = vec![
            AgentAlert {
                id: "BgpPeerDown".into(),
                target: Some("peer1".into()),
                message: "peer down".into(),
                in_alert_since: None,
            },
            AgentAlert {
                id: "BgpRoutesMissing".into(),
                target: None,
                message: String::new(),
                in_alert_since: None,
            },
        ];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        assert_eq!(checks.len(), 1, "only headline expected, BGP filtered out");
        assert_eq!(checks[0].status, Status::Ok);
    }

    #[test]
    fn network_config_error_does_not_surface_in_dpu_health() {
        let mut snap = snap_healthy();
        snap.alerts = vec![AgentAlert {
            id: "NetworkConfigError".into(),
            target: None,
            message: "apply failed".into(),
            in_alert_since: None,
        }];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        assert_eq!(checks.len(), 1, "config-error owned by hbn");
        assert_eq!(checks[0].status, Status::Ok);
    }

    // ── alerts: non-BGP / non-Ib categories surface as Fail details ──────

    #[test]
    fn heartbeat_timeout_surfaces_as_fail_detail_with_agent_category() {
        let mut snap = snap_healthy();
        snap.alerts = vec![AgentAlert {
            id: "HeartbeatTimeout".into(),
            target: Some("dpu-42".into()),
            message: "no health report received".into(),
            in_alert_since: None,
        }];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        assert_eq!(checks.iter().filter(|c| c.kind == CheckKind::Detail).count(), 1);

        let detail = checks
            .iter()
            .find(|c| c.kind == CheckKind::Detail)
            .unwrap();
        assert_eq!(detail.status, Status::Fail);
        assert!(detail.value.contains("[agent]"), "value: {}", detail.value);
        assert!(detail.value.contains("HeartbeatTimeout"));
        assert!(detail.value.contains("no health report received"));

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(headline.status, Status::Fail);
        assert!(headline.value.contains("dpu-42"));
    }

    #[test]
    fn alerts_grouped_by_category_in_output_order() {
        let mut snap = snap_healthy();
        snap.alerts = vec![
            AgentAlert {
                id: "SkuValidation".into(),
                target: None,
                message: String::new(),
                in_alert_since: None,
            },
            AgentAlert {
                id: "HeartbeatTimeout".into(),
                target: Some("dpu-42".into()),
                message: String::new(),
                in_alert_since: None,
            },
            AgentAlert {
                id: "Quarantine".into(),
                target: None,
                message: String::new(),
                in_alert_since: None,
            },
            AgentAlert {
                id: "StaleAgentVersion".into(),
                target: Some("dpu-42".into()),
                message: String::new(),
                in_alert_since: None,
            },
        ];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        let details: Vec<&Check> = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Detail)
            .collect();
        assert_eq!(details.len(), 4);

        // Categories sorted alphabetically: agent, operator, sku
        let cats: Vec<&str> = details
            .iter()
            .map(|c| {
                if c.value.contains("[agent]") { "agent" }
                else if c.value.contains("[operator]") { "operator" }
                else if c.value.contains("[sku]") { "sku" }
                else { "?" }
            })
            .collect();
        assert_eq!(cats, vec!["agent", "agent", "operator", "sku"]);
    }

    // ── agent-version drift ──────────────────────────────────────────────

    #[test]
    fn agent_version_drift_emits_warn_detail_with_superseded_timestamp() {
        let now = Utc::now();
        let superseded = now - chrono::Duration::days(3);
        let mut snap = snap_healthy();
        snap.agent_version = Some("1.5.0".into());
        snap.agent_version_superseded_at = Some(superseded);
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD);

        let drift = checks
            .iter()
            .find(|c| c.name == "agent_version_drift")
            .expect("agent_version_drift detail");
        assert_eq!(drift.status, Status::Warn);
        assert!(drift.value.contains("1.5.0"));
        assert!(
            drift.value.contains("superseded since"),
            "value: {}",
            drift.value,
        );
        assert!(
            drift
                .next_command
                .as_deref()
                .unwrap()
                .contains("plan dpu-agent upgrade"),
        );
    }

    #[test]
    fn agent_version_drift_handles_unknown_version() {
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.agent_version = None;
        snap.agent_version_superseded_at = Some(now - chrono::Duration::days(1));
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD);
        let drift = checks
            .iter()
            .find(|c| c.name == "agent_version_drift")
            .unwrap();
        assert!(drift.value.contains("<unknown>"), "value: {}", drift.value);
    }

    #[test]
    fn no_drift_when_superseded_at_is_none() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        assert!(checks.iter().all(|c| c.name != "agent_version_drift"));
    }

    // ── DHCP staleness ────────────────────────────────────────────────────

    #[test]
    fn dhcp_stale_emits_warn_detail_when_last_dhcp_older_than_threshold() {
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.interfaces = vec![InterfaceDhcp {
            mac_address: "aa:bb:cc:dd:ee:ff".into(),
            last_dhcp: Some(now - chrono::Duration::hours(5)),
        }];
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD);

        let stale = checks
            .iter()
            .find(|c| c.name == "dhcp_stale")
            .expect("dhcp_stale detail");
        assert_eq!(stale.status, Status::Warn);
        assert!(stale.value.contains("aa:bb:cc:dd:ee:ff"));
        assert!(stale.value.contains("threshold"));
    }

    #[test]
    fn dhcp_stale_silent_when_under_threshold() {
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.interfaces = vec![InterfaceDhcp {
            mac_address: "aa:bb:cc:dd:ee:ff".into(),
            last_dhcp: Some(now - chrono::Duration::minutes(30)),
        }];
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD);
        assert!(checks.iter().all(|c| c.name != "dhcp_stale"));
    }

    #[test]
    fn dhcp_stale_silent_when_last_dhcp_unknown() {
        let mut snap = snap_healthy();
        snap.interfaces = vec![InterfaceDhcp {
            mac_address: "aa:bb:cc:dd:ee:ff".into(),
            last_dhcp: None,
        }];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD);
        assert!(checks.iter().all(|c| c.name != "dhcp_stale"));
    }

    #[test]
    fn custom_dhcp_threshold_changes_classification_boundary() {
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.interfaces = vec![InterfaceDhcp {
            mac_address: "aa:bb:cc:dd:ee:ff".into(),
            last_dhcp: Some(now - chrono::Duration::minutes(45)),
        }];
        // Tighter threshold of 30m ⇒ now stale
        let checks = assemble_checks(&snap, now, Duration::from_secs(30 * 60));
        assert!(
            checks.iter().any(|c| c.name == "dhcp_stale"),
            "expected dhcp_stale detail under 30m threshold",
        );
    }

    // ── headline aggregation ──────────────────────────────────────────────

    #[test]
    fn alert_makes_headline_fail_drift_alone_makes_warn() {
        let now = Utc::now();

        let mut s_drift = snap_healthy();
        s_drift.agent_version_superseded_at = Some(now - chrono::Duration::days(1));
        let drift_checks = assemble_checks(&s_drift, now, DEFAULT_DHCP_STALE_THRESHOLD);
        assert_eq!(
            drift_checks
                .iter()
                .find(|c| c.kind == CheckKind::Headline)
                .unwrap()
                .status,
            Status::Warn
        );

        let mut s_alert = snap_healthy();
        s_alert.alerts = vec![AgentAlert {
            id: "HeartbeatTimeout".into(),
            target: Some("dpu-42".into()),
            message: String::new(),
            in_alert_since: None,
        }];
        let alert_checks = assemble_checks(&s_alert, now, DEFAULT_DHCP_STALE_THRESHOLD);
        assert_eq!(
            alert_checks
                .iter()
                .find(|c| c.kind == CheckKind::Headline)
                .unwrap()
                .status,
            Status::Fail
        );
    }

    // ── no-status / error rendering ───────────────────────────────────────

    #[test]
    fn assemble_no_status_yields_unknown_headline_only() {
        let checks = assemble_no_status_checks("dpu-42");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("dpu-42"));
        assert!(checks[0].value.contains("no machines row"));
    }

    #[test]
    fn assemble_error_surfaces_underlying_error_text() {
        let checks = assemble_error_checks("dpu-42", "postgres unreachable");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("postgres unreachable"));
        assert!(checks[0].value.contains("dpu-42"));
    }

    // ── SQL: producer-side machines columns only (PRD-002) ────────────────

    #[test]
    fn fetch_snapshot_sql_targets_producer_side_machines_columns() {
        let cols = FETCH_SNAPSHOT_COLS;
        let from = FETCH_SNAPSHOT_FROM;
        let combined = format!("{cols} {from}");

        assert!(
            !combined.contains("dpu_network_status"),
            "old table dpu_network_status referenced: {combined}"
        );
        assert!(
            !combined.contains("FROM health_report"),
            "old health_report relational table referenced: {combined}"
        );
        assert!(
            !combined.contains("alert_name") && !combined.contains("in_alert_since"),
            "old health_report column shape (alert_name / in_alert_since) referenced: {combined}"
        );
        assert!(
            from.contains("FROM machines"),
            "must read from machines table: {from}"
        );
        assert!(
            cols.contains("dpu_agent_health_report"),
            "must read dpu_agent_health_report JSON: {cols}"
        );
        assert!(
            cols.contains("agent_version"),
            "must read agent_version: {cols}"
        );
        assert!(
            cols.contains("agent_version_superseded_at"),
            "must read agent_version_superseded_at: {cols}"
        );
    }

    #[test]
    fn fetch_interfaces_sql_targets_machine_interfaces_table() {
        let sql = FETCH_INTERFACES_SQL;
        assert!(
            sql.contains("machine_interfaces"),
            "interfaces query must read machine_interfaces: {sql}"
        );
        assert!(sql.contains("last_dhcp"), "must read last_dhcp: {sql}");
        assert!(sql.contains("mac_address"), "must read mac_address: {sql}");
    }
}
