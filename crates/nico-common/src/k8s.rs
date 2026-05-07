//! Low-level Kubernetes client primitives shared by `nico-doctor` and
//! `nico-correlate`.
//!
//! Both binaries previously defined their own higher-level `K8sClient`
//! traits. The version here exposes only the primitive operations actually
//! issued against `kube::Client` (list pods, list events, fetch logs);
//! callers compose these into their own domain views.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Event as CoreEvent, Pod};
use kube::api::{ListParams, LogParams};
use kube::{Api, Client};

use crate::bootstrap::{run_with_budget, BootstrapStepError};

/// Where to look for pods.
#[derive(Debug, Clone)]
pub enum PodScope<'a> {
    /// All pods in a namespace.
    Namespace(&'a str),
    /// Pods across all namespaces matching a label selector
    /// (e.g. `workflow_id=hp-abc`).
    AllWithLabel(&'a str),
}

/// Minimal pod shape needed by both crates.
#[derive(Debug, Clone)]
pub struct RawPod {
    pub name: String,
    pub namespace: String,
    pub phase: Option<String>,
    pub ready: bool,
    pub restart_count: u32,
    pub succeeded: bool,
    pub crash_loop: bool,
}

/// Minimal Kubernetes event shape needed by both crates. Callers
/// typically filter by `event_type == "Warning"` and a since-cutoff.
#[derive(Debug, Clone)]
pub struct RawEvent {
    pub ts: Option<DateTime<Utc>>,
    pub event_type: Option<String>,
    pub reason: Option<String>,
    pub message: Option<String>,
}

#[async_trait]
pub trait K8sClient: Send + Sync {
    async fn list_pods(&self, scope: PodScope<'_>) -> Result<Vec<RawPod>>;

    /// List all events in `namespace`, optionally narrowed by a field selector
    /// (e.g. `involvedObject.name=foo,type=Warning`). The caller filters by
    /// type / time as needed.
    async fn list_events(
        &self,
        namespace: &str,
        field_selector: Option<&str>,
    ) -> Result<Vec<RawEvent>>;

    async fn pod_logs(
        &self,
        namespace: &str,
        pod: &str,
        since: Duration,
    ) -> Result<Vec<String>>;
}

pub(crate) fn map_pod(pod: Pod) -> RawPod {
    let name = pod.metadata.name.clone().unwrap_or_default();
    let namespace = pod.metadata.namespace.clone().unwrap_or_default();
    let status = pod.status.as_ref();
    let phase = status.and_then(|s| s.phase.clone());
    let succeeded = phase.as_deref() == Some("Succeeded");

    let container_statuses = status.and_then(|s| s.container_statuses.as_ref());
    let ready = container_statuses
        .map(|cs| !cs.is_empty() && cs.iter().all(|c| c.ready))
        .unwrap_or(false);
    let restart_count: u32 = container_statuses
        .map(|cs| cs.iter().map(|c| c.restart_count.max(0) as u32).sum())
        .unwrap_or(0);
    let crash_loop = container_statuses
        .map(|cs| {
            cs.iter().any(|c| {
                c.state
                    .as_ref()
                    .and_then(|s| s.waiting.as_ref())
                    .and_then(|w| w.reason.as_deref())
                    == Some("CrashLoopBackOff")
            })
        })
        .unwrap_or(false);

    RawPod {
        name,
        namespace,
        phase,
        ready,
        restart_count,
        succeeded,
        crash_loop,
    }
}

pub(crate) fn map_event(event: CoreEvent) -> RawEvent {
    let ts = event
        .last_timestamp
        .as_ref()
        .map(|t| t.0)
        .or_else(|| event.first_timestamp.as_ref().map(|t| t.0));
    RawEvent {
        ts,
        event_type: event.type_,
        reason: event.reason,
        message: event.message,
    }
}

/// Real `kube::Client`-backed implementation.
pub struct KubeRsK8sClient {
    client: Client,
}

impl KubeRsK8sClient {
    /// Build a client from kubeconfig, bounded by `budget`. Exceeding the
    /// budget surfaces `BootstrapStepError::TimedOut(budget)`; any other
    /// error is wrapped in `BootstrapStepError::Other`.
    pub async fn try_new(
        context: Option<&str>,
        budget: Duration,
    ) -> Result<Self, BootstrapStepError> {
        let context = context.map(str::to_owned);
        run_with_budget(budget, async move {
            let client = if let Some(ctx) = context {
                use kube::config::KubeConfigOptions;
                let config = kube::Config::from_kubeconfig(&KubeConfigOptions {
                    context: Some(ctx),
                    ..Default::default()
                })
                .await?;
                Client::try_from(config)?
            } else {
                Client::try_default().await?
            };
            Ok(Self { client })
        }).await
    }

    pub fn raw_client(&self) -> &Client {
        &self.client
    }
}

#[async_trait]
impl K8sClient for KubeRsK8sClient {
    async fn list_pods(&self, scope: PodScope<'_>) -> Result<Vec<RawPod>> {
        match scope {
            PodScope::Namespace(ns) => {
                let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
                let list = api
                    .list(&ListParams::default())
                    .await
                    .map_err(|e| anyhow::anyhow!("k8s list_pods failed: {e}"))?;
                Ok(list.items.into_iter().map(map_pod).collect())
            }
            PodScope::AllWithLabel(label_selector) => {
                let api: Api<Pod> = Api::all(self.client.clone());
                let list = api
                    .list(&ListParams::default().labels(label_selector))
                    .await
                    .map_err(|e| anyhow::anyhow!("k8s list_pods failed: {e}"))?;
                Ok(list.items.into_iter().map(map_pod).collect())
            }
        }
    }

    async fn list_events(
        &self,
        namespace: &str,
        field_selector: Option<&str>,
    ) -> Result<Vec<RawEvent>> {
        let api: Api<CoreEvent> = Api::namespaced(self.client.clone(), namespace);
        let mut params = ListParams::default();
        if let Some(fs) = field_selector {
            params = params.fields(fs);
        }
        let list = api
            .list(&params)
            .await
            .map_err(|e| anyhow::anyhow!("k8s list_events failed: {e}"))?;
        Ok(list.items.into_iter().map(map_event).collect())
    }

    async fn pod_logs(
        &self,
        namespace: &str,
        pod: &str,
        since: Duration,
    ) -> Result<Vec<String>> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = LogParams {
            since_seconds: Some(since.as_secs() as i64),
            ..Default::default()
        };
        let log_data = api
            .logs(pod, &lp)
            .await
            .map_err(|e| anyhow::anyhow!("k8s pod_logs failed: {e}"))?;
        Ok(log_data.lines().map(|l| l.to_string()).collect())
    }
}

/// Test fakes shared across both crates' test suites.
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// Configurable in-memory fake. Set whichever return values are needed
    /// for a given test. Unconfigured methods return empty results.
    pub struct MockK8sClient {
        pub pods: Mutex<Result<Vec<RawPod>>>,
        pub events: Mutex<Result<Vec<RawEvent>>>,
        pub logs: Mutex<Result<Vec<String>>>,
        /// If set, `list_pods` returns these pods only when the scope is a
        /// label-selector match for this exact selector. Useful for
        /// correlate-style tests that depend on label scoping.
        pub require_label_selector: Option<String>,
    }

    impl Default for MockK8sClient {
        fn default() -> Self {
            Self {
                pods: Mutex::new(Ok(vec![])),
                events: Mutex::new(Ok(vec![])),
                logs: Mutex::new(Ok(vec![])),
                require_label_selector: None,
            }
        }
    }

    impl MockK8sClient {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_pods(mut self, pods: Vec<RawPod>) -> Self {
            self.pods = Mutex::new(Ok(pods));
            self
        }

        pub fn with_pods_err(mut self, err: impl std::fmt::Display) -> Self {
            self.pods = Mutex::new(Err(anyhow::anyhow!("{err}")));
            self
        }

        pub fn with_events(mut self, events: Vec<RawEvent>) -> Self {
            self.events = Mutex::new(Ok(events));
            self
        }

        pub fn with_events_err(mut self, err: impl std::fmt::Display) -> Self {
            self.events = Mutex::new(Err(anyhow::anyhow!("{err}")));
            self
        }

        pub fn with_logs(mut self, logs: Vec<String>) -> Self {
            self.logs = Mutex::new(Ok(logs));
            self
        }
    }

    fn clone_result<T: Clone>(slot: &Mutex<Result<Vec<T>>>) -> Result<Vec<T>> {
        let guard = slot.lock().unwrap();
        match &*guard {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }

    #[async_trait]
    impl K8sClient for MockK8sClient {
        async fn list_pods(&self, scope: PodScope<'_>) -> Result<Vec<RawPod>> {
            if let (Some(required), PodScope::AllWithLabel(actual)) =
                (self.require_label_selector.as_deref(), &scope)
            && required != *actual
            {
                return Ok(vec![]);
            }
            clone_result(&self.pods)
        }

        async fn list_events(
            &self,
            _namespace: &str,
            _field_selector: Option<&str>,
        ) -> Result<Vec<RawEvent>> {
            clone_result(&self.events)
        }

        async fn pod_logs(
            &self,
            _namespace: &str,
            _pod: &str,
            _since: Duration,
        ) -> Result<Vec<String>> {
            clone_result(&self.logs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{ContainerState, ContainerStateWaiting, ContainerStatus, PodStatus};
    use kube::api::ObjectMeta;

    fn pod(name: &str, phase: &str) -> Pod {
        Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some("nico".into()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pod_with_containers(name: &str, ready: bool, restart_count: i32, crash_loop: bool) -> Pod {
        let cs = ContainerStatus {
            ready,
            restart_count,
            state: if crash_loop {
                Some(ContainerState {
                    waiting: Some(ContainerStateWaiting {
                        reason: Some("CrashLoopBackOff".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            } else {
                None
            },
            ..Default::default()
        };
        Pod {
            metadata: ObjectMeta {
                name: Some(name.into()),
                namespace: Some("nico".into()),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some("Running".into()),
                container_statuses: Some(vec![cs]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn map_pod_extracts_name_namespace_and_phase() {
        let raw = map_pod(pod("core-abc", "Running"));
        assert_eq!(raw.name, "core-abc");
        assert_eq!(raw.namespace, "nico");
        assert_eq!(raw.phase.as_deref(), Some("Running"));
        assert!(!raw.succeeded);
    }

    #[test]
    fn map_pod_succeeded_phase_sets_succeeded_true() {
        let raw = map_pod(pod("job-xyz", "Succeeded"));
        assert!(raw.succeeded);
    }

    #[test]
    fn map_pod_ready_container_sets_ready_true_and_restart_count() {
        let raw = map_pod(pod_with_containers("core-abc", true, 3, false));
        assert!(raw.ready);
        assert_eq!(raw.restart_count, 3);
        assert!(!raw.crash_loop);
    }

    #[test]
    fn map_pod_crash_loop_back_off_sets_crash_loop_true() {
        let raw = map_pod(pod_with_containers("bad-pod", false, 5, true));
        assert!(raw.crash_loop);
        assert!(!raw.ready);
    }

    #[tokio::test]
    async fn mock_list_pods_returns_configured_pods() {
        use testing::MockK8sClient;
        let pods = vec![RawPod {
            name: "core-abc".into(),
            namespace: "nico".into(),
            phase: Some("Running".into()),
            ready: true,
            restart_count: 0,
            succeeded: false,
            crash_loop: false,
        }];
        let client = MockK8sClient::new().with_pods(pods);
        let out = client.list_pods(PodScope::Namespace("nico")).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "core-abc");
    }

    #[tokio::test]
    async fn mock_list_pods_returns_err_when_configured_with_err() {
        use testing::MockK8sClient;
        let client = MockK8sClient::new().with_pods_err("cluster unreachable");
        let result = client.list_pods(PodScope::Namespace("nico")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cluster unreachable"));
    }

    #[tokio::test]
    async fn mock_label_selector_filters_pods() {
        use testing::MockK8sClient;
        let pods = vec![RawPod {
            name: "match".into(),
            namespace: "nico".into(),
            phase: None,
            ready: true,
            restart_count: 0,
            succeeded: false,
            crash_loop: false,
        }];
        let mut client = MockK8sClient::new().with_pods(pods);
        client.require_label_selector = Some("workflow_id=hp-abc".into());

        let matched = client.list_pods(PodScope::AllWithLabel("workflow_id=hp-abc")).await.unwrap();
        assert_eq!(matched.len(), 1);

        let unmatched = client.list_pods(PodScope::AllWithLabel("workflow_id=other")).await.unwrap();
        assert!(unmatched.is_empty());
    }
}
