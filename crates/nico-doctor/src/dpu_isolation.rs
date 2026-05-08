//! DPU isolation verdict — distinguishes the three reasons a DPU might
//! have no traffic so the operator does not have to triage them by hand:
//!
//! - `not-yet-known` — machine row absent, or scout discovery has not
//!   completed for it. We have nothing to quarantine or measure freshness
//!   against.
//! - `quarantined` — `MachineQuarantineState` is set (e.g.
//!   `BlockAllTraffic`). Deliberate.
//! - `lost-connection` — most recent `DpuNetworkStatus.last_seen_at` is
//!   older than the freshness threshold (or absent entirely).
//! - `healthy` — none of the above.
//!
//! Precedence (mutually exclusive): `not-yet-known` > `quarantined` >
//! `lost-connection` > `healthy`. Pure `assess()` over a small
//! [`IsolationSnapshot`] keeps the logic testable without touching
//! Postgres; the [`DpuIsolationClient`] trait is the seam.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};

/// Default `last_seen_at` freshness window before we declare the DPU
/// `lost-connection`. Matches the HBN verdict default so the two
/// commands tell the same story about the same DPU.
pub const DEFAULT_FRESHNESS_THRESHOLD: Duration = Duration::from_secs(90);

/// All four data points the verdict needs, fetched as one snapshot so
/// the assessment is pure.
#[derive(Debug, Clone)]
pub struct IsolationSnapshot {
    pub machine_id: String,
    /// `machines.id` exists in forgedb. False ⇒ the operator typed an
    /// ID we have never seen.
    pub registered: bool,
    /// Scout discovery has completed for this machine — i.e. there is
    /// enough state to evaluate quarantine / freshness signals against.
    /// False before scout runs, true after.
    pub scout_discovery_complete: bool,
    /// `MachineQuarantineState` for this machine, e.g.
    /// `Some("BlockAllTraffic")`. `None` ⇒ not quarantined.
    pub quarantine_state: Option<String>,
    /// Most recent `DpuNetworkStatus.last_seen_at`. `None` ⇒ no row
    /// has ever been observed (treated as `lost-connection` for a
    /// registered + scouted DPU).
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// Read-only seam over the isolation data layer (machines /
/// scout-discovery / MachineQuarantineState / DpuNetworkStatus). The
/// real impl issues the canonical queries against forgedb and degrades
/// soft when the schema is absent (returning `not-yet-known`); tests
/// inject mocks.
#[async_trait]
pub trait DpuIsolationClient: Send + Sync {
    async fn fetch_snapshot(&self, machine_id: &str) -> Result<IsolationSnapshot>;
}

/// Default sqlx-backed [`DpuIsolationClient`].
///
/// Follows the same schema-probe-and-degrade pattern as
/// [`crate::hbn::SqlxHbnClient`]: when `dpu_network_status` is absent
/// (carbide drift, #213), every machine reports `not-yet-known` rather
/// than crashing. The tracer-bullet query reads `quarantine_state` and
/// `last_seen_at` from the most recent row; presence of the row implies
/// registration + scout completion. Once the carbide port (#213) lands,
/// this becomes four separate queries against `machines`,
/// scout-discovery state, `MachineQuarantineState`, and
/// `dpu_network_status`.
pub struct SqlxDpuIsolationClient {
    pool: sqlx::PgPool,
}

impl SqlxDpuIsolationClient {
    pub fn new(url: &str) -> Result<Self> {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_lazy(url)
            .map_err(|e| anyhow::anyhow!("invalid postgres URL: {e}"))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl DpuIsolationClient for SqlxDpuIsolationClient {
    async fn fetch_snapshot(&self, machine_id: &str) -> Result<IsolationSnapshot> {
        let exists: (bool,) = sqlx::query_as(
            "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
             WHERE table_name = 'dpu_network_status')",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("dpu_isolation schema probe failed: {e}"))?;

        if !exists.0 {
            return Ok(IsolationSnapshot {
                machine_id: machine_id.to_string(),
                registered: false,
                scout_discovery_complete: false,
                quarantine_state: None,
                last_seen_at: None,
            });
        }

        let row: Option<(Option<String>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT s.quarantine_state, s.last_seen_at \
             FROM dpu_network_status s \
             WHERE s.dpu_id = $1 \
             ORDER BY s.last_seen_at DESC \
             LIMIT 1",
        )
        .bind(machine_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| anyhow::anyhow!("dpu_isolation query failed: {e}"))?;

        Ok(match row {
            Some((quarantine_state, last_seen_at)) => IsolationSnapshot {
                machine_id: machine_id.to_string(),
                registered: true,
                scout_discovery_complete: true,
                quarantine_state,
                last_seen_at: Some(last_seen_at),
            },
            None => IsolationSnapshot {
                machine_id: machine_id.to_string(),
                registered: false,
                scout_discovery_complete: false,
                quarantine_state: None,
                last_seen_at: None,
            },
        })
    }
}

/// The four possible verdicts. `last_seen_age` is included on the
/// healthy / lost-connection variants so the operator sees the actual
/// number, not just a category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsolationVerdict {
    NotYetKnown { reason: NotYetKnownReason },
    Quarantined { state: String },
    LostConnection { last_seen_age: Option<Duration>, threshold: Duration },
    Healthy { last_seen_age: Duration },
}

/// Why a DPU is `not-yet-known`. Two distinct sub-cases the operator
/// should be able to tell apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotYetKnownReason {
    NotRegistered,
    ScoutDiscoveryIncomplete,
}

/// Run the precedence ladder over a snapshot. `now` is the caller's
/// clock so this stays pure.
pub fn assess(
    snapshot: &IsolationSnapshot,
    now: DateTime<Utc>,
    freshness_threshold: Duration,
) -> IsolationVerdict {
    if !snapshot.registered {
        return IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::NotRegistered,
        };
    }
    if !snapshot.scout_discovery_complete {
        return IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::ScoutDiscoveryIncomplete,
        };
    }
    if let Some(state) = snapshot.quarantine_state.as_deref()
        && !state.is_empty()
    {
        return IsolationVerdict::Quarantined {
            state: state.to_string(),
        };
    }
    let age = snapshot
        .last_seen_at
        .map(|t| (now - t).to_std().unwrap_or(Duration::ZERO));
    match age {
        Some(a) if a <= freshness_threshold => {
            IsolationVerdict::Healthy { last_seen_age: a }
        }
        _ => IsolationVerdict::LostConnection {
            last_seen_age: age,
            threshold: freshness_threshold,
        },
    }
}

