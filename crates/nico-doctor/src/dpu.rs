//! Fleet-wide DPU holistic summary — the `dpu` layer's data + assembly
//! seam (PRD-003 slice 6, #310; PRD-004 slice 5, #315 — adds `infiniband`).
//!
//! Iterates the fleet, calls each per-DPU axis verdict (`cert_verdict`,
//! `isolation_verdict`, `hbn_verdict`, `services_verdict`,
//! `ib_verdict`), and emits **one headline per axis** with a rollup
//! count + worst-status DPU examples. Fleet-specific concerns that have
//! not yet been carved into axis verdicts (`probe-stuck`) live
//! alongside the per-axis headlines.
//!
//! Pure assembly over a snapshot vec — no I/O, no clock reads. The
//! [`DpuClient`] trait is the seam over forgedb / Postgres; tests inject
//! mocks.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::dpu_cert::{CertSnapshot, DEFAULT_WARN_THRESHOLD as CERT_WARN_THRESHOLD};
use crate::dpu_isolation::{IsolationSnapshot, DEFAULT_FRESHNESS_THRESHOLD as ISOLATION_FRESHNESS_THRESHOLD};
use crate::dpu_services::{ServiceStatus, ServicesSnapshot, DEFAULT_OBSERVATION_STALE_THRESHOLD};
use crate::formatter::FINDINGS_CAP;
use crate::hbn::{HbnSnapshot, DEFAULT_FRESHNESS_THRESHOLD as HBN_FRESHNESS_THRESHOLD};
use crate::infiniband::{
    IbAlert, IbPort, IbSnapshot,
    DEFAULT_OBSERVATION_STALE_THRESHOLD as IB_STALE_THRESHOLD,
};
use crate::layer::{Check, CheckKind};
use crate::verdicts::{
    cert_verdict, hbn_verdict, ib_verdict, isolation_verdict, services_verdict, AxisSummary,
};

pub use nico_common::config::DpuConfig;

/// One row in the fleet view — the union of producer-side fields the
/// four axis verdicts need plus the agent-alert column used by the
/// fleet-specific `probe-stuck` headline.
#[derive(Debug, Clone)]
pub struct DpuSnapshot {
    pub dpu_id: String,
    pub applied_managed_host_config_version: String,
    pub desired_managed_host_config_version: String,
    pub applied_instance_network_config_version: String,
    pub desired_instance_network_config_version: String,
    pub quarantine_state: Option<String>,
    pub last_seen_at: DateTime<Utc>,
    pub client_certificate_expiry: Option<DateTime<Utc>>,
    pub health_alerts: Vec<HealthAlert>,
    pub network_config_error: Option<String>,
    pub hbn_version: String,
    pub bgp_alerts: Vec<String>,
    pub extension_services_observed_at: Option<DateTime<Utc>>,
    pub extension_services: Vec<ServiceStatus>,
    /// `infiniband_status_observation->>'observed_at'`. PRD-004 slice 5.
    pub infiniband_observed_at: Option<DateTime<Utc>>,
    /// `infiniband_status_observation->>'ufm_observable'`. `Some(false)`
    /// ⇒ UFM has lost visibility into the fabric ⇒ contributes Warn via
    /// `ib_verdict`.
    pub infiniband_ufm_observable: Option<bool>,
    /// Parsed `infiniband_status_observation->'ports'` array.
    pub infiniband_ports: Vec<IbPort>,
    /// IB-typed alerts from `dpu_agent_health_report` (id prefix `Ib`).
    /// Parsed by [`crate::infiniband::parse_ib_alerts`].
    pub ib_alerts: Vec<IbAlert>,
}

/// One entry from the agent's `HealthReport.alerts` array, persisted on
/// `machines.dpu_agent_health_report` as JSONB (PRD-002). The fleet
/// `probe-stuck` headline filters this list for the `PostConfigCheckWait`
/// probe id.
#[derive(Debug, Clone)]
pub struct HealthAlert {
    pub id: String,
    pub in_alert_since: Option<DateTime<Utc>>,
}

/// Extract `HealthAlert`s from the raw `machines.dpu_agent_health_report`
/// JSONB blob. Tolerates the column being NULL or having no `alerts`
/// array. Accepts `in_alert_since` as RFC3339 string, Unix epoch seconds
/// (i64), or `null` — the agent's serializer history has used all three.
pub fn parse_health_alerts(blob: Option<&serde_json::Value>) -> Vec<HealthAlert> {
    let Some(v) = blob else { return Vec::new() };
    let Some(arr) = v.get("alerts").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let in_alert_since = entry.get("in_alert_since").and_then(parse_in_alert_since);
            Some(HealthAlert { id, in_alert_since })
        })
        .collect()
}

fn parse_in_alert_since(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    if v.is_null() {
        return None;
    }
    if let Some(s) = v.as_str() {
        return DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc));
    }
    if let Some(n) = v.as_i64() {
        return DateTime::<Utc>::from_timestamp(n, 0);
    }
    None
}

