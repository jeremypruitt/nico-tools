//! `nico doctor dpu-services <machine-id>` layer.
//!
//! Wraps a [`DpuServicesClient`] and reduces the fetched
//! [`ServicesSnapshot`] to a headline + per-service detail bullets via
//! the pure [`assemble_checks`] / [`assemble_no_status_checks`] /
//! [`assemble_error_checks`] trio in [`crate::dpu_services`].

use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use chrono::Utc;

use crate::dpu_services::{
    self, DpuServicesClient, DEFAULT_OBSERVATION_STALE_THRESHOLD,
};
use crate::layer::{Layer, LayerOutcome, RunOpts};

pub struct DpuServicesLayer {
    client: Arc<dyn DpuServicesClient>,
    dpu_id: String,
    stale_threshold: Duration,
}

impl DpuServicesLayer {
    pub fn new(client: Arc<dyn DpuServicesClient>, dpu_id: impl Into<String>) -> Self {
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
impl Layer for DpuServicesLayer {
    fn name(&self) -> &'static str {
        "dpu_services"
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_snapshot(&self.dpu_id).await {
            Ok(Some(snapshot)) => LayerOutcome::Checks(dpu_services::assemble_checks(
                &snapshot,
                Utc::now(),
                self.stale_threshold,
            )),
            Ok(None) => {
                LayerOutcome::Checks(dpu_services::assemble_no_status_checks(&self.dpu_id))
            }
            Err(e) => LayerOutcome::Checks(dpu_services::assemble_error_checks(
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

    use crate::dpu_services::{ServiceStatus, ServicesSnapshot};

    struct StubClient {
        result: std::sync::Mutex<Option<Result<Option<ServicesSnapshot>, String>>>,
    }

    impl StubClient {
        fn ok(snap: Option<ServicesSnapshot>) -> Arc<dyn DpuServicesClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(snap))),
            })
        }
        fn err(msg: &str) -> Arc<dyn DpuServicesClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Err(msg.to_string()))),
            })
        }
    }

    #[async_trait]
    impl DpuServicesClient for StubClient {
        async fn fetch_snapshot(&self, _dpu_id: &str) -> Result<Option<ServicesSnapshot>> {
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

    fn snap_healthy() -> ServicesSnapshot {
        ServicesSnapshot {
            dpu_id: "dpu-42".into(),
            observed_at: Some(Utc::now()),
            services: vec![ServiceStatus {
                service_name: "doca-bfb".into(),
                version: "2.5.0".into(),
                overall_state: "Running".into(),
                message: String::new(),
                removed: None,
            }],
        }
    }

    #[tokio::test]
    async fn healthy_snapshot_runs_as_ok_layer() {
        let layer = DpuServicesLayer::new(StubClient::ok(Some(snap_healthy())), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.name, "dpu_services");
        assert_eq!(result.status, Status::Ok);
        // Headline now uses the shared verdict's "M/N ready" form.
        assert!(result.checks[0].value.contains("1/1 ready"));
    }

    #[tokio::test]
    async fn missing_machine_row_runs_as_unknown_layer() {
        let layer = DpuServicesLayer::new(StubClient::ok(None), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("no machines row"));
    }

    #[tokio::test]
    async fn failed_service_runs_as_warn_layer() {
        let mut snap = snap_healthy();
        snap.services = vec![ServiceStatus {
            service_name: "doca-telemetry".into(),
            version: "2.4.0".into(),
            overall_state: "Failed".into(),
            message: "container restart".into(),
            removed: None,
        }];
        let layer = DpuServicesLayer::new(StubClient::ok(Some(snap)), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Warn);
    }

    #[tokio::test]
    async fn client_error_runs_as_unknown_with_message() {
        let layer = DpuServicesLayer::new(StubClient::err("postgres unreachable"), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }

    #[tokio::test]
    async fn custom_stale_threshold_changes_layer_status() {
        let now = Utc::now();
        let mut snap = snap_healthy();
        snap.observed_at = Some(now - chrono::Duration::minutes(2));
        // Default 5m ⇒ Ok
        let layer_default = DpuServicesLayer::new(StubClient::ok(Some(snap.clone())), "dpu-42");
        assert_eq!(
            layer_default.run(&RunOpts::default()).await.status,
            Status::Ok,
        );
        // Tighter 1m ⇒ Warn (observation_stale)
        let layer_tight = DpuServicesLayer::new(StubClient::ok(Some(snap)), "dpu-42")
            .with_stale_threshold(Duration::from_secs(60));
        assert_eq!(
            layer_tight.run(&RunOpts::default()).await.status,
            Status::Warn,
        );
    }
}
