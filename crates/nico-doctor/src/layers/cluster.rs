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

    let mut checks = vec![
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
    ];

    for p in pods.iter().filter(|p| p.restart_count > 0) {
        checks.push(Check {
            name: "pod_restart",
            status: Status::Warn,
            value: format!("{}: {} restarts", p.name, p.restart_count),
            next_command: Some(format!("kubectl describe pod {} -n {namespace}", p.name)),
            kind: CheckKind::Detail,
        });
    }

    for p in pods.iter().filter(|p| !p.ready && !p.succeeded) {
        let phase = p.phase.as_deref().unwrap_or("Unknown");
        let mut value = format!("{}: {}", p.name, phase);
        if p.crash_loop {
            value.push_str(" (CrashLoopBackOff)");
        }
        checks.push(Check {
            name: "pod_not_ready",
            status: Status::Warn,
            value,
            next_command: Some(format!("kubectl describe pod {} -n {namespace}", p.name)),
            kind: CheckKind::Detail,
        });
    }

    checks
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

    fn pod_with_phase(name: &str, ready: bool, phase: &str, crash_loop: bool) -> RawPod {
        RawPod {
            name: name.into(),
            namespace: "nico".into(),
            phase: Some(phase.into()),
            ready,
            restart_count: 0,
            succeeded: false,
            crash_loop,
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
        let headline: Vec<_> = checks.iter().filter(|c| c.kind == CheckKind::Headline).collect();
        assert_eq!(headline.len(), 3);
        assert!(headline.iter().all(|c| c.status == Status::Warn));
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

    #[test]
    fn checks_from_zero_restarting_pods_emits_no_pod_restart_checks() {
        let p1 = pod("core-abc", true, 0, false);
        let p2 = pod("rest-xyz", true, 0, false);
        let pods = vec![&p1, &p2];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        assert_eq!(checks.iter().filter(|c| c.name == "pod_restart").count(), 0);
    }

    #[test]
    fn checks_from_multiple_restarting_pods_emits_one_pod_restart_each() {
        let p1 = pod("core-abc", true, 3, false);
        let p2 = pod("rest-xyz", true, 0, false);
        let p3 = pod("workflow-svc", true, 5, false);
        let pods = vec![&p1, &p2, &p3];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        let pod_restarts: Vec<_> = checks.iter().filter(|c| c.name == "pod_restart").collect();
        assert_eq!(pod_restarts.len(), 2);

        let values: Vec<_> = pod_restarts.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"core-abc: 3 restarts"));
        assert!(values.contains(&"workflow-svc: 5 restarts"));

        let cmds: Vec<_> = pod_restarts.iter().filter_map(|c| c.next_command.as_deref()).collect();
        assert!(cmds.contains(&"kubectl describe pod core-abc -n nico"));
        assert!(cmds.contains(&"kubectl describe pod workflow-svc -n nico"));

        let recent = checks.iter().find(|c| c.name == "recent_restarts").unwrap();
        assert_eq!(recent.value, "8");
        assert_eq!(recent.kind, CheckKind::Headline);
    }

    #[test]
    fn checks_from_pod_with_restarts_emits_detail_pod_restart_check() {
        let p1 = pod("core-abc", true, 3, false);
        let pods = vec![&p1];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        let pod_restarts: Vec<_> = checks.iter().filter(|c| c.name == "pod_restart").collect();
        assert_eq!(pod_restarts.len(), 1);
        let pr = pod_restarts[0];
        assert_eq!(pr.kind, CheckKind::Detail);
        assert_eq!(pr.status, Status::Warn);
        assert_eq!(pr.value, "core-abc: 3 restarts");
        assert_eq!(
            pr.next_command.as_deref(),
            Some("kubectl describe pod core-abc -n nico"),
        );
    }

    #[test]
    fn checks_from_single_not_ready_pod_emits_one_pod_not_ready_detail() {
        let p1 = pod_with_phase("core-abc", false, "Pending", false);
        let pods = vec![&p1];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        let details: Vec<_> = checks.iter().filter(|c| c.name == "pod_not_ready").collect();
        assert_eq!(details.len(), 1);
        let pnr = details[0];
        assert_eq!(pnr.kind, CheckKind::Detail);
        assert_eq!(pnr.status, Status::Warn);
        assert_eq!(pnr.value, "core-abc: Pending");
        assert_eq!(
            pnr.next_command.as_deref(),
            Some("kubectl describe pod core-abc -n nico"),
        );
    }

    #[test]
    fn checks_from_crash_loop_pod_appends_crashloopbackoff_suffix() {
        let p1 = pod_with_phase("core-abc", false, "Running", true);
        let pods = vec![&p1];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        let pnr = checks
            .iter()
            .find(|c| c.name == "pod_not_ready")
            .expect("pod_not_ready check");
        assert_eq!(pnr.value, "core-abc: Running (CrashLoopBackOff)");
    }

    #[test]
    fn checks_from_succeeded_pod_emits_no_pod_not_ready_check() {
        let p1 = RawPod {
            name: "migrate-job-xyz".into(),
            namespace: "nico".into(),
            phase: Some("Succeeded".into()),
            ready: false,
            restart_count: 0,
            succeeded: true,
            crash_loop: false,
        };
        let pods = vec![&p1];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        assert_eq!(checks.iter().filter(|c| c.name == "pod_not_ready").count(), 0);
    }

    #[test]
    fn checks_from_all_ready_cluster_emits_no_pod_not_ready_checks() {
        let p1 = pod("core-abc", true, 0, false);
        let p2 = pod("rest-xyz", true, 0, false);
        let pods = vec![&p1, &p2];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        assert_eq!(checks.iter().filter(|c| c.name == "pod_not_ready").count(), 0);
        let pods_ready = checks
            .iter()
            .find(|c| c.name == "pods_ready")
            .expect("pods_ready");
        assert_eq!(pods_ready.value, "2/2");
        assert_eq!(pods_ready.kind, CheckKind::Headline);
    }

    #[test]
    fn checks_from_mixed_cluster_emits_one_pod_not_ready_per_non_ready_pod() {
        let p1 = pod_with_phase("core-abc", true, "Running", false);
        let p2 = pod_with_phase("rest-xyz", false, "Pending", false);
        let p3 = pod_with_phase("workflow-svc", false, "Running", true);
        let pods = vec![&p1, &p2, &p3];
        let events: Vec<&RawEvent> = vec![];
        let checks = checks_from(&pods, &events, "nico");

        let details: Vec<_> = checks.iter().filter(|c| c.name == "pod_not_ready").collect();
        assert_eq!(details.len(), 2);
        assert!(details.iter().all(|c| c.kind == CheckKind::Detail));
        assert!(details.iter().all(|c| c.status == Status::Warn));

        let values: Vec<_> = details.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"rest-xyz: Pending"));
        assert!(values.contains(&"workflow-svc: Running (CrashLoopBackOff)"));

        let cmds: Vec<_> = details.iter().filter_map(|c| c.next_command.as_deref()).collect();
        assert!(cmds.contains(&"kubectl describe pod rest-xyz -n nico"));
        assert!(cmds.contains(&"kubectl describe pod workflow-svc -n nico"));

        let pods_ready = checks
            .iter()
            .find(|c| c.name == "pods_ready")
            .expect("pods_ready");
        assert_eq!(pods_ready.value, "1/3");
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
