//! Shared fixture generators and inline mocks for PRD-005 follow-up
//! per-layer criterion benches (#353).
//!
//! Each per-layer bench targets the public seam (`Layer::run`) of one
//! `nico_doctor::layers::*` module, using the same `Arc<dyn …Client>`
//! shape the live wiring uses but with a synchronous in-memory fake
//! that performs no I/O. The fleet-scoped layers parametrize over
//! `N ∈ {1, 18, 250, 1000, 10000}`; layers whose work is constant in
//! fleet size (`grpc`, `postgres`) keep a single flat bench.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nico_common::k8s::{K8sClient, PodScope, RawEvent, RawPod};
use nico_common::temporal::TemporalClient;
use nico_doctor::dpu::{DpuClient, DpuConfig, DpuSnapshot};
use nico_doctor::grpc::{GrpcInspectResult, GrpcInspector, GrpcServiceInfo};
use nico_doctor::http::{HttpClient, ServiceEndpoint};
use nico_doctor::log_source::{LogCollection, LogSource, PodLogsCache};
use nico_doctor::postgres::{LockWait, PoolStats, PostgresClient};
use temporal_sdk_core_protos::temporal::api::common::v1::{WorkflowExecution, WorkflowType};
use temporal_sdk_core_protos::temporal::api::history::v1::History;
use temporal_sdk_core_protos::temporal::api::workflow::v1::WorkflowExecutionInfo;

/// The fleet-size sweep PRD-005 Slice 0a established. Reused so all
/// per-layer benches plot on the same x-axis.
pub const FLEET_SIZES: &[usize] = &[1, 18, 250, 1000, 10000];

// ---------------------------------------------------------------- pods
// `cluster` + `logs` benches consume the same shape of synthetic pod
// rows. Half restart, a quarter Pending, the rest Running+ready.

pub fn fleet_pods(n: usize) -> Vec<RawPod> {
    (0..n)
        .map(|i| RawPod {
            name: format!("dpu-agent-bench-{i:08x}"),
            namespace: "nico".to_string(),
            phase: Some(if i % 4 == 0 { "Pending".into() } else { "Running".into() }),
            ready: i % 4 != 0,
            restart_count: (i % 8) as u32,
            succeeded: false,
            crash_loop: i % 16 == 0,
        })
        .collect()
}

pub fn fleet_events(n: usize) -> Vec<RawEvent> {
    // One Warning per pod with restart_count > 0 — keeps event volume
    // tied to the synthetic pod fleet rather than blowing up
    // independently.
    let now: DateTime<Utc> = Utc::now();
    (0..n)
        .filter(|i| i % 8 != 0)
        .map(|i| RawEvent {
            ts: Some(now),
            event_type: Some("Warning".into()),
            reason: Some("BackOff".into()),
            message: Some(format!("Back-off restarting failed container ({i})")),
            involved_object: Some(format!("dpu-agent-bench-{i:08x}")),
        })
        .collect()
}

/// In-process `K8sClient` fake. Returns the same vecs on every call;
/// `list_pods` ignores `scope`, `list_events` ignores the field
/// selector — both representative of a real refresh's fast path.
pub struct BenchK8s {
    pods: Vec<RawPod>,
    events: Vec<RawEvent>,
}

impl BenchK8s {
    pub fn new(pods: Vec<RawPod>, events: Vec<RawEvent>) -> Self {
        Self { pods, events }
    }
}

#[async_trait]
impl K8sClient for BenchK8s {
    async fn list_pods(&self, _scope: PodScope<'_>) -> Result<Vec<RawPod>> {
        Ok(self.pods.clone())
    }

    async fn list_events(
        &self,
        _namespace: &str,
        _field_selector: Option<&str>,
    ) -> Result<Vec<RawEvent>> {
        Ok(self.events.clone())
    }

    async fn pod_logs(&self, _: &str, _: &str, _: Duration) -> Result<Vec<String>> {
        Ok(vec![])
    }
}

