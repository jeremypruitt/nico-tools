//! `dpu` layer — fleet-wide DPU/HBN roll-up (issue #214).
//!
//! Wraps a [`DpuClient`] (forgedb / Postgres in production, mocks in
//! tests) and reduces the fetched fleet snapshot to the five sub-checks
//! defined in [`crate::dpu::assemble_checks`]. All non-trivial logic
//! (drift age, threshold rules, top-N, headline-vs-detail) lives in
//! [`crate::dpu`] and is unit-tested without touching this layer.

use std::sync::Arc;
use async_trait::async_trait;
use chrono::Utc;
use nico_common::output::Status;

use crate::bootstrap::LayerInputs;
use crate::dpu::{self, DpuClient, DpuConfig, SqlxDpuClient};
use crate::layer::{self, Check, CheckKind, Layer, LayerOutcome, RunOpts};

pub const NAME: &str = "dpu";

/// Factory consumed by `bootstrap::prepare_layers`.
pub fn register(inputs: &LayerInputs) -> Box<dyn Layer> {
    match SqlxDpuClient::new(&inputs.postgres_url) {
        Ok(client) => Box::new(DpuLayer::new(Arc::new(client), inputs.dpu_config)),
        Err(_) => layer::UnconfiguredLayer::new(NAME, "invalid postgres URL"),
    }
}

pub struct DpuLayer {
    client: Arc<dyn DpuClient>,
    config: DpuConfig,
}

impl DpuLayer {
    pub fn new(client: Arc<dyn DpuClient>, config: DpuConfig) -> Self {
        Self { client, config }
    }
}

#[async_trait]
impl Layer for DpuLayer {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_fleet().await {
            Ok(fleet) => LayerOutcome::Checks(dpu::assemble_checks(&fleet, Utc::now(), &self.config)),
            Err(e) => LayerOutcome::Checks(vec![Check {
                name: "dpu",
                status: Status::Unknown,
                value: format!("dpu fleet query failed: {e}"),
                next_command: Some("kubectl get svc -n nico | grep postgres".into()),
                kind: CheckKind::Headline,
            }]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use chrono::Duration as ChronoDuration;

    use crate::dpu::DpuSnapshot;

    struct StubClient {
        fleet: std::sync::Mutex<Option<Result<Vec<DpuSnapshot>, String>>>,
    }

    impl StubClient {
        fn ok(fleet: Vec<DpuSnapshot>) -> Arc<dyn DpuClient> {
            Arc::new(Self {
                fleet: std::sync::Mutex::new(Some(Ok(fleet))),
            })
        }
        fn err(msg: &str) -> Arc<dyn DpuClient> {
            Arc::new(Self {
                fleet: std::sync::Mutex::new(Some(Err(msg.to_string()))),
            })
        }
    }

    #[async_trait]
    impl DpuClient for StubClient {
        async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>> {
            match self.fleet.lock().unwrap().take().expect("fetch_fleet called twice") {
                Ok(v) => Ok(v),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    fn snap(dpu_id: &str) -> DpuSnapshot {
        DpuSnapshot {
            dpu_id: dpu_id.into(),
            applied_managed_host_config_version: "v1".into(),
            desired_managed_host_config_version: "v1".into(),
            applied_instance_network_config_version: "v1".into(),
            desired_instance_network_config_version: "v1".into(),
            quarantine_state: None,
            last_seen_at: Utc::now(),
            client_certificate_expiry: None,
        }
    }

    #[tokio::test]
    async fn empty_fleet_run_reports_ok_layer() {
        let layer = DpuLayer::new(StubClient::ok(vec![]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.name, "dpu");
        assert_eq!(result.status, Status::Ok);
    }

    #[tokio::test]
    async fn quarantined_only_yields_warn_layer_status() {
        let mut s = snap("dpu-1");
        s.quarantine_state = Some("BlockAllTraffic".into());
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Warn);
    }

    /// Issue acceptance: layer aggregate is worst-of children. Fail in
    /// any sub-check → Fail layer regardless of other Warn sub-checks.
    #[tokio::test]
    async fn mixed_subcheck_statuses_yield_worst_of_layer_status() {
        let now = Utc::now();
        // One DPU drifting > 60m (managed_host Fail), one quarantined (Warn).
        let mut drifter = snap("drifter");
        drifter.applied_managed_host_config_version = "v1".into();
        drifter.desired_managed_host_config_version = "v2".into();
        drifter.last_seen_at = now - ChronoDuration::seconds(70 * 60);
        let mut quar = snap("quar");
        quar.quarantine_state = Some("BlockAllTraffic".into());

        let layer = DpuLayer::new(
            StubClient::ok(vec![drifter, quar]),
            DpuConfig::default(),
        );
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        // Both sub-check headlines should be present.
        assert!(result.checks.iter().any(|c| c.name == "drift-managed-host"
            && c.kind == CheckKind::Headline
            && c.status == Status::Fail));
        assert!(result.checks.iter().any(|c| c.name == "quarantine"
            && c.kind == CheckKind::Headline
            && c.status == Status::Warn));
    }

    #[tokio::test]
    async fn client_error_run_reports_unknown_layer() {
        let layer = DpuLayer::new(StubClient::err("postgres unreachable"), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }
}
