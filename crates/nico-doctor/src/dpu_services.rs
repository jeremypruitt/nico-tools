//! Per-DPU extension-service inventory verdict — drill-down on
//! `network_status_observation->'extension_service_observation'->'extension_service_statuses'`
//! for a single DPU (PRD-002 / issue #263).
//!
//! Distinct from `dpu_health` because extension services are structured
//! per-service inventory (service_name, version, overall_state, message,
//! removed), not an alert stream — the layer preserves that shape and
//! emits one detail per service rather than collapsing into category
//! buckets.
//!
//! Pure `assemble_checks` over a small [`ServicesSnapshot`]; the
//! [`DpuServicesClient`] trait is the seam over forgedb. Tests inject
//! mocks.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};

/// Default staleness threshold for the extension-service observation.
/// 5m is comfortably longer than the controller's normal observation
/// cadence (DPU agents push every ~30s) so a 5-min gap means the agent
/// has stopped reporting — worth flagging. Configurable via
/// `nico doctor dpu-services --stale`.
pub const DEFAULT_OBSERVATION_STALE_THRESHOLD: Duration = Duration::from_secs(5 * 60);

/// One per-service row narrowed to the fields the verdict reads.
/// `removed` is `Some(reason)` when the service has been marked for
/// removal; we surface the reason verbatim as an info line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub service_name: String,
    pub version: String,
    pub overall_state: String,
    pub message: String,
    pub removed: Option<String>,
}

/// All inputs the `dpu_services` verdict needs for one DPU. Snapshot
/// shape is data-in / checks-out so `assemble_checks` is pure.
#[derive(Debug, Clone)]
pub struct ServicesSnapshot {
    pub dpu_id: String,
    /// `network_status_observation->'extension_service_observation'->>'observed_at'`.
    /// `None` ⇒ observation column was missing the timestamp; staleness
    /// check stays silent.
    pub observed_at: Option<DateTime<Utc>>,
    pub services: Vec<ServiceStatus>,
}

/// Read-only seam over forgedb for the `dpu_services` layer. Returning
/// `Ok(None)` means "no `machines` row for this DPU" — same gentle
/// not-found contract as `hbn` / `dpu_cert` / `dpu_health`.
#[async_trait]
pub trait DpuServicesClient: Send + Sync {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<ServicesSnapshot>>;
}

/// Default sqlx-backed [`DpuServicesClient`]. Reads the
/// `extension_service_observation` JSON sub-object out of
/// `machines.network_status_observation`. Schema-probes the `machines`
/// table and degrades to `Ok(None)` when absent so dev clusters that
/// haven't run the carbide schema render "no machines row" instead of
/// panicking.
pub struct SqlxDpuServicesClient {
    pool: sqlx::PgPool,
}

/// SQL columns the per-DPU snapshot reads. Extracted as constants so
/// the schema choice is pinned by a unit test.
pub(crate) const FETCH_SNAPSHOT_COLS: &str = "\
    m.id, \
    (m.network_status_observation->'extension_service_observation'->>'observed_at')::timestamptz, \
    m.network_status_observation->'extension_service_observation'->'extension_service_statuses'";

pub(crate) const FETCH_SNAPSHOT_FROM: &str = "FROM machines m";

impl SqlxDpuServicesClient {
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
    .map_err(|e| anyhow::anyhow!("dpu_services schema probe failed: {e}"))?;
    Ok(exists.0)
}

#[async_trait]
impl DpuServicesClient for SqlxDpuServicesClient {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<ServicesSnapshot>> {
        if !machines_table_exists(&self.pool).await? {
            return Ok(None);
        }

        let sql = format!(
            "SELECT {FETCH_SNAPSHOT_COLS} {FETCH_SNAPSHOT_FROM} \
             WHERE m.id = $1 LIMIT 1"
        );
        let row: Option<(String, Option<DateTime<Utc>>, Option<serde_json::Value>)> =
            sqlx::query_as(&sql)
                .bind(dpu_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| anyhow::anyhow!("dpu_services snapshot query failed: {e}"))?;

        let Some((id, observed_at, statuses_blob)) = row else {
            return Ok(None);
        };

