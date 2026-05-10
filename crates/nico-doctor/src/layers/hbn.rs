//! `nico doctor hbn <dpu-id>` layer.
//!
//! Wraps an [`HbnClient`] (forgedb / Postgres in production, mocks in
//! tests) and reduces the fetched [`HbnSnapshot`] to a layer's worth of
//! [`Check`]s via [`crate::hbn::assemble_checks`]. All non-trivial logic
//! (version comparison, headline aggregation, status assignment) lives
//! in [`crate::hbn`] and is unit-tested without touching this layer.

use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use chrono::Utc;

use crate::hbn::{
    self, HbnClient, DEFAULT_FRESHNESS_THRESHOLD,
};
use crate::layer::{Layer, LayerOutcome, RunOpts};

pub struct HbnLayer {
    client: Arc<dyn HbnClient>,
    dpu_id: String,
    freshness_threshold: Duration,
}

impl HbnLayer {
    pub fn new(client: Arc<dyn HbnClient>, dpu_id: impl Into<String>) -> Self {
        Self {
            client,
            dpu_id: dpu_id.into(),
            freshness_threshold: DEFAULT_FRESHNESS_THRESHOLD,
        }
    }

    pub fn with_freshness_threshold(mut self, threshold: Duration) -> Self {
        self.freshness_threshold = threshold;
        self
    }
}

