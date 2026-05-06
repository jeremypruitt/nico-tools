use std::sync::Arc;
use std::time::Instant;
use async_trait::async_trait;
use nico_common::output::Status;
use crate::k8s::K8sClient;
use crate::loki::{LokiClient, LokiQueryResult};
use crate::layer::{aggregate_status, Check, Layer, LayerResult, RunOpts};

const LOG_LINE_LIMIT: usize = 500;

fn is_error_line(s: &str) -> bool {
    let l = s.to_lowercase();
    l.contains("error") || l.contains("panic") || l.contains("fatal")
}

pub struct LogsLayer {
    loki: Arc<dyn LokiClient>,
    k8s: Arc<dyn K8sClient>,
}

impl LogsLayer {
    pub fn new(loki: Arc<dyn LokiClient>, k8s: Arc<dyn K8sClient>) -> Self {
        Self { loki, k8s }
    }
}

#[async_trait]
impl Layer for LogsLayer {
    fn name(&self) -> &'static str { "logs" }

    async fn run(&self, opts: &RunOpts) -> LayerResult {
        let start = Instant::now();

        let (pod_errors, source_label, source_ok) =
            match self.loki.query_errors(&opts.namespace, opts.since, LOG_LINE_LIMIT).await {
                Ok(LokiQueryResult::Lines(lines)) => {
                    let errors: Vec<(String, String)> = lines.into_iter()
                        .map(|l| (l.pod, l.text))
                        .collect();
                    (errors, "loki".to_string(), true)
                }
                _ => {
                    let pods = self.k8s.list_pods(&opts.namespace).await.unwrap_or_default();
                    let mut errors = Vec::new();
                    for pod in &pods {
                        let lines = self.k8s.pod_logs(&opts.namespace, &pod.name, opts.since)
                            .await
                            .unwrap_or_default();
                        for line in lines.into_iter().take(LOG_LINE_LIMIT) {
                            if is_error_line(&line) {
                                errors.push((pod.name.clone(), line));
                            }
                        }
                    }
                    (errors, "k8s (loki unavailable)".to_string(), false)
                }
            };

        let checks = checks_from(&pod_errors, &source_label, source_ok, &opts.namespace);
        let overall = aggregate_status(&checks);

        LayerResult {
            name: "logs",
            status: overall,
            checks,
            duration_ms: start.elapsed().as_millis() as u64,
        }
    }
}

