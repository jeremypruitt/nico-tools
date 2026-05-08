//! `nico doctor dpu-isolation <machine-id>` layer.
//!
//! Wraps a [`DpuIsolationClient`] and reduces the fetched
//! [`IsolationSnapshot`] to a single headline [`Check`] via the pure
//! [`assess`] / [`assemble_checks`] pair in [`crate::dpu_isolation`].

use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use chrono::Utc;

use crate::dpu_isolation::{
    self, DpuIsolationClient, DEFAULT_FRESHNESS_THRESHOLD,
};
use crate::layer::{Layer, LayerOutcome, RunOpts};

pub struct DpuIsolationLayer {
    client: Arc<dyn DpuIsolationClient>,
    machine_id: String,
    freshness_threshold: Duration,
}

impl DpuIsolationLayer {
    pub fn new(client: Arc<dyn DpuIsolationClient>, machine_id: impl Into<String>) -> Self {
        Self {
            client,
            machine_id: machine_id.into(),
            freshness_threshold: DEFAULT_FRESHNESS_THRESHOLD,
        }
    }

    pub fn with_freshness_threshold(mut self, threshold: Duration) -> Self {
        self.freshness_threshold = threshold;
        self
    }
}

#[async_trait]
impl Layer for DpuIsolationLayer {
    fn name(&self) -> &'static str {
        "dpu_isolation"
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_snapshot(&self.machine_id).await {
            Ok(snapshot) => {
                let verdict = dpu_isolation::assess(
                    &snapshot,
                    Utc::now(),
                    self.freshness_threshold,
                );
                LayerOutcome::Checks(dpu_isolation::assemble_checks(
                    &self.machine_id,
                    &verdict,
                ))
            }
            Err(e) => LayerOutcome::Checks(dpu_isolation::assemble_error_checks(
                &self.machine_id,
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

    use crate::dpu_isolation::IsolationSnapshot;

    struct StubClient {
        result: std::sync::Mutex<Option<Result<IsolationSnapshot, String>>>,
    }

    impl StubClient {
        fn ok(snap: IsolationSnapshot) -> Arc<dyn DpuIsolationClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(snap))),
            })
        }
        fn err(msg: &str) -> Arc<dyn DpuIsolationClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Err(msg.to_string()))),
            })
        }
    }

    #[async_trait]
    impl DpuIsolationClient for StubClient {
        async fn fetch_snapshot(&self, _machine_id: &str) -> Result<IsolationSnapshot> {
            match self.result.lock().unwrap().take().expect("fetch_snapshot called twice") {
                Ok(s) => Ok(s),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    fn snap_healthy() -> IsolationSnapshot {
        IsolationSnapshot {
            machine_id: "machine-42".into(),
            registered: true,
            scout_discovery_complete: true,
            quarantine_state: None,
            last_seen_at: Some(Utc::now()),
        }
    }

    #[tokio::test]
    async fn healthy_run_reports_ok_layer() {
        let layer = DpuIsolationLayer::new(StubClient::ok(snap_healthy()), "machine-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.name, "dpu_isolation");
        assert_eq!(result.status, Status::Ok);
        assert!(result.checks[0].value.contains("healthy"));
    }

    #[tokio::test]
    async fn quarantined_run_reports_fail_layer() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("BlockAllTraffic".into());
        let layer = DpuIsolationLayer::new(StubClient::ok(snap), "machine-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        assert!(result.checks[0].value.contains("BlockAllTraffic"));
    }

    #[tokio::test]
    async fn unregistered_run_reports_unknown_layer() {
        let mut snap = snap_healthy();
        snap.registered = false;
        snap.scout_discovery_complete = false;
        snap.last_seen_at = None;
        let layer = DpuIsolationLayer::new(StubClient::ok(snap), "machine-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("not-yet-known"));
    }

    #[tokio::test]
    async fn lost_connection_run_reports_fail_layer() {
        let mut snap = snap_healthy();
        snap.last_seen_at = Some(Utc::now() - chrono::Duration::seconds(600));
        let layer = DpuIsolationLayer::new(StubClient::ok(snap), "machine-42")
            .with_freshness_threshold(Duration::from_secs(90));
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        assert!(result.checks[0].value.contains("lost-connection"));
    }

    #[tokio::test]
    async fn client_error_run_reports_unknown_with_message() {
        let layer = DpuIsolationLayer::new(StubClient::err("postgres unreachable"), "machine-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }
}
