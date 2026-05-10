//! DPU isolation verdict — distinguishes the four reasons a DPU might
//! have no traffic so the operator does not have to triage them by hand:
//!
//! - `not-yet-known` — `machines` row absent, or no
//!   `network_status_observation` has been written for it. Nothing to
//!   quarantine or measure freshness against.
//! - `quarantine-requested` — `network_config->'quarantine_state'->>'mode'`
//!   is set (e.g. `BlockAllTraffic`). Operator intent (desired-side), not
//!   observed effect — see PRD-002.
//! - `lost-connection` — `network_status_observation->>'observed_at'` is
//!   older than the freshness threshold (or the observation row is
//!   absent entirely).
//! - `healthy` — none of the above.
//!
//! Precedence (mutually exclusive): `not-yet-known` >
//! `quarantine-requested` > `lost-connection` > `healthy`. Pure
//! `assess()` over a small [`IsolationSnapshot`] keeps the logic
//! testable without touching Postgres; the [`DpuIsolationClient`] trait
//! is the seam.
//!
//! Since PRD-003 Slice 2 (#306) the verdict precedence's `AxisSummary`
//! shape lives in the shared [`crate::verdicts::isolation_verdict`]
//! primitive (returning an [`crate::verdicts::AxisSummary`] that
//! downstream holistic rollups consume); [`assemble_checks`] here is
//! the per-layer renderer that turns that summary into a headline
//! `Check` plus isolation-specific detail rows (quarantine state,
//! last-seen timestamp, freshness threshold echo). The
//! [`DpuIsolationClient`] trait remains the I/O seam.

use std::time::Duration;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::output::Status;

use crate::layer::{Check, CheckKind};
use crate::verdicts::{isolation_verdict, AxisSummary};

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
    /// `network_status_observation` has been written for this machine —
    /// i.e. the agent has reported at least once, so quarantine /
    /// freshness signals are evaluable. False before the first
    /// observation lands, true after.
    pub scout_discovery_complete: bool,
    /// Desired-side quarantine mode pulled from
    /// `network_config->'quarantine_state'->>'mode'`, e.g.
    /// `Some("BlockAllTraffic")`. `None` ⇒ no quarantine requested.
    /// Operator intent — not observed effect — per PRD-002.
    pub quarantine_state: Option<String>,
    /// `network_status_observation->>'observed_at'`. `None` ⇒ no
    /// observation has ever landed (treated as `lost-connection` for a
    /// registered + observed DPU).
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// Read-only seam over forgedb for the `dpu_isolation` layer. The real
/// impl reads the producer-side `machines` row (PRD-002) and degrades
/// soft when the schema is absent (returning `not-yet-known`); tests
/// inject mocks.
#[async_trait]
pub trait DpuIsolationClient: Send + Sync {
    async fn fetch_snapshot(&self, machine_id: &str) -> Result<IsolationSnapshot>;
}

/// Default sqlx-backed [`DpuIsolationClient`]. Reads:
///
/// - `machines.id` — presence ⇒ registered.
/// - `machines.network_config->'quarantine_state'->>'mode'` — desired-side
///   quarantine intent (PRD-002). The companion
///   `network_status_observation` axis would record observed effect, but
///   the layer reports intent: "what did the operator ask for".
/// - `(machines.network_status_observation->>'observed_at')::timestamptz`
///   — most recent observation timestamp; presence ⇒ the agent has
///   reported at least once.
///
/// Schema-probes the `machines` table and degrades to `not-yet-known`
/// when absent so dev clusters that haven't run the carbide schema
/// render the gentle Unknown verdict instead of crashing.
pub struct SqlxDpuIsolationClient {
    pool: sqlx::PgPool,
}

/// SQL columns the snapshot reads. Extracted as a constant so the
/// schema choice (which producer-side JSON paths the layer reads) is
/// pinned by a unit test, even though we can't exercise the full query
/// without a live postgres.
pub(crate) const FETCH_SNAPSHOT_COLS: &str = "\
    m.id, \
    m.network_config->'quarantine_state'->>'mode', \
    (m.network_status_observation->>'observed_at')::timestamptz";

pub(crate) const FETCH_SNAPSHOT_FROM: &str = "FROM machines m";

impl SqlxDpuIsolationClient {
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
    .map_err(|e| anyhow::anyhow!("dpu_isolation schema probe failed: {e}"))?;
    Ok(exists.0)
}