/// Read-only seam over the fleet data layer (forgedb + Postgres).
#[async_trait]
pub trait DpuClient: Send + Sync {
    async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>>;
}

/// Default sqlx-backed [`DpuClient`].
pub struct SqlxDpuClient {
    pool: sqlx::PgPool,
}

/// SQL used by [`SqlxDpuClient::fetch_fleet`]. Extracted as a constant
/// so the schema choice (which tables / JSON paths the layer reads) is
/// pinned by a unit test.
pub(crate) const FETCH_FLEET_SQL: &str = "\
    SELECT \
        m.id, \
        COALESCE(m.network_status_observation->>'network_config_version', ''), \
        COALESCE(m.network_status_observation->'instance_network_observation'->>'config_version', ''), \
        m.network_config_version, \
        COALESCE(i.network_config_version, ''), \
        m.network_config->'quarantine_state'->>'mode', \
        (m.network_status_observation->>'observed_at')::timestamptz, \
        (m.network_status_observation->>'client_certificate_expiry')::bigint, \
        m.dpu_agent_health_report, \
        NULLIF(m.network_status_observation->>'network_config_error', ''), \
        (SELECT c->>'version' FROM jsonb_array_elements(COALESCE(m.inventory->'components', '[]'::jsonb)) c \
           WHERE c->>'name' = 'hbn' LIMIT 1), \
        (m.network_status_observation->'extension_service_observation'->>'observed_at')::timestamptz, \
        m.network_status_observation->'extension_service_observation'->'extension_service_statuses', \
        (m.infiniband_status_observation->>'observed_at')::timestamptz, \
        (m.infiniband_status_observation->>'ufm_observable')::boolean, \
        m.infiniband_status_observation->'ports' \
    FROM machines m \
    LEFT JOIN instances i ON i.machine_id = m.id \
    WHERE m.network_status_observation IS NOT NULL";

impl SqlxDpuClient {
    pub fn new(url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .map_err(|e| anyhow::anyhow!("invalid postgres URL: {e}"))?;
        Ok(Self { pool })
    }
}

type FleetRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    DateTime<Utc>,
    Option<i64>,
    Option<serde_json::Value>,
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<serde_json::Value>,
    Option<DateTime<Utc>>,
    Option<bool>,
    Option<serde_json::Value>,
);

/// Parse the `extension_service_statuses` JSON array into
/// [`ServiceStatus`] rows. Tolerates the column being NULL or absent.
pub fn parse_extension_services(blob: Option<&serde_json::Value>) -> Vec<ServiceStatus> {
    let Some(arr) = blob.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|entry| {
            let service_name = entry.get("service_name")?.as_str()?.to_owned();
            let version = entry
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let overall_state = entry
                .get("overall_state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let message = entry
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            let removed = entry
                .get("removed")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            Some(ServiceStatus {
                service_name,
                version,
                overall_state,
                message,
                removed,
            })
        })
        .collect()
}

#[async_trait]
impl DpuClient for SqlxDpuClient {
    async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>> {
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'machines')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("dpu fleet schema probe failed: {e}"))?;
        if !exists.0 {
            return Ok(Vec::new());
        }

        let rows: Vec<FleetRow> = sqlx::query_as(FETCH_FLEET_SQL)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| anyhow::anyhow!("dpu fleet query failed: {e}"))?;

        Ok(rows
            .into_iter()
            .map(|r| DpuSnapshot {
                dpu_id: r.0,
                applied_managed_host_config_version: r.1,
                applied_instance_network_config_version: r.2,
                desired_managed_host_config_version: r.3,
                desired_instance_network_config_version: r.4,
                quarantine_state: r.5,
                last_seen_at: r.6,
                client_certificate_expiry: r.7.and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
                health_alerts: parse_health_alerts(r.8.as_ref()),
                network_config_error: r.9,
                hbn_version: r.10.unwrap_or_default(),
                bgp_alerts: crate::hbn::parse_bgp_alerts(r.8.as_ref()),
                extension_services_observed_at: r.11,
                extension_services: parse_extension_services(r.12.as_ref()),
                infiniband_observed_at: r.13,
                infiniband_ufm_observable: r.14,
                infiniband_ports: crate::infiniband::parse_ports(r.15.as_ref()),
                ib_alerts: crate::infiniband::parse_ib_alerts(r.8.as_ref()),
            })
            .collect())
    }
}

/// Per-axis verdict for one DPU. Builds the axis-specific snapshot from
/// the fleet [`DpuSnapshot`] and delegates to the shared verdict
/// function so the fleet rollup says exactly what the per-DPU drill-down
/// would say.
fn cert_summary(s: &DpuSnapshot, now: DateTime<Utc>) -> AxisSummary {
    cert_verdict(
        &CertSnapshot {
            dpu_id: s.dpu_id.clone(),
            client_certificate_expiry: s.client_certificate_expiry,
        },
        now,
        CERT_WARN_THRESHOLD,
    )
}

