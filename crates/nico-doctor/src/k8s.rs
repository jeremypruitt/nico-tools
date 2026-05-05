use std::time::Duration;
use async_trait::async_trait;
use anyhow::Result;
use chrono::Utc;
use kube::{Client, Api};
use kube::api::{ListParams, LogParams};
use k8s_openapi::api::core::v1::{Pod, Event as CoreEvent};

pub struct PodInfo {
    pub name: String,
    pub ready: bool,
    pub restart_count: u32,
}

pub struct EventInfo {
    #[allow(dead_code)]
    pub message: String,
    #[allow(dead_code)]
    pub reason: String,
}

#[async_trait]
pub trait K8sClient: Send + Sync {
    async fn list_pods(&self, namespace: &str) -> Result<Vec<PodInfo>>;
    async fn list_events(&self, namespace: &str, since: Duration) -> Result<Vec<EventInfo>>;
    async fn pod_logs(&self, namespace: &str, pod: &str, since: Duration) -> Result<Vec<String>>;
}

pub struct KubeRsK8sClient {
    client: Client,
}

impl KubeRsK8sClient {
    pub async fn try_new(context: Option<&str>) -> Result<Self> {
        let client = if let Some(ctx) = context {
            use kube::config::KubeConfigOptions;
            let config = kube::Config::from_kubeconfig(&KubeConfigOptions {
                context: Some(ctx.to_string()),
                ..Default::default()
            }).await?;
            Client::try_from(config)?
        } else {
            Client::try_default().await?
        };
        Ok(Self { client })
    }
}

#[async_trait]
impl K8sClient for KubeRsK8sClient {
    async fn list_pods(&self, namespace: &str) -> Result<Vec<PodInfo>> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod_list = api.list(&ListParams::default()).await
            .map_err(|e| anyhow::anyhow!("k8s list_pods failed: {e}"))?;

        let infos = pod_list.items.into_iter().map(|pod| {
            let name = pod.metadata.name.unwrap_or_default();
            let container_statuses = pod.status.as_ref()
                .and_then(|s| s.container_statuses.as_ref());
            let ready = container_statuses
                .map(|cs| !cs.is_empty() && cs.iter().all(|c| c.ready))
                .unwrap_or(false);
            let restart_count: u32 = container_statuses
                .map(|cs| cs.iter().map(|c| c.restart_count.max(0) as u32).sum())
                .unwrap_or(0);
            PodInfo { name, ready, restart_count }
        }).collect();

        Ok(infos)
    }

    async fn list_events(&self, namespace: &str, since: Duration) -> Result<Vec<EventInfo>> {
        let api: Api<CoreEvent> = Api::namespaced(self.client.clone(), namespace);
        let event_list = api.list(&ListParams::default()).await
            .map_err(|e| anyhow::anyhow!("k8s list_events failed: {e}"))?;

        let since_chrono = chrono::Duration::from_std(since)
            .unwrap_or_else(|_| chrono::Duration::hours(24));
        let cutoff = Utc::now() - since_chrono;

        let infos = event_list.items.into_iter().filter_map(|e| {
            if e.type_.as_deref() != Some("Warning") {
                return None;
            }
            let ts = e.last_timestamp.as_ref().map(|t| t.0)
                .or_else(|| e.first_timestamp.as_ref().map(|t| t.0))?;
            if ts < cutoff {
                return None;
            }
            let message = e.message.unwrap_or_default();
            let reason = e.reason.unwrap_or_default();
            Some(EventInfo { message, reason })
        }).collect();

        Ok(infos)
    }

    async fn pod_logs(&self, namespace: &str, pod: &str, since: Duration) -> Result<Vec<String>> {
        let api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = LogParams {
            since_seconds: Some(since.as_secs() as i64),
            ..Default::default()
        };
        let log_data = api.logs(pod, &lp).await
            .map_err(|e| anyhow::anyhow!("k8s pod_logs failed: {e}"))?;
        Ok(log_data.lines().map(|l| l.to_string()).collect())
    }
}