#[async_trait]
impl DpuIsolationClient for SqlxDpuIsolationClient {
    async fn fetch_snapshot(&self, machine_id: &str) -> Result<IsolationSnapshot> {
        if !machines_table_exists(&self.pool).await? {
            return Ok(IsolationSnapshot {
                machine_id: machine_id.to_string(),
                registered: false,
                scout_discovery_complete: false,
                quarantine_state: None,
                last_seen_at: None,
            });
        }

        let sql = format!(
            "SELECT {FETCH_SNAPSHOT_COLS} {FETCH_SNAPSHOT_FROM} WHERE m.id = $1 LIMIT 1"
        );
        let row: Option<(String, Option<String>, Option<DateTime<Utc>>)> =
            sqlx::query_as(&sql)
                .bind(machine_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| anyhow::anyhow!("dpu_isolation query failed: {e}"))?;

        Ok(match row {
            Some((_id, quarantine_state, last_seen_at)) => IsolationSnapshot {
                machine_id: machine_id.to_string(),
                registered: true,
                scout_discovery_complete: last_seen_at.is_some(),
                quarantine_state,
                last_seen_at,
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

/// Render the isolation axis as a headline `Check` (sourced from
/// [`isolation_verdict`]) followed by isolation-specific detail rows:
/// the operator-set quarantine mode (when set), the absolute last-seen
/// timestamp (when present), and the freshness-threshold echo. The
/// detail rows give the operator the raw data the punchy headline
/// elides; the rollup layers (PRD-003 slices 5 + 6) consume only the
/// headline.
///
/// JSON ordering — issue #306 acceptance criteria: headline first
/// (`kind: "headline"`), then detail (`kind: "detail"`).
pub fn assemble_checks(
    snapshot: &IsolationSnapshot,
    now: DateTime<Utc>,
    freshness_threshold: Duration,
) -> Vec<Check> {
    let summary = isolation_verdict(snapshot, now, freshness_threshold);
    let mut checks = vec![headline_from(&summary)];

    if let Some(state) = snapshot.quarantine_state.as_deref()
        && !state.is_empty()
    {
        checks.push(Check {
            name: "quarantine-state",
            status: Status::Ok,
            value: state.to_string(),
            next_command: None,
            kind: CheckKind::Detail,
        });
    }

    if let Some(last_seen) = snapshot.last_seen_at {
        checks.push(Check {
            name: "last-seen",
            status: Status::Ok,
            value: last_seen.to_rfc3339(),
            next_command: None,
            kind: CheckKind::Detail,
        });
        checks.push(Check {
            name: "freshness-threshold",
            status: Status::Ok,
            value: format_seconds(freshness_threshold),
            next_command: None,
            kind: CheckKind::Detail,
        });
    }

    checks
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

fn format_seconds(d: Duration) -> String {
    format!("{}s", d.as_secs())
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

    // ── assemble_checks: headline-then-detail ordering + detail population
    //
    // Verdict precedence + per-variant content live in
    // [`crate::verdicts::isolation`] tests. Tests here cover the layer
    // renderer: headline-vs-detail ordering, detail-row contents, and
    // data-layer error surfacing.

    #[test]
    fn healthy_check_is_headline_then_last_seen_and_threshold_detail_rows() {
        let snap = snap_healthy();
        let checks = assemble_checks(&snap, snap.last_seen_at.unwrap(), DEFAULT_FRESHNESS_THRESHOLD);

        assert_eq!(checks.len(), 3, "headline + 2 detail rows");
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Ok);
        assert!(checks[0].value.contains("machine-42"));
        assert!(checks[0].value.contains("healthy"));
        assert!(checks[0].next_command.is_none());

        assert_eq!(checks[1].kind, CheckKind::Detail);
        assert_eq!(checks[1].name, "last-seen");
        assert!(checks[1].value.contains('T'), "last-seen: {}", checks[1].value);

        assert_eq!(checks[2].kind, CheckKind::Detail);
        assert_eq!(checks[2].name, "freshness-threshold");
        assert_eq!(checks[2].value, "90s");
    }

    #[test]
    fn quarantined_check_emits_quarantine_state_detail_row_with_mode() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);

        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].value.contains("quarantine requested"));
        assert!(checks[0].value.contains("BlockAllTraffic"));

        let qstate = checks
            .iter()
            .find(|c| c.name == "quarantine-state")
            .expect("quarantine-state detail row");
        assert_eq!(qstate.kind, CheckKind::Detail);
        assert_eq!(qstate.value, "BlockAllTraffic");
    }

    #[test]
    fn lost_connection_check_headline_includes_age_and_threshold_seconds() {
        let mut snap = snap_healthy();
        let now = Utc::now();
        snap.last_seen_at = Some(now - chrono::Duration::seconds(180));
        let checks = assemble_checks(&snap, now, Duration::from_secs(90));

        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].value.contains("180s"));
        assert!(checks[0].value.contains("90s"));
    }

    #[test]
    fn lost_connection_no_last_seen_omits_last_seen_and_threshold_details() {
        let mut snap = snap_healthy();
        snap.last_seen_at = None;
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Fail);
        assert!(checks[0].value.contains("no network_status_observation"));
    }

    #[test]
    fn unregistered_machine_emits_only_unknown_headline_no_detail() {
        let mut snap = snap_healthy();
        snap.registered = false;
        snap.scout_discovery_complete = false;
        snap.last_seen_at = None;
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
        assert_eq!(checks[0].status, Status::Unknown);
        assert!(checks[0].value.contains("machines row"));
    }

    #[test]
    fn empty_string_quarantine_state_does_not_emit_detail_row() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("".into());
        let checks = assemble_checks(&snap, snap.last_seen_at.unwrap(), DEFAULT_FRESHNESS_THRESHOLD);

        assert!(
            checks.iter().all(|c| c.name != "quarantine-state"),
            "empty quarantine_state should not produce a detail row",
        );
    }

    // ── SQL: producer-side machines columns only (PRD-002) ────────────────

    #[test]
    fn fetch_snapshot_sql_targets_producer_side_machines_columns() {
        let cols = FETCH_SNAPSHOT_COLS;
        let from = FETCH_SNAPSHOT_FROM;
        let combined = format!("{cols} {from}");

        assert!(
            !combined.contains("dpu_network_status"),
            "old table dpu_network_status must not be referenced: {combined}"
        );
        assert!(
            from.contains("FROM machines"),
            "must read from machines table: {from}"
        );
        assert!(
            cols.contains("network_config->'quarantine_state'->>'mode'"),
            "must read desired-side quarantine mode: {cols}"
        );
        assert!(
            cols.contains("network_status_observation->>'observed_at'"),
            "must read observed_at for last_seen_at: {cols}"
        );
    }

    #[test]
    fn not_yet_known_scout_incomplete_emits_only_unknown_headline_distinct_from_unregistered() {
        let mut snap = snap_healthy();
        snap.scout_discovery_complete = false;
        snap.last_seen_at = None;
        let checks = assemble_checks(&snap, Utc::now(), DEFAULT_FRESHNESS_THRESHOLD);

        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].kind, CheckKind::Headline);
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
