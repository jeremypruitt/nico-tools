use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use nico_common::config::DeploymentType;
use nico_common::output::Status;

#[derive(Clone)]
pub struct RunOpts {
    pub namespace: String,
    pub since: Duration,
    pub timeout: Duration,
    /// Per-refresh pod log cache populated by
    /// [`crate::log_collector::LogCollectorStage`] *before* `runner::run`
    /// fans out the layers. `ClusterLayer` (`pod_log_tail`) and
    /// `K8sLogSource` both read from this map instead of issuing their
    /// own `pod_logs` calls; this caps log fetches at one per pod per
    /// refresh. Empty for callers who skip the stage (e.g. test fixtures
    /// using `RunOpts::default()` and the snapshot logs panel).
    pub pod_logs: Arc<HashMap<String, Vec<String>>>,
}

impl Default for RunOpts {
    fn default() -> Self {
        Self {
            namespace: String::new(),
            since: Duration::from_secs(600),
            timeout: Duration::from_secs(5),
            pod_logs: Arc::new(HashMap::new()),
        }
    }
}

/// A `Check` is either a **headline** (summarizes the layer at a glance,
/// joined into the layer summary line) or a **detail** (one-per-finding
/// evidence, never in the summary line). See ADR-0003 (2026-05-07 amendment).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum CheckKind {
    #[default]
    Headline,
    Detail,
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub value: String,
    pub next_command: Option<String>,
    pub kind: CheckKind,
}

pub struct LayerResult {
    pub name: &'static str,
    pub status: Status,
    pub checks: Vec<Check>,
    pub duration_ms: u64,
    /// Operator-readable explanation when `status == Skipped`. `None` for
    /// unconditional skips (e.g. `--skip dpu`); `Some` when a layer is
    /// n/a-by-design under the resolved deployment-type — see PRD-001 §
    /// "Status semantics for 'n/a in this deployment-type'".
    pub skipped_reason: Option<String>,
}

pub enum LayerOutcome {
    Checks(Vec<Check>),
    Skipped { reason: Option<String> },
}

#[async_trait]
pub trait Layer: Send + Sync {
    fn name(&self) -> &'static str;
    async fn collect(&self, opts: &RunOpts) -> LayerOutcome;

    async fn run(&self, opts: &RunOpts) -> LayerResult {
        let start = Instant::now();
        let outcome = self.collect(opts).await;
        let (status, checks, skipped_reason) = match outcome {
            LayerOutcome::Skipped { reason } => (Status::Skipped, vec![], reason),
            LayerOutcome::Checks(checks) => (aggregate_status(&checks), checks, None),
        };
        LayerResult {
            name: self.name(),
            status,
            checks,
            duration_ms: start.elapsed().as_millis() as u64,
            skipped_reason,
        }
    }
}

/// Returns the worst-case status across a slice of checks.
/// Priority order: Fail > Warn > Unknown > Ok. Empty slice returns Ok.
pub fn aggregate_status(checks: &[Check]) -> Status {
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

pub struct SkippedLayer {
    name: &'static str,
    reason: Option<String>,
}

impl SkippedLayer {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(name: &'static str) -> Box<dyn Layer> {
        Box::new(Self { name, reason: None })
    }

    /// Skip with an operator-readable reason — used when a layer is
    /// n/a-by-design under the resolved deployment-type (PRD-001).
    #[allow(clippy::new_ret_no_self)]
    pub fn with_reason(name: &'static str, reason: impl Into<String>) -> Box<dyn Layer> {
        Box::new(Self {
            name,
            reason: Some(reason.into()),
        })
    }
}

#[async_trait]
impl Layer for SkippedLayer {
    fn name(&self) -> &'static str { self.name }
    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        LayerOutcome::Skipped { reason: self.reason.clone() }
    }
}

/// PRD-001 §"Status semantics for 'n/a in this deployment-type'": when the
/// resolved deployment-type lacks forgedb, return a [`SkippedLayer`] with
/// reason `n/a in <type>: no forgedb`. `None` deployment-type (auto
/// unresolved) preserves pre-PRD-001 behavior — the caller proceeds with
/// the real layer.
pub fn forgedb_skip_layer(
    name: &'static str,
    deployment_type: Option<DeploymentType>,
) -> Option<Box<dyn Layer>> {
    let dt = deployment_type?;
    if dt.forgedb_present() {
        return None;
    }
    Some(SkippedLayer::with_reason(
        name,
        format!("n/a in {}: no forgedb", dt.label()),
    ))
}