// ----------------------------------------------------------------- log
// `logs` layer benches drive a `LogSource` directly. The chain
// adapter is a thin wrapper; we exercise the chain's tail rather
// than rebuild it.

pub fn fleet_log_entries(n: usize) -> Vec<(String, String)> {
    (0..n)
        .map(|i| {
            (
                format!("dpu-agent-bench-{i:08x}"),
                format!("ERROR: synthetic line {i}: connection refused"),
            )
        })
        .collect()
}

pub struct BenchLogSource {
    label: String,
    primary_ok: bool,
    entries: Vec<(String, String)>,
}

impl BenchLogSource {
    pub fn new(label: &str, primary_ok: bool, entries: Vec<(String, String)>) -> Self {
        Self {
            label: label.to_string(),
            primary_ok,
            entries,
        }
    }
}

#[async_trait]
impl LogSource for BenchLogSource {
    fn name(&self) -> &str {
        &self.label
    }

    async fn collect(
        &self,
        _: &str,
        _: Duration,
        _: usize,
        _: &PodLogsCache,
    ) -> Result<LogCollection> {
        Ok(LogCollection {
            label: self.label.clone(),
            primary_ok: self.primary_ok,
            entries: self.entries.clone(),
        })
    }
}

// ----------------------------------------------------------- workflows
// `workflows` layer queries the Temporal visibility API twice per
// `collect`: one query for stuck workflows, one for failed. The mock
// dispatches on substring to return realistic execution lists.

pub fn fleet_executions(n: usize, mark_failed: bool) -> Vec<WorkflowExecutionInfo> {
    let now = Utc::now();
    (0..n)
        .map(|i| {
            let start_dt: DateTime<Utc> = now - chrono::Duration::hours((i % 24 + 1) as i64);
            WorkflowExecutionInfo {
                execution: Some(WorkflowExecution {
                    workflow_id: format!(
                        "{}-bench-{i:08x}",
                        if mark_failed { "decommission" } else { "provisioning" }
                    ),
                    run_id: format!("run-{i:08x}"),
                }),
                r#type: Some(WorkflowType {
                    name: if mark_failed {
                        "HostDecommission".into()
                    } else {
                        "HostProvisioning".into()
                    },
                }),
                start_time: Some(prost_wkt_types::Timestamp {
                    seconds: start_dt.timestamp(),
                    nanos: start_dt.timestamp_subsec_nanos() as i32,
                }),
                history_length: ((i % 50) + 5) as i64,
                ..Default::default()
            }
        })
        .collect()
}

pub struct BenchTemporal {
    pub stuck: Vec<WorkflowExecutionInfo>,
    pub failed: Vec<WorkflowExecutionInfo>,
}

#[async_trait]
impl TemporalClient for BenchTemporal {
    async fn list_workflow_executions(
        &self,
        _namespace: &str,
        query: &str,
        _page_size: i32,
    ) -> Result<Vec<WorkflowExecutionInfo>> {
        if query.contains("Running") {
            Ok(self.stuck.clone())
        } else {
            Ok(self.failed.clone())
        }
    }

    async fn get_workflow_history(&self, _: &str, _: &str) -> Result<History> {
        Ok(History::default())
    }
}

// -------------------------------------------------------------- health
// `health` exercises N HTTP probes per `collect`. The fake returns a
// status code without any network I/O so the bench measures the
// per-probe orchestration cost, not network latency.

pub fn fleet_endpoints(n: usize) -> Vec<ServiceEndpoint> {
    (0..n)
        .map(|i| ServiceEndpoint {
            name: format!("svc-{i}"),
            base_url: format!("http://bench-host-{i}:8080"),
        })
        .collect()
}

pub struct BenchHttp;

#[async_trait]
impl HttpClient for BenchHttp {
    async fn get_status(&self, url: &str) -> Result<u16> {
        // Heuristic mirror of the real layer's "healthz then readyz"
        // shape: every fourth endpoint marks readyz as 503 so the
        // bench exercises the degraded path that builds a finding.
        if url.contains("readyz") && url.contains("svc-0") {
            Ok(503)
        } else {
            Ok(200)
        }
    }
}

