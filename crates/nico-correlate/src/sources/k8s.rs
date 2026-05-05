use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult, SourceOutput, SourceUnavailable, StateEntry};
use kube::{Client, Api};
use kube::api::ListParams;
use k8s_openapi::api::core::v1::{Pod, Event as CoreEvent};

#[derive(Clone)]
pub struct K8sPod {
    pub name: String,
    pub status: String,
    pub restart_count: u32,
    pub crash_loop: bool,
}

#[derive(Clone)]
pub struct K8sWarningEvent {
    pub ts: DateTime<Utc>,
    pub pod_name: String,
    pub reason: String,
    pub message: String,
}

#[derive(Clone)]
pub struct K8sPodData {
    pub pod: K8sPod,
    pub warning_events: Vec<K8sWarningEvent>,
}

#[async_trait]
pub trait K8sClient: Send + Sync {
    async fn find_pods_with_events(&self, id: &str, id_type: &IdType) -> Result<Vec<K8sPodData>>;
}

pub struct KubeRsK8sClient {
    client: Client,
}

impl KubeRsK8sClient {
    pub async fn try_default() -> Result<Self> {
        let client = Client::try_default().await?;
        Ok(Self { client })
    }

}

#[async_trait]
impl K8sClient for KubeRsK8sClient {
    async fn find_pods_with_events(&self, id: &str, id_type: &IdType) -> Result<Vec<K8sPodData>> {
        let label_key = match id_type {
            IdType::Workflow => "workflow_id",
            IdType::Host => "host_id",
            IdType::Dpu => "dpu_id",
            IdType::Request => "request_id",
        };
        let label_selector = format!("{label_key}={id}");

        let pods: Api<Pod> = Api::all(self.client.clone());
        let pod_list = pods.list(&ListParams::default().labels(&label_selector)).await
            .map_err(|e| anyhow::anyhow!("k8s pod list failed: {e}"))?;

        let mut results = Vec::new();
        for pod in pod_list.items {
            let name = pod.metadata.name.unwrap_or_default();
            let namespace = pod.metadata.namespace.unwrap_or_else(|| "default".into());

            let status = pod.status.as_ref()
                .and_then(|s| s.phase.as_deref())
                .unwrap_or("Unknown")
                .to_string();

            let restart_count: u32 = pod.status.as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .map(|cs| cs.iter().map(|c| c.restart_count as u32).sum())
                .unwrap_or(0);

            let crash_loop = pod.status.as_ref()
                .and_then(|s| s.container_statuses.as_ref())
                .map(|cs| cs.iter().any(|c| {
                    c.state.as_ref()
                        .and_then(|s| s.waiting.as_ref())
                        .and_then(|w| w.reason.as_deref())
                        == Some("CrashLoopBackOff")
                }))
                .unwrap_or(false);

            let events_api: Api<CoreEvent> = Api::namespaced(self.client.clone(), &namespace);
            let field_selector = format!(
                "involvedObject.kind=Pod,involvedObject.name={name},type=Warning"
            );
            let raw_events = events_api.list(&ListParams::default().fields(&field_selector))
                .await
                .map(|l| l.items)
                .unwrap_or_default();

            let warning_events: Vec<K8sWarningEvent> = raw_events.into_iter().filter_map(|e| {
                let ts = e.last_timestamp
                    .map(|t| t.0)
                    .or_else(|| e.first_timestamp.map(|t| t.0))
                    .unwrap_or_else(Utc::now);
                let reason = e.reason?;
                let message = e.message.unwrap_or_default();
                Some(K8sWarningEvent { ts, pod_name: name.clone(), reason, message })
            }).collect();

            results.push(K8sPodData {
                pod: K8sPod { name, status, restart_count, crash_loop },
                warning_events,
            });
        }

        Ok(results)
    }
}

pub struct K8sSource {
    client: Box<dyn K8sClient>,
}

impl K8sSource {
    pub fn new(client: Box<dyn K8sClient>) -> Self {
        Self { client }
    }
}

fn pod_state_entry(pod: &K8sPod) -> StateEntry {
    let restart_word = if pod.restart_count == 1 { "restart" } else { "restarts" };
    let display_status = if pod.crash_loop { "CrashLoopBackOff" } else { pod.status.as_str() };
    StateEntry {
        source: "k8s",
        key: pod.name.clone(),
        value: format!("{display_status} ({} {})", pod.restart_count, restart_word),
    }
}

fn warning_event_to_event(e: K8sWarningEvent) -> Event {
    Event {
        ts: e.ts,
        source: "k8s".into(),
        kind: e.reason.clone(),
        message: format!("{}: {}", e.pod_name, e.message),
        severity: Severity::Warning,
        tags: Default::default(),
    }
}

