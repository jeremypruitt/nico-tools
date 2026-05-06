use std::sync::Arc;
use std::time::Instant;
use async_trait::async_trait;
use nico_common::output::Status;
use crate::k8s::K8sClient;
use crate::layer::{aggregate_status, Check, Layer, LayerResult, RunOpts};

pub struct ClusterLayer {
    k8s: Arc<dyn K8sClient>,
}

impl ClusterLayer {
    pub fn new(k8s: Arc<dyn K8sClient>) -> Self {
        Self { k8s }
    }
}

#[async_trait]
impl Layer for ClusterLayer {
    fn name(&self) -> &'static str {
        "cluster"
    }

    async fn run(&self, opts: &RunOpts) -> LayerResult {
        let start = Instant::now();
        let pods = self.k8s.list_pods(&opts.namespace).await.unwrap_or_default();
        let events = self.k8s.list_events(&opts.namespace, opts.since).await.unwrap_or_default();

        let total = pods.len();
        let ready = pods.iter().filter(|p| p.ready).count();
        let restarts: u32 = pods.iter().map(|p| p.restart_count).sum();
        let warn_events = events.len();

        let pods_status = if ready == total { Status::Ok } else { Status::Warn };
        let restart_status = if restarts == 0 { Status::Ok } else { Status::Warn };
        let events_status = if warn_events == 0 { Status::Ok } else { Status::Warn };

        let not_ready: Vec<&str> = pods.iter()
            .filter(|p| !p.ready)
            .map(|p| p.name.as_str())
            .collect();

        let pods_cmd = if not_ready.is_empty() {
            None
        } else {
            Some(format!("kubectl get pods -n {} | grep -v Running", opts.namespace))
        };

        let checks = vec![
            Check {
                name: "pods_ready",
                status: pods_status,
                value: format!("{ready}/{total}"),
                next_command: pods_cmd,
            },
            Check {
                name: "recent_restarts",
                status: restart_status,
                value: restarts.to_string(),
                next_command: if restarts > 0 {
                    Some(format!("kubectl get pods -n {} -o wide", opts.namespace))
                } else {
                    None
                },
            },
            Check {
                name: "warning_events",
                status: events_status,
                value: warn_events.to_string(),
                next_command: if warn_events > 0 {
                    Some(format!("kubectl get events -n {} --field-selector type=Warning", opts.namespace))
                } else {
                    None
                },
            },
        ];

        let overall = aggregate_status(&checks);

        LayerResult {
            name: "cluster",
            status: overall,
            checks,
            duration_ms: start.elapsed().as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use anyhow::Result;
    use async_trait::async_trait;
    use crate::k8s::{EventInfo, PodInfo};

    struct MockK8sClient {
        pods: Vec<PodInfo>,
        events: Vec<EventInfo>,
    }

    #[async_trait]
    impl K8sClient for MockK8sClient {
        async fn list_pods(&self, _namespace: &str) -> Result<Vec<PodInfo>> {
            Ok(self.pods.iter().map(|p| PodInfo {
                name: p.name.clone(),
                ready: p.ready,
                restart_count: p.restart_count,
            }).collect())
        }
        async fn list_events(&self, _namespace: &str, _since: Duration) -> Result<Vec<EventInfo>> {
            Ok(self.events.iter().map(|e| EventInfo {
                message: e.message.clone(),
                reason: e.reason.clone(),
            }).collect())
        }
        async fn pod_logs(&self, _namespace: &str, _pod: &str, _since: Duration) -> Result<Vec<String>> {
            Ok(vec![])
        }
    }

    fn opts() -> RunOpts {
        RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(600),
            timeout: Duration::from_secs(5),
        }
    }

    fn check_value<'a>(result: &'a LayerResult, name: &str) -> &'a str {
        result.checks.iter().find(|c| c.name == name)
            .map(|c| c.value.as_str())
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    fn check_status<'a>(result: &'a LayerResult, name: &str) -> &'a Status {
        result.checks.iter().find(|c| c.name == name)
            .map(|c| &c.status)
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    #[tokio::test]
    async fn warning_events_report_warn() {
        let client = Arc::new(MockK8sClient {
            pods: vec![PodInfo { name: "core-abc".into(), ready: true, restart_count: 0 }],
            events: vec![
                EventInfo { message: "OOMKilled".into(), reason: "OOMKilling".into() },
                EventInfo { message: "Backoff".into(), reason: "BackOff".into() },
            ],
        });
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Warn);
        assert_eq!(check_value(&result, "warning_events"), "2");
        assert_eq!(check_status(&result, "warning_events"), &Status::Warn);
        assert_eq!(check_status(&result, "pods_ready"), &Status::Ok);
    }

    #[tokio::test]
    async fn pod_with_recent_restarts_reports_warn() {
        let client = Arc::new(MockK8sClient {
            pods: vec![
                PodInfo { name: "core-abc".into(), ready: true, restart_count: 3 },
                PodInfo { name: "rest-xyz".into(), ready: true, restart_count: 0 },
            ],
            events: vec![],
        });
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Warn);
        assert_eq!(check_value(&result, "recent_restarts"), "3");
        assert_eq!(check_status(&result, "recent_restarts"), &Status::Warn);
        assert_eq!(check_status(&result, "pods_ready"), &Status::Ok);
    }

    #[tokio::test]
    async fn pod_not_ready_reports_warn() {
        let client = Arc::new(MockK8sClient {
            pods: vec![
                PodInfo { name: "core-abc".into(), ready: true, restart_count: 0 },
                PodInfo { name: "rest-xyz".into(), ready: false, restart_count: 0 },
            ],
            events: vec![],
        });
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Warn);
        assert_eq!(check_value(&result, "pods_ready"), "1/2");
        assert_eq!(check_status(&result, "pods_ready"), &Status::Warn);
        assert!(result.checks.iter().find(|c| c.name == "pods_ready")
            .and_then(|c| c.next_command.as_deref()).is_some());
    }

    #[tokio::test]
    async fn all_pods_ready_no_issues_reports_ok() {
        let client = Arc::new(MockK8sClient {
            pods: vec![
                PodInfo { name: "core-abc".into(), ready: true, restart_count: 0 },
                PodInfo { name: "rest-xyz".into(), ready: true, restart_count: 0 },
            ],
            events: vec![],
        });
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Ok);
        assert_eq!(check_value(&result, "pods_ready"), "2/2");
        assert_eq!(check_value(&result, "recent_restarts"), "0");
        assert_eq!(check_value(&result, "warning_events"), "0");
    }
}