// ---------------------------------------------------------------- grpc
// `grpc` does a single inspection per `collect` — no N-sweep.

pub struct BenchGrpc;

#[async_trait]
impl GrpcInspector for BenchGrpc {
    async fn inspect(&self, _addr: &str) -> Result<GrpcInspectResult> {
        Ok(GrpcInspectResult::Reachable {
            services: vec![
                GrpcServiceInfo {
                    name: "nico.v1.HostService".into(),
                    method_count: 12,
                },
                GrpcServiceInfo {
                    name: "nico.v1.DpuService".into(),
                    method_count: 7,
                },
            ],
        })
    }
}

// ------------------------------------------------------------ postgres
// `postgres` reads pool stats + lock waits — both bounded in size
// regardless of fleet size. Single flat bench.

pub struct BenchPostgres {
    pub stats: PoolStats,
    pub waits: Vec<LockWait>,
}

#[async_trait]
impl PostgresClient for BenchPostgres {
    async fn pool_stats(&self) -> Result<PoolStats> {
        Ok(PoolStats {
            active: self.stats.active,
            max_conn: self.stats.max_conn,
        })
    }
    async fn lock_waits(&self) -> Result<Vec<LockWait>> {
        Ok(self
            .waits
            .iter()
            .map(|w| LockWait {
                waiting_pid: w.waiting_pid,
                blocking_pid: w.blocking_pid,
                relation: w.relation.clone(),
                wait_secs: w.wait_secs,
            })
            .collect())
    }
}

// ----------------------------------------------------------------- dpu
// `dpu` fleet rollup runs `assemble_checks` over the snapshot vec.
// `BenchDpuClient` returns N synthetic snapshots; the assembly cost
// dominates and is what we want to baseline.

pub fn fleet_dpu_snapshots(n: usize) -> Vec<DpuSnapshot> {
    let now = Utc::now();
    (0..n)
        .map(|i| {
            let drifted = i % 7 == 0;
            DpuSnapshot {
                dpu_id: format!("dpu-bench-{i:08x}"),
                applied_managed_host_config_version: "v42".into(),
                desired_managed_host_config_version: if drifted { "v43".into() } else { "v42".into() },
                applied_instance_network_config_version: "v7".into(),
                desired_instance_network_config_version: "v7".into(),
                quarantine_state: if i % 13 == 0 { Some("network".into()) } else { None },
                last_seen_at: now - chrono::Duration::minutes((i % 30) as i64),
                client_certificate_expiry: Some(now + chrono::Duration::days(180 + (i as i64 % 60))),
                health_alerts: Vec::new(),
                network_config_error: None,
                hbn_version: "1.4.0".into(),
                bgp_alerts: Vec::new(),
                extension_services_observed_at: Some(now - chrono::Duration::seconds(30)),
                extension_services: Vec::new(),
                infiniband_observed_at: Some(now - chrono::Duration::seconds(60)),
                infiniband_ufm_observable: Some(true),
                infiniband_ports: Vec::new(),
                ib_alerts: Vec::new(),
            }
        })
        .collect()
}

pub struct BenchDpuClient {
    snapshots: std::sync::Mutex<Vec<DpuSnapshot>>,
}

impl BenchDpuClient {
    pub fn new(snapshots: Vec<DpuSnapshot>) -> Self {
        Self {
            snapshots: std::sync::Mutex::new(snapshots),
        }
    }
}

#[async_trait]
impl DpuClient for BenchDpuClient {
    async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>> {
        Ok(self.snapshots.lock().unwrap().clone())
    }
}

pub fn dpu_config() -> DpuConfig {
    DpuConfig::default()
}

pub fn http_client() -> Arc<dyn HttpClient> {
    Arc::new(BenchHttp)
}