fn isolation_summary(s: &DpuSnapshot, now: DateTime<Utc>) -> AxisSummary {
    isolation_verdict(
        &IsolationSnapshot {
            machine_id: s.dpu_id.clone(),
            registered: true,
            scout_discovery_complete: true,
            quarantine_state: s.quarantine_state.clone(),
            last_seen_at: Some(s.last_seen_at),
        },
        now,
        ISOLATION_FRESHNESS_THRESHOLD,
    )
}

fn hbn_summary(s: &DpuSnapshot, now: DateTime<Utc>) -> AxisSummary {
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
            last_seen_at: s.last_seen_at,
        },
        now,
        HBN_FRESHNESS_THRESHOLD,
    )
}

fn services_summary(s: &DpuSnapshot, now: DateTime<Utc>) -> AxisSummary {
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

fn infiniband_summary(s: &DpuSnapshot, now: DateTime<Utc>) -> AxisSummary {
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

/// Worst-case status across a slice of `AxisSummary` verdicts.
/// Priority: Fail > Warn > Unknown > Ok.
fn worst_status(verdicts: &[AxisSummary]) -> Status {
    if verdicts.iter().any(|v| v.status == Status::Fail) {
        Status::Fail
    } else if verdicts.iter().any(|v| v.status == Status::Warn) {
        Status::Warn
    } else if verdicts.iter().any(|v| v.status == Status::Unknown) {
        Status::Unknown
    } else {
        Status::Ok
    }
}

/// Fleet-rollup headline message for one axis. Counts non-Ok verdicts
/// and renders the human-readable count line (e.g. `3 DPUs drifted`).
/// The empty / all-Ok case renders the axis-specific `no <foo>` line.
fn axis_headline_value(axis: &'static str, verdicts: &[AxisSummary]) -> String {
    let total = verdicts.len();
    let fail = verdicts.iter().filter(|v| v.status == Status::Fail).count();
    let warn = verdicts.iter().filter(|v| v.status == Status::Warn).count();
    let unknown = verdicts.iter().filter(|v| v.status == Status::Unknown).count();
    let bad = fail + warn + unknown;
    if total == 0 {
        return ok_message_for(axis, 0);
    }
    if bad == 0 {
        return ok_message_for(axis, total);
    }
    let noun = match (axis, bad) {
        ("dpu_cert", _) => "cert",
        ("dpu_isolation", _) => "isolation",
        ("hbn", _) => "hbn",
        ("dpu_services", _) => "services",
        ("infiniband", _) => "infiniband",
        _ => "axis",
    };
    let mut parts: Vec<String> = Vec::new();
    if fail > 0 {
        parts.push(format!("{fail} fail"));
    }
    if warn > 0 {
        parts.push(format!("{warn} warn"));
    }
    if unknown > 0 {
        parts.push(format!("{unknown} unknown"));
    }
    format!("{bad}/{total} DPUs {noun}: {}", parts.join(", "))
}

fn ok_message_for(axis: &'static str, total: usize) -> String {
    match axis {
        "dpu_cert" => "no expiring certs".to_string(),
        "dpu_isolation" => format!("{total} DPUs healthy"),
        "hbn" => "no drift".to_string(),
        "dpu_services" => "no degraded services".to_string(),
        "infiniband" => format!("{total} DPUs IB healthy"),
        _ => format!("{total} DPUs ok"),
    }
}

/// Sort key: Fail > Warn > Unknown > Ok. Lower = worse → sort ascending
/// to put worst first.
fn status_rank(s: &Status) -> u8 {
    match s {
        Status::Fail => 0,
        Status::Warn => 1,
        Status::Unknown => 2,
        Status::Ok => 3,
        Status::Skipped => 4,
    }
}

/// Assemble the fleet `dpu` layer's holistic checks from a snapshot vec.
/// Pure — the caller supplies `now` so the function stays clock-free.
///
/// `infiniband_present` is the boot-probe-derived capability gate
/// (PRD-004 slice 1): `Some(false)` ⇒ the IB axis row is omitted
/// entirely (n/a-by-design — no IB fabric to summarise); `Some(true)`
/// or `None` ⇒ include the IB axis. `None` matches the existing
/// auto-unresolved / force-mode behaviour: render the axis using
/// whatever data the snapshot carries (the per-DPU verdict yields
/// `Unknown` for empty observations).
///
/// Output ordering:
///
/// 1. One headline `Check` per per-DPU axis (cert, isolation, hbn,
///    services, and — when not gated off — infiniband), tagged with the
///    axis name. Each headline's `value` summarises the rollup count;
///    status = worst-of the per-DPU verdicts on that axis.
/// 2. Fleet-specific headlines that have not yet migrated onto the
///    verdict primitive — currently just `probe-stuck`.
/// 3. Detail rows for the worst non-Ok DPUs across all axes, capped at
///    [`FINDINGS_CAP`], sorted Fail before Warn before Unknown, then
///    by axis. Each detail row carries the per-DPU verdict message
///    and the verdict's `next_command` (the per-DPU drill-down hint).
pub fn assemble_checks(
    snapshots: &[DpuSnapshot],
    now: DateTime<Utc>,
    config: &DpuConfig,
    infiniband_present: Option<bool>,
) -> Vec<Check> {
    let cert: Vec<AxisSummary> = snapshots.iter().map(|s| cert_summary(s, now)).collect();
    let isolation: Vec<AxisSummary> =
        snapshots.iter().map(|s| isolation_summary(s, now)).collect();
    let hbn: Vec<AxisSummary> = snapshots.iter().map(|s| hbn_summary(s, now)).collect();
    let services: Vec<AxisSummary> = snapshots.iter().map(|s| services_summary(s, now)).collect();
    let include_ib = infiniband_present != Some(false);
    let infiniband: Vec<AxisSummary> = if include_ib {
        snapshots.iter().map(|s| infiniband_summary(s, now)).collect()
    } else {
        Vec::new()
    };

    let mut out: Vec<Check> = vec![
        axis_headline("dpu_cert", &cert),
        axis_headline("dpu_isolation", &isolation),
        axis_headline("hbn", &hbn),
        axis_headline("dpu_services", &services),
    ];
    if include_ib {
        out.push(axis_headline("infiniband", &infiniband));
    }
    out.extend(probe_stuck_headline(snapshots, now, config));

    // Detail rows: collect every non-Ok per-DPU verdict across all axes,
    // sort worst-first (Fail > Warn > Unknown), and cap. IB sits after
    // services in the axis-rank tiebreaker so its detail rows follow
    // the cert/isolation/hbn/services ordering established by slice 6.
    let mut axes: Vec<(&'static str, &[AxisSummary])> = vec![
        ("dpu_cert", &cert),
        ("dpu_isolation", &isolation),
        ("hbn", &hbn),
        ("dpu_services", &services),
    ];
    if include_ib {
        axes.push(("infiniband", &infiniband));
    }
    let mut findings: Vec<(u8, u8, &'static str, &AxisSummary)> = Vec::new();
    for (axis_rank, (name, verdicts)) in axes.iter().enumerate() {
        for v in verdicts.iter() {
            if v.status == Status::Ok || v.status == Status::Skipped {
                continue;
            }
            findings.push((status_rank(&v.status), axis_rank as u8, name, v));
        }
    }
    findings.sort_by_key(|(s, a, _, _)| (*s, *a));
    for (_, _, name, v) in findings.into_iter().take(FINDINGS_CAP) {
        out.push(Check {
            name,
            status: v.status.clone(),
            value: v.message.clone(),
            next_command: v.next_command.clone(),
            kind: CheckKind::Detail,
        });
    }

    // Probe-stuck details follow the per-axis details — they're
    // fleet-specific findings that share the same JSON section.
    out.extend(probe_stuck_details(snapshots, now, config));

    out
}

fn axis_headline(axis: &'static str, verdicts: &[AxisSummary]) -> Check {
    Check {
        name: axis,
        status: worst_status(verdicts),
        value: axis_headline_value(axis, verdicts),
        next_command: None,
        kind: CheckKind::Headline,
    }
}

/// Fleet-specific `probe-stuck` headline. Counts DPUs whose
/// `PostConfigCheckWait` health probe has been in alert state longer
/// than [`DpuConfig::probe_stuck_grace`]. Encodes failure mode 3 from
/// `docs/learning/topics/01-hbn.md` §3.
fn probe_stuck_headline(
    snapshots: &[DpuSnapshot],
    now: DateTime<Utc>,
    config: &DpuConfig,
) -> Vec<Check> {
    let stuck = collect_stuck(snapshots, now, config);
    let count = stuck.len();
    let status = if count > 0 { Status::Fail } else { Status::Ok };
    vec![Check {
        name: "probe-stuck",
        status,
        value: format!("{count} DPUs with stuck PostConfigCheckWait"),
        next_command: None,
        kind: CheckKind::Headline,
    }]
}

fn probe_stuck_details(
    snapshots: &[DpuSnapshot],
    now: DateTime<Utc>,
    config: &DpuConfig,
) -> Vec<Check> {
    let mut stuck = collect_stuck(snapshots, now, config);
    stuck.sort_by_key(|(_, age)| std::cmp::Reverse(*age));
    stuck
        .into_iter()
        .take(FINDINGS_CAP)
        .map(|(dpu_id, age)| Check {
            name: "probe-stuck",
            status: Status::Fail,
            value: format!(
                "dpu {dpu_id} PostConfigCheckWait stuck (in_alert_since {}s ago)",
                age.as_secs()
            ),
            next_command: Some(format!("nico doctor hbn {dpu_id}")),
            kind: CheckKind::Detail,
        })
        .collect()
}

fn collect_stuck<'a>(
    snapshots: &'a [DpuSnapshot],
    now: DateTime<Utc>,
    config: &DpuConfig,
) -> Vec<(&'a str, Duration)> {
    const PROBE_ID: &str = "PostConfigCheckWait";
    let mut stuck: Vec<(&str, Duration)> = Vec::new();
    for s in snapshots {
        for alert in &s.health_alerts {
            if alert.id != PROBE_ID {
                continue;
            }
            let Some(since) = alert.in_alert_since else {
                continue;
            };
            let age = (now - since).to_std().unwrap_or(Duration::ZERO);
            if age > config.probe_stuck_grace {
                stuck.push((s.dpu_id.as_str(), age));
            }
        }
    }
    stuck
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;

    fn snap(dpu_id: &str) -> DpuSnapshot {
        DpuSnapshot {
            dpu_id: dpu_id.into(),
            applied_managed_host_config_version: "v1".into(),
            desired_managed_host_config_version: "v1".into(),
            applied_instance_network_config_version: "v1".into(),
            desired_instance_network_config_version: "v1".into(),
            quarantine_state: None,
            last_seen_at: Utc::now(),
            client_certificate_expiry: None,
            health_alerts: Vec::new(),
            network_config_error: None,
            hbn_version: String::new(),
            bgp_alerts: Vec::new(),
            extension_services_observed_at: None,
            extension_services: Vec::new(),
            infiniband_observed_at: None,
            infiniband_ufm_observable: None,
            infiniband_ports: Vec::new(),
            ib_alerts: Vec::new(),
        }
    }

    fn headline<'a>(checks: &'a [Check], name: &str) -> &'a Check {
        checks
            .iter()
            .find(|c| c.name == name && c.kind == CheckKind::Headline)
            .unwrap_or_else(|| {
                let names: Vec<(&str, CheckKind)> =
                    checks.iter().map(|c| (c.name, c.kind)).collect();
                panic!("missing headline {name}: {names:?}")
            })
    }

    fn details<'a>(checks: &'a [Check], name: &str) -> Vec<&'a Check> {
        checks
            .iter()
            .filter(|c| c.name == name && c.kind == CheckKind::Detail)
            .collect()
    }

    fn alert(id: &str, in_alert_since: Option<DateTime<Utc>>) -> HealthAlert {
        HealthAlert { id: id.into(), in_alert_since }
    }

    // ── tracer bullet: empty fleet ──────────────────────────────────────

    #[test]
    fn empty_fleet_emits_four_axis_headlines_plus_probe_stuck_all_ok() {
        let now = Utc::now();
        let checks = assemble_checks(&[], now, &DpuConfig::default(), Some(true));
        for axis in [
            "dpu_cert",
            "dpu_isolation",
            "hbn",
            "dpu_services",
            "infiniband",
            "probe-stuck",
        ] {
            assert_eq!(headline(&checks, axis).status, Status::Ok, "{axis}");
        }
        assert!(
            checks.iter().all(|c| c.kind == CheckKind::Headline),
            "empty fleet should have no detail rows: {:?}",
            checks.iter().map(|c| (c.name, c.kind)).collect::<Vec<_>>(),
        );
    }

    // ── per-axis rollup: cert ───────────────────────────────────────────

    #[test]
    fn expired_cert_on_one_dpu_flips_cert_axis_to_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.client_certificate_expiry = Some(now - ChronoDuration::days(1));
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        let h = headline(&checks, "dpu_cert");
        assert_eq!(h.status, Status::Fail);
        assert!(h.value.contains("DPUs cert"), "value: {}", h.value);
    }

    #[test]
    fn cert_within_warn_window_flips_cert_axis_to_warn() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.client_certificate_expiry = Some(now + ChronoDuration::days(20));
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "dpu_cert").status, Status::Warn);
    }

    // ── per-axis rollup: isolation ──────────────────────────────────────

    #[test]
    fn one_dpu_with_lost_connection_flips_isolation_axis_to_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.last_seen_at = now - ChronoDuration::seconds(300); // > 90s freshness
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "dpu_isolation").status, Status::Fail);
    }

    #[test]
    fn one_quarantined_dpu_flips_isolation_axis_to_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.quarantine_state = Some("BlockAllTraffic".into());
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "dpu_isolation").status, Status::Fail);
    }

    // ── per-axis rollup: hbn ────────────────────────────────────────────

    #[test]
    fn managed_host_drift_flips_hbn_axis_to_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.applied_managed_host_config_version = "v1".into();
        s.desired_managed_host_config_version = "v2".into();
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "hbn").status, Status::Fail);
    }

    #[test]
    fn bgp_alert_flips_hbn_axis_to_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.bgp_alerts = vec!["BgpPeerDown".into()];
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "hbn").status, Status::Fail);
    }

    // ── per-axis rollup: services ───────────────────────────────────────

    #[test]
    fn one_failed_service_flips_services_axis_to_warn() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.extension_services_observed_at = Some(now);
        s.extension_services = vec![ServiceStatus {
            service_name: "doca-telemetry".into(),
            version: "1.0.0".into(),
            overall_state: "Failed".into(),
            message: String::new(),
            removed: None,
        }];
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "dpu_services").status, Status::Warn);
    }

    // ── per-axis rollup: infiniband (PRD-004 slice 5) ───────────────────

    fn ib_active_port() -> crate::infiniband::IbPort {
        crate::infiniband::IbPort {
            guid: "fe80::1".into(),
            fabric_id: "ib-fabric-1".into(),
            lid: 7,
            port_state: "Active".into(),
        }
    }

    /// Default `snap()` has no IB observation → ib_verdict yields
    /// Unknown. Construct a healthy IB observation explicitly so the
    /// rollup's IB axis lands on Ok.
    fn snap_with_healthy_ib(dpu_id: &str, now: DateTime<Utc>) -> DpuSnapshot {
        let mut s = snap(dpu_id);
        s.infiniband_observed_at = Some(now);
        s.infiniband_ufm_observable = Some(true);
        s.infiniband_ports = vec![ib_active_port()];
        s
    }

    #[test]
    fn healthy_ib_fleet_emits_ok_infiniband_axis() {
        let now = Utc::now();
        let s = snap_with_healthy_ib("dpu-1", now);
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        let h = headline(&checks, "infiniband");
        assert_eq!(h.status, Status::Ok);
        assert!(
            h.value.contains("IB healthy"),
            "ok message reads as 'IB healthy', got: {}",
            h.value,
        );
    }

    #[test]
    fn empty_fabric_id_flips_infiniband_axis_to_fail() {
        let now = Utc::now();
        let mut s = snap_with_healthy_ib("dpu-1", now);
        s.infiniband_ports[0].fabric_id = "".into();
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "infiniband").status, Status::Fail);
    }

    #[test]
    fn ufm_unobservable_flips_infiniband_axis_to_warn() {
        let now = Utc::now();
        let mut s = snap_with_healthy_ib("dpu-1", now);
        s.infiniband_ufm_observable = Some(false);
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "infiniband").status, Status::Warn);
    }

    #[test]
    fn infiniband_detail_row_carries_ib_verdict_next_command() {
        let now = Utc::now();
        let mut s = snap_with_healthy_ib("dpu-x", now);
        s.infiniband_ports[0].fabric_id = "".into();
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        let d = details(&checks, "infiniband");
        assert_eq!(d.len(), 1);
        assert!(d[0].value.contains("dpu-x"));
        assert!(
            d[0].next_command
                .as_deref()
                .unwrap_or("")
                .contains("nico doctor infiniband dpu-x"),
            "expected ib drill-down hint: {:?}",
            d[0].next_command,
        );
    }

    #[test]
    fn infiniband_present_some_false_omits_ib_axis_entirely() {
        let now = Utc::now();
        let mut s = snap_with_healthy_ib("dpu-1", now);
        // Even an active IB observation should not surface when the
        // capability gate says the fleet has no IB fabric — n/a by
        // design.
        s.infiniband_ports[0].fabric_id = "".into();
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(false));
        assert!(
            !checks.iter().any(|c| c.name == "infiniband"),
            "infiniband row should be omitted when capability gate is Some(false), got {:?}",
            checks.iter().map(|c| (c.name, c.kind)).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn infiniband_present_none_falls_back_to_per_dpu_observation() {
        // Force / auto-unresolved deployment: render the axis using
        // whatever the snapshot carries. A healthy snapshot ⇒ Ok.
        let now = Utc::now();
        let s = snap_with_healthy_ib("dpu-1", now);
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), None);
        assert!(checks.iter().any(|c| c.name == "infiniband"));
        assert_eq!(headline(&checks, "infiniband").status, Status::Ok);
    }

    #[test]
    fn infiniband_axis_appears_after_dpu_services_in_headlines() {
        let now = Utc::now();
        let checks = assemble_checks(&[snap("dpu-1")], now, &DpuConfig::default(), Some(true));
        let order: Vec<&str> = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Headline)
            .map(|c| c.name)
            .collect();
        let services_idx = order.iter().position(|&n| n == "dpu_services").unwrap();
        let ib_idx = order.iter().position(|&n| n == "infiniband").unwrap();
        let probe_idx = order.iter().position(|&n| n == "probe-stuck").unwrap();
        assert!(
            services_idx < ib_idx && ib_idx < probe_idx,
            "expected order ... dpu_services, infiniband, probe-stuck; got {order:?}",
        );
    }

    // ── fleet rollup count is non-Ok DPUs ───────────────────────────────

    #[test]
    fn cert_axis_headline_counts_three_expiring_in_a_fleet_of_five() {
        let now = Utc::now();
        let mut snaps: Vec<DpuSnapshot> = (0..5)
            .map(|i| {
                let mut s = snap(&format!("dpu-{i}"));
                // Default-Ok cert: 180 days remaining.
                s.client_certificate_expiry = Some(now + ChronoDuration::days(180));
                s
            })
            .collect();
        for s in snaps.iter_mut().take(3) {
            s.client_certificate_expiry = Some(now + ChronoDuration::days(15));
        }
        let checks = assemble_checks(&snaps, now, &DpuConfig::default(), Some(true));
        let h = headline(&checks, "dpu_cert");
        assert_eq!(h.status, Status::Warn);
        assert!(h.value.contains("3/5"), "value: {}", h.value);
        assert!(h.value.contains("3 warn"), "value: {}", h.value);
    }

    // ── detail capping at FINDINGS_CAP, sorted worst-first ──────────────

    #[test]
    fn fleet_detail_caps_at_findings_cap_with_fail_before_warn() {
        let now = Utc::now();
        let mut snaps: Vec<DpuSnapshot> = (0..3)
            .map(|i| {
                let mut s = snap(&format!("warner-{i}"));
                // Warn-tier cert: 20d to expiry < 30d warn threshold.
                s.client_certificate_expiry = Some(now + ChronoDuration::days(20));
                s
            })
            .collect();
        for i in 0..7 {
            let mut s = snap(&format!("failer-{i}"));
            // Fail-tier: expired cert.
            s.client_certificate_expiry = Some(now - ChronoDuration::days(1));
            snaps.push(s);
        }
        let checks = assemble_checks(&snaps, now, &DpuConfig::default(), Some(true));
        // All details across all axes, capped at 5.
        let total_details = checks.iter().filter(|c| c.kind == CheckKind::Detail).count();
        assert_eq!(total_details, FINDINGS_CAP);
        // Every detail is Fail (Fail-tier sorts before Warn-tier).
        for d in checks.iter().filter(|c| c.kind == CheckKind::Detail) {
            assert_eq!(
                d.status, Status::Fail,
                "expected all Fail details, got status {:?} value {:?}",
                d.status, d.value,
            );
        }
    }

    // ── detail rows carry next_command from the verdict ────────────────

    #[test]
    fn cert_detail_row_carries_cert_verdict_next_command() {
        let now = Utc::now();
        let mut s = snap("dpu-x");
        s.client_certificate_expiry = Some(now - ChronoDuration::days(2));
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        let d = details(&checks, "dpu_cert");
        assert_eq!(d.len(), 1);
        assert!(d[0].value.contains("dpu-x"));
        assert!(
            d[0].next_command
                .as_deref()
                .unwrap_or("")
                .contains("rotate"),
            "expected cert verdict's rotate next_command: {:?}",
            d[0].next_command,
        );
    }

    #[test]
    fn isolation_detail_row_carries_isolation_verdict_next_command() {
        let now = Utc::now();
        let mut s = snap("dpu-y");
        s.last_seen_at = now - ChronoDuration::seconds(300);
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        let d = details(&checks, "dpu_isolation");
        assert_eq!(d.len(), 1);
        assert!(d[0].value.contains("dpu-y"));
        assert!(d[0]
            .next_command
            .as_deref()
            .unwrap_or("")
            .contains("nico correlate"));
    }

    // ── output ordering: headlines first, details after ────────────────

    #[test]
    fn json_ordering_emits_all_headlines_before_any_detail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.client_certificate_expiry = Some(now - ChronoDuration::days(1));
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        let mut seen_detail = false;
        for c in &checks {
            match c.kind {
                CheckKind::Headline => {
                    assert!(
                        !seen_detail,
                        "headline {} appeared after a detail row",
                        c.name
                    );
                }
                CheckKind::Detail => seen_detail = true,
            }
        }
    }

    // ── per-axis headlines come before fleet-specific headlines ────────

    #[test]
    fn per_axis_headlines_come_before_fleet_specific_headlines() {
        let now = Utc::now();
        let checks = assemble_checks(&[snap("dpu-1")], now, &DpuConfig::default(), Some(true));
        let order: Vec<&str> = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Headline)
            .map(|c| c.name)
            .collect();
        let probe_idx = order.iter().position(|&n| n == "probe-stuck").unwrap();
        for axis in ["dpu_cert", "dpu_isolation", "hbn", "dpu_services"] {
            let i = order.iter().position(|&n| n == axis).unwrap();
            assert!(
                i < probe_idx,
                "{axis} should precede probe-stuck, order: {order:?}",
            );
        }
    }

    // ── probe-stuck preserved ──────────────────────────────────────────

    #[test]
    fn probe_stuck_fleet_headline_preserved_with_top_n_details() {
        let now = Utc::now();
        let snaps: Vec<DpuSnapshot> = (0..3)
            .map(|i| {
                let mut s = snap(&format!("stuck-{i}"));
                s.health_alerts = vec![alert(
                    "PostConfigCheckWait",
                    Some(now - ChronoDuration::seconds(60 + i * 10)),
                )];
                s
            })
            .collect();
        let checks = assemble_checks(&snaps, now, &DpuConfig::default(), Some(true));
        let h = headline(&checks, "probe-stuck");
        assert_eq!(h.status, Status::Fail);
        assert!(h.value.starts_with("3 DPUs"), "value: {}", h.value);
        let d = details(&checks, "probe-stuck");
        assert!(!d.is_empty(), "probe-stuck details should appear");
        assert!(d[0].value.contains("stuck-"));
    }

    // ── empty / all-Ok axis messages are axis-specific ─────────────────

    #[test]
    fn empty_fleet_cert_axis_value_reads_no_expiring_certs() {
        let checks = assemble_checks(&[], Utc::now(), &DpuConfig::default(), Some(true));
        assert!(
            headline(&checks, "dpu_cert").value.contains("no expiring"),
            "value: {}",
            headline(&checks, "dpu_cert").value,
        );
    }

    #[test]
    fn all_healthy_services_axis_value_reads_no_degraded() {
        let now = Utc::now();
        let s = snap("dpu-1");
        let checks = assemble_checks(&[s], now, &DpuConfig::default(), Some(true));
        assert_eq!(headline(&checks, "dpu_services").status, Status::Ok);
        assert!(
            headline(&checks, "dpu_services").value.contains("no degraded"),
            "value: {}",
            headline(&checks, "dpu_services").value,
        );
    }

    // ── parse_health_alerts (unchanged from prior slice) ───────────────

    #[test]
    fn parse_health_alerts_returns_empty_for_none() {
        assert!(parse_health_alerts(None).is_empty());
    }

    #[test]
    fn parse_health_alerts_extracts_rfc3339_in_alert_since() {
        let v = serde_json::json!({
            "alerts": [
                {"id": "PostConfigCheckWait", "in_alert_since": "2024-01-15T12:34:56Z"}
            ]
        });
        let out = parse_health_alerts(Some(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "PostConfigCheckWait");
    }

    #[test]
    fn parse_health_alerts_extracts_unix_epoch_in_alert_since() {
        let v = serde_json::json!({
            "alerts": [
                {"id": "PostConfigCheckWait", "in_alert_since": 1_700_000_000_i64}
            ]
        });
        let out = parse_health_alerts(Some(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].in_alert_since,
            DateTime::<Utc>::from_timestamp(1_700_000_000, 0),
        );
    }

    // ── parse_extension_services ───────────────────────────────────────

    #[test]
    fn parse_extension_services_returns_empty_for_none() {
        assert!(parse_extension_services(None).is_empty());
    }

    #[test]
    fn parse_extension_services_extracts_service_rows() {
        let v = serde_json::json!([
            {
                "service_name": "doca-telemetry",
                "version": "1.2.3",
                "overall_state": "Running",
                "message": "",
            },
            {
                "service_name": "doca-bfb",
                "version": "0.9.0",
                "overall_state": "Failed",
                "message": "exit 1",
                "removed": "operator decommissioned",
            }
        ]);
        let out = parse_extension_services(Some(&v));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].service_name, "doca-telemetry");
        assert_eq!(out[0].overall_state, "Running");
        assert_eq!(out[1].removed.as_deref(), Some("operator decommissioned"));
    }

    // ── fetch_fleet SQL schema (PRD-002 + slice-6 additions) ───────────

    #[test]
    fn fetch_fleet_sql_targets_producer_side_machines_columns() {
        let sql = FETCH_FLEET_SQL;
        assert!(
            !sql.contains("dpu_network_status"),
            "old table dpu_network_status still referenced: {sql}"
        );
        assert!(sql.contains("FROM machines"), "missing machines table: {sql}");
        assert!(
            sql.contains("network_status_observation"),
            "missing applied-side JSON column: {sql}"
        );
        assert!(
            sql.contains("network_config->'quarantine_state'"),
            "quarantine must be read desired-side from network_config: {sql}"
        );
        assert!(
            sql.contains("dpu_agent_health_report"),
            "agent alert JSON column must be read: {sql}"
        );
    }

    #[test]
    fn fetch_fleet_sql_includes_verdict_inputs_added_in_slice_6() {
        let sql = FETCH_FLEET_SQL;
        assert!(
            sql.contains("network_config_error"),
            "hbn verdict input missing: {sql}"
        );
        assert!(
            sql.contains("extension_service_observation"),
            "services verdict input missing: {sql}"
        );
        assert!(
            sql.contains("inventory->'components'") || sql.contains("inventory ->'components'"),
            "hbn version input missing: {sql}"
        );
    }

    #[test]
    fn fetch_fleet_sql_includes_infiniband_inputs_added_in_prd_004_slice_5() {
        let sql = FETCH_FLEET_SQL;
        assert!(
            sql.contains("infiniband_status_observation"),
            "IB verdict observation column missing: {sql}"
        );
        assert!(
            sql.contains("'ufm_observable'"),
            "IB ufm_observable column missing: {sql}"
        );
        assert!(sql.contains("'ports'"), "IB ports column missing: {sql}");
    }
}
