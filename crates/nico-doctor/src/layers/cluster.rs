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
        let all_pods = self.k8s.list_pods(&opts.namespace).await.unwrap_or_default();
        let events = self.k8s.list_events(&opts.namespace, opts.since).await.unwrap_or_default();
        let pods: Vec<_> = all_pods.into_iter().filter(|p| !p.succeeded).collect();
        let checks = checks_from(&pods, &events, &opts.namespace);
        let overall = aggregate_status(&checks);
        LayerResult {
            name: "cluster",
            status: overall,
            checks,
            duration_ms: start.elapsed().as_millis() as u64,
        }
    }
}

fn checks_from(pods: &[crate::k8s::PodInfo], events: &[crate::k8s::EventInfo], namespace: &str) -> Vec<Check> {
    let total = pods.len();
    let ready = pods.iter().filter(|p| p.ready).count();
    let restarts: u32 = pods.iter().map(|p| p.restart_count).sum();
    let warn_events = events.len();

    let pods_status = if ready == total { Status::Ok } else { Status::Warn };
    let restart_status = if restarts == 0 { Status::Ok } else { Status::Warn };
    let events_status = if warn_events == 0 { Status::Ok } else { Status::Warn };

    let any_not_ready = pods.iter().any(|p| !p.ready);

    vec![
        Check {
            name: "pods_ready",
            status: pods_status,
            value: format!("{ready}/{total}"),
            next_command: if any_not_ready {
                Some(format!("kubectl get pods -n {namespace} | grep -v Running"))
            } else {
                None
            },
        },
        Check {
            name: "recent_restarts",
            status: restart_status,
            value: restarts.to_string(),
            next_command: if restarts > 0 {
                Some(format!("kubectl get pods -n {namespace} -o wide"))
            } else {
                None
            },
        },
        Check {
            name: "warning_events",
            status: events_status,
            value: warn_events.to_string(),
            next_command: if warn_events > 0 {
                Some(format!("kubectl get events -n {namespace} --field-selector type=Warning"))
            } else {
                None
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use anyhow::Result;
    use async_trait::async_trait;
    use nico_common::output::Status;
    use crate::k8s::{EventInfo, PodInfo};

    #[test]
    fn checks_from_all_healthy_returns_three_ok_checks() {
        let pods = vec![
            PodInfo { name: "core-abc".into(), ready: true, restart_count: 0, succeeded: false },
            PodInfo { name: "rest-xyz".into(), ready: true, restart_count: 0, succeeded: false },
        ];
        let events: Vec<EventInfo> = vec![];
        let checks = checks_from(&pods, &events, "nico");
        assert_eq!(checks.len(), 3);
        assert!(checks.iter().all(|c| c.status == Status::Ok));
    }

    #[test]
    fn checks_from_not_ready_pod_and_warning_event_returns_warn_statuses() {
        let pods = vec![
            PodInfo { name: "core-abc".into(), ready: false, restart_count: 2, succeeded: false },
        ];
        let events = vec![
            EventInfo { message: "OOMKilled".into(), reason: "OOMKilling".into() },
        ];
        let checks = checks_from(&pods, &events, "nico");
        assert_eq!(checks.len(), 3);
        let pods_check = checks.iter().find(|c| c.name == "pods_ready").unwrap();
        let restart_check = checks.iter().find(|c| c.name == "recent_restarts").unwrap();
        let events_check = checks.iter().find(|c| c.name == "warning_events").unwrap();
        assert_eq!(pods_check.status, Status::Warn);
        assert_eq!(restart_check.status, Status::Warn);
        assert_eq!(events_check.status, Status::Warn);
    }

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
                succeeded: p.succeeded,
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
            pods: vec![PodInfo { name: "core-abc".into(), ready: true, restart_count: 0, succeeded: false }],
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
                PodInfo { name: "core-abc".into(), ready: true, restart_count: 3, succeeded: false },
                PodInfo { name: "rest-xyz".into(), ready: true, restart_count: 0, succeeded: false },
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
                PodInfo { name: "core-abc".into(), ready: true, restart_count: 0, succeeded: false },
                PodInfo { name: "rest-xyz".into(), ready: false, restart_count: 0, succeeded: false },
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
                PodInfo { name: "core-abc".into(), ready: true, restart_count: 0, succeeded: false },
                PodInfo { name: "rest-xyz".into(), ready: true, restart_count: 0, succeeded: false },
            ],
            events: vec![],
        });
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Ok);
        assert_eq!(check_value(&result, "pods_ready"), "2/2");
        assert_eq!(check_value(&result, "recent_restarts"), "0");
        assert_eq!(check_value(&result, "warning_events"), "0");
    }

    #[tokio::test]
    async fn succeeded_pod_excluded_from_readiness_count() {
        let client = Arc::new(MockK8sClient {
            pods: vec![
                PodInfo { name: "core-abc".into(), ready: true, restart_count: 0, succeeded: false },
                PodInfo { name: "migrate-job-xyz".into(), ready: false, restart_count: 0, succeeded: true },
            ],
            events: vec![],
        });
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Ok, "succeeded pods must not trigger a warning");
        assert_eq!(check_value(&result, "pods_ready"), "1/1");
        assert_eq!(check_value(&result, "recent_restarts"), "0");
    }
}
