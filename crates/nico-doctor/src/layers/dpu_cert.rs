//! `nico doctor dpu-cert <dpu-id>` layer.
//!
//! Wraps a [`DpuCertClient`] and reduces the fetched [`CertSnapshot`]
//! to a headline `Check` (sourced from
//! [`crate::verdicts::cert_verdict`]) followed by cert-specific
//! detail rows via [`crate::dpu_cert::assemble_checks`].

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
            Ok(snapshot) => LayerOutcome::Checks(dpu_cert::assemble_checks(
                &snapshot,
                Utc::now(),
                self.warn_threshold,
            )),
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

    // PRD-003 Slice 1 — issue #305: dpu_cert.checks ordering is headline
    // first (`CheckKind::Headline`, sourced from `cert_verdict()`), then
    // cert-specific detail rows (`CheckKind::Detail`). Holistic per-DPU
    // and fleet rollups (slices 5 + 6) consume the headline; the
    // operator gets the raw expiry timestamp + threshold echo as
    // separate detail rows.
    #[tokio::test]
    async fn healthy_run_emits_headline_then_expiry_and_threshold_detail_rows() {
        use crate::layer::CheckKind;

        let layer = DpuCertLayer::new(StubClient::ok(snap_with_expiry_in(180)), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        assert_eq!(result.status, Status::Ok);
        assert_eq!(result.checks.len(), 3, "headline + 2 detail rows");
        assert_eq!(result.checks[0].kind, CheckKind::Headline);
        assert_eq!(result.checks[1].kind, CheckKind::Detail);
        assert_eq!(result.checks[2].kind, CheckKind::Detail);
        assert_eq!(result.checks[1].name, "expiry");
        assert_eq!(result.checks[2].name, "warn-threshold");
        assert!(
            result.checks[2].value.contains("30d"),
            "threshold detail: {:?}",
            result.checks[2].value,
        );
    }

    #[tokio::test]
    async fn no_recent_status_run_omits_expiry_detail_row() {
        use crate::layer::CheckKind;

        let snap = CertSnapshot {
            dpu_id: "dpu-42".into(),
            client_certificate_expiry: None,
        };
        let layer = DpuCertLayer::new(StubClient::ok(snap), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        // No expiry to render → only the headline. We deliberately do
        // NOT echo a threshold detail when there's no expiry to compare
        // it to (would be noise without the anchor).
        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].kind, CheckKind::Headline);
    }

    #[tokio::test]
    async fn data_layer_error_run_emits_only_unknown_headline() {
        use crate::layer::CheckKind;

        let layer = DpuCertLayer::new(StubClient::err("postgres unreachable"), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].kind, CheckKind::Headline);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }
}