        Ok(Some(ServicesSnapshot {
            dpu_id: id,
            observed_at,
            services: parse_services(statuses_blob.as_ref()),
        }))
    }
}

/// Extract [`ServiceStatus`] rows from the
/// `extension_service_statuses` JSON array. Tolerates the column being
/// NULL or non-array. Missing string fields default to `""`.
pub fn parse_services(blob: Option<&serde_json::Value>) -> Vec<ServiceStatus> {
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
                .and_then(|s| s.as_str())
                .unwrap_or("Unknown")
                .to_owned();
            let message = entry
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_owned();
            let removed = entry
                .get("removed")
                .and_then(|r| r.as_str())
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

/// Classify one `overall_state` value into the verdict bucket.
/// `Failed` / `Error` are unconditional warns; `Pending` / `Unknown`
/// warn only after the observation is older than the threshold (the
/// controller's lens on "transient" is "we haven't seen progress in a
/// while"). `Running` is healthy. `Terminating` / `Terminated` are
/// lifecycle states — informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateBucket {
    Healthy,
    Lifecycle,
    Transient,
    Bad,
}

fn classify_state(state: &str) -> StateBucket {
    match state {
        "Running" => StateBucket::Healthy,
        "Terminating" | "Terminated" => StateBucket::Lifecycle,
        "Pending" | "Unknown" => StateBucket::Transient,
        // Failed, Error, or any unrecognized value → bad.
        _ => StateBucket::Bad,
    }
}

/// Assemble the per-DPU `dpu_services` check list from a snapshot.
///
/// Headline summarises the worst-of across staleness and per-service
/// state. Detail bullets are emitted in this order:
///
/// 1. Stale-observation warning (when `observed_at` is older than
///    `stale_threshold`).
/// 2. One detail per service, sorted by service_name. Bucket maps to
///    Status: Bad → Warn, Transient (only if stale) → Warn, Lifecycle
///    → Ok (info-only), Healthy → no detail emitted, removed → Ok
///    (info-only with reason).
///
/// Pure — no I/O, no clock reads (caller supplies `now`).
pub fn assemble_checks(
    snapshot: &ServicesSnapshot,
    now: DateTime<Utc>,
    stale_threshold: Duration,
) -> Vec<Check> {
    let mut details: Vec<Check> = Vec::new();

    let observation_is_stale = match snapshot.observed_at {
        Some(ts) => (now - ts).to_std().unwrap_or(Duration::ZERO) > stale_threshold,
        None => false,
    };

    if let Some(ts) = snapshot.observed_at
        && observation_is_stale
    {
        let age = (now - ts).to_std().unwrap_or(Duration::ZERO);
        details.push(Check {
            name: "observation_stale",
            status: Status::Warn,
            value: format!(
                "extension_service observation {}s old (threshold {}s)",
                age.as_secs(),
                stale_threshold.as_secs()
            ),
            next_command: Some(format!(
                "nico correlate {} # last activity for this DPU",
                snapshot.dpu_id
            )),
            kind: CheckKind::Detail,
        });
    }

    let mut sorted: Vec<&ServiceStatus> = snapshot.services.iter().collect();
    sorted.sort_by(|a, b| a.service_name.cmp(&b.service_name));

    for svc in sorted {
        if let Some(reason) = svc.removed.as_deref() {
            details.push(removed_check(svc, reason));
            continue;
        }
        match classify_state(&svc.overall_state) {
            StateBucket::Healthy => {}
            StateBucket::Lifecycle => details.push(lifecycle_check(svc)),
            StateBucket::Transient => {
                if observation_is_stale {
                    details.push(non_running_check(svc, Status::Warn));
                }
            }
            StateBucket::Bad => details.push(non_running_check(svc, Status::Warn)),
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
        name: "dpu_services",
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
        name: "dpu_services",
        status: Status::Unknown,
        value: format!("dpu_services data layer error for {dpu_id}: {err}"),
        next_command: Some("check forgedb / postgres connectivity".to_string()),
        kind: CheckKind::Headline,
    }]
}

