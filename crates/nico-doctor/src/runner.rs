use nico_common::output::Status;
use crate::layer::{Layer, LayerResult, RunOpts};

pub struct Report {
    pub layers: Vec<LayerResult>,
}

impl Report {
    pub fn summary_status(&self) -> Status {
        if self.layers.iter().any(|l| l.status == Status::Fail) {
            Status::Fail
        } else if self.layers.iter().any(|l| l.status == Status::Warn) {
            Status::Warn
        } else if self.layers.iter().any(|l| l.status == Status::Unknown) {
            Status::Unknown
        } else {
            Status::Ok
        }
    }

    #[allow(dead_code)]
    pub fn layer(&self, name: &str) -> Option<&LayerResult> {
        self.layers.iter().find(|l| l.name == name)
    }
}

pub async fn run(layers: &[Box<dyn Layer>], opts: &RunOpts) -> Report {
    let futures: Vec<_> = layers.iter().map(|layer| {
        let timeout = opts.timeout;
        async move {
            match tokio::time::timeout(timeout, layer.run(opts)).await {
                Ok(result) => result,
                Err(_) => LayerResult {
                    name: layer.name(),
                    status: Status::Unknown,
                    checks: vec![],
                    duration_ms: timeout.as_millis() as u64,
                },
            }
        }
    }).collect();

    let results = futures::future::join_all(futures).await;
    Report { layers: results }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use async_trait::async_trait;
    use crate::layer::{Check, CheckKind, LayerOutcome, RunOpts};

    struct StubLayer {
        name: &'static str,
        result: Status,
    }

    impl StubLayer {
        fn ok(name: &'static str) -> Box<Self> {
            Box::new(Self { name, result: Status::Ok })
        }
        fn warn(name: &'static str) -> Box<Self> {
            Box::new(Self { name, result: Status::Warn })
        }
    }

    #[async_trait]
    impl Layer for StubLayer {
        fn name(&self) -> &'static str { self.name }
        async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
            LayerOutcome::Checks(vec![Check {
                name: "stub",
                status: self.result.clone(),
                value: String::new(),
                next_command: None,
                kind: CheckKind::Headline,
            }])
        }
    }

    fn opts() -> RunOpts {
        RunOpts { namespace: "nico".into(), since: Duration::from_secs(600), timeout: Duration::from_secs(5) }
    }

    struct SlowLayer { name: &'static str, delay: Duration }

    #[async_trait]
    impl Layer for SlowLayer {
        fn name(&self) -> &'static str { self.name }
        async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
            tokio::time::sleep(self.delay).await;
            LayerOutcome::Checks(vec![])
        }
    }

    #[tokio::test]
    async fn warn_layer_gives_warn_summary() {
        let layers: Vec<Box<dyn Layer>> = vec![StubLayer::ok("cluster"), StubLayer::warn("logs")];
        let report = run(&layers, &opts()).await;
        assert_eq!(report.summary_status(), Status::Warn);
    }

    #[tokio::test]
    async fn layer_timeout_reports_unknown_not_fail() {
        let layers: Vec<Box<dyn Layer>> = vec![
            Box::new(SlowLayer { name: "slow", delay: Duration::from_millis(100) }),
        ];
        let tight_opts = RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(600),
            timeout: Duration::from_millis(10),
        };
        let report = run(&layers, &tight_opts).await;
        assert_eq!(report.layer("slow").unwrap().status, Status::Unknown);
        assert_ne!(report.layer("slow").unwrap().status, Status::Fail);
    }

    #[tokio::test]
    async fn all_ok_layers_give_ok_report() {
        let layers: Vec<Box<dyn Layer>> = vec![StubLayer::ok("cluster"), StubLayer::ok("logs")];
        let report = run(&layers, &opts()).await;
        assert_eq!(report.summary_status(), Status::Ok);
        assert_eq!(report.layer("cluster").unwrap().status, Status::Ok);
        assert_eq!(report.layer("logs").unwrap().status, Status::Ok);
    }
}
