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

use crate::dpu_cert::{CertSnapshot, DEFAULT_WARN_THRESHOLD as CERT_WARN_THRESHOLD};
use crate::dpu_isolation::{
    IsolationSnapshot, DEFAULT_FRESHNESS_THRESHOLD as ISOLATION_FRESHNESS_THRESHOLD,
};
use crate::dpu_services::{ServicesSnapshot, DEFAULT_OBSERVATION_STALE_THRESHOLD};
use crate::hbn::{HbnSnapshot, DEFAULT_FRESHNESS_THRESHOLD as HBN_FRESHNESS_THRESHOLD};
use crate::infiniband::{
    IbSnapshot, DEFAULT_OBSERVATION_STALE_THRESHOLD as IB_STALE_THRESHOLD,
};
use crate::layer::{Check, CheckKind};
use crate::verdicts::{
    cert_verdict, hbn_verdict, ib_verdict, isolation_verdict, services_verdict, AxisSummary,
};

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
///
/// **PRD-003 slice 5 (holistic per-DPU summary)**: this snapshot now
/// carries the union of fields the four per-DPU axis verdicts
/// (`cert_verdict`, `isolation_verdict`, `hbn_verdict`,
/// `services_verdict`) consume, in addition to the agent-health-specific
/// content (alerts, agent-version drift, DHCP staleness) it owned
/// pre-refactor. `assemble_checks` emits one headline per axis first,
/// then the agent-health detail rows beneath.
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
    // ── PRD-003 slice 5: per-axis verdict inputs ─────────────────────
    /// `network_status_observation->>'client_certificate_expiry'` —
    /// consumed by `cert_verdict`.
    pub client_certificate_expiry: Option<DateTime<Utc>>,
    /// `network_config->'quarantine_state'->>'mode'` (desired-side
    /// quarantine intent). Consumed by `isolation_verdict` and
    /// `hbn_verdict`.
    pub quarantine_state: Option<String>,
    /// `network_status_observation->>'observed_at'`. `None` ⇒ no agent
    /// observation row landed yet. Consumed by `isolation_verdict` and
    /// `hbn_verdict` for the freshness signal.
    pub last_seen_at: Option<DateTime<Utc>>,
    /// True when the `machines` row exists (which it does whenever
    /// `fetch_snapshot` returns `Ok(Some(_))`); kept on the snapshot so
    /// the holistic verdicts read the same shape the per-DPU isolation
    /// drill-down would.
    pub registered: bool,
    /// True when `network_status_observation IS NOT NULL` — the agent
    /// has reported at least once. Consumed by `isolation_verdict`.
    pub scout_discovery_complete: bool,
    /// HBN component version from `machines.inventory` (`components[]`
    /// where `name == 'hbn'`). Empty when inventory is absent. Consumed
    /// by `hbn_verdict` for the version-below-minimum signal.
    pub hbn_version: String,
    /// `NULLIF(network_status_observation->>'network_config_error', '')`.
    /// Producer-side agent error. Consumed by `hbn_verdict` as the
    /// highest-priority Fail trigger.
    pub network_config_error: Option<String>,
    /// `network_status_observation->>'network_config_version'`. Consumed
    /// by `hbn_verdict`.
    pub applied_managed_host_config_version: String,
    /// `machines.network_config_version`. Consumed by `hbn_verdict`.
    pub desired_managed_host_config_version: String,
    /// `network_status_observation->'instance_network_observation'->>'config_version'`.
    pub applied_instance_network_config_version: String,
    /// `instances.network_config_version` joined on `instances.machine_id`.
    pub desired_instance_network_config_version: String,
    /// BGP-typed alerts (filtered subset of `alerts`). Consumed by
    /// `hbn_verdict`. Parsed via `crate::hbn::parse_bgp_alerts`.
    pub bgp_alerts: Vec<String>,
    /// `extension_service_observation->>'observed_at'`. Consumed by
    /// `services_verdict` for staleness.
    pub extension_services_observed_at: Option<DateTime<Utc>>,
    /// `extension_service_observation->'extension_service_statuses'`
    /// parsed into per-service rows. Consumed by `services_verdict`.
    pub extension_services: Vec<crate::dpu_services::ServiceStatus>,
    // ── PRD-004 slice 4: infiniband verdict inputs ────────────────────
    /// `infiniband_status_observation->>'observed_at'`. Consumed by
    /// `ib_verdict` (treated as "no observation" when `None`).
    pub infiniband_observed_at: Option<DateTime<Utc>>,
    /// `infiniband_status_observation->>'ufm_observable'`. `Some(false)`
    /// ⇒ UFM lost visibility ⇒ Warn via `ib_verdict`.
    pub infiniband_ufm_observable: Option<bool>,
    /// Parsed `infiniband_status_observation->'ports'` array. Consumed
    /// by `ib_verdict` (empty `fabric_id` or `lid == 0xffff` ⇒ Fail).
    pub infiniband_ports: Vec<crate::infiniband::IbPort>,
    /// IB-typed alerts from `dpu_agent_health_report` (id prefix `Ib`).
    /// Consumed by `ib_verdict` as a Warn trigger. Parsed via
    /// `crate::infiniband::parse_ib_alerts`.
    pub ib_alerts: Vec<crate::infiniband::IbAlert>,
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
/// the schema choice is pinned by a unit test. Holistic per-DPU
/// summary (PRD-003 slice 5) adds the verdict inputs the four shared
/// helpers consume so `assemble_checks` can emit one headline per axis.
pub(crate) const FETCH_SNAPSHOT_COLS: &str = "\
    m.id, \
    m.network_status_observation->>'agent_version', \
    (m.network_status_observation->>'agent_version_superseded_at')::timestamptz, \
    m.dpu_agent_health_report, \
    (m.network_status_observation->>'client_certificate_expiry')::bigint, \
    m.network_config->'quarantine_state'->>'mode', \
    (m.network_status_observation->>'observed_at')::timestamptz, \
    NULLIF(m.network_status_observation->>'network_config_error', ''), \
    (SELECT c->>'version' FROM jsonb_array_elements(COALESCE(m.inventory->'components', '[]'::jsonb)) c \
       WHERE c->>'name' = 'hbn' LIMIT 1), \
    COALESCE(m.network_status_observation->>'network_config_version', ''), \
    m.network_config_version, \
    COALESCE(m.network_status_observation->'instance_network_observation'->>'config_version', ''), \
    COALESCE(i.network_config_version, ''), \
    (m.network_status_observation->'extension_service_observation'->>'observed_at')::timestamptz, \
    m.network_status_observation->'extension_service_observation'->'extension_service_statuses', \
    m.infiniband_status_observation";