/// Render the verdict as a single headline [`Check`]. The doctor
/// formatter already paints the per-status colour and the
/// next-command hint, so the verdict is self-contained: one line, no
/// detail bullets.
pub fn assemble_checks(machine_id: &str, verdict: &IsolationVerdict) -> Vec<Check> {
    let (status, value, next_command) = match verdict {
        IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::NotRegistered,
        } => (
            Status::Unknown,
            format!("dpu {machine_id} not-yet-known: no machines row"),
            Some(format!(
                "nico correlate {machine_id} # confirm the ID is correct"
            )),
        ),
        IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::ScoutDiscoveryIncomplete,
        } => (
            Status::Unknown,
            format!("dpu {machine_id} not-yet-known: scout discovery has not completed"),
            Some(format!(
                "nico correlate {machine_id} # last scout activity"
            )),
        ),
        IsolationVerdict::Quarantined { state } => (
            Status::Fail,
            format!("dpu {machine_id} quarantined: {state}"),
            Some(format!(
                "nico correlate {machine_id} # see why this DPU was quarantined"
            )),
        ),
        IsolationVerdict::LostConnection {
            last_seen_age,
            threshold,
        } => (
            Status::Fail,
            match last_seen_age {
                Some(age) => format!(
                    "dpu {machine_id} lost-connection: last seen {}s ago (threshold {}s)",
                    age.as_secs(),
                    threshold.as_secs()
                ),
                None => format!(
                    "dpu {machine_id} lost-connection: no DpuNetworkStatus row ever observed"
                ),
            },
            Some(format!(
                "nico correlate {machine_id} # check DPU agent connectivity"
            )),
        ),
        IsolationVerdict::Healthy { last_seen_age } => (
            Status::Ok,
            format!(
                "dpu {machine_id} healthy: last seen {}s ago",
                last_seen_age.as_secs()
            ),
            None,
        ),
    };
    vec![Check {
        name: "dpu_isolation",
        status,
        value,
        next_command,
        kind: CheckKind::Headline,
    }]
}