/// PRD-001 slice 10: when the resolved deployment-type lacks Temporal
/// (only `core-only` today), return a [`SkippedLayer`] with reason
/// `n/a in <type>: no Temporal`. Mirrors [`forgedb_skip_layer`].
pub fn temporal_skip_layer(
    name: &'static str,
    deployment_type: Option<DeploymentType>,
) -> Option<Box<dyn Layer>> {
    let dt = deployment_type?;
    if dt.temporal_present() {
        return None;
    }
    Some(SkippedLayer::with_reason(
        name,
        format!("n/a in {}: no Temporal", dt.label()),
    ))
}

/// PRD-004 slice 2 IB capability gate: when the boot-probed
/// `infiniband_present` is `Some(false)` (RoCE / ethernet-only fleet)
/// or `None` (force mode / detection unavailable), return a
/// [`SkippedLayer`] with an operator-readable reason. `Some(true)`
/// returns `None` so the caller installs the real IB layer.
pub fn infiniband_skip_layer(
    name: &'static str,
    infiniband_present: Option<bool>,
) -> Option<Box<dyn Layer>> {
    match infiniband_present {
        Some(true) => None,
        Some(false) => Some(SkippedLayer::with_reason(
            name,
            "n/a: no InfiniBand fabric detected",
        )),
        None => Some(SkippedLayer::with_reason(
            name,
            "n/a: InfiniBand presence not detected (force mode)",
        )),
    }
}

pub struct UnconfiguredLayer {
    name: &'static str,
    reason: String,
}

impl UnconfiguredLayer {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(name: &'static str, reason: impl Into<String>) -> Box<dyn Layer> {
        Box::new(Self { name, reason: reason.into() })
    }
}

