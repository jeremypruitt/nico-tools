//! `nico doctor dpu-cert <dpu-id>` layer.
//!
//! Wraps a [`DpuCertClient`] and reduces the fetched [`CertSnapshot`]
//! to a single headline [`Check`] via the pure [`assess`] /
//! [`assemble_checks`] pair in [`crate::dpu_cert`].

use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use chrono::Utc;

use crate::dpu_cert::{
    self, DpuCertClient, DEFAULT_WARN_THRESHOLD,
};
use crate::layer::{Layer, LayerOutcome, RunOpts};

pub struct DpuCertLayer {
    client: Arc<dyn DpuCertClient>,
    dpu_id: String,
    warn_threshold: Duration,
}

impl DpuCertLayer {
    pub fn new(client: Arc<dyn DpuCertClient>, dpu_id: impl Into<String>) -> Self {
        Self {
            client,
            dpu_id: dpu_id.into(),
            warn_threshold: DEFAULT_WARN_THRESHOLD,
        }
    }

    pub fn with_warn_threshold(mut self, threshold: Duration) -> Self {
        self.warn_threshold = threshold;
        self
    }
}

#[async_trait]
impl Layer for DpuCertLayer {
    fn name(&self) -> &'static str {
        "dpu_cert"
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_snapshot(&self.dpu_id).await {
            Ok(snapshot) => {
                let verdict = dpu_cert::assess(&snapshot, Utc::now(), self.warn_threshold);
                LayerOutcome::Checks(dpu_cert::assemble_checks(&self.dpu_id, &verdict))
            }
            Err(e) => LayerOutcome::Checks(dpu_cert::assemble_error_checks(
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

    use crate::dpu_cert::CertSnapshot;

    struct StubClient {
        result: std::sync::Mutex<Option<Result<CertSnapshot, String>>>,
    }

    impl StubClient {
        fn ok(snap: CertSnapshot) -> Arc<dyn DpuCertClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(snap))),
            })
        }
        fn err(msg: &str) -> Arc<dyn DpuCertClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Err(msg.to_string()))),
            })
        }
    }

    #[async_trait]
    impl DpuCertClient for StubClient {
        async fn fetch_snapshot(&self, _dpu_id: &str) -> Result<CertSnapshot> {
            match self.result.lock().unwrap().take().expect("fetch_snapshot called twice") {
                Ok(s) => Ok(s),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
    }

    fn snap_with_expiry_in(days: i64) -> CertSnapshot {
        CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: Some(Utc::now() + chrono::Duration::days(days)),
        }
    }

    #[tokio::test]
    async fn healthy_run_reports_ok_layer() {
        let layer = DpuCertLayer::new(StubClient::ok(snap_with_expiry_in(180)), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.name, "dpu_cert");
        assert_eq!(result.status, Status::Ok);
        assert!(result.checks[0].value.contains("healthy"));
    }

    #[tokio::test]
    async fn expiring_soon_run_reports_warn_layer() {
        let layer = DpuCertLayer::new(StubClient::ok(snap_with_expiry_in(15)), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Warn);
        assert!(result.checks[0].value.contains("expires in"));
    }

    #[tokio::test]
    async fn expired_run_reports_fail_layer() {
        let layer = DpuCertLayer::new(StubClient::ok(snap_with_expiry_in(-2)), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        assert!(result.checks[0].value.contains("expired"));
    }

    #[tokio::test]
    async fn no_recent_status_run_reports_unknown_layer() {
        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: None,
        };
        let layer = DpuCertLayer::new(StubClient::ok(snap), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("no recent"));
    }

    #[tokio::test]
    async fn client_error_run_reports_unknown_with_message() {
        let layer = DpuCertLayer::new(StubClient::err("postgres unreachable"), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }

    #[tokio::test]
    async fn custom_warn_threshold_changes_classification_boundary() {
        // 100 days remaining; default 30d threshold ⇒ Healthy.
        // Custom 200d threshold ⇒ ExpiringSoon (Warn).
        let layer = DpuCertLayer::new(StubClient::ok(snap_with_expiry_in(100)), "dpu-42")
            .with_warn_threshold(Duration::from_secs(200 * 86_400));
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Warn);
    }
}