/// Render a data-layer error as an `Unknown` headline so the verdict
/// surfaces the underlying message verbatim.
pub fn assemble_error_checks(machine_id: &str, err: &str) -> Vec<Check> {
    vec![Check {
        name: "dpu_isolation",
        status: Status::Unknown,
        value: format!("dpu_isolation data layer error for {machine_id}: {err}"),
        next_command: Some("check forgedb / postgres connectivity".to_string()),
        kind: CheckKind::Headline,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_healthy() -> IsolationSnapshot {
        IsolationSnapshot {
            machine_id: "machine-42".into(),
            registered: true,
            scout_discovery_complete: true,
            quarantine_state: None,
            last_seen_at: Some(Utc::now()),
        }
    }

    #[test]
    fn healthy_machine_yields_healthy_verdict() {
        let snap = snap_healthy();
        let now = snap.last_seen_at.unwrap();
        let verdict = assess(&snap, now, DEFAULT_FRESHNESS_THRESHOLD);
        match verdict {
            IsolationVerdict::Healthy { last_seen_age } => {
                assert!(last_seen_age <= Duration::from_secs(1));
            }
            other => panic!("expected Healthy, got {other:?}"),
        }
    }

    #[test]
    fn unregistered_machine_yields_not_yet_known_not_registered() {
        let mut snap = snap_healthy();
        snap.registered = false;
        snap.scout_discovery_complete = false;
        snap.last_seen_at = None;
        let verdict = assess(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            verdict,
            IsolationVerdict::NotYetKnown {
                reason: NotYetKnownReason::NotRegistered,
            }
        );
    }

    #[test]
    fn stale_last_seen_yields_lost_connection_with_age_and_threshold() {
        let mut snap = snap_healthy();
        let now = Utc::now();
        snap.last_seen_at = Some(now - chrono::Duration::seconds(180));
        let threshold = Duration::from_secs(90);
        let verdict = assess(&snap, now, threshold);
        match verdict {
            IsolationVerdict::LostConnection { last_seen_age, threshold: t } => {
                assert_eq!(t, threshold);
                let age = last_seen_age.expect("age should be present");
                assert!((175..=185).contains(&age.as_secs()), "age was {}s", age.as_secs());
            }
            other => panic!("expected LostConnection, got {other:?}"),
        }
    }

    #[test]
    fn no_last_seen_row_for_registered_scouted_machine_is_lost_connection() {
        let mut snap = snap_healthy();
        snap.last_seen_at = None;
        let verdict = assess(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        match verdict {
            IsolationVerdict::LostConnection { last_seen_age, .. } => {
                assert!(last_seen_age.is_none());
            }
            other => panic!("expected LostConnection, got {other:?}"),
        }
    }

    #[test]
    fn quarantined_machine_yields_quarantined_verdict_with_state() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        let verdict = assess(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            verdict,
            IsolationVerdict::Quarantined {
                state: "BlockAllTraffic".into(),
            }
        );
    }

    #[test]
    fn empty_string_quarantine_state_is_treated_as_not_quarantined() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("".into());
        let verdict = assess(&snap, snap.last_seen_at.unwrap(), DEFAULT_FRESHNESS_THRESHOLD);
        assert!(matches!(verdict, IsolationVerdict::Healthy { .. }));
    }

    #[test]
    fn registered_but_scout_incomplete_yields_not_yet_known_scout() {
        let mut snap = snap_healthy();
        snap.scout_discovery_complete = false;
        snap.last_seen_at = None;
        let verdict = assess(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            verdict,
            IsolationVerdict::NotYetKnown {
                reason: NotYetKnownReason::ScoutDiscoveryIncomplete,
            }
        );
    }

    // ── precedence: which signal wins when several apply ─────────────────

    #[test]
    fn unregistered_beats_quarantined_and_stale() {
        let mut snap = snap_healthy();
        snap.registered = false;
        snap.scout_discovery_complete = false;
        snap.quarantine_state = Some("BlockAllTraffic".into());
        snap.last_seen_at = Some(Utc::now() - chrono::Duration::hours(1));
        let verdict = assess(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            verdict,
            IsolationVerdict::NotYetKnown {
                reason: NotYetKnownReason::NotRegistered,
            }
        );
    }

    #[test]
    fn scout_incomplete_beats_quarantined_and_stale() {
        let mut snap = snap_healthy();
        snap.scout_discovery_complete = false;
        snap.quarantine_state = Some("BlockAllTraffic".into());
        snap.last_seen_at = Some(Utc::now() - chrono::Duration::hours(1));
        let verdict = assess(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            verdict,
            IsolationVerdict::NotYetKnown {
                reason: NotYetKnownReason::ScoutDiscoveryIncomplete,
            }
        );
    }

    #[test]
    fn quarantined_beats_stale_last_seen() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        snap.last_seen_at = Some(Utc::now() - chrono::Duration::hours(1));
        let verdict = assess(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);
        assert_eq!(
            verdict,
            IsolationVerdict::Quarantined {
                state: "BlockAllTraffic".into(),
            }
        );
    }

    // ── assemble_checks: rendering each verdict as a headline ────────────

    #[test]
    fn healthy_check_is_single_ok_headline_no_next_command() {
        let snap = snap_healthy();
        let verdict = assess(&snap, snap.last_seen_at.unwrap(), DEFAULT_FRESHNESS_THRESHOLD);
        let checks = assemble_checks(&snap.machine_id, &verdict);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].value.contains("machine-42"));
        assert!(checks[0].value.contains("healthy"));
        assert!(checks[0].next_command.is_none());
    }

    #[test]
    fn quarantined_check_is_fail_with_state_and_correlate_hint() {
        let verdict = IsolationVerdict::Quarantined {
            state: "BlockAllTraffic".into(),
        };
        let checks = assemble_checks("machine-42", &verdict);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].value.contains("BlockAllTraffic"));
        assert!(checks[0].value.contains("quarantined"));
        assert!(checks[0]
            .next_command
            .as_deref()
            .unwrap()
            .contains("nico correlate"));
    }

    #[test]
    fn lost_connection_check_includes_age_and_threshold_seconds() {
        let verdict = IsolationVerdict::LostConnection {
            last_seen_age: Some(Duration::from_secs(180)),
            threshold: Duration::from_secs(90),
        };
        let checks = assemble_checks("machine-42", &verdict);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].value.contains("180s"));
        assert!(checks[0].value.contains("90s"));
    }

    #[test]
    fn lost_connection_check_handles_no_last_seen_row() {
        let verdict = IsolationVerdict::LostConnection {
            last_seen_age: None,
            threshold: Duration::from_secs(90),
        };
        let checks = assemble_checks("machine-42", &verdict);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].value.contains("no DpuNetworkStatus"));
    }

    #[test]
    fn not_yet_known_unregistered_check_is_unknown_with_distinct_reason() {
        let verdict = IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::NotRegistered,
        };
        let checks = assemble_checks("machine-42", &verdict);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("not-yet-known"));
        assert!(checks[0].value.contains("machines row"));
    }

    #[test]
    fn not_yet_known_scout_incomplete_check_distinguishes_from_unregistered() {
        let verdict = IsolationVerdict::NotYetKnown {
            reason: NotYetKnownReason::ScoutDiscoveryIncomplete,
        };
        let checks = assemble_checks("machine-42", &verdict);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("scout"));
    }

    #[test]
    fn assemble_error_checks_surfaces_underlying_error() {
        let checks = assemble_error_checks("machine-42", "postgres unreachable");
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("postgres unreachable"));
        assert!(checks[0].value.contains("machine-42"));
    }
}
