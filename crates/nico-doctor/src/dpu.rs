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

/// One row in the fleet view — the union of `DpuNetworkStatus` columns
/// and the desired-config peer needed by all five sub-checks.
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
}

/// Read-only seam over the fleet data layer (forgedb + Postgres). The
/// real impl issues one bulk `DpuNetworkStatus` + desired-config join
/// covering every DPU; tests inject mocks. Empty vec means "no DPUs"
/// (or schema absent on dev clusters — we degrade gracefully).
#[async_trait]
pub trait DpuClient: Send + Sync {
    async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>>;
}

/// Default sqlx-backed [`DpuClient`]. Single bulk query joins
/// `dpu_network_status` (latest row per DPU) with
/// `dpu_desired_network_config` to produce one [`DpuSnapshot`] per DPU.
pub struct SqlxDpuClient {
    pool: sqlx::PgPool,
}

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
             WHERE table_name = 'dpu_network_status')",
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
        )> = sqlx::query_as(
            "SELECT DISTINCT ON (s.dpu_id) \
                s.dpu_id, \
                s.applied_managed_host_config_version, \
                s.applied_instance_network_config_version, \
                d.managed_host_config_version, \
                d.instance_network_config_version, \
                s.quarantine_state, \
                s.last_seen_at, \
                s.client_certificate_expiry_unix_epoch_secs \
             FROM dpu_network_status s \
             JOIN dpu_desired_network_config d ON d.dpu_id = s.dpu_id \
             ORDER BY s.dpu_id, s.last_seen_at DESC",
        )
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

/// Assemble the five `dpu` layer sub-checks (plus drift detail lines)
/// from a fleet snapshot. Pure — the caller supplies `now` so the
/// function stays clock-free.
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
    fn empty_fleet_emits_five_ok_headlines_and_no_details() {
        let now = Utc::now();
        let checks = assemble_checks(&[], now, &DpuConfig::default());
        let headline_count = checks
            .iter()
            .filter(|c| c.kind == CheckKind::Headline)
            .count();
        assert_eq!(headline_count, 5, "expected 5 headlines, got {headline_count}");
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