pub(crate) const FETCH_SNAPSHOT_FROM: &str =
    "FROM machines m LEFT JOIN instances i ON i.machine_id = m.id";

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
        type SnapshotRow = (
            String,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<serde_json::Value>,
            Option<i64>,
            Option<String>,
            Option<DateTime<Utc>>,
            Option<String>,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<DateTime<Utc>>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
        );
        let row: Option<SnapshotRow> = sqlx::query_as(&sql)
            .bind(dpu_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("dpu_health snapshot query failed: {e}"))?;

        let Some((
            id,
            agent_version,
            superseded_at,
            health_report,
            cert_expiry,
            quarantine_state,
            last_seen_at,
            network_config_error,
            hbn_version,
            applied_managed_host,
            desired_managed_host,
            applied_instance,
            desired_instance,
            services_observed_at,
            services_blob,
            ib_observation_blob,
        )) = row
        else {
            return Ok(None);
        };

        // PRD-004 slice 4: parse `infiniband_status_observation` JSONB
        // sub-fields client-side so the row tuple stays under sqlx's
        // 16-element FromRow limit.
        let (ib_observed_at, ib_ufm_observable, ib_ports_blob) =
            parse_ib_observation(ib_observation_blob.as_ref());

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
            bgp_alerts: crate::hbn::parse_bgp_alerts(health_report.as_ref()),
            alerts: parse_alerts(health_report.as_ref()),
            interfaces: interfaces
                .into_iter()
                .map(|(mac_address, last_dhcp)| InterfaceDhcp {
                    mac_address,
                    last_dhcp,
                })
                .collect(),
            client_certificate_expiry: cert_expiry
                .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
            quarantine_state,
            last_seen_at,
            registered: true,
            scout_discovery_complete: last_seen_at.is_some(),
            hbn_version: hbn_version.unwrap_or_default(),
            network_config_error,
            applied_managed_host_config_version: applied_managed_host,
            desired_managed_host_config_version: desired_managed_host,
            applied_instance_network_config_version: applied_instance,
            desired_instance_network_config_version: desired_instance,
            extension_services_observed_at: services_observed_at,
            extension_services: crate::dpu::parse_extension_services(services_blob.as_ref()),
            infiniband_observed_at: ib_observed_at,
            infiniband_ufm_observable: ib_ufm_observable,
            infiniband_ports: crate::infiniband::parse_ports(ib_ports_blob.as_ref()),
            ib_alerts: crate::infiniband::parse_ib_alerts(health_report.as_ref()),
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

