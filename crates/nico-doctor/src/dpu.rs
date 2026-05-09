//! Fleet-wide DPU/HBN roll-up — the `dpu` layer's data + assembly seam.
//!
//! Closes #214. Encodes the muscle-memory tell from
//! `docs/learning/topics/01-hbn.md`: most weird stuck states are
//! version-drift, not network failure — so the default ladder asks "are
//! any DPUs drifting?" before deeper diagnosis.
//!
//! Five parallel sub-checks, each its own headline `Check`:
//! `drift-managed-host`, `drift-instance`, `cert-fleet`, `quarantine`,
//! `lost-connection`. Drift sub-checks also emit top-N=5 detail lines
//! pointing at `nico doctor hbn <id>` (#205) for per-DPU drill-down.
//!
//! Pure assembly over a snapshot vec — no I/O, no clock reads. The
//! [`DpuClient`] trait is the seam over forgedb / Postgres; tests inject
//! mocks.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};

pub use nico_common::config::DpuConfig;

/// Number of detail lines a drift sub-check emits, capped to match #184.
pub const DRIFT_DETAIL_TOP_N: usize = 5;

/// One row in the fleet view — the union of producer-side fields the
/// six sub-checks need: applied state from `machines.network_status_observation`,
/// desired managed-host version from `machines.network_config_version`
/// (top-level column), desired quarantine + other config from
/// `machines.network_config` JSON, agent alerts from
/// `machines.dpu_agent_health_report` JSON, and desired instance
/// version from `instances.network_config_version` (joined on
/// `instances.machine_id`). PRD-002.
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
}

/// One entry from the agent's `HealthReport.alerts` array, persisted on
/// `machines.dpu_agent_health_report` as JSONB (PRD-002). The `dpu`
/// layer only consumes the fields it acts on — `id` to filter for a
/// known probe (e.g. `PostConfigCheckWait`) and `in_alert_since` to
/// age the alert.
#[derive(Debug, Clone)]
pub struct HealthAlert {
    pub id: String,
    pub in_alert_since: Option<DateTime<Utc>>,
}

/// Extract `HealthAlert`s from the raw `machines.dpu_agent_health_report`
/// JSONB blob. Tolerates the column being NULL or having no `alerts` array.
/// Accepts `in_alert_since` as RFC3339 string, Unix epoch seconds (i64),
/// or `null` — the agent's serializer history has used all three.
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

/// Read-only seam over the fleet data layer (forgedb + Postgres). The
/// real impl issues one bulk SELECT over `machines` (producer-side
/// JSON columns) joined with `instances`, covering every DPU; tests
/// inject mocks. Empty vec means "no DPUs" (or schema absent on dev
/// clusters — we degrade gracefully).
#[async_trait]
pub trait DpuClient: Send + Sync {
    async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>>;
}

/// Default sqlx-backed [`DpuClient`]. Single bulk query reads
/// producer-side state from `machines` row (PRD-002): `network_status_observation`
/// JSON for applied state, top-level `network_config_version` column for
/// the desired managed-host version, `network_config` JSON for desired
/// quarantine state, and `instances.network_config_version` (joined on
/// `instances.machine_id`) for the desired instance version.
pub struct SqlxDpuClient {
    pool: sqlx::PgPool,
}

/// SQL used by [`SqlxDpuClient::fetch_fleet`]. Extracted as a constant
/// so the schema choice (which tables / JSON paths the layer reads) is
/// pinned by a unit test, even though we can't exercise the full query
/// without a live postgres.
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
        m.dpu_agent_health_report \
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

        let rows: Vec<(
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            DateTime<Utc>,
            Option<i64>,
            Option<serde_json::Value>,
        )> = sqlx::query_as(FETCH_FLEET_SQL)
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
            })
            .collect())
    }
}

/// Per-DPU drift report: how long a DPU's applied version has lagged
/// behind desired. `age` is `now - last_seen_at` and a coarse lower bound
/// — the DPU has been in the observed applied state since at least
/// `last_seen_at`. Drift sub-checks rank DPUs by `age` for the top-N
/// detail list and pick worst-of for the headline.
#[derive(Debug, Clone)]
struct DriftReport<'a> {
    dpu_id: &'a str,
    applied: &'a str,
    desired: &'a str,
    age: Duration,
}