#[async_trait]
impl Layer for UnconfiguredLayer {
    fn name(&self) -> &'static str { self.name }
    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        LayerOutcome::Checks(vec![Check {
            name: "config",
            status: Status::Unknown,
            value: self.reason.clone(),
            next_command: None,
            kind: CheckKind::Headline,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(status: Status) -> Check {
        Check { name: "x", status, value: String::new(), next_command: None, kind: CheckKind::Headline }
    }

    #[test]
    fn check_kind_default_is_headline() {
        assert_eq!(CheckKind::default(), CheckKind::Headline);
    }

    #[test]
    fn empty_slice_is_ok() {
        assert_eq!(aggregate_status(&[]), Status::Ok);
    }

    #[test]
    fn all_green_is_ok() {
        let checks = vec![check(Status::Ok), check(Status::Ok)];
        assert_eq!(aggregate_status(&checks), Status::Ok);
    }

    #[test]
    fn one_warning_is_warning() {
        let checks = vec![check(Status::Ok), check(Status::Warn)];
        assert_eq!(aggregate_status(&checks), Status::Warn);
    }

    #[test]
    fn one_critical_is_critical() {
        let checks = vec![check(Status::Ok), check(Status::Warn), check(Status::Fail)];
        assert_eq!(aggregate_status(&checks), Status::Fail);
    }

    #[test]
    fn unknown_beats_ok_but_not_warn() {
        assert_eq!(aggregate_status(&[check(Status::Unknown)]), Status::Unknown);
        assert_eq!(aggregate_status(&[check(Status::Warn), check(Status::Unknown)]), Status::Warn);
    }

    struct StubLayer {
        outcome: std::sync::Mutex<Option<LayerOutcome>>,
    }

    impl StubLayer {
        fn new(outcome: LayerOutcome) -> Self {
            Self { outcome: std::sync::Mutex::new(Some(outcome)) }
        }
    }

    #[async_trait]
    impl Layer for StubLayer {
        fn name(&self) -> &'static str { "stub" }
        async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
            self.outcome.lock().unwrap().take().expect("collect called twice")
        }
    }

    fn opts() -> RunOpts {
        RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(60),
            timeout: Duration::from_secs(5),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn default_run_skipped_outcome_produces_skipped_status_and_no_checks() {
        let layer = StubLayer::new(LayerOutcome::Skipped { reason: None });
        let result = layer.run(&opts()).await;
        assert_eq!(result.name, "stub");
        assert_eq!(result.status, Status::Skipped);
        assert!(result.checks.is_empty());
        assert_eq!(result.skipped_reason, None);
    }

    #[tokio::test]
    async fn forgedb_skip_layer_returns_skip_with_reason_for_rest_only_mock() {
        let layer = forgedb_skip_layer("hbn", Some(DeploymentType::RestOnlyMock))
            .expect("rest-only-mock has no forgedb → skip layer expected");
        let r = layer.run(&opts()).await;
        assert_eq!(r.name, "hbn");
        assert_eq!(r.status, Status::Skipped);
        assert_eq!(r.skipped_reason.as_deref(), Some("n/a in rest-only-mock: no forgedb"));
    }

    #[test]
    fn forgedb_skip_layer_returns_none_when_forgedb_present() {
        for dt in [DeploymentType::Full, DeploymentType::CoreOnly, DeploymentType::Force] {
            assert!(
                forgedb_skip_layer("hbn", Some(dt)).is_none(),
                "{dt:?}: forgedb present → no skip",
            );
        }
    }

    #[test]
    fn forgedb_skip_layer_returns_none_when_deployment_type_unresolved() {
        assert!(forgedb_skip_layer("hbn", None).is_none());
    }

    // PRD-004 slice 2 — infiniband_skip_layer tests.

    #[tokio::test]
    async fn infiniband_skip_layer_skips_when_capability_some_false() {
        let layer = infiniband_skip_layer("infiniband", Some(false))
            .expect("Some(false) ⇒ skip layer expected");
        let r = layer.run(&opts()).await;
        assert_eq!(r.name, "infiniband");
        assert_eq!(r.status, Status::Skipped);
        assert_eq!(
            r.skipped_reason.as_deref(),
            Some("n/a: no InfiniBand fabric detected")
        );
    }

    #[tokio::test]
    async fn infiniband_skip_layer_skips_when_capability_none() {
        let layer = infiniband_skip_layer("infiniband", None)
            .expect("None ⇒ skip layer expected (force mode / detection unavailable)");
        let r = layer.run(&opts()).await;
        assert_eq!(r.status, Status::Skipped);
        assert!(r
            .skipped_reason
            .as_deref()
            .unwrap()
            .contains("force mode"));
    }

    #[test]
    fn infiniband_skip_layer_returns_none_when_capability_some_true() {
        assert!(infiniband_skip_layer("infiniband", Some(true)).is_none());
    }

    #[tokio::test]
    async fn temporal_skip_layer_returns_skip_with_reason_for_core_only() {
        let layer = temporal_skip_layer("workflows", Some(DeploymentType::CoreOnly))
            .expect("core-only has no Temporal → skip layer expected");
        let r = layer.run(&opts()).await;
        assert_eq!(r.name, "workflows");
        assert_eq!(r.status, Status::Skipped);
        assert_eq!(
            r.skipped_reason.as_deref(),
            Some("n/a in core-only: no Temporal"),
        );
    }

    #[test]
    fn temporal_skip_layer_returns_none_when_temporal_present() {
        for dt in [
            DeploymentType::Full,
            DeploymentType::RestOnlyMock,
            DeploymentType::Force,
        ] {
            assert!(
                temporal_skip_layer("workflows", Some(dt)).is_none(),
                "{dt:?}: temporal present → no skip",
            );
        }
    }

    #[test]
    fn temporal_skip_layer_returns_none_when_deployment_type_unresolved() {
        assert!(temporal_skip_layer("workflows", None).is_none());
    }

    #[tokio::test]
    async fn skipped_outcome_with_reason_propagates_to_layer_result() {
        let layer = StubLayer::new(LayerOutcome::Skipped {
            reason: Some("n/a in rest-only-mock: no forgedb".into()),
        });
        let result = layer.run(&opts()).await;
        assert_eq!(result.status, Status::Skipped);
        assert!(result.checks.is_empty());
        assert_eq!(
            result.skipped_reason.as_deref(),
            Some("n/a in rest-only-mock: no forgedb"),
        );
    }

    #[tokio::test]
    async fn default_run_aggregates_checks_status() {
        let layer = StubLayer::new(LayerOutcome::Checks(vec![
            check(Status::Ok),
            check(Status::Warn),
        ]));
        let result = layer.run(&opts()).await;
        assert_eq!(result.status, Status::Warn);
        assert_eq!(result.checks.len(), 2);
    }

    #[tokio::test]
    async fn default_run_uses_layer_name_for_result_name() {
        let layer = StubLayer::new(LayerOutcome::Checks(vec![check(Status::Ok)]));
        let result = layer.run(&opts()).await;
        assert_eq!(result.name, layer.name());
    }
}