fn checks_from(
    pod_errors: &[(String, String)],
    source_label: &str,
    source_ok: bool,
    namespace: &str,
) -> Vec<Check> {
    let error_count = pod_errors.len();
    let mut checks = vec![
        Check {
            name: "error_lines",
            status: if error_count == 0 { Status::Ok } else { Status::Warn },
            value: format!("{error_count} errors"),
            next_command: None,
        },
        Check {
            name: "source",
            status: if source_ok { Status::Ok } else { Status::Warn },
            value: source_label.to_string(),
            next_command: None,
        },
    ];

    for (pod, line) in pod_errors {
        let excerpt = if line.len() > 80 {
            format!("{}…", &line[..79])
        } else {
            line.clone()
        };
        checks.push(Check {
            name: "pod_error",
            status: Status::Warn,
            value: format!("{pod}: {excerpt}"),
            next_command: Some(format!("kubectl logs {pod} -n {namespace}")),
        });
    }

    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use anyhow::Result;
    use async_trait::async_trait;
    use crate::k8s::{EventInfo, PodInfo};
    use crate::loki::{LokiLine, LokiQueryResult};

    enum MockLokiResponse { Lines(Vec<(String, String)>), Unreachable, Err }

    struct MockLoki(MockLokiResponse);

    #[async_trait]
    impl LokiClient for MockLoki {
        async fn query_errors(&self, _: &str, _: Duration, _: usize) -> Result<LokiQueryResult> {
            match &self.0 {
                MockLokiResponse::Lines(ls) => Ok(LokiQueryResult::Lines(ls.iter().map(|(p, t)| LokiLine {
                    pod: p.clone(), text: t.clone(),
                }).collect())),
                MockLokiResponse::Unreachable => Ok(LokiQueryResult::Unreachable),
                MockLokiResponse::Err => Err(anyhow::anyhow!("loki error")),
            }
        }
    }

    struct MockK8s {
        pods: Vec<PodInfo>,
        logs: Vec<(String, Vec<String>)>,
    }

    #[async_trait]
    impl K8sClient for MockK8s {
        async fn list_pods(&self, _: &str) -> Result<Vec<PodInfo>> {
            Ok(self.pods.iter().map(|p| PodInfo {
                name: p.name.clone(), ready: p.ready, restart_count: p.restart_count, succeeded: p.succeeded,
            }).collect())
        }
        async fn list_events(&self, _: &str, _: Duration) -> Result<Vec<EventInfo>> {
            Ok(vec![])
        }
        async fn pod_logs(&self, _: &str, pod: &str, _: Duration) -> Result<Vec<String>> {
            Ok(self.logs.iter()
                .find(|(name, _)| name == pod)
                .map(|(_, lines)| lines.clone())
                .unwrap_or_default())
        }
    }

    fn opts() -> RunOpts {
        RunOpts { namespace: "nico".into(), since: Duration::from_secs(600), timeout: Duration::from_secs(5) }
    }

    #[tokio::test]
    async fn loki_errors_show_as_warn_with_kubectl_hints() {
        let loki = Arc::new(MockLoki(MockLokiResponse::Lines(vec![
            ("core-abc".into(), "ERROR: disk full".into()),
            ("rest-xyz".into(), "FATAL: oom".into()),
        ])));
        let k8s = Arc::new(MockK8s { pods: vec![], logs: vec![] });
        let result = LogsLayer::new(loki, k8s).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let err_check = result.checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err_check.value, "2 errors");
        let src = result.checks.iter().find(|c| c.name == "source").unwrap();
        assert_eq!(src.value, "loki");
        assert_eq!(src.status, Status::Ok);
        let pod_errors: Vec<_> = result.checks.iter().filter(|c| c.name == "pod_error").collect();
        assert_eq!(pod_errors.len(), 2);
        assert!(pod_errors[0].next_command.as_deref().unwrap().starts_with("kubectl logs"));
    }

    #[tokio::test]
    async fn loki_unreachable_falls_back_to_k8s_and_annotates() {
        let loki = Arc::new(MockLoki(MockLokiResponse::Unreachable));
        let k8s = Arc::new(MockK8s {
            pods: vec![PodInfo { name: "core-abc".into(), ready: true, restart_count: 0, succeeded: false }],
            logs: vec![("core-abc".into(), vec![
                "INFO: started".into(),
                "ERROR: connection refused".into(),
            ])],
        });
        let result = LogsLayer::new(loki, k8s).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let src = result.checks.iter().find(|c| c.name == "source").unwrap();
        assert!(src.value.contains("loki unavailable"), "source value: {}", src.value);
        assert_eq!(src.status, Status::Warn);
        let pod_errors: Vec<_> = result.checks.iter().filter(|c| c.name == "pod_error").collect();
        assert_eq!(pod_errors.len(), 1);
        assert!(pod_errors[0].value.contains("core-abc"));
    }

    #[tokio::test]
    async fn loki_error_falls_back_to_k8s() {
        let loki = Arc::new(MockLoki(MockLokiResponse::Err));
        let k8s = Arc::new(MockK8s {
            pods: vec![PodInfo { name: "agent-1".into(), ready: true, restart_count: 0, succeeded: false }],
            logs: vec![("agent-1".into(), vec!["PANIC: nil pointer".into()])],
        });
        let result = LogsLayer::new(loki, k8s).run(&opts()).await;

        assert_eq!(result.status, Status::Warn);
        let src = result.checks.iter().find(|c| c.name == "source").unwrap();
        assert!(src.value.contains("loki unavailable"));
        let pod_errors: Vec<_> = result.checks.iter().filter(|c| c.name == "pod_error").collect();
        assert_eq!(pod_errors.len(), 1);
        assert!(pod_errors[0].value.contains("PANIC"));
    }

    #[test]
    fn checks_from_no_errors_reports_ok() {
        let checks = checks_from(&[], "loki", true, "nico");
        assert_eq!(aggregate_status(&checks), Status::Ok);
        assert_eq!(checks.iter().filter(|c| c.name == "pod_error").count(), 0);
    }

    #[test]
    fn checks_from_errors_present_reports_warn_with_one_pod_error_per_entry() {
        let pod_errors = vec![
            ("core-abc".to_string(), "ERROR: disk full".to_string()),
            ("rest-xyz".to_string(), "FATAL: oom".to_string()),
        ];
        let checks = checks_from(&pod_errors, "loki", true, "nico");

        assert_eq!(aggregate_status(&checks), Status::Warn);
        assert_eq!(checks.iter().filter(|c| c.name == "pod_error").count(), 2);
        let err_check = checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err_check.status, Status::Warn);
    }

    #[test]
    fn checks_from_source_unavailable_marks_source_warn() {
        let checks = checks_from(&[], "k8s (loki unavailable)", false, "nico");
        let src = checks.iter().find(|c| c.name == "source").unwrap();
        assert_eq!(src.status, Status::Warn);
        assert_eq!(checks.iter().filter(|c| c.name == "source").count(), 1);
    }

    #[tokio::test]
    async fn no_errors_reports_ok() {
        let loki = Arc::new(MockLoki(MockLokiResponse::Lines(vec![])));
        let k8s = Arc::new(MockK8s { pods: vec![], logs: vec![] });
        let result = LogsLayer::new(loki, k8s).run(&opts()).await;

        assert_eq!(result.status, Status::Ok);
        let err_check = result.checks.iter().find(|c| c.name == "error_lines").unwrap();
        assert_eq!(err_check.value, "0 errors");
        assert!(result.checks.iter().filter(|c| c.name == "pod_error").count() == 0);
    }
}
