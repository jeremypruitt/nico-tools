//! `nico doctor dpu-health <machine-id>` layer.
//!
//! Wraps a [`DpuHealthClient`] and reduces the fetched [`HealthSnapshot`]
//! to a headline + grouped detail bullets via the pure [`assemble_checks`]
//! / [`assemble_no_status_checks`] / [`assemble_error_checks`] trio in
//! [`crate::dpu_health`].

use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use chrono::Utc;

use crate::dpu_health::{
    self, DpuHealthClient, DEFAULT_DHCP_STALE_THRESHOLD,
};
use crate::layer::{Layer, LayerOutcome, RunOpts};

pub struct DpuHealthLayer {
    client: Arc<dyn DpuHealthClient>,
    dpu_id: String,
    dhcp_stale_threshold: Duration,
}

impl DpuHealthLayer {
    pub fn new(client: Arc<dyn DpuHealthClient>, dpu_id: impl Into<String>) -> Self {
        Self {
            client,
            dpu_id: dpu_id.into(),
            dhcp_stale_threshold: DEFAULT_DHCP_STALE_THRESHOLD,
        }
    }

    pub fn with_dhcp_stale_threshold(mut self, threshold: Duration) -> Self {
        self.dhcp_stale_threshold = threshold;
        self
    }
}

#[async_trait]
impl Layer for DpuHealthLayer {
    fn name(&self) -> &'static str {
        "dpu_health"
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_snapshot(&self.dpu_id).await {
            Ok(Some(snapshot)) => LayerOutcome::Checks(dpu_health::assemble_checks(
                &snapshot,
                Utc::now(),
                self.dhcp_stale_threshold,
            )),
            Ok(None) => LayerOutcome::Checks(dpu_health::assemble_no_status_checks(&self.dpu_id)),
            Err(e) => LayerOutcome::Checks(dpu_health::assemble_error_checks(
                &self.dpu_id,
                &e.to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use nico_common::output::Status;

    use crate::dpu_health::{AgentAlert, HealthSnapshot, InterfaceDhcp};

    struct StubClient {
        result: std::sync::Mutex<Option<Result<Option<HealthSnapshot>, String>>>,
    }

    impl StubClient {
        fn ok(snap: Option<HealthSnapshot>) -> Arc<dyn DpuHealthClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(snap))),
            })
        }
        fn err(msg: &str) -> Arc<dyn DpuHealthClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Err(msg.to_string()))),
            })
        }
    }

    #[async_trait]
    impl DpuHealthClient for StubClient {
        async fn fetch_snapshot(&self, _dpu_id: &str) -> Result<Option<HealthSnapshot>> {
            match self
                .result
                .lock()
                .unwrap()
                .take()
                .expect("fetch_snapshot called twice")
            {
                Ok(s) => Ok(s),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    fn snap_healthy() -> HealthSnapshot {
        HealthSnapshot {
            dpu_id: "dpu-42".into(),
            agent_version: Some("2.0.0".into()),
            agent_version_superseded_at: None,
            alerts: vec![],
            interfaces: vec![],
        }
    }

    #[tokio::test]
    async fn healthy_snapshot_runs_as_ok_layer() {
        let layer = DpuHealthLayer::new(StubClient::ok(Some(snap_healthy())), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.name, "dpu_health");
        assert_eq!(result.status, Status::Ok);
        assert!(result.checks[0].value.contains("healthy"));
    }

    #[tokio::test]
    async fn missing_machine_row_runs_as_unknown_layer() {
        let layer = DpuHealthLayer::new(StubClient::ok(None), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("no machines row"));
    }

    #[tokio::test]
    async fn alert_runs_as_fail_layer() {
        let mut snap = snap_healthy();
        snap.alerts = vec![AgentAlert {
            id: "HeartbeatTimeout".into(),
            target: Some("dpu-42".into()),
            message: "no health report received".into(),
            in_alert_since: None,
        }];
        let layer = DpuHealthLayer::new(StubClient::ok(Some(snap)), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
    }

    #[tokio::test]
    async fn agent_version_drift_runs_as_warn_layer() {
        let mut snap = snap_healthy();
        snap.agent_version_superseded_at = Some(Utc::now() - chrono::Duration::days(2));
        let layer = DpuHealthLayer::new(StubClient::ok(Some(snap)), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Warn);
    }

    #[tokio::test]
    async fn client_error_runs_as_unknown_with_message() {
        let layer = DpuHealthLayer::new(StubClient::err("postgres unreachable"), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }

    #[tokio::test]
    async fn custom_dhcp_threshold_changes_layer_status() {
        let mut snap = snap_healthy();
        snap.interfaces = vec![InterfaceDhcp {
            mac_address: "aa:bb:cc:dd:ee:ff".into(),
            last_dhcp: Some(Utc::now() - chrono::Duration::minutes(45)),
        }];
        // Default 4h threshold ⇒ Ok
        let layer_default = DpuHealthLayer::new(StubClient::ok(Some(snap.clone())), "dpu-42");
        assert_eq!(
            layer_default.run(&RunOpts::default()).await.status,
            Status::Ok
        );

        // Tighter 30m threshold ⇒ Warn
        let layer_tight = DpuHealthLayer::new(StubClient::ok(Some(snap)), "dpu-42")
            .with_dhcp_stale_threshold(Duration::from_secs(30 * 60));
        assert_eq!(
            layer_tight.run(&RunOpts::default()).await.status,
            Status::Warn
        );
    }
}
