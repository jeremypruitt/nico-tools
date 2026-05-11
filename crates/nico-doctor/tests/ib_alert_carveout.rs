//! PRD-004 slice 3 integration test: a single `Ib*` alert in the raw
//! `dpu_agent_health_report` JSON surfaces **only** in the `infiniband`
//! layer's output and is dropped by the `dpu_health` layer. Together the
//! two layers partition the health-report alert stream so each alert has
//! exactly one owning drill-down.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use nico_common::output::Status;

use nico_doctor::dpu_health::{
    parse_alerts, DpuHealthClient, HealthSnapshot, DEFAULT_DHCP_STALE_THRESHOLD,
};
use nico_doctor::infiniband::{
    parse_ib_alerts, IbClient, IbSnapshot, DEFAULT_OBSERVATION_STALE_THRESHOLD,
};
use nico_doctor::layer::{CheckKind, Layer, RunOpts};
use nico_doctor::layers::dpu_health::DpuHealthLayer;
use nico_doctor::layers::infiniband::InfinibandLayer;

/// One JSON blob — the same column shape both layers read in production
/// (`machines.dpu_agent_health_report`).
fn shared_health_report() -> serde_json::Value {
    serde_json::json!({
        "alerts": [
            {
                "id": "IbPortDown",
                "target": "fe80::1",
                "message": "port 1 down",
                "in_alert_since": "2024-01-15T12:34:56Z"
            },
            {
                "id": "HeartbeatTimeout",
                "target": "dpu-42",
                "message": "no health report received",
                "in_alert_since": null
            }
        ]
    })
}

struct StubHealthClient {
    snap: Mutex<Option<HealthSnapshot>>,
}

#[async_trait]
impl DpuHealthClient for StubHealthClient {
    async fn fetch_snapshot(&self, _dpu_id: &str) -> Result<Option<HealthSnapshot>> {
        Ok(self.snap.lock().unwrap().take())
    }
}

struct StubIbClient {
    snap: Mutex<Option<IbSnapshot>>,
}

#[async_trait]
impl IbClient for StubIbClient {
    async fn fetch_snapshot(&self, _dpu_id: &str) -> Result<Option<IbSnapshot>> {
        Ok(self.snap.lock().unwrap().take())
    }
}

#[tokio::test]
async fn ib_alert_surfaces_in_infiniband_layer_only_not_dpu_health() {
    let blob = shared_health_report();

    let now = Utc::now();
    let dpu_health_snap = HealthSnapshot {
        dpu_id: "dpu-42".into(),
        agent_version: Some("2.0.0".into()),
        agent_version_superseded_at: None,
        // `dpu_health` parses every alert (filtering happens in
        // assemble_checks); production wiring runs the same parser.
        alerts: parse_alerts(Some(&blob)),
        interfaces: vec![],
        client_certificate_expiry: Some(now + chrono::Duration::days(365)),
        quarantine_state: None,
        last_seen_at: Some(now),
        registered: true,
        scout_discovery_complete: true,
        hbn_version: "2.0.0-doca2.5.0".into(),
        network_config_error: None,
        applied_managed_host_config_version: "v1".into(),
        desired_managed_host_config_version: "v1".into(),
        applied_instance_network_config_version: "v1".into(),
        desired_instance_network_config_version: "v1".into(),
        bgp_alerts: vec![],
        extension_services_observed_at: Some(now),
        extension_services: vec![],
        infiniband_observed_at: None,
        infiniband_ufm_observable: None,
        infiniband_ports: vec![],
        ib_alerts: vec![],
    };
    let ib_snap = IbSnapshot {
        dpu_id: "dpu-42".into(),
        observed_at: Some(Utc::now()),
        ufm_observable: Some(true),
        ports: vec![],
        // `infiniband` parser keeps only `Ib*` ids — same column, same
        // blob. Together the two parsers partition the alert stream.
        ib_alerts: parse_ib_alerts(Some(&blob)),
    };

    let dpu_health_layer = DpuHealthLayer::new(
        Arc::new(StubHealthClient {
            snap: Mutex::new(Some(dpu_health_snap)),
        }),
        "dpu-42",
    )
    .with_dhcp_stale_threshold(DEFAULT_DHCP_STALE_THRESHOLD);
    let ib_layer = InfinibandLayer::new(
        Arc::new(StubIbClient {
            snap: Mutex::new(Some(ib_snap)),
        }),
        "dpu-42",
    )
    .with_stale_threshold(DEFAULT_OBSERVATION_STALE_THRESHOLD);

    let opts = RunOpts {
        timeout: Duration::from_secs(2),
        ..Default::default()
    };
    let dpu_health_result = dpu_health_layer.run(&opts).await;
    let ib_result = ib_layer.run(&opts).await;

    let dpu_health_text: String = dpu_health_result
        .checks
        .iter()
        .map(|c| c.value.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let ib_text: String = ib_result
        .checks
        .iter()
        .map(|c| c.value.clone())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !dpu_health_text.contains("IbPortDown"),
        "Ib* alert leaked into dpu_health output:\n{dpu_health_text}",
    );
    assert!(
        ib_text.contains("IbPortDown"),
        "Ib* alert missing from infiniband output:\n{ib_text}",
    );
    assert!(
        dpu_health_text.contains("HeartbeatTimeout"),
        "non-Ib alert dropped from dpu_health output:\n{dpu_health_text}",
    );
    assert!(
        !ib_text.contains("HeartbeatTimeout"),
        "non-Ib alert leaked into infiniband output:\n{ib_text}",
    );

    let ib_alert_detail = ib_result
        .checks
        .iter()
        .find(|c| c.name == "ib_alert")
        .expect("expected an ib_alert detail row");
    assert_eq!(ib_alert_detail.kind, CheckKind::Detail);
    assert_eq!(ib_alert_detail.status, Status::Warn);
    assert_eq!(ib_result.status, Status::Warn);
}