#[async_trait]
impl Source for K8sSource {
    fn name(&self) -> &'static str {
        "k8s"
    }

    async fn collect(&self, id: &str, id_type: &IdType) -> SourceResult {
        match self.client.find_pods_with_events(id, id_type).await {
            Ok(pod_data) => {
                let state = pod_data.iter().map(|pd| pod_state_entry(&pd.pod)).collect();
                let events = pod_data.into_iter()
                    .flat_map(|pd| pd.warning_events.into_iter().map(warning_event_to_event))
                    .collect();
                SourceResult::Output(SourceOutput { events, state })
            }
            Err(e) => SourceResult::Unavailable(SourceUnavailable {
                name: "k8s",
                reason: e.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    struct FakeK8sClient {
        result: Result<Vec<K8sPodData>>,
    }

    impl FakeK8sClient {
        fn ok(data: Vec<K8sPodData>) -> Self {
            Self { result: Ok(data) }
        }
        fn err(msg: &str) -> Self {
            Self { result: Err(anyhow::anyhow!(msg.to_string())) }
        }
    }

    #[async_trait]
    impl K8sClient for FakeK8sClient {
        async fn find_pods_with_events(&self, _id: &str, _id_type: &IdType) -> Result<Vec<K8sPodData>> {
            match &self.result {
                Ok(data) => Ok(data.clone()),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            }
        }
    }

    fn ts(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[tokio::test]
    async fn pods_become_state_entries() {
        let data = vec![K8sPodData {
            pod: K8sPod { name: "hp-worker-xyz".into(), status: "Running".into(), restart_count: 3, crash_loop: false },
            warning_events: vec![],
        }];
        let source = K8sSource::new(Box::new(FakeK8sClient::ok(data)));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state.len(), 1);
        assert_eq!(output.state[0].source, "k8s");
        assert_eq!(output.state[0].key, "hp-worker-xyz");
        assert_eq!(output.state[0].value, "Running (3 restarts)");
        assert!(output.events.is_empty());
    }

    #[tokio::test]
    async fn singular_restart_word() {
        let data = vec![K8sPodData {
            pod: K8sPod { name: "hp-worker-xyz".into(), status: "Running".into(), restart_count: 1, crash_loop: false },
            warning_events: vec![],
        }];
        let source = K8sSource::new(Box::new(FakeK8sClient::ok(data)));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state[0].value, "Running (1 restart)");
    }

    #[tokio::test]
    async fn warning_events_map_to_warning_severity_events() {
        let data = vec![K8sPodData {
            pod: K8sPod { name: "hp-worker-xyz".into(), status: "Running".into(), restart_count: 2, crash_loop: false },
            warning_events: vec![K8sWarningEvent {
                ts: ts(1000),
                pod_name: "hp-worker-xyz".into(),
                reason: "OOMKilled".into(),
                message: "container ran out of memory".into(),
            }],
        }];
        let source = K8sSource::new(Box::new(FakeK8sClient::ok(data)));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.events.len(), 1);
        assert_eq!(output.events[0].severity, Severity::Warning);
        assert_eq!(output.events[0].source, "k8s");
        assert_eq!(output.events[0].kind, "OOMKilled");
        assert_eq!(output.events[0].message, "hp-worker-xyz: container ran out of memory");
    }

    #[tokio::test]
    async fn crash_loop_pod_shows_crash_loop_back_off_status() {
        let data = vec![K8sPodData {
            pod: K8sPod { name: "hp-worker-xyz".into(), status: "Running".into(), restart_count: 5, crash_loop: true },
            warning_events: vec![],
        }];
        let source = K8sSource::new(Box::new(FakeK8sClient::ok(data)));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        let output = match result {
            SourceResult::Output(o) => o,
            _ => panic!("expected Output"),
        };
        assert_eq!(output.state[0].value, "CrashLoopBackOff (5 restarts)");
    }

    #[tokio::test]
    async fn unavailable_client_returns_unavailable() {
        let source = K8sSource::new(Box::new(FakeK8sClient::err("cluster unreachable")));
        let result = source.collect("hp-abc", &IdType::Workflow).await;
        match result {
            SourceResult::Unavailable(u) => {
                assert_eq!(u.name, "k8s");
                assert!(u.reason.contains("cluster unreachable"));
            }
            _ => panic!("expected Unavailable"),
        }
    }

    #[tokio::test]
    async fn smoke_real_k8s_skips_when_no_kubeconfig() {
        let client = match KubeRsK8sClient::try_default().await {
            Ok(c) => c,
            Err(_) => return,
        };
        // Ok or Err both accepted; cluster may be unreachable in CI
        let _ = client.find_pods_with_events("hp-smoke-test", &IdType::Workflow).await;
        let _ = client.find_pods_with_events("host-r12u5", &IdType::Host).await;
    }
}
