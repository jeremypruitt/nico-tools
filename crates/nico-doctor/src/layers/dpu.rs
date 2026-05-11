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
///
/// Three resolution paths:
///
/// 1. Resolved deployment-type without forgedb (`rest-only-mock`) →
///    [`SkippedLayer`] with reason `n/a in <type>: no forgedb` per
///    PRD-001 §"Status semantics for 'n/a in this deployment-type'".
///    This is distinct from [`UnconfiguredLayer`] (the `Unknown` /
///    soft-fail path) — n/a-by-design must not look like a fail.
/// 2. Invalid postgres URL → [`UnconfiguredLayer`] (`Unknown`).
/// 3. Otherwise → live [`DpuLayer`].
pub fn register(inputs: &LayerInputs) -> Box<dyn Layer> {
    if let Some(skip) = layer::forgedb_skip_layer(NAME, inputs.deployment_type) {
        return skip;
    }
    match SqlxDpuClient::new(&inputs.postgres_url) {
        Ok(client) => Box::new(
            DpuLayer::new(Arc::new(client), inputs.dpu_config)
                .with_infiniband_present(inputs.infiniband_present),
        ),
        Err(_) => layer::UnconfiguredLayer::new(NAME, "invalid postgres URL"),
    }
}

pub struct DpuLayer {
    client: Arc<dyn DpuClient>,
    config: DpuConfig,
    /// Boot-probe-resolved IB capability gate (PRD-004 slice 1). Threaded
    /// through to [`dpu::assemble_checks`] so the fleet rollup omits the
    /// `infiniband` axis row on non-IB fleets.
    infiniband_present: Option<bool>,
}

impl DpuLayer {
    pub fn new(client: Arc<dyn DpuClient>, config: DpuConfig) -> Self {
        Self {
            client,
            config,
            infiniband_present: None,
        }
    }

    /// Set the IB capability gate. `Some(false)` ⇒ the IB axis is omitted
    /// from the fleet rollup; `Some(true)` or `None` ⇒ included.
    pub fn with_infiniband_present(mut self, val: Option<bool>) -> Self {
        self.infiniband_present = val;
        self
    }
}

#[async_trait]
impl Layer for DpuLayer {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn collect(&self, _opts: &RunOpts) -> LayerOutcome {
        match self.client.fetch_fleet().await {
            Ok(fleet) => LayerOutcome::Checks(dpu::assemble_checks(
                &fleet,
                Utc::now(),
                &self.config,
                self.infiniband_present,
            )),
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

    use crate::dpu::{DpuSnapshot, HealthAlert};

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
        // Default: healthy across every axis. Tests override fields to
        // drive a specific verdict. A cert expiry far in the future and
        // a healthy IB observation are required because the
        // corresponding verdicts return `Unknown` for missing inputs,
        // which would otherwise contaminate the layer aggregate in
        // `Ok`-expecting tests.
        DpuSnapshot {
            dpu_id: dpu_id.into(),
            applied_managed_host_config_version: "v1".into(),
            desired_managed_host_config_version: "v1".into(),
            applied_instance_network_config_version: "v1".into(),
            desired_instance_network_config_version: "v1".into(),
            quarantine_state: None,
            last_seen_at: Utc::now(),
            client_certificate_expiry: Some(Utc::now() + ChronoDuration::days(365)),
            health_alerts: Vec::new(),
            network_config_error: None,
            hbn_version: String::new(),
            bgp_alerts: Vec::new(),
            extension_services_observed_at: None,
            extension_services: Vec::new(),
            infiniband_observed_at: Some(Utc::now()),
            infiniband_ufm_observable: Some(true),
            infiniband_ports: vec![crate::infiniband::IbPort {
                guid: "fe80::1".into(),
                fabric_id: "ib-fabric-1".into(),
                lid: 7,
                port_state: "Active".into(),
            }],
            ib_alerts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn empty_fleet_run_reports_ok_layer() {
        let layer = DpuLayer::new(StubClient::ok(vec![]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.name, "dpu");
        assert_eq!(result.status, Status::Ok);
    }

    /// Per PRD-003 slice 6, quarantine is reduced through the shared
    /// `isolation_verdict`, which classifies a quarantine-requested DPU
    /// as `Fail` (not the old fleet-only `Warn` cap). The fleet rollup
    /// now reports exactly what the per-DPU drill-down would.
    #[tokio::test]
    async fn quarantined_only_flips_isolation_axis_to_fail() {
        let mut s = snap("dpu-1");
        s.quarantine_state = Some("BlockAllTraffic".into());
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        assert!(result.checks.iter().any(|c| c.name == "dpu_isolation"
            && c.kind == CheckKind::Headline
            && c.status == Status::Fail));
    }

    /// Layer aggregate is worst-of children. Drift on one DPU folds into
    /// the `hbn` axis verdict (Fail); a separate DPU lost-connection
    /// folds into `dpu_isolation` (Fail). Both per-axis headlines surface.
    #[tokio::test]
    async fn mixed_axis_statuses_yield_worst_of_layer_status() {
        let now = Utc::now();
        let mut drifter = snap("drifter");
        drifter.applied_managed_host_config_version = "v1".into();
        drifter.desired_managed_host_config_version = "v2".into();
        let mut silent = snap("silent");
        silent.last_seen_at = now - ChronoDuration::seconds(300);

        let layer = DpuLayer::new(
            StubClient::ok(vec![drifter, silent]),
            DpuConfig::default(),
        );
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Fail);
        assert!(result.checks.iter().any(|c| c.name == "hbn"
            && c.kind == CheckKind::Headline
            && c.status == Status::Fail));
        assert!(result.checks.iter().any(|c| c.name == "dpu_isolation"
            && c.kind == CheckKind::Headline
            && c.status == Status::Fail));
    }

    /// PRD-004 slice 5 acceptance: when the boot-probe reports the
    /// fleet has no IB fabric (`Some(false)`), the rollup omits the
    /// `infiniband` axis entirely rather than showing an empty Ok row.
    #[tokio::test]
    async fn infiniband_capability_gate_some_false_omits_axis_from_layer_output() {
        let s = snap("dpu-1");
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default())
            .with_infiniband_present(Some(false));
        let result = layer.run(&RunOpts::default()).await;
        assert!(
            !result.checks.iter().any(|c| c.name == "infiniband"),
            "infiniband row should be omitted under Some(false): {:?}",
            result.checks.iter().map(|c| (c.name, c.kind)).collect::<Vec<_>>(),
        );
    }

    #[tokio::test]
    async fn infiniband_capability_gate_some_true_includes_axis_in_layer_output() {
        let s = snap("dpu-1");
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default())
            .with_infiniband_present(Some(true));
        let result = layer.run(&RunOpts::default()).await;
        assert!(
            result.checks.iter().any(|c| c.name == "infiniband"
                && c.kind == CheckKind::Headline),
            "infiniband row should appear under Some(true)",
        );
    }

