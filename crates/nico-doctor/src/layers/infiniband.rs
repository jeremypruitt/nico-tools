//! `nico doctor infiniband <dpu-id>` layer.
//!
//! Wraps an [`IbClient`] and reduces the fetched [`IbSnapshot`] to a
//! headline `Check` (sourced from [`crate::verdicts::ib_verdict`])
//! followed by per-port detail rows + freshness via
//! [`crate::infiniband::assemble_checks`]. Capability gating
//! (`infiniband_present`) is handled at construction time by
//! [`crate::layer::infiniband_skip_layer`] — when the deployment has
//! no IB fabric the caller installs a `SkippedLayer` instead of this
//! one, so `collect()` here always runs the real query.

use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use chrono::Utc;

use crate::infiniband::{
    self, IbClient, DEFAULT_OBSERVATION_STALE_THRESHOLD,
};
use crate::layer::{Layer, LayerOutcome, RunOpts};

pub struct InfinibandLayer {
    client: Arc<dyn IbClient>,
    dpu_id: String,
    stale_threshold: Duration,
}

impl InfinibandLayer {
    pub fn new(client: Arc<dyn IbClient>, dpu_id: impl Into<String>) -> Self {
        Self {
            client,
            dpu_id: dpu_id.into(),
            stale_threshold: DEFAULT_OBSERVATION_STALE_THRESHOLD,
        }
    }

    pub fn with_stale_threshold(mut self, threshold: Duration) -> Self {
        self.stale_threshold = threshold;
        self
    }
}

#[async_trait]
impl Layer for InfinibandLayer {
    fn name(&self) -> &'static str {
        "infiniband"
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_snapshot(&self.dpu_id).await {
            Ok(Some(snapshot)) => LayerOutcome::Checks(infiniband::assemble_checks(
                &snapshot,
                Utc::now(),
                self.stale_threshold,
            )),
            Ok(None) => {
                LayerOutcome::Checks(infiniband::assemble_no_status_checks(&self.dpu_id))
            }
            Err(e) => LayerOutcome::Checks(infiniband::assemble_error_checks(
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

    use crate::infiniband::{IbPort, IbSnapshot};
    use crate::layer::CheckKind;

    struct StubClient {
        result: std::sync::Mutex<Option<Result<Option<IbSnapshot>, String>>>,
    }

    impl StubClient {
        fn ok(snap: IbSnapshot) -> Arc<dyn IbClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(Some(snap)))),
            })
        }
        fn ok_none() -> Arc<dyn IbClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(None))),
            })
        }
        fn err(msg: &str) -> Arc<dyn IbClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Err(msg.to_string()))),
            })
        }
    }

    #[async_trait]
    impl IbClient for StubClient {
        async fn fetch_snapshot(&self, _dpu_id: &str) -> Result<Option<IbSnapshot>> {
            match self
                .result
                .lock()
                .unwrap()
                .take()
                .expect("fetch_snapshot called twice")
            {
                Ok(snap) => Ok(snap),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    fn healthy_snap() -> IbSnapshot {
        IbSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: Some(Utc::now()),
            ufm_observable: Some(true),
            ports: vec![IbPort {
                guid: "fe80::1".into(),
                fabric_id: "ib-fabric-1".into(),
                lid: 7,
                port_state: "Active".into(),
            }],
            ib_alerts: vec![],
        }
    }

    #[tokio::test]
    async fn healthy_run_reports_ok_layer() {
        let layer = InfinibandLayer::new(StubClient::ok(healthy_snap()), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.name, "infiniband");
        assert_eq!(result.status, Status::Ok);
        assert!(result.checks[0].value.contains("healthy"));
    }

    #[tokio::test]
    async fn fail_run_reports_fail_layer() {
        let mut snap = healthy_snap();
        snap.ports[0].fabric_id = "".into();
        let layer = InfinibandLayer::new(StubClient::ok(snap), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
    }

    #[tokio::test]
    async fn no_machines_row_run_reports_unknown_with_correlate_hint() {
        let layer = InfinibandLayer::new(StubClient::ok_none(), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].kind, CheckKind::Headline);
        assert!(result.checks[0].value.contains("no machines row"));
        assert!(result.checks[0]
            .next_command
            .as_deref()
            .unwrap()
            .contains("nico correlate"));
    }

    #[tokio::test]
    async fn client_error_run_reports_unknown_with_message() {
        let layer = InfinibandLayer::new(StubClient::err("postgres unreachable"), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }

    #[tokio::test]
    async fn healthy_run_emits_headline_then_per_port_detail_rows_then_freshness() {
        let layer = InfinibandLayer::new(StubClient::ok(healthy_snap()), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        assert_eq!(result.status, Status::Ok);
        assert_eq!(
            result.checks.len(),
            3,
            "headline + 1 port detail + 1 observed_at detail"
        );
        assert_eq!(result.checks[0].kind, CheckKind::Headline);
        assert_eq!(result.checks[1].kind, CheckKind::Detail);
        assert_eq!(result.checks[1].name, "port");
        assert!(result.checks[1].value.contains("fe80::1"));
        assert!(result.checks[1].value.contains("ib-fabric-1"));
        assert!(result.checks[1].value.contains("lid=7"));
        assert!(result.checks[1].value.contains("Active"));
        assert_eq!(result.checks[2].name, "observed_at");
    }

    #[tokio::test]
    async fn stale_observation_emits_freshness_detail_with_warn_status() {
        let mut snap = healthy_snap();
        snap.observed_at = Some(Utc::now() - chrono::Duration::hours(5));
        let layer = InfinibandLayer::new(StubClient::ok(snap), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        assert_eq!(result.status, Status::Warn);
        let freshness = result
            .checks
            .iter()
            .find(|c| c.name == "freshness")
            .expect("freshness detail row");
        assert_eq!(freshness.kind, CheckKind::Detail);
        assert_eq!(freshness.status, Status::Warn);
        assert!(freshness.value.contains("stale"));
    }

    #[tokio::test]
    async fn custom_stale_threshold_changes_freshness_classification() {
        // Observation is 30m old. Default threshold (4h) ⇒ Ok. Custom
        // 10m threshold ⇒ Warn (freshness).
        let mut snap = healthy_snap();
        snap.observed_at = Some(Utc::now() - chrono::Duration::minutes(30));
        let layer =
            InfinibandLayer::new(StubClient::ok(snap), "dpu-42")
                .with_stale_threshold(Duration::from_secs(10 * 60));
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Warn);
    }
}