#[async_trait]
impl Layer for HbnLayer {
    fn name(&self) -> &'static str {
        "hbn"
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_snapshot(&self.dpu_id).await {
            Ok(Some(snapshot)) => LayerOutcome::Checks(hbn::assemble_checks(
                &snapshot,
                Utc::now(),
                self.freshness_threshold,
            )),
            Ok(None) => LayerOutcome::Checks(hbn::assemble_no_status_checks(&self.dpu_id)),
            Err(e) => LayerOutcome::Checks(hbn::assemble_error_checks(&self.dpu_id, &e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use nico_common::output::Status;

    use crate::hbn::HbnSnapshot;
    use crate::layer::CheckKind;

    struct StubClient {
        result: std::sync::Mutex<Option<Result<Option<HbnSnapshot>, String>>>,
    }

    impl StubClient {
        fn ok(snap: HbnSnapshot) -> Arc<dyn HbnClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(Some(snap)))),
            })
        }
        fn no_row() -> Arc<dyn HbnClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Ok(None))),
            })
        }
        fn err(msg: &str) -> Arc<dyn HbnClient> {
            Arc::new(Self {
                result: std::sync::Mutex::new(Some(Err(msg.to_string()))),
            })
        }
    }

    #[async_trait]
    impl HbnClient for StubClient {
        async fn fetch_snapshot(&self, _dpu_id: &str) -> Result<Option<HbnSnapshot>> {
            match self.result.lock().unwrap().take().expect("fetch_snapshot called twice") {
                Ok(opt) => Ok(opt),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
        async fn fetch_all_snapshots(&self) -> Result<Vec<HbnSnapshot>> {
            Ok(Vec::new())
        }
    }

    fn snap_healthy() -> HbnSnapshot {
        HbnSnapshot {
            dpu_id: "dpu-42".into(),
            hbn_version: "2.0.0-doca2.5.0".into(),
            applied_managed_host_config_version: "v17".into(),
            desired_managed_host_config_version: "v17".into(),
            applied_instance_network_config_version: "v9".into(),
            desired_instance_network_config_version: "v9".into(),
            network_config_error: None,
            bgp_alerts: vec![],
            quarantine_state: None,
            last_seen_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn all_healthy_run_reports_ok_layer() {
        let layer = HbnLayer::new(StubClient::ok(snap_healthy()), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        assert_eq!(result.name, "hbn");
        assert_eq!(result.status, Status::Ok);
        let headline = result.checks.iter().find(|c| c.kind == CheckKind::Headline).unwrap();
        assert!(headline.value.contains("dpu-42"));
        assert!(headline.value.contains("ok"));
    }

    #[tokio::test]
    async fn quarantined_run_reports_fail_layer() {
        let mut snap = snap_healthy();
        snap.quarantine_state = Some("manual".into());
        let layer = HbnLayer::new(StubClient::ok(snap), "dpu-42");

        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        assert!(result.checks.iter().any(|c| c.name == "quarantine" && c.status == Status::Fail));
    }

    #[tokio::test]
    async fn no_row_run_reports_unknown_with_single_headline() {
        let layer = HbnLayer::new(StubClient::no_row(), "dpu-99");
        let result = layer.run(&RunOpts::default()).await;

        assert_eq!(result.status, Status::Unknown);
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].kind, CheckKind::Headline);
        assert!(result.checks[0].value.contains("dpu-99"));
    }

    #[tokio::test]
    async fn client_error_run_reports_unknown_with_message() {
        let layer = HbnLayer::new(StubClient::err("postgres unreachable"), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }

    #[tokio::test]
    async fn version_stale_run_reports_fail_layer() {
        let mut snap = snap_healthy();
        snap.hbn_version = "1.9.0-doca2.4.0".into();
        let layer = HbnLayer::new(StubClient::ok(snap), "dpu-42");

        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        assert!(result.checks.iter().any(|c| c.name == "version_nvue" && c.status == Status::Fail));
    }

    #[tokio::test]
    async fn network_config_error_run_reports_fail_layer_with_error_in_headline() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("nvue apply failed".into());
        let layer = HbnLayer::new(StubClient::ok(snap), "dpu-42");

        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        let headline = result
            .checks
            .iter()
            .find(|c| c.kind == CheckKind::Headline)
            .unwrap();
        assert!(
            headline.value.contains("nvue apply failed"),
            "headline: {}",
            headline.value
        );
    }

    #[tokio::test]
    async fn single_version_drift_run_reports_fail_layer() {
        let mut snap = snap_healthy();
        snap.applied_instance_network_config_version = "v8".into();
        snap.desired_instance_network_config_version = "v9".into();
        let layer = HbnLayer::new(StubClient::ok(snap), "dpu-42");

        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        let drift = result
            .checks
            .iter()
            .find(|c| c.name == "instance_network_config")
            .unwrap();
        assert_eq!(drift.status, Status::Fail);
        assert!(drift.value.contains("v8") && drift.value.contains("v9"));
    }

    // PRD-003 Slice 3 — issue #307: hbn.checks ordering is headline first
    // (`CheckKind::Headline`, sourced from `hbn_verdict()`), then per-signal
    // detail rows. Holistic per-DPU and fleet rollups (slices 4 + 5)
    // consume the headline; the operator gets the full per-signal
    // breakdown as detail rows beneath.
    #[tokio::test]
    async fn run_emits_exactly_one_headline_first_then_detail_rows() {
        let layer = HbnLayer::new(StubClient::ok(snap_healthy()), "dpu-42");
        let result = layer.run(&RunOpts::default()).await;

        let headline_count = result
            .checks
            .iter()
            .filter(|c| c.kind == CheckKind::Headline)
            .count();
        assert_eq!(headline_count, 1, "exactly one headline expected");
        assert_eq!(
            result.checks[0].kind,
            CheckKind::Headline,
            "headline must be first"
        );
        assert!(
            result.checks[1..].iter().all(|c| c.kind == CheckKind::Detail),
            "every check after the headline must be a detail row",
        );
    }

    // Issue #307 acceptance: no detail dropped — every signal previously
    // surfaced as a headline still appears in detail. Constructing a
    // snapshot that triggers all five signals at once verifies that the
    // detail-row layout preserves the full breakdown.
    #[tokio::test]
    async fn unhealthy_run_preserves_every_signal_as_a_detail_row() {
        let mut snap = snap_healthy();
        snap.network_config_error = Some("nvue apply failed".into());
        snap.hbn_version = "1.9.0-doca2.4.0".into(); // below NVUE minimum
        snap.applied_managed_host_config_version = "v16".into();
        snap.desired_managed_host_config_version = "v17".into();
        snap.bgp_alerts = vec!["BgpPeerDown".into()];
        snap.last_seen_at = Utc::now() - chrono::Duration::seconds(600);

        let layer = HbnLayer::new(StubClient::ok(snap), "dpu-42")
            .with_freshness_threshold(Duration::from_secs(90));
        let result = layer.run(&RunOpts::default()).await;

        let names: Vec<&str> = result
            .checks
            .iter()
            .filter(|c| c.kind == CheckKind::Detail)
            .map(|c| c.name)
            .collect();
        for required in [
            "network_config_error",
            "version_nvue",
            "managed_host_config",
            "bgp",
            "last_seen",
        ] {
            assert!(
                names.contains(&required),
                "expected detail row {required:?} in {names:?}"
            );
        }
    }
}
