use std::sync::Arc;
use nico_common::output::Status;
use crate::layer::{Layer, LayerResult, RunOpts};
use crate::log_collector::LogCollectorStage;

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
                    skipped_reason: None,
                },
            }
        }
    }).collect();

    let results = futures::future::join_all(futures).await;
    Report { layers: results }
}

/// Refresh entrypoint: runs the [`LogCollectorStage`] (if any) once,
/// populates `opts.pod_logs` with its result, and then calls [`run`].
/// This is the only path that satisfies the "at most one `pod_logs` call
/// per pod per refresh" guarantee from issue #201; callers that bypass it
/// (e.g. fixed-fixture tests) get an empty cache and the per-pod detail
/// checks degrade gracefully.
pub async fn run_with_log_collector(
    layers: &[Box<dyn Layer>],
    opts: &RunOpts,
    collector: Option<&LogCollectorStage>,
) -> Report {
    let opts = with_collected_logs(opts, collector).await;
    run(layers, &opts).await
}

pub(crate) async fn with_collected_logs(
    opts: &RunOpts,
    collector: Option<&LogCollectorStage>,
) -> RunOpts {
    match collector {
        Some(c) => {
            let cache = c.collect(&opts.namespace, opts.since).await;
            RunOpts {
                pod_logs: Arc::new(cache),
                ..opts.clone()
            }
        }
        None => opts.clone(),
    }
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
        RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(600),
            timeout: Duration::from_secs(5),
            ..Default::default()
        }
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
            ..Default::default()
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

    /// Layer that snapshots `opts.pod_logs` so tests can verify the
    /// runner populated the per-refresh cache before fanning out.
    type CacheSnapshot = std::sync::Mutex<Option<std::collections::HashMap<String, Vec<String>>>>;
    struct CacheCapture {
        seen: Arc<CacheSnapshot>,
    }

    #[async_trait]
    impl Layer for CacheCapture {
        fn name(&self) -> &'static str {
            "capture"
        }
        async fn collect(&self, opts: &RunOpts) -> LayerOutcome {
            *self.seen.lock().unwrap() = Some((*opts.pod_logs).clone());
            LayerOutcome::Checks(vec![])
        }
    }

    #[tokio::test]
    async fn run_with_log_collector_populates_pod_logs_before_layers_run() {
        use nico_common::k8s::testing::MockK8sClient;
        use nico_common::k8s::RawPod;

        let k8s = Arc::new(
            MockK8sClient::new()
                .with_pods(vec![RawPod {
                    name: "p1".into(),
                    namespace: "nico".into(),
                    phase: None,
                    ready: true,
                    restart_count: 0,
                    succeeded: false,
                    crash_loop: false,
                }])
                .with_logs(vec!["ERROR boom".into()]),
        );
        let collector = LogCollectorStage::new(k8s);
        let seen = Arc::new(std::sync::Mutex::new(None));
        let layers: Vec<Box<dyn Layer>> = vec![Box::new(CacheCapture { seen: seen.clone() })];

        let _ = run_with_log_collector(&layers, &opts(), Some(&collector)).await;

        let captured = seen.lock().unwrap().clone().expect("layer ran");
        assert_eq!(captured.get("p1").unwrap(), &vec!["ERROR boom".to_string()]);
    }

    #[tokio::test]
    async fn run_with_log_collector_with_none_runs_layers_with_empty_cache() {
        let seen = Arc::new(std::sync::Mutex::new(None));
        let layers: Vec<Box<dyn Layer>> = vec![Box::new(CacheCapture { seen: seen.clone() })];

        let _ = run_with_log_collector(&layers, &opts(), None).await;

        let captured = seen.lock().unwrap().clone().expect("layer ran");
        assert!(captured.is_empty());
    }
}