    #[tokio::test]
    async fn client_error_run_reports_unknown_layer() {
        let layer = DpuLayer::new(StubClient::err("postgres unreachable"), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        assert_eq!(result.status, Status::Unknown);
        assert!(result.checks[0].value.contains("postgres unreachable"));
    }

    // ── probe-stuck integration (issue #239 acceptance) ─────────────────

    fn alert(id: &str, since: Option<chrono::DateTime<Utc>>) -> HealthAlert {
        HealthAlert { id: id.into(), in_alert_since: since }
    }

    /// Acceptance variant: probe absent → pass.
    #[tokio::test]
    async fn probe_stuck_layer_no_alert_is_ok() {
        let s = snap("dpu-1");
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        let h = result
            .checks
            .iter()
            .find(|c| c.name == "probe-stuck" && c.kind == CheckKind::Headline)
            .expect("probe-stuck headline present");
        assert_eq!(h.status, Status::Ok);
        assert_eq!(result.status, Status::Ok);
    }

    /// Acceptance variant: probe present < grace → pass.
    #[tokio::test]
    async fn probe_stuck_layer_alert_within_grace_is_ok() {
        let now = Utc::now();
        let mut s = snap("dpu-1");
        s.health_alerts = vec![alert(
            "PostConfigCheckWait",
            Some(now - ChronoDuration::seconds(5)),
        )];
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        let h = result
            .checks
            .iter()
            .find(|c| c.name == "probe-stuck" && c.kind == CheckKind::Headline)
            .expect("probe-stuck headline present");
        assert_eq!(h.status, Status::Ok);
    }

    /// Acceptance variant: probe present > grace → fail; layer status = Fail.
    #[tokio::test]
    async fn probe_stuck_layer_alert_past_grace_is_fail() {
        let now = Utc::now();
        let mut s = snap("dpu-stuck");
        s.health_alerts = vec![alert(
            "PostConfigCheckWait",
            Some(now - ChronoDuration::seconds(120)),
        )];
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        let h = result
            .checks
            .iter()
            .find(|c| c.name == "probe-stuck" && c.kind == CheckKind::Headline)
            .expect("probe-stuck headline present");
        assert_eq!(h.status, Status::Fail);
        assert!(h.value.starts_with("1 DPUs"));
        // Layer aggregate flips to Fail.
        assert_eq!(result.status, Status::Fail);
        // Per-DPU detail line points the operator to per-DPU drill-down.
        let detail = result
            .checks
            .iter()
            .find(|c| c.name == "probe-stuck" && c.kind == CheckKind::Detail)
            .expect("probe-stuck detail present");
        assert!(detail.value.contains("dpu-stuck"));
        assert_eq!(
            detail.next_command.as_deref(),
            Some("nico doctor hbn dpu-stuck"),
        );
    }

    /// Acceptance variant: probe cleared between reports → pass.
    /// Modeled by a subsequent fleet snapshot whose `health_alerts` no
    /// longer contains a `PostConfigCheckWait` entry.
    #[tokio::test]
    async fn probe_stuck_layer_alert_cleared_between_reports_is_ok() {
        let mut s = snap("dpu-1");
        s.health_alerts = Vec::new(); // alert cleared upstream
        let layer = DpuLayer::new(StubClient::ok(vec![s]), DpuConfig::default());
        let result = layer.run(&RunOpts::default()).await;
        let h = result
            .checks
            .iter()
            .find(|c| c.name == "probe-stuck" && c.kind == CheckKind::Headline)
            .expect("probe-stuck headline present");
        assert_eq!(h.status, Status::Ok);
    }
}