fn drift_reports<'a>(
    snapshots: &'a [DpuSnapshot],
    now: DateTime<Utc>,
    select_axis: impl Fn(&'a DpuSnapshot) -> (&'a str, &'a str),
) -> Vec<DriftReport<'a>> {
    snapshots
        .iter()
        .filter_map(|s| {
            let (applied, desired) = select_axis(s);
            if applied == desired {
                return None;
            }
            let age = (now - s.last_seen_at).to_std().unwrap_or(Duration::ZERO);
            Some(DriftReport {
                dpu_id: &s.dpu_id,
                applied,
                desired,
                age,
            })
        })
        .collect()
}

fn drift_status(reports: &[DriftReport<'_>], warn: Duration, fail: Duration) -> Status {
    if reports.iter().any(|r| r.age > fail) {
        Status::Fail
    } else if reports.iter().any(|r| r.age > warn) {
        Status::Warn
    } else if reports.is_empty() {
        Status::Ok
    } else {
        // Drifting but under warn threshold → still Ok (within churn window).
        Status::Ok
    }
}

fn drift_check(
    name: &'static str,
    reports: &[DriftReport<'_>],
    fleet_size: usize,
    warn: Duration,
    fail: Duration,
) -> Vec<Check> {
    let status = drift_status(reports, warn, fail);
    let drifting = reports.iter().filter(|r| r.age > warn).count();
    let value = format!("{drifting}/{fleet_size} drifting");
    let mut out = vec![Check {
        name,
        status: status.clone(),
        value,
        next_command: None,
        kind: CheckKind::Headline,
    }];

    if status == Status::Ok {
        return out;
    }

    let mut sorted: Vec<&DriftReport<'_>> = reports.iter().filter(|r| r.age > warn).collect();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.age));
    for r in sorted.into_iter().take(DRIFT_DETAIL_TOP_N) {
        let detail_status = if r.age > fail { Status::Fail } else { Status::Warn };
        out.push(Check {
            name,
            status: detail_status,
            value: format!(
                "dpu {} applied={} desired={} (drift {}s)",
                r.dpu_id,
                r.applied,
                r.desired,
                r.age.as_secs()
            ),
            next_command: Some(format!("nico doctor hbn {}", r.dpu_id)),
            kind: CheckKind::Detail,
        });
    }
    out
}

fn cert_check(snapshots: &[DpuSnapshot], now: DateTime<Utc>, config: &DpuConfig) -> Check {
    let mut warn_count = 0usize;
    let mut fail_count = 0usize;
    for s in snapshots {
        if let Some(exp) = s.client_certificate_expiry {
            let until = (exp - now).to_std().unwrap_or(Duration::ZERO);
            if until < config.cert_fail {
                fail_count += 1;
            } else if until < config.cert_warn {
                warn_count += 1;
            }
        }
    }
    let status = if fail_count > 0 {
        Status::Fail
    } else if warn_count > 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    Check {
        name: "cert-fleet",
        status,
        value: format!(
            "{} expiring < {}d, {} expiring < {}d",
            fail_count,
            config.cert_fail.as_secs() / 86_400,
            warn_count,
            config.cert_warn.as_secs() / 86_400,
        ),
        next_command: None,
        kind: CheckKind::Headline,
    }
}

fn quarantine_check(snapshots: &[DpuSnapshot]) -> Check {
    let count = snapshots
        .iter()
        .filter(|s| s.quarantine_state.as_deref() == Some("BlockAllTraffic"))
        .count();
    // Issue rule: quarantine sub-check is capped at Warn — quarantine is
    // sometimes deliberate, never auto-Fail.
    let status = if count > 0 { Status::Warn } else { Status::Ok };
    Check {
        name: "quarantine",
        status,
        value: format!("{count} quarantined"),
        next_command: None,
        kind: CheckKind::Headline,
    }
}

fn lost_connection_check(
    snapshots: &[DpuSnapshot],
    now: DateTime<Utc>,
    config: &DpuConfig,
) -> Check {
    let fleet = snapshots.len();
    let mut over_warn = 0usize;
    let mut over_fail_age = 0usize;
    for s in snapshots {
        let age = (now - s.last_seen_at).to_std().unwrap_or(Duration::ZERO);
        if age > config.lost_connection_warn {
            over_warn += 1;
        }
        if age > config.lost_connection_fail_age {
            over_fail_age += 1;
        }
    }
    let pct_over_warn = if fleet > 0 {
        over_warn as f64 / fleet as f64
    } else {
        0.0
    };
    let status = if over_fail_age > 0 || pct_over_warn > config.lost_connection_fail_pct {
        Status::Fail
    } else if over_warn > 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    Check {
        name: "lost-connection",
        status,
        value: format!(
            "{}/{} silent > {}s",
            over_warn,
            fleet,
            config.lost_connection_warn.as_secs()
        ),
        next_command: None,
        kind: CheckKind::Headline,
    }
}

/// `probe-stuck` sub-check (issue #239). Flags DPUs whose
/// `PostConfigCheckWait` health probe has been in alert state longer
/// than [`DpuConfig::probe_stuck_grace`]. Encodes failure mode 3 from
/// `docs/learning/topics/01-hbn.md` §3 — DPU traffic is dropped
/// post-config and the agent's `PostConfigCheckWait` probe stays in
/// alert.
fn probe_stuck_check(
    snapshots: &[DpuSnapshot],
    now: DateTime<Utc>,
    config: &DpuConfig,
) -> Vec<Check> {
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

    let count = stuck.len();
    let status = if count > 0 { Status::Fail } else { Status::Ok };
    let mut out = vec![Check {
        name: "probe-stuck",
        status: status.clone(),
        value: format!("{count} DPUs with stuck PostConfigCheckWait"),
        next_command: None,
        kind: CheckKind::Headline,
    }];

    if status == Status::Ok {
        return out;
    }

    stuck.sort_by_key(|(_, age)| std::cmp::Reverse(*age));
    for (dpu_id, age) in stuck.into_iter().take(DRIFT_DETAIL_TOP_N) {
        out.push(Check {
            name: "probe-stuck",
            status: Status::Fail,
            value: format!(
                "dpu {dpu_id} PostConfigCheckWait stuck (in_alert_since {}s ago)",
                age.as_secs()
            ),
            next_command: Some(format!("nico doctor hbn {dpu_id}")),
            kind: CheckKind::Detail,
        });
    }
    out
}

/// Assemble the six `dpu` layer sub-checks (plus drift / probe-stuck
/// detail lines) from a fleet snapshot. Pure — the caller supplies
/// `now` so the function stays clock-free.
pub fn assemble_checks(
    snapshots: &[DpuSnapshot],
    now: DateTime<Utc>,
    config: &DpuConfig,
) -> Vec<Check> {
    let fleet = snapshots.len();
    let managed_drift = drift_reports(snapshots, now, |s| {
        (
            s.applied_managed_host_config_version.as_str(),
            s.desired_managed_host_config_version.as_str(),
        )
    });
    let instance_drift = drift_reports(snapshots, now, |s| {
        (
            s.applied_instance_network_config_version.as_str(),
            s.desired_instance_network_config_version.as_str(),
        )
    });

    let mut out = Vec::new();
    out.extend(drift_check(
        "drift-managed-host",
        &managed_drift,
        fleet,
        config.drift_managed_host_warn,
        config.drift_managed_host_fail,
    ));
    out.extend(drift_check(
        "drift-instance",
        &instance_drift,
        fleet,
        config.drift_instance_warn,
        config.drift_instance_fail,
    ));
    out.push(cert_check(snapshots, now, config));
    out.push(quarantine_check(snapshots));
    out.push(lost_connection_check(snapshots, now, config));
    out.extend(probe_stuck_check(snapshots, now, config));
    out
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

    // ── empty + single-DPU fleets ────────────────────────────────────────

    #[test]
    fn empty_fleet_emits_six_ok_headlines_and_no_details() {
        let now = Utc::now();
        let checks = assemble_checks(&[], now, &DpuConfig::default());
        let headline_count = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Headline)
            .count();
        assert_eq!(headline_count, 6, "expected 6 headlines, got {headline_count}");
        assert!(
            checks.iter().all(|c| c.kind == CheckKind::Headline),
            "no details expected on empty fleet",
        );
        for name in [
            "drift-managed-host",
            "drift-instance",
            "cert-fleet",
            "quarantine",
            "lost-connection",
            "probe-stuck",
        ] {
            assert_eq!(headline(&checks, name).status, Status::Ok, "{name}");
        }
    }

    #[test]
    fn single_healthy_dpu_emits_only_ok_headlines() {
        let now = Utc::now();
        let s = snap("dpu-1");
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        for name in [
            "drift-managed-host",
            "drift-instance",
            "cert-fleet",
            "quarantine",
            "lost-connection",
            "probe-stuck",
        ] {
            assert_eq!(headline(&checks, name).status, Status::Ok, "{name}");
        }
        assert!(checks.iter().all(|c| c.kind == CheckKind::Headline));
    }

    // ── drift-managed-host ───────────────────────────────────────────────

    #[test]
    fn managed_host_drift_under_warn_threshold_is_ok() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.applied_managed_host_config_version = "v1".into();
        s.desired_managed_host_config_version = "v2".into();
        s.last_seen_at = now - ChronoDuration::seconds(60); // 1m < 15m
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "drift-managed-host").status, Status::Ok);
    }

    #[test]
    fn managed_host_drift_above_warn_threshold_is_warn_with_detail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.applied_managed_host_config_version = "v1".into();
        s.desired_managed_host_config_version = "v2".into();
        s.last_seen_at = now - ChronoDuration::seconds(20 * 60); // 20m > 15m
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "drift-managed-host").status, Status::Warn);
        let d = details(&checks, "drift-managed-host");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].status, Status::Warn);
        assert!(d[0].value.contains("dpu-1"));
        assert_eq!(d[0].next_command.as_deref(), Some("nico doctor hbn dpu-1"));
    }

    #[test]
    fn managed_host_drift_above_fail_threshold_is_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.applied_managed_host_config_version = "v1".into();
        s.desired_managed_host_config_version = "v2".into();
        s.last_seen_at = now - ChronoDuration::seconds(70 * 60); // 70m > 60m
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "drift-managed-host").status, Status::Fail);
        let d = details(&checks, "drift-managed-host");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].status, Status::Fail);
    }

    #[test]
    fn drift_detail_top_n_caps_at_five() {
        let now = Utc::now();
        let snaps: Vec<DpuSnapshot> = (0..10)
            .map(|i| {
                let mut s = snap(&format!("dpu-{i}"));
                s.applied_managed_host_config_version = "v1".into();
                s.desired_managed_host_config_version = "v2".into();
                // Mix ages so older ones rank first.
                s.last_seen_at = now - ChronoDuration::seconds(20 * 60 + i * 60);
                s
            })
            .collect();
        let checks = assemble_checks(&snaps, now, &DpuConfig::default());
        let d = details(&checks, "drift-managed-host");
        assert_eq!(d.len(), DRIFT_DETAIL_TOP_N);
        // Top-N should be the OLDEST (largest age) — sorted desc.
        assert!(d[0].value.contains("dpu-9"), "{}", d[0].value);
    }

    #[test]
    fn drift_managed_and_instance_are_independent_axes() {
        let now = Utc::now();
        // DPU drifts on managed_host (above warn) but instance is aligned.
        let mut s = snap("dpu-1");
        s.applied_managed_host_config_version = "v1".into();
        s.desired_managed_host_config_version = "v2".into();
        s.last_seen_at = now - ChronoDuration::seconds(20 * 60);
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "drift-managed-host").status, Status::Warn);
        assert_eq!(headline(&checks, "drift-instance").status, Status::Ok);
    }

    // ── drift-instance ───────────────────────────────────────────────────

    #[test]
    fn instance_drift_above_warn_threshold_is_warn() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.applied_instance_network_config_version = "v1".into();
        s.desired_instance_network_config_version = "v2".into();
        s.last_seen_at = now - ChronoDuration::seconds(3 * 60); // 3m > 2m
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "drift-instance").status, Status::Warn);
    }

    #[test]
    fn instance_drift_above_fail_threshold_is_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.applied_instance_network_config_version = "v1".into();
        s.desired_instance_network_config_version = "v2".into();
        s.last_seen_at = now - ChronoDuration::seconds(31 * 60);
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "drift-instance").status, Status::Fail);
    }

    // ── cert-fleet ───────────────────────────────────────────────────────

    #[test]
    fn cert_with_no_expiring_certs_is_ok() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.client_certificate_expiry = Some(now + ChronoDuration::days(60));
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "cert-fleet").status, Status::Ok);
    }

    #[test]
    fn cert_under_warn_window_is_warn() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.client_certificate_expiry = Some(now + ChronoDuration::days(20));
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "cert-fleet").status, Status::Warn);
    }

    #[test]
    fn cert_under_fail_window_is_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.client_certificate_expiry = Some(now + ChronoDuration::days(3));
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "cert-fleet").status, Status::Fail);
    }

    // ── quarantine: capped at Warn ──────────────────────────────────────

    #[test]
    fn no_quarantined_dpus_is_ok() {
        let now = Utc::now();
        let s = snap("dpu-1");
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "quarantine").status, Status::Ok);
    }

    #[test]
    fn one_quarantined_dpu_is_warn_not_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.quarantine_state = Some("BlockAllTraffic".into());
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "quarantine").status, Status::Warn);
    }

    #[test]
    fn all_quarantined_fleet_still_yields_warn_never_fail() {
        let now = Utc::now();
        let snaps: Vec<DpuSnapshot> = (0..20)
            .map(|i| {
                let mut s = snap(&format!("dpu-{i}"));
                s.quarantine_state = Some("BlockAllTraffic".into());
                s
            })
            .collect();
        let checks = assemble_checks(&snaps, now, &DpuConfig::default());
        let q = headline(&checks, "quarantine");
        assert_eq!(q.status, Status::Warn);
        assert_ne!(q.status, Status::Fail);
    }

    // ── lost-connection: dual fail rule ─────────────────────────────────

    #[test]
    fn lost_connection_under_warn_threshold_is_ok() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.last_seen_at = now - ChronoDuration::seconds(60);
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "lost-connection").status, Status::Ok);
    }

    #[test]
    fn lost_connection_above_warn_threshold_is_warn() {
        let now = Utc::now();
        // 1/100 silent > 5m: above warn but below the 5%-of-fleet
        // Fail-pct rule and below 30m absolute. Should be Warn.
        let mut snaps: Vec<DpuSnapshot> = (0..99)
            .map(|i| {
                let mut s = snap(&format!("ok-{i}"));
                s.last_seen_at = now - ChronoDuration::seconds(1);
                s
            })
            .collect();
        let mut silent = snap("silent");
        silent.last_seen_at = now - ChronoDuration::seconds(10 * 60);
        snaps.push(silent);
        let checks = assemble_checks(&snaps, now, &DpuConfig::default());
        assert_eq!(headline(&checks, "lost-connection").status, Status::Warn);
    }

    #[test]
    fn lost_connection_absolute_age_over_30m_is_fail() {
        let now = Utc::now();
        // One DPU silent > 30m, in a fleet that's mostly fresh — pct
        // alone wouldn't trip Fail (1/100 = 1%).
        let mut bad = snap("bad");
        bad.last_seen_at = now - ChronoDuration::seconds(31 * 60);
        let mut snaps: Vec<DpuSnapshot> = (0..99)
            .map(|i| {
                let mut s = snap(&format!("dpu-{i}"));
                s.last_seen_at = now - ChronoDuration::seconds(1);
                s
            })
            .collect();
        snaps.push(bad);
        let checks = assemble_checks(&snaps, now, &DpuConfig::default());
        assert_eq!(headline(&checks, "lost-connection").status, Status::Fail);
    }

    #[test]
    fn lost_connection_over_5pct_of_fleet_is_fail_independent_of_age() {
        let now = Utc::now();
        // 10/100 silent > 5m (warn) but none over 30m absolute. Pct
        // rule alone should still trip Fail.
        let mut snaps: Vec<DpuSnapshot> = (0..90)
            .map(|i| {
                let mut s = snap(&format!("ok-{i}"));
                s.last_seen_at = now - ChronoDuration::seconds(1);
                s
            })
            .collect();
        for i in 0..10 {
            let mut s = snap(&format!("silent-{i}"));
            s.last_seen_at = now - ChronoDuration::seconds(10 * 60);
            snaps.push(s);
        }
        let checks = assemble_checks(&snaps, now, &DpuConfig::default());
        assert_eq!(headline(&checks, "lost-connection").status, Status::Fail);
    }

    #[test]
    fn lost_connection_5pct_exactly_does_not_trip_fail_pct_rule() {
        let now = Utc::now();
        // 5/100 silent > 5m, none absolute > 30m. 5% is NOT > 5%.
        let mut snaps: Vec<DpuSnapshot> = (0..95)
            .map(|i| {
                let mut s = snap(&format!("ok-{i}"));
                s.last_seen_at = now - ChronoDuration::seconds(1);
                s
            })
            .collect();
        for i in 0..5 {
            let mut s = snap(&format!("silent-{i}"));
            s.last_seen_at = now - ChronoDuration::seconds(10 * 60);
            snaps.push(s);
        }
        let checks = assemble_checks(&snaps, now, &DpuConfig::default());
        assert_eq!(headline(&checks, "lost-connection").status, Status::Warn);
    }

    // ── parse_health_alerts (issue #239) ────────────────────────────────

    #[test]
    fn parse_health_alerts_returns_empty_for_none() {
        assert!(parse_health_alerts(None).is_empty());
    }

    #[test]
    fn parse_health_alerts_returns_empty_when_alerts_missing() {
        let v = serde_json::json!({"other": "field"});
        assert!(parse_health_alerts(Some(&v)).is_empty());
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
        assert_eq!(
            out[0].in_alert_since,
            Some(
                DateTime::parse_from_rfc3339("2024-01-15T12:34:56Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
        );
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

    #[test]
    fn parse_health_alerts_handles_null_in_alert_since() {
        let v = serde_json::json!({
            "alerts": [
                {"id": "PostConfigCheckWait", "in_alert_since": null}
            ]
        });
        let out = parse_health_alerts(Some(&v));
        assert_eq!(out.len(), 1);
        assert!(out[0].in_alert_since.is_none());
    }

    #[test]
    fn parse_health_alerts_skips_entries_without_id() {
        let v = serde_json::json!({
            "alerts": [
                {"in_alert_since": "2024-01-15T12:34:56Z"},
                {"id": "Good", "in_alert_since": "2024-01-15T12:34:56Z"}
            ]
        });
        let out = parse_health_alerts(Some(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "Good");
    }

    // ── probe-stuck (issue #239) ────────────────────────────────────────

    fn alert(id: &str, in_alert_since: Option<DateTime<Utc>>) -> HealthAlert {
        HealthAlert { id: id.into(), in_alert_since }
    }

    #[test]
    fn probe_stuck_within_grace_window_is_ok() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        // In alert for 10s — under the 30s grace window.
        s.health_alerts = vec![alert(
            "PostConfigCheckWait",
            Some(now - ChronoDuration::seconds(10)),
        )];
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        let h = headline(&checks, "probe-stuck");
        assert_eq!(h.status, Status::Ok);
        assert_eq!(h.value, "0 DPUs with stuck PostConfigCheckWait");
        assert!(details(&checks, "probe-stuck").is_empty());
    }

    #[test]
    fn probe_stuck_past_grace_window_is_fail_with_detail() {
        let now = Utc::now();
        let mut s = snap("dpu-stuck");
        // In alert for 60s — past the 30s grace window.
        s.health_alerts = vec![alert(
            "PostConfigCheckWait",
            Some(now - ChronoDuration::seconds(60)),
        )];
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        let h = headline(&checks, "probe-stuck");
        assert_eq!(h.status, Status::Fail);
        assert_eq!(h.value, "1 DPUs with stuck PostConfigCheckWait");
        let d = details(&checks, "probe-stuck");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].status, Status::Fail);
        assert!(d[0].value.contains("dpu-stuck"));
        assert!(d[0].value.contains("60s"));
        assert_eq!(
            d[0].next_command.as_deref(),
            Some("nico doctor hbn dpu-stuck")
        );
    }

    #[test]
    fn probe_stuck_only_postconfigcheckwait_id_counts() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.health_alerts = vec![
            // Some other probe in alert > grace — should NOT count.
            alert("OtherProbe", Some(now - ChronoDuration::seconds(120))),
        ];
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "probe-stuck").status, Status::Ok);
    }

    #[test]
    fn probe_stuck_alert_with_no_in_alert_since_does_not_count() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        // Alert entry exists but in_alert_since is None — treat as not stuck.
        s.health_alerts = vec![alert("PostConfigCheckWait", None)];
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "probe-stuck").status, Status::Ok);
    }

    #[test]
    fn probe_stuck_cleared_between_reports_is_ok() {
        let now = Utc::now();
        // Acceptance criterion: "probe cleared between reports (pass)".
        // After clear, the alert vec contains no PostConfigCheckWait entry.
        let s = snap("dpu-1");
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "probe-stuck").status, Status::Ok);
    }

    #[test]
    fn probe_stuck_at_exact_grace_threshold_is_ok() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.health_alerts = vec![alert(
            "PostConfigCheckWait",
            Some(now - ChronoDuration::seconds(30)),
        )];
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        // Strict > grace: exactly 30s does NOT trip.
        assert_eq!(headline(&checks, "probe-stuck").status, Status::Ok);
    }

    #[test]
    fn probe_stuck_detail_lines_capped_and_sorted_by_age() {
        let now = Utc::now();
        let snaps: Vec<DpuSnapshot> = (0..10)
            .map(|i| {
                let mut s = snap(&format!("dpu-{i}"));
                s.health_alerts = vec![alert(
                    "PostConfigCheckWait",
                    Some(now - ChronoDuration::seconds(60 + i * 10)),
                )];
                s
            })
            .collect();
        let checks = assemble_checks(&snaps, now, &DpuConfig::default());
        let h = headline(&checks, "probe-stuck");
        assert_eq!(h.status, Status::Fail);
        assert_eq!(h.value, "10 DPUs with stuck PostConfigCheckWait");
        let d = details(&checks, "probe-stuck");
        assert_eq!(d.len(), DRIFT_DETAIL_TOP_N);
        // Oldest first — dpu-9 has age 60+90=150s, the largest.
        assert!(d[0].value.contains("dpu-9"), "{}", d[0].value);
    }

    // ── fetch_fleet schema (PRD-002) ────────────────────────────────────

    /// PRD-002 acceptance: fleet query reads producer-side JSON columns
    /// from the `machines` row, never the (non-existent) old tables.
    #[test]
    fn fetch_fleet_sql_targets_producer_side_machines_columns() {
        let sql = FETCH_FLEET_SQL;
        // Old schema must not be referenced.
        assert!(
            !sql.contains("dpu_network_status"),
            "old table dpu_network_status still referenced: {sql}"
        );
        assert!(
            !sql.contains("dpu_desired_network_config"),
            "old table dpu_desired_network_config still referenced: {sql}"
        );
        // New schema must be referenced.
        assert!(sql.contains("FROM machines"), "missing machines table: {sql}");
        assert!(
            sql.contains("network_status_observation"),
            "missing applied-side JSON column: {sql}"
        );
        assert!(
            sql.contains("network_config_version"),
            "missing desired managed-host version column: {sql}"
        );
        assert!(
            sql.contains("network_config->'quarantine_state'"),
            "quarantine must be read desired-side from network_config: {sql}"
        );
        assert!(
            sql.contains("dpu_agent_health_report"),
            "agent alert JSON column must be read for probe-stuck: {sql}"
        );
    }

    // ── drift threshold boundary ────────────────────────────────────────

    #[test]
    fn drift_at_exact_warn_threshold_is_ok() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.applied_managed_host_config_version = "v1".into();
        s.desired_managed_host_config_version = "v2".into();
        s.last_seen_at = now - ChronoDuration::seconds(15 * 60); // exactly 15m
        let checks = assemble_checks(&[s], now, &DpuConfig::default());
        assert_eq!(headline(&checks, "drift-managed-host").status, Status::Ok);
    }
}
