use std::time::{Duration, Instant};
use async_trait::async_trait;
use nico_common::output::Status;

#[derive(Clone)]
pub struct RunOpts {
    pub namespace: String,
    pub since: Duration,
    pub timeout: Duration,
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
}

pub enum LayerOutcome {
    Checks(Vec<Check>),
    Skipped,
}

#[async_trait]
pub trait Layer: Send + Sync {
    fn name(&self) -> &'static str;
    async fn collect(&self, opts: &RunOpts) -> LayerOutcome;

    async fn run(&self, opts: &RunOpts) -> LayerResult {
        let start = Instant::now();
        let outcome = self.collect(opts).await;
        let (status, checks) = match outcome {
            LayerOutcome::Skipped => (Status::Skipped, vec![]),
            LayerOutcome::Checks(checks) => (aggregate_status(&checks), checks),
        };
        LayerResult {
            name: self.name(),
            status,
            checks,
            duration_ms: start.elapsed().as_millis() as u64,
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
}

impl SkippedLayer {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(name: &'static str) -> Box<dyn Layer> {
        Box::new(Self { name })
    }
}

#[async_trait]
impl Layer for SkippedLayer {
    fn name(&self) -> &'static str { self.name }
    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        LayerOutcome::Skipped
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
        }
    }

    #[tokio::test]
    async fn default_run_skipped_outcome_produces_skipped_status_and_no_checks() {
        let layer = StubLayer::new(LayerOutcome::Skipped);
        let result = layer.run(&opts()).await;
        assert_eq!(result.name, "stub");
        assert_eq!(result.status, Status::Skipped);
        assert!(result.checks.is_empty());
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