/// Split the raw `infiniband_status_observation` JSONB into the three
/// sub-fields the verdict consumes: `observed_at`, `ufm_observable`,
/// and the `ports` array (returned as a borrowable JSON value so the
/// caller can pass it to `crate::infiniband::parse_ports`).
///
/// Tolerates the column being NULL or missing any sub-field; verdict
/// adapters handle the resulting `(None, None, [])` as "no IB
/// observation" — yielding `Unknown` via `ib_verdict`.
pub fn parse_ib_observation(
    blob: Option<&serde_json::Value>,
) -> (Option<DateTime<Utc>>, Option<bool>, Option<serde_json::Value>) {
    let Some(v) = blob else { return (None, None, None) };
    let observed_at = v
        .get("observed_at")
        .and_then(|x| x.as_str())
        .and_then(|s| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|d| d.with_timezone(&Utc))
        });
    let ufm_observable = v.get("ufm_observable").and_then(|x| x.as_bool());
    let ports = v.get("ports").cloned();
    (observed_at, ufm_observable, ports)
}

/// Assemble the per-DPU `dpu_health` check list from a snapshot — the
/// **holistic per-DPU summary** introduced in PRD-003 slice 5 (#309) and
/// extended with the `infiniband` axis in PRD-004 slice 4 (#314).
///
/// `infiniband_present` mirrors the fleet `dpu` layer's capability gate
/// (PRD-004 slice 1):
///
/// - `Some(true)` ⇒ emit the IB headline using `ib_verdict`.
/// - `Some(false)` ⇒ omit the IB headline entirely (n/a-by-design — no
///   IB fabric on this deployment).
/// - `None` ⇒ emit an `Unknown` IB headline (probe was skipped: force
///   mode, postgres unreachable, deployment-type unresolved, or
///   `rest-only-mock`). The message reads "infiniband: presence not
///   detected" so the operator knows why.
///
/// Output ordering (matches the JSON-ordering acceptance):
///
/// 1. One headline `Check` per per-DPU axis (`dpu_cert`, `dpu_isolation`,
///    `hbn`, `dpu_services`, and — when not gated off — `infiniband`),
///    sourced from the shared verdict helpers. Each headline's `value`
///    is the verdict's one-line message; each carries `next_command`
///    pointing at `nico doctor <axis> <id>` so the operator can drill
///    in.
/// 2. Agent-health-specific detail rows (unchanged in shape from
///    PRD-002 slice 6, #262):
///    - Agent-version drift (when `agent_version_superseded_at` is set).
///    - One detail per non-BGP/non-IB alert, grouped by category.
///    - One detail per interface whose `last_dhcp` is older than
///      `dhcp_stale_threshold`.
///
/// Pure — no I/O, no clock reads (caller supplies `now`).
pub fn assemble_checks(
    snapshot: &HealthSnapshot,
    now: DateTime<Utc>,
    dhcp_stale_threshold: Duration,
    infiniband_present: Option<bool>,
) -> Vec<Check> {
    let cert = cert_summary(snapshot, now);
    let isolation = isolation_summary(snapshot, now);
    let hbn = hbn_summary(snapshot, now);
    let services = services_summary(snapshot, now);

    let mut out: Vec<Check> = vec![
        axis_headline_check(&cert, &snapshot.dpu_id),
        axis_headline_check(&isolation, &snapshot.dpu_id),
        axis_headline_check(&hbn, &snapshot.dpu_id),
        axis_headline_check(&services, &snapshot.dpu_id),
    ];

    if let Some(ib_check) = ib_axis_headline(snapshot, now, infiniband_present) {
        out.push(ib_check);
    }

    out.extend(agent_health_details(snapshot, now, dhcp_stale_threshold));
    out
}

