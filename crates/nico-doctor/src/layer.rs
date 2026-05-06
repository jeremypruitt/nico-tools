use std::time::Duration;
use async_trait::async_trait;
use nico_common::output::Status;

pub struct RunOpts {
    pub namespace: String,
    pub since: Duration,
    pub timeout: Duration,
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub value: String,
    pub next_command: Option<String>,
}

pub struct LayerResult {
    pub name: &'static str,
    pub status: Status,
    pub checks: Vec<Check>,
    pub duration_ms: u64,
}

#[async_trait]
pub trait Layer: Send + Sync {
    fn name(&self) -> &'static str;
    async fn run(&self, opts: &RunOpts) -> LayerResult;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn check(status: Status) -> Check {
        Check { name: "x", status, value: String::new(), next_command: None }
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
    async fn run(&self, _opts: &RunOpts) -> LayerResult {
        LayerResult {
            name: self.name,
            status: Status::Skipped,
            checks: vec![],
            duration_ms: 0,
        }
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
    async fn run(&self, _opts: &RunOpts) -> LayerResult {
        LayerResult {
            name: self.name,
            status: Status::Unknown,
            checks: vec![Check {
                name: "config",
                status: Status::Unknown,
                value: self.reason.clone(),
                next_command: None,
            }],
            duration_ms: 0,
        }
    }
}