fn non_running_check(svc: &ServiceStatus, status: Status) -> Check {
    let message = if svc.message.is_empty() {
        String::new()
    } else {
        format!(": {}", svc.message)
    };
    Check {
        name: "service_state",
        status,
        value: format!(
            "{} v{} state={}{message}",
            svc.service_name, svc.version, svc.overall_state
        ),
        next_command: Some(format!(
            "nico correlate {} # trace this service",
            svc.service_name
        )),
        kind: CheckKind::Detail,
    }
}

fn lifecycle_check(svc: &ServiceStatus) -> Check {
    Check {
        name: "service_lifecycle",
        status: Status::Ok,
        value: format!(
            "{} v{} {} (lifecycle)",
            svc.service_name, svc.version, svc.overall_state
        ),
        next_command: None,
        kind: CheckKind::Detail,
    }
}

fn removed_check(svc: &ServiceStatus, reason: &str) -> Check {
    Check {
        name: "service_removed",
        status: Status::Ok,
        value: format!("{} v{} removed: {reason}", svc.service_name, svc.version),
        next_command: None,
        kind: CheckKind::Detail,
    }
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

fn headline_check(snapshot: &ServicesSnapshot, aggregate: Status, details: &[Check]) -> Check {
    let value = match aggregate {
        Status::Ok => format!(
            "dpu {} services healthy ({} reported)",
            snapshot.dpu_id,
            snapshot.services.len()
        ),
        Status::Skipped => format!("dpu {} services skipped", snapshot.dpu_id),
        Status::Unknown => format!("dpu {} services unknown", snapshot.dpu_id),
        Status::Warn | Status::Fail => {
            let bad: Vec<&str> = details
                .iter()
                .filter(|c| matches!(c.status, Status::Warn | Status::Fail))
                .map(|c| c.name)
                .collect();
            format!("dpu {} services issues: {}", snapshot.dpu_id, bad.join(", "))
        }
    };
    Check {
        name: "dpu_services",
        status: aggregate,
        value,
        next_command: None,
        kind: CheckKind::Headline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_empty() -> ServicesSnapshot {
        ServicesSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: Some(Utc::now()),
            services: vec![],
        }
    }

    fn svc(name: &str, state: &str) -> ServiceStatus {
        ServiceStatus {
            service_name: name.into(),
            version: "1.0.0".into(),
            overall_state: state.into(),
            message: String::new(),
            removed: None,
        }
    }

    // ── parse_services ────────────────────────────────────────────────────

    #[test]
    fn parse_services_returns_empty_when_blob_absent() {
        assert!(parse_services(None).is_empty());
    }

    #[test]
    fn parse_services_returns_empty_when_blob_not_array() {
        let v = serde_json::json!({"oops": "not an array"});
        assert!(parse_services(Some(&v)).is_empty());
    }

    #[test]
    fn parse_services_extracts_all_documented_fields() {
        let v = serde_json::json!([
            {
                "service_name": "doca-bfb",
                "version": "2.5.0",
                "overall_state": "Running",
                "message": "ok",
                "removed": null
            },
            {
                "service_name": "doca-telemetry",
                "version": "2.4.0",
                "overall_state": "Failed",
                "message": "container restart",
                "removed": "operator marked for removal"
            }
        ]);
        let out = parse_services(Some(&v));
        assert_eq!(out.len(), 2);

        assert_eq!(out[0].service_name, "doca-bfb");
        assert_eq!(out[0].version, "2.5.0");
        assert_eq!(out[0].overall_state, "Running");
        assert_eq!(out[0].message, "ok");
        assert_eq!(out[0].removed, None);

        assert_eq!(out[1].service_name, "doca-telemetry");
        assert_eq!(out[1].overall_state, "Failed");
        assert_eq!(
            out[1].removed.as_deref(),
            Some("operator marked for removal"),
        );
    }

    #[test]
    fn parse_services_skips_entries_without_service_name() {
        let v = serde_json::json!([
            {"version": "1.0", "overall_state": "Running"},
            {"service_name": "kept", "overall_state": "Running"}
        ]);
        let out = parse_services(Some(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].service_name, "kept");
    }

    #[test]
    fn parse_services_defaults_overall_state_to_unknown_when_missing() {
        let v = serde_json::json!([{"service_name": "svc1"}]);
        let out = parse_services(Some(&v));
        assert_eq!(out[0].overall_state, "Unknown");
    }

    // ── classify_state ────────────────────────────────────────────────────

    #[test]
    fn classify_state_maps_known_states_to_correct_buckets() {
        let cases = [
            ("Running", StateBucket::Healthy),
            ("Terminating", StateBucket::Lifecycle),
            ("Terminated", StateBucket::Lifecycle),
            ("Pending", StateBucket::Transient),
            ("Unknown", StateBucket::Transient),
            ("Failed", StateBucket::Bad),
            ("Error", StateBucket::Bad),
            ("WeirdNewState", StateBucket::Bad),
        ];
        for (state, expected) in cases {
            assert_eq!(classify_state(state), expected, "state={state}");
        }
    }

    // ── assemble_checks: empty + healthy ──────────────────────────────────

    #[test]
    fn empty_snapshot_yields_single_ok_headline_no_details() {
        let snap = snap_empty();
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].value.contains("dpu-42"));
        assert!(checks[0].value.contains("healthy"));
    }

    #[test]
    fn all_running_services_yield_ok_headline_no_details() {
        let mut snap = snap_empty();
        snap.services = vec![svc("doca-bfb", "Running"), svc("doca-telemetry", "Running")];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert_eq!(checks.len(), 1, "no details for healthy services");
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].value.contains("2 reported"));
    }

    // ── per-service state verdicts ────────────────────────────────────────

    #[test]
    fn failed_service_emits_warn_detail() {
        let mut snap = snap_empty();
        let mut s = svc("doca-telemetry", "Failed");
        s.message = "container exited with status 1".into();
        snap.services = vec![s];

        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);

        let detail = checks
            .iter()
            .find(|c| c.kind == CheckKind::Detail)
            .expect("expected service_state detail");
        assert_eq!(detail.name, "service_state");
        assert_eq!(detail.status, Status::Warn);
        assert!(detail.value.contains("doca-telemetry"));
        assert!(detail.value.contains("Failed"));
        assert!(detail.value.contains("container exited"));

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(headline.status, Status::Warn);
        assert!(headline.value.contains("dpu-42"));
    }

    #[test]
    fn error_service_emits_warn_detail() {
        let mut snap = snap_empty();
        snap.services = vec![svc("doca-x", "Error")];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        let detail = checks
            .iter()
            .find(|c| c.kind == CheckKind::Detail)
            .unwrap();
        assert_eq!(detail.status, Status::Warn);
        assert!(detail.value.contains("Error"));
    }

    #[test]
    fn pending_service_silent_when_observation_fresh() {
        let now = Utc::now();
        let mut snap = snap_empty();
        snap.observed_at = Some(now - chrono::Duration::seconds(30));
        snap.services = vec![svc("doca-x", "Pending")];
        let checks = assemble_checks(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        // Pending + fresh observation ⇒ silent (transient)
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Ok);
    }

    #[test]
    fn pending_service_warns_when_observation_stale() {
        let now = Utc::now();
        let mut snap = snap_empty();
        snap.observed_at = Some(now - chrono::Duration::minutes(10));
        snap.services = vec![svc("doca-x", "Pending")];
        let checks = assemble_checks(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        // Pending + stale observation ⇒ warn detail (stuck) plus the
        // observation_stale warn detail itself.
        let states: Vec<&Check> = checks
            .iter()
            .filter(|c| c.name == "service_state")
            .collect();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].status, Status::Warn);
    }

    #[test]
    fn terminating_service_emits_info_only_lifecycle_detail() {
        let mut snap = snap_empty();
        snap.services = vec![svc("doca-x", "Terminating")];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        let detail = checks
            .iter()
            .find(|c| c.kind == CheckKind::Detail)
            .expect("expected lifecycle detail");
        assert_eq!(detail.name, "service_lifecycle");
        assert_eq!(detail.status, Status::Ok);
        assert!(detail.value.contains("Terminating"));

        // Headline stays Ok — lifecycle is info, not warn.
        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(headline.status, Status::Ok);
    }

    #[test]
    fn removed_flag_emits_info_only_detail_with_reason() {
        let mut snap = snap_empty();
        let mut s = svc("doca-x", "Running");
        s.removed = Some("operator decommissioned".into());
        snap.services = vec![s];

        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        let detail = checks
            .iter()
            .find(|c| c.kind == CheckKind::Detail)
            .expect("expected removed detail");
        assert_eq!(detail.name, "service_removed");
        assert_eq!(detail.status, Status::Ok);
        assert!(detail.value.contains("operator decommissioned"));

        let headline = checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert_eq!(headline.status, Status::Ok);
    }

    // ── stale observation ─────────────────────────────────────────────────

    #[test]
    fn stale_observation_emits_warn_detail() {
        let now = Utc::now();
        let mut snap = snap_empty();
        snap.observed_at = Some(now - chrono::Duration::minutes(10));
        let checks = assemble_checks(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);

        let stale = checks
            .iter()
            .find(|c| c.name == "observation_stale")
            .expect("expected observation_stale detail");
        assert_eq!(stale.status, Status::Warn);
        assert!(stale.value.contains("threshold"));
    }

    #[test]
    fn fresh_observation_silent() {
        let now = Utc::now();
        let mut snap = snap_empty();
        snap.observed_at = Some(now - chrono::Duration::seconds(30));
        let checks = assemble_checks(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert!(checks.iter().all(|c| c.name != "observation_stale"));
    }

    #[test]
    fn unknown_observed_at_silent() {
        let mut snap = snap_empty();
        snap.observed_at = None;
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert!(checks.iter().all(|c| c.name != "observation_stale"));
    }

    #[test]
    fn custom_stale_threshold_changes_classification_boundary() {
        let now = Utc::now();
        let mut snap = snap_empty();
        snap.observed_at = Some(now - chrono::Duration::minutes(2));
        // Default 5m ⇒ fresh
        let default_checks = assemble_checks(&snap, now, DEFAULT_OBSERVATION_STALE_THRESHOLD);
        assert!(
            default_checks.iter().all(|c| c.name != "observation_stale"),
            "expected fresh under default 5m"
        );
        // Tighter 1m ⇒ stale
        let tight_checks = assemble_checks(&snap, now, Duration::from_secs(60));
        assert!(
            tight_checks.iter().any(|c| c.name == "observation_stale"),
            "expected stale under 1m threshold"
        );
    }

    // ── ordering ──────────────────────────────────────────────────────────

    #[test]
    fn details_sorted_by_service_name() {
        let mut snap = snap_empty();
        snap.services = vec![
            svc("zeta", "Failed"),
            svc("alpha", "Failed"),
            svc("mu", "Failed"),
        ];
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_OBSERVATION_STALE_THRESHOLD);
        let names: Vec<&str> = checks
            .iter()
            .filter(|c| c.name == "service_state")
            .map(|c| {
                if c.value.contains("alpha") { "alpha" }
                else if c.value.contains("mu") { "mu" }
                else if c.value.contains("zeta") { "zeta" }
                else { "?" }
            })
            .collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
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
    fn fetch_snapshot_sql_targets_extension_service_observation_path() {
        let cols = FETCH_SNAPSHOT_COLS;
        let from = FETCH_SNAPSHOT_FROM;
        let combined = format!("{cols} {from}");

        assert!(
            !combined.contains("dpu_network_status"),
            "old table dpu_network_status referenced: {combined}"
        );
        assert!(
            from.contains("FROM machines"),
            "must read from machines table: {from}"
        );
        assert!(
            cols.contains("network_status_observation"),
            "must read network_status_observation: {cols}"
        );
        assert!(
            cols.contains("extension_service_observation"),
            "must read extension_service_observation: {cols}"
        );
        assert!(
            cols.contains("extension_service_statuses"),
            "must read extension_service_statuses: {cols}"
        );
        assert!(
            cols.contains("observed_at"),
            "must read observed_at: {cols}"
        );
    }
}