/// Build the `infiniband` axis headline per the PRD-004 slice 4 gate:
/// `Some(true)` ⇒ `ib_verdict`; `Some(false)` ⇒ omit (returns `None`);
/// `None` ⇒ an explicit `Unknown` "presence not detected" headline that
/// keeps the row visible in the holistic grid even when the boot probe
/// hasn't resolved IB presence.
fn ib_axis_headline(
    snapshot: &HealthSnapshot,
    now: DateTime<Utc>,
    infiniband_present: Option<bool>,
) -> Option<Check> {
    match infiniband_present {
        Some(false) => None,
        Some(true) => {
            let summary = ib_summary(snapshot, now);
            Some(axis_headline_check(&summary, &snapshot.dpu_id))
        }
        None => Some(Check {
            name: "infiniband",
            status: Status::Unknown,
            value: format!(
                "dpu {} infiniband: presence not detected (force mode \
                 or boot probe skipped)",
                snapshot.dpu_id
            ),
            next_command: Some(format!("nico doctor infiniband {}", snapshot.dpu_id)),
            kind: CheckKind::Headline,
        }),
    }
}

/// Drill-down command suffix per axis — appended to `nico doctor` so the
/// operator can drill in from a headline. CLI uses hyphenated forms
/// (`dpu-cert`, `dpu-isolation`, `dpu-services`) whereas the axis tag
/// (matching `Layer::name`) uses underscores; we keep both vocabularies
/// in lock-step here so the headlines render the correct command.
fn drilldown_cli_name(axis: &'static str) -> &'static str {
    match axis {
        "dpu_cert" => "dpu-cert",
        "dpu_isolation" => "dpu-isolation",
        "dpu_services" => "dpu-services",
        // `hbn` and `infiniband` already match their CLI names.
        _ => axis,
    }
}

fn axis_headline_check(summary: &AxisSummary, dpu_id: &str) -> Check {
    Check {
        name: summary.axis,
        status: summary.status.clone(),
        value: summary.message.clone(),
        next_command: Some(format!(
            "nico doctor {} {dpu_id}",
            drilldown_cli_name(summary.axis)
        )),
        kind: CheckKind::Headline,
    }
}

fn cert_summary(s: &HealthSnapshot, now: DateTime<Utc>) -> AxisSummary {
    cert_verdict(
        &CertSnapshot {
            dpu_id: s.dpu_id.clone(),
            client_certificate_expiry: s.client_certificate_expiry,
        },
        now,
        CERT_WARN_THRESHOLD,
    )
}

fn isolation_summary(s: &HealthSnapshot, now: DateTime<Utc>) -> AxisSummary {
    isolation_verdict(
        &IsolationSnapshot {
            machine_id: s.dpu_id.clone(),
            registered: s.registered,
            scout_discovery_complete: s.scout_discovery_complete,
            quarantine_state: s.quarantine_state.clone(),
            last_seen_at: s.last_seen_at,
        },
        now,
        ISOLATION_FRESHNESS_THRESHOLD,
    )
}

fn hbn_summary(s: &HealthSnapshot, now: DateTime<Utc>) -> AxisSummary {
    hbn_verdict(
        &HbnSnapshot {
            dpu_id: s.dpu_id.clone(),
            hbn_version: s.hbn_version.clone(),
            applied_managed_host_config_version: s.applied_managed_host_config_version.clone(),
            desired_managed_host_config_version: s.desired_managed_host_config_version.clone(),
            applied_instance_network_config_version: s
                .applied_instance_network_config_version
                .clone(),
            desired_instance_network_config_version: s
                .desired_instance_network_config_version
                .clone(),
            network_config_error: s.network_config_error.clone(),
            bgp_alerts: s.bgp_alerts.clone(),
            quarantine_state: s.quarantine_state.clone(),
            last_seen_at: s.last_seen_at.unwrap_or(now),
        },
        now,
        HBN_FRESHNESS_THRESHOLD,
    )
}

fn ib_summary(s: &HealthSnapshot, now: DateTime<Utc>) -> AxisSummary {
    ib_verdict(
        &IbSnapshot {
            dpu_id: s.dpu_id.clone(),
            observed_at: s.infiniband_observed_at,
            ufm_observable: s.infiniband_ufm_observable,
            ports: s.infiniband_ports.clone(),
            ib_alerts: s.ib_alerts.clone(),
        },
        now,
        IB_STALE_THRESHOLD,
    )
}

fn services_summary(s: &HealthSnapshot, now: DateTime<Utc>) -> AxisSummary {
    services_verdict(
        &ServicesSnapshot {
            dpu_id: s.dpu_id.clone(),
            observed_at: s.extension_services_observed_at,
            services: s.extension_services.clone(),
        },
        now,
        DEFAULT_OBSERVATION_STALE_THRESHOLD,
    )
}

fn agent_health_details(
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

    details
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


#[cfg(test)]
mod tests {
    use super::*;

    fn snap_healthy() -> HealthSnapshot {
        // Default: healthy across every per-axis verdict so a test that
        // exercises (say) the alerts detail row doesn't accidentally
        // contaminate the cert / isolation / hbn / services headlines.
        // Verdict adapters fall back to Unknown for missing inputs, so
        // each axis input is set to a benign value here.
        let now = Utc::now();
        HealthSnapshot {
            dpu_id: "dpu-42".into(),
            agent_version: Some("2.0.0".into()),
            agent_version_superseded_at: None,
            alerts: vec![],
            interfaces: vec![],
            client_certificate_expiry: Some(now + chrono::Duration::days(365)),
            quarantine_state: None,
            last_seen_at: Some(now),
            registered: true,
            scout_discovery_complete: true,
            hbn_version: "2.0.0-doca2.5.0".into(),
            network_config_error: None,
            applied_managed_host_config_version: "v1".into(),
            desired_managed_host_config_version: "v1".into(),
            applied_instance_network_config_version: "v1".into(),
            desired_instance_network_config_version: "v1".into(),
            bgp_alerts: vec![],
            extension_services_observed_at: Some(now),
            extension_services: vec![],
            infiniband_observed_at: Some(now),
            infiniband_ufm_observable: Some(true),
            infiniband_ports: vec![crate::infiniband::IbPort {
                guid: "fe80::1".into(),
                fabric_id: "ib-fabric-1".into(),
                lid: 7,
                port_state: "Active".into(),
            }],
            ib_alerts: vec![],
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

    // ── assemble_checks: empty (holistic shape, PRD-003 slice 5) ─────────

    #[test]
    fn healthy_snapshot_yields_five_axis_headlines_all_ok() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let headlines: Vec<&Check> = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Headline)
            .collect();
        assert_eq!(headlines.len(), 5, "one headline per axis incl. infiniband");
        for axis in [
            "dpu_cert",
            "dpu_isolation",
            "hbn",
            "dpu_services",
            "infiniband",
        ] {
            let h = headlines
                .iter()
                .find(|c| c.name == axis)
                .unwrap_or_else(|| panic!("missing {axis} headline"));
            assert_eq!(h.status, Status::Ok, "{axis}");
        }
        assert!(
            checks.iter().all(|c| c.kind == CheckKind::Headline),
            "no detail rows on a fully-healthy snapshot",
        );
    }

    // ── alerts: BGP and config-error are HBN-owned, not surfaced as agent details ────

    #[test]
    fn ib_typed_alerts_do_not_surface_in_agent_health_details() {
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
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        assert!(
            checks.iter().all(|c| c.kind == CheckKind::Headline),
            "no agent-health detail rows expected (Ib* filtered out)",
        );
    }

    #[test]
    fn bgp_typed_alerts_do_not_surface_in_agent_health_details() {
        let mut snap = snap_healthy();
        // Mirror the alerts list AND the bgp_alerts feed-in so the hbn
        // verdict can also detect them; here we only assert the
        // agent-health details path drops the BGP-typed entries.
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
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        assert!(
            checks
                .iter()
                .filter(|c| c.kind == CheckKind::Detail)
                .all(|c| c.name != "alert"),
            "no BGP alert detail rows expected on agent-health side",
        );
    }

    #[test]
    fn network_config_error_does_not_surface_in_agent_health_details() {
        let mut snap = snap_healthy();
        snap.alerts = vec![AgentAlert {
            id: "NetworkConfigError".into(),
            target: None,
            message: "apply failed".into(),
            in_alert_since: None,
        }];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        assert!(
            checks
                .iter()
                .filter(|c| c.kind == CheckKind::Detail)
                .all(|c| c.name != "alert"),
            "NetworkConfigError owned by hbn, not surfaced as agent-health alert",
        );
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
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let details: Vec<&Check> = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Detail)
            .collect();
        assert_eq!(details.len(), 1);

        let detail = details[0];
        assert_eq!(detail.status, Status::Fail);
        assert!(detail.value.contains("[agent]"), "value: {}", detail.value);
        assert!(detail.value.contains("HeartbeatTimeout"));
        assert!(detail.value.contains("no health report received"));

        // Per-axis headlines stay Ok (the heartbeat alert is not an
        // axis signal); layer-aggregate Fail comes from the Fail detail
        // row at `Layer::run` time, exercised in layer-level tests.
        for axis in ["dpu_cert", "dpu_isolation", "hbn", "dpu_services"] {
            let h = checks
                .iter()
                .find(|c| c.name == axis && c.kind == CheckKind::Headline)
                .unwrap();
            assert_eq!(h.status, Status::Ok, "{axis} should stay Ok");
        }
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
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
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
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));

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
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let drift = checks
            .iter()
            .find(|c| c.name == "agent_version_drift")
            .unwrap();
        assert!(drift.value.contains("<unknown>"), "value: {}", drift.value);
    }

    #[test]
    fn no_drift_when_superseded_at_is_none() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
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
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));

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
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        assert!(checks.iter().all(|c| c.name != "dhcp_stale"));
    }

    #[test]
    fn dhcp_stale_silent_when_last_dhcp_unknown() {
        let mut snap = snap_healthy();
        snap.interfaces = vec![InterfaceDhcp {
            mac_address: "aa:bb:cc:dd:ee:ff".into(),
            last_dhcp: None,
        }];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
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
        let checks = assemble_checks(&snap, now, Duration::from_secs(30 * 60), Some(true));
        assert!(
            checks.iter().any(|c| c.name == "dhcp_stale"),
            "expected dhcp_stale detail under 30m threshold",
        );
    }

    // ── headline aggregation ──────────────────────────────────────────────

    /// Holistic layer aggregation: per-axis headlines stay Ok for
    /// signals that aren't axis-modeled (agent-version drift, generic
    /// alerts), but the layer aggregate at `Layer::run` time picks up
    /// the Fail/Warn detail rows via `layer::aggregate_status`. The
    /// per-axis verdict headlines are unaffected.
    #[test]
    fn agent_health_signals_emit_details_without_flipping_axis_headlines() {
        let now = Utc::now();

        let mut s_drift = snap_healthy();
        s_drift.agent_version_superseded_at = Some(now - chrono::Duration::days(1));
        let drift_checks = assemble_checks(&s_drift, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let drift_detail = drift_checks
            .iter()
            .find(|c| c.name == "agent_version_drift")
            .expect("agent_version_drift detail");
        assert_eq!(drift_detail.status, Status::Warn);
        // Axis headlines unaffected by agent-version drift.
        for axis in ["dpu_cert", "dpu_isolation", "hbn", "dpu_services"] {
            let h = drift_checks
                .iter()
                .find(|c| c.name == axis && c.kind == CheckKind::Headline)
                .unwrap();
            assert_eq!(h.status, Status::Ok, "{axis}");
        }

        let mut s_alert = snap_healthy();
        s_alert.alerts = vec![AgentAlert {
            id: "HeartbeatTimeout".into(),
            target: Some("dpu-42".into()),
            message: String::new(),
            in_alert_since: None,
        }];
        let alert_checks = assemble_checks(&s_alert, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let alert_detail = alert_checks
            .iter()
            .find(|c| c.name == "alert")
            .expect("alert detail");
        assert_eq!(alert_detail.status, Status::Fail);
        for axis in ["dpu_cert", "dpu_isolation", "hbn", "dpu_services"] {
            let h = alert_checks
                .iter()
                .find(|c| c.name == axis && c.kind == CheckKind::Headline)
                .unwrap();
            assert_eq!(h.status, Status::Ok, "{axis}");
        }
    }

    // ── holistic shape (PRD-003 slice 5) ─────────────────────────────────

    #[test]
    fn cert_axis_headline_carries_dpu_cert_drilldown_command() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let h = checks
            .iter()
            .find(|c| c.name == "dpu_cert" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(
            h.next_command.as_deref(),
            Some("nico doctor dpu-cert dpu-42"),
        );
    }

    #[test]
    fn isolation_axis_headline_carries_dpu_isolation_drilldown_command() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let h = checks
            .iter()
            .find(|c| c.name == "dpu_isolation" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(
            h.next_command.as_deref(),
            Some("nico doctor dpu-isolation dpu-42"),
        );
    }

    #[test]
    fn hbn_axis_headline_carries_hbn_drilldown_command() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let h = checks
            .iter()
            .find(|c| c.name == "hbn" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(h.next_command.as_deref(), Some("nico doctor hbn dpu-42"));
    }

    #[test]
    fn services_axis_headline_carries_dpu_services_drilldown_command() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let h = checks
            .iter()
            .find(|c| c.name == "dpu_services" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(
            h.next_command.as_deref(),
            Some("nico doctor dpu-services dpu-42"),
        );
    }

    #[test]
    fn headlines_come_before_any_detail_row_in_output_order() {
        let mut snap = snap_healthy();
        snap.alerts = vec![AgentAlert {
            id: "HeartbeatTimeout".into(),
            target: Some("dpu-42".into()),
            message: String::new(),
            in_alert_since: None,
        }];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let mut seen_detail = false;
        for c in &checks {
            match c.kind {
                CheckKind::Headline => assert!(
                    !seen_detail,
                    "headline {} appeared after a detail row",
                    c.name
                ),
                CheckKind::Detail => seen_detail = true,
            }
        }
    }

    #[test]
    fn axis_headline_order_is_cert_isolation_hbn_services_infiniband() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let names: Vec<&str> = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Headline)
            .map(|c| c.name)
            .collect();
        assert_eq!(
            names,
            vec![
                "dpu_cert",
                "dpu_isolation",
                "hbn",
                "dpu_services",
                "infiniband",
            ],
        );
    }

    // ── infiniband axis (PRD-004 slice 4) ────────────────────────────────

    #[test]
    fn infiniband_present_some_true_emits_ib_axis_headline_via_verdict() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let h = checks
            .iter()
            .find(|c| c.name == "infiniband" && c.kind == CheckKind::Headline)
            .expect("infiniband headline expected when capability is Some(true)");
        assert_eq!(h.status, Status::Ok);
        assert!(h.value.contains("healthy"), "value: {}", h.value);
    }

    #[test]
    fn infiniband_present_some_false_omits_ib_axis_entirely() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(false));
        assert!(
            !checks.iter().any(|c| c.name == "infiniband"),
            "infiniband row should be omitted when capability is Some(false)",
        );
    }

    #[test]
    fn infiniband_present_none_emits_unknown_ib_axis() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, None);
        let h = checks
            .iter()
            .find(|c| c.name == "infiniband" && c.kind == CheckKind::Headline)
            .expect("infiniband headline expected when capability is None");
        assert_eq!(h.status, Status::Unknown);
        assert!(
            h.value.contains("presence not detected"),
            "value: {}",
            h.value,
        );
        assert!(
            h.next_command
                .as_deref()
                .unwrap_or("")
                .contains("nico doctor infiniband"),
        );
    }

    #[test]
    fn ib_fabric_id_empty_flips_infiniband_axis_to_fail() {
        let mut snap = snap_healthy();
        snap.infiniband_ports[0].fabric_id = "".into();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let h = checks
            .iter()
            .find(|c| c.name == "infiniband" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(h.status, Status::Fail);
    }

    #[test]
    fn ufm_unobservable_flips_infiniband_axis_to_warn() {
        let mut snap = snap_healthy();
        snap.infiniband_ufm_observable = Some(false);
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let h = checks
            .iter()
            .find(|c| c.name == "infiniband" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(h.status, Status::Warn);
    }

    #[test]
    fn hbn_drift_flips_hbn_axis_to_fail_in_holistic_summary() {
        let snap = HealthSnapshot {
            applied_managed_host_config_version: "v1".into(),
            desired_managed_host_config_version: "v2".into(),
            ..snap_healthy()
        };
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let hbn = checks
            .iter()
            .find(|c| c.name == "hbn" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(hbn.status, Status::Fail);
        assert!(hbn.value.contains("drift"), "value: {}", hbn.value);
    }

    #[test]
    fn quarantine_flips_isolation_axis_to_fail() {
        let snap = HealthSnapshot {
            quarantine_state: Some("BlockAllTraffic".into()),
            ..snap_healthy()
        };
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let iso = checks
            .iter()
            .find(|c| c.name == "dpu_isolation" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(iso.status, Status::Fail);
    }

    #[test]
    fn expired_cert_flips_cert_axis_to_fail() {
        let now = Utc::now();
        let snap = HealthSnapshot {
            client_certificate_expiry: Some(now - chrono::Duration::days(2)),
            ..snap_healthy()
        };
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let cert = checks
            .iter()
            .find(|c| c.name == "dpu_cert" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(cert.status, Status::Fail);
    }

    #[test]
    fn failed_service_flips_services_axis_to_warn() {
        let now = Utc::now();
        let snap = HealthSnapshot {
            extension_services_observed_at: Some(now),
            extension_services: vec![crate::dpu_services::ServiceStatus {
                service_name: "doca-telemetry".into(),
                version: "1.0.0".into(),
                overall_state: "Failed".into(),
                message: String::new(),
                removed: None,
            }],
            ..snap_healthy()
        };
        let checks = assemble_checks(&snap, now, DEFAULT_DHCP_STALE_THRESHOLD, Some(true));
        let svc = checks
            .iter()
            .find(|c| c.name == "dpu_services" && c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(svc.status, Status::Warn);
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
