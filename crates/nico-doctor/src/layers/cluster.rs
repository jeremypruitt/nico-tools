use std::sync::Arc;
use std::time::{Duration, SystemTime};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::k8s::{K8sClient, PodScope, RawEvent, RawPod};
use nico_common::output::Status;
use crate::layer::{Check, CheckKind, Layer, LayerOutcome, RunOpts};

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

    async fn collect(&self, opts: &RunOpts) -> LayerOutcome {
        let all_pods = self
            .k8s
            .list_pods(PodScope::Namespace(&opts.namespace))
            .await
            .unwrap_or_default();
        let raw_events = self
            .k8s
            .list_events(&opts.namespace, None)
            .await
            .unwrap_or_default();
        let warning_events = filter_warning_events(&raw_events, SystemTime::now(), opts.since);
        let pods: Vec<&RawPod> = all_pods.iter().filter(|p| !p.succeeded).collect();
        LayerOutcome::Checks(checks_from(&pods, &warning_events, &opts.namespace))
    }
}

fn filter_warning_events(
    events: &[RawEvent],
    now: SystemTime,
    since: Duration,
) -> Vec<&RawEvent> {
    let now_dt: DateTime<Utc> = now.into();
    let cutoff = now_dt
        - chrono::Duration::from_std(since).unwrap_or_else(|_| chrono::Duration::hours(24));
    events
        .iter()
        .filter(|e| e.event_type.as_deref() == Some("Warning"))
        .filter(|e| e.ts.map(|t| t >= cutoff).unwrap_or(false))
        .collect()
}

fn checks_from(pods: &[&RawPod], warning_events: &[&RawEvent], namespace: &str) -> Vec<Check> {
    let total = pods.len();
    let ready = pods.iter().filter(|p| p.ready).count();
    let restarts: u32 = pods.iter().map(|p| p.restart_count).sum();
    let warn_events = warning_events.len();

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
            kind: CheckKind::Headline,
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
            kind: CheckKind::Headline,
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
            kind: CheckKind::Headline,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use nico_common::k8s::testing::MockK8sClient;
    use nico_common::output::Status;
    use crate::layer::LayerResult;

    fn pod(name: &str, ready: bool, restart_count: u32, succeeded: bool) -> RawPod {
        RawPod {
            name: name.into(),
            namespace: "nico".into(),
            phase: None,
            ready,
            restart_count,
            succeeded,
            crash_loop: false,
        }
    }

    fn warning_event(reason: &str) -> RawEvent {
        RawEvent {
            ts: Some(Utc::now()),
            event_type: Some("Warning".into()),
            reason: Some(reason.into()),
            message: Some(reason.into()),
        }
    }

    #[test]
    fn checks_from_all_healthy_returns_three_ok_checks() {
        let p1 = pod("core-abc", true, 0, false);
        let p2 = pod("rest-xyz", true, 0, false);
        let pods = vec![&p1, &p2];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");
        assert_eq!(checks.len(), 3);
        assert!(checks.iter().all(|c| c.status == Status::Ok));
    }

    #[test]
    fn checks_from_not_ready_pod_and_warning_event_returns_warn_statuses() {
        let p1 = pod("core-abc", false, 2, false);
        let pods = vec![&p1];
        let e1 = warning_event("OOMKilling");
        let events = vec![&e1];
        let checks = checks_from(&pods, &events, "nico");
        assert_eq!(checks.len(), 3);
        assert!(checks.iter().all(|c| c.status == Status::Warn));
    }

    #[test]
    fn filter_warning_events_drops_normal_events_and_old_events() {
        let now = SystemTime::now();
        let recent: DateTime<Utc> = now.into();
        let old = recent - chrono::Duration::hours(48);
        let events = vec![
            RawEvent {
                ts: Some(recent),
                event_type: Some("Warning".into()),
                reason: Some("OOM".into()),
                message: None,
            },
            RawEvent {
                ts: Some(recent),
                event_type: Some("Normal".into()),
                reason: None,
                message: None,
            },
            RawEvent {
                ts: Some(old),
                event_type: Some("Warning".into()),
                reason: None,
                message: None,
            },
        ];
        let filtered = filter_warning_events(&events, now, Duration::from_secs(600));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].reason.as_deref(), Some("OOM"));
    }

    fn opts() -> RunOpts {
        RunOpts {
            namespace: "nico".into(),
            since: Duration::from_secs(600),
            timeout: Duration::from_secs(5),
        }
    }

    fn check_value<'a>(result: &'a LayerResult, name: &str) -> &'a str {
        result
            .checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.value.as_str())
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    fn check_status<'a>(result: &'a LayerResult, name: &str) -> &'a Status {
        result
            .checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.status)
            .unwrap_or_else(|| panic!("check '{name}' not found"))
    }

    #[tokio::test]
    async fn warning_events_report_warn() {
        let client = Arc::new(
            MockK8sClient::new()
                .with_pods(vec![pod("core-abc", true, 0, false)])
                .with_events(vec![warning_event("OOMKilling"), warning_event("BackOff")]),
        );
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Warn);
        assert_eq!(check_value(&result, "warning_events"), "2");
        assert_eq!(check_status(&result, "warning_events"), &Status::Warn);
        assert_eq!(check_status(&result, "pods_ready"), &Status::Ok);
    }

    #[tokio::test]
    async fn pod_with_recent_restarts_reports_warn() {
        let client = Arc::new(MockK8sClient::new().with_pods(vec![
            pod("core-abc", true, 3, false),
            pod("rest-xyz", true, 0, false),
        ]));
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Warn);
        assert_eq!(check_value(&result, "recent_restarts"), "3");
        assert_eq!(check_status(&result, "recent_restarts"), &Status::Warn);
        assert_eq!(check_status(&result, "pods_ready"), &Status::Ok);
    }

    #[tokio::test]
    async fn pod_not_ready_reports_warn() {
        let client = Arc::new(MockK8sClient::new().with_pods(vec![
            pod("core-abc", true, 0, false),
            pod("rest-xyz", false, 0, false),
        ]));
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Warn);
        assert_eq!(check_value(&result, "pods_ready"), "1/2");
        assert_eq!(check_status(&result, "pods_ready"), &Status::Warn);
        assert!(result
            .checks
            .iter()
            .find(|c| c.name == "pods_ready")
            .and_then(|c| c.next_command.as_deref())
            .is_some());
    }

    #[tokio::test]
    async fn all_pods_ready_no_issues_reports_ok() {
        let client = Arc::new(MockK8sClient::new().with_pods(vec![
            pod("core-abc", true, 0, false),
            pod("rest-xyz", true, 0, false),
        ]));
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Ok);
        assert_eq!(check_value(&result, "pods_ready"), "2/2");
        assert_eq!(check_value(&result, "recent_restarts"), "0");
        assert_eq!(check_value(&result, "warning_events"), "0");
    }

    #[tokio::test]
    async fn succeeded_pod_excluded_from_readiness_count() {
        let client = Arc::new(MockK8sClient::new().with_pods(vec![
            pod("core-abc", true, 0, false),
            pod("migrate-job-xyz", false, 0, true),
        ]));
        let result = ClusterLayer::new(client).run(&opts()).await;
        assert_eq!(result.status, Status::Ok, "succeeded pods must not trigger a warning");
        assert_eq!(check_value(&result, "pods_ready"), "1/1");
        assert_eq!(check_value(&result, "recent_restarts"), "0");
    }
}
