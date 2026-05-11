//! Per-layer Source-trait counting decorators (PRD-005 Slice 0b.2).
//!
//! Companion to `perf_instrument.rs` (raw-wire decorators from Slice 0b.1)
//! and `nico_common::perf`. The Source layer's dominant cost is the
//! `serde_json::Value` → typed-struct parse that lives inside each Sqlx
//! impl (PRD-005 Findings #7 + #8). Each `Counting<X>Client` wraps any
//! `T: <X>Client`, forwards every trait call to the inner client, and
//! brackets it with `Instant::now()` to record `deserialize_time`. The
//! raw-wire decorators from Slice 0b.1 sit below this layer and capture
//! the SQL/network cost separately, so the two layers compose without
//! double-counting.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;

use crate::dpu::{DpuClient, DpuSnapshot};
use crate::dpu_cert::{CertSnapshot, DpuCertClient};
use crate::dpu_health::{DpuHealthClient, HealthSnapshot};
use crate::dpu_isolation::{DpuIsolationClient, IsolationSnapshot};
use crate::dpu_services::{DpuServicesClient, ServicesSnapshot};
use crate::hbn::{HbnClient, HbnSnapshot};

/// Snapshot of one Source-trait method's captured counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMethodStats {
    pub call_count: u64,
    pub deserialize_time_total: Duration,
    pub deserialize_time_p50: Duration,
    pub deserialize_time_p99: Duration,
}

/// Per-method counter cell for Source-trait decorators.
#[derive(Debug, Default)]
pub struct SourceMethodCounter {
    call_count: AtomicU64,
    samples: Mutex<Vec<Duration>>,
}

impl SourceMethodCounter {
    /// Append one observation.
    pub fn record(&self, deserialize_time: Duration) {
        self.call_count.fetch_add(1, Ordering::Relaxed);
        self.samples.lock().unwrap().push(deserialize_time);
    }

    /// Snapshot the current totals + percentile breakdown.
    pub fn snapshot(&self) -> SourceMethodStats {
        let mut s = self.samples.lock().unwrap().clone();
        s.sort();
        let total: Duration = s.iter().copied().sum();
        let (p50, p99) = percentiles(&s);
        SourceMethodStats {
            call_count: self.call_count.load(Ordering::Relaxed),
            deserialize_time_total: total,
            deserialize_time_p50: p50,
            deserialize_time_p99: p99,
        }
    }
}

fn percentiles(sorted: &[Duration]) -> (Duration, Duration) {
    if sorted.is_empty() {
        return (Duration::ZERO, Duration::ZERO);
    }
    let idx = |q: f64| {
        let last = sorted.len() - 1;
        let i = (q * last as f64).round() as usize;
        i.min(last)
    };
    (sorted[idx(0.50)], sorted[idx(0.99)])
}

// -- DpuClient ----------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DpuSourceStats {
    pub fetch_fleet: SourceMethodStats,
}

pub struct CountingDpuClient<T: DpuClient> {
    inner: T,
    fetch_fleet: SourceMethodCounter,
}

impl<T: DpuClient> CountingDpuClient<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            fetch_fleet: SourceMethodCounter::default(),
        }
    }

    pub fn stats(&self) -> DpuSourceStats {
        DpuSourceStats {
            fetch_fleet: self.fetch_fleet.snapshot(),
        }
    }
}

#[async_trait]
impl<T: DpuClient> DpuClient for CountingDpuClient<T> {
    async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>> {
        let start = Instant::now();
        let result = self.inner.fetch_fleet().await;
        self.fetch_fleet.record(start.elapsed());
        result
    }
}

// -- DpuHealthClient ----------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DpuHealthSourceStats {
    pub fetch_snapshot: SourceMethodStats,
}

pub struct CountingDpuHealthClient<T: DpuHealthClient> {
    inner: T,
    fetch_snapshot: SourceMethodCounter,
}

impl<T: DpuHealthClient> CountingDpuHealthClient<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            fetch_snapshot: SourceMethodCounter::default(),
        }
    }

    pub fn stats(&self) -> DpuHealthSourceStats {
        DpuHealthSourceStats {
            fetch_snapshot: self.fetch_snapshot.snapshot(),
        }
    }
}

#[async_trait]
impl<T: DpuHealthClient> DpuHealthClient for CountingDpuHealthClient<T> {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HealthSnapshot>> {
        let start = Instant::now();
        let result = self.inner.fetch_snapshot(dpu_id).await;
        self.fetch_snapshot.record(start.elapsed());
        result
    }
}

// -- DpuServicesClient --------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DpuServicesSourceStats {
    pub fetch_snapshot: SourceMethodStats,
}

pub struct CountingDpuServicesClient<T: DpuServicesClient> {
    inner: T,
    fetch_snapshot: SourceMethodCounter,
}

impl<T: DpuServicesClient> CountingDpuServicesClient<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            fetch_snapshot: SourceMethodCounter::default(),
        }
    }

    pub fn stats(&self) -> DpuServicesSourceStats {
        DpuServicesSourceStats {
            fetch_snapshot: self.fetch_snapshot.snapshot(),
        }
    }
}

#[async_trait]
impl<T: DpuServicesClient> DpuServicesClient for CountingDpuServicesClient<T> {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<ServicesSnapshot>> {
        let start = Instant::now();
        let result = self.inner.fetch_snapshot(dpu_id).await;
        self.fetch_snapshot.record(start.elapsed());
        result
    }
}

// -- DpuIsolationClient -------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DpuIsolationSourceStats {
    pub fetch_snapshot: SourceMethodStats,
}

pub struct CountingDpuIsolationClient<T: DpuIsolationClient> {
    inner: T,
    fetch_snapshot: SourceMethodCounter,
}

impl<T: DpuIsolationClient> CountingDpuIsolationClient<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            fetch_snapshot: SourceMethodCounter::default(),
        }
    }

    pub fn stats(&self) -> DpuIsolationSourceStats {
        DpuIsolationSourceStats {
            fetch_snapshot: self.fetch_snapshot.snapshot(),
        }
    }
}

#[async_trait]
impl<T: DpuIsolationClient> DpuIsolationClient for CountingDpuIsolationClient<T> {
    async fn fetch_snapshot(&self, machine_id: &str) -> Result<IsolationSnapshot> {
        let start = Instant::now();
        let result = self.inner.fetch_snapshot(machine_id).await;
        self.fetch_snapshot.record(start.elapsed());
        result
    }
}

// -- DpuCertClient ------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DpuCertSourceStats {
    pub fetch_snapshot: SourceMethodStats,
}

pub struct CountingDpuCertClient<T: DpuCertClient> {
    inner: T,
    fetch_snapshot: SourceMethodCounter,
}

impl<T: DpuCertClient> CountingDpuCertClient<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            fetch_snapshot: SourceMethodCounter::default(),
        }
    }

    pub fn stats(&self) -> DpuCertSourceStats {
        DpuCertSourceStats {
            fetch_snapshot: self.fetch_snapshot.snapshot(),
        }
    }
}

#[async_trait]
impl<T: DpuCertClient> DpuCertClient for CountingDpuCertClient<T> {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<CertSnapshot> {
        let start = Instant::now();
        let result = self.inner.fetch_snapshot(dpu_id).await;
        self.fetch_snapshot.record(start.elapsed());
        result
    }
}

// -- HbnClient ----------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct HbnSourceStats {
    pub fetch_snapshot: SourceMethodStats,
    pub fetch_all_snapshots: SourceMethodStats,
}

pub struct CountingHbnClient<T: HbnClient> {
    inner: T,
    fetch_snapshot: SourceMethodCounter,
    fetch_all_snapshots: SourceMethodCounter,
}

impl<T: HbnClient> CountingHbnClient<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            fetch_snapshot: SourceMethodCounter::default(),
            fetch_all_snapshots: SourceMethodCounter::default(),
        }
    }

    pub fn stats(&self) -> HbnSourceStats {
        HbnSourceStats {
            fetch_snapshot: self.fetch_snapshot.snapshot(),
            fetch_all_snapshots: self.fetch_all_snapshots.snapshot(),
        }
    }
}

#[async_trait]
impl<T: HbnClient> HbnClient for CountingHbnClient<T> {
    async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HbnSnapshot>> {
        let start = Instant::now();
        let result = self.inner.fetch_snapshot(dpu_id).await;
        self.fetch_snapshot.record(start.elapsed());
        result
    }

    async fn fetch_all_snapshots(&self) -> Result<Vec<HbnSnapshot>> {
        let start = Instant::now();
        let result = self.inner.fetch_all_snapshots().await;
        self.fetch_all_snapshots.record(start.elapsed());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpu::{DpuSnapshot, HealthAlert};
    use chrono::Utc;
    use std::sync::Mutex as StdMutex;

    fn fleet_snap(id: &str) -> DpuSnapshot {
        DpuSnapshot {
            dpu_id: id.into(),
            applied_managed_host_config_version: "v1".into(),
            desired_managed_host_config_version: "v1".into(),
            applied_instance_network_config_version: "v1".into(),
            desired_instance_network_config_version: "v1".into(),
            quarantine_state: None,
            last_seen_at: Utc::now(),
            client_certificate_expiry: None,
            health_alerts: Vec::<HealthAlert>::new(),
            network_config_error: None,
            hbn_version: String::new(),
            bgp_alerts: Vec::new(),
            extension_services_observed_at: None,
            extension_services: Vec::new(),
            infiniband_observed_at: None,
            infiniband_ufm_observable: None,
            infiniband_ports: Vec::new(),
            ib_alerts: Vec::new(),
        }
    }

    struct StubDpuClient {
        rows: StdMutex<Vec<serde_json::Value>>,
        err: Option<String>,
    }

    impl StubDpuClient {
        fn new() -> Self {
            Self {
                rows: StdMutex::new(Vec::new()),
                err: None,
            }
        }
        fn with_rows(mut self, rows: Vec<serde_json::Value>) -> Self {
            self.rows = StdMutex::new(rows);
            self
        }
        fn with_err(mut self, msg: &str) -> Self {
            self.err = Some(msg.into());
            self
        }
    }

    #[async_trait]
    impl DpuClient for StubDpuClient {
        async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>> {
            if let Some(e) = &self.err {
                return Err(anyhow::anyhow!("{e}"));
            }
            // Real parse work so deserialize_time on real fixtures is
            // non-zero — drains the JSON blobs and calls the production
            // parsers, mirroring what SqlxDpuClient does after fetch.
            let rows = self.rows.lock().unwrap().clone();
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let id = row.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let services = crate::dpu::parse_extension_services(
                    row.get("network_status_observation")
                        .and_then(|n| n.get("extension_service_observation"))
                        .and_then(|e| e.get("extension_service_statuses")),
                );
                let mut snap = fleet_snap(&id);
                snap.extension_services = services;
                out.push(snap);
            }
            Ok(out)
        }
    }

    #[tokio::test]
    async fn dpu_fetch_fleet_increments_call_count_per_call() {
        let client = CountingDpuClient::new(StubDpuClient::new());
        client.fetch_fleet().await.unwrap();
        client.fetch_fleet().await.unwrap();
        client.fetch_fleet().await.unwrap();
        assert_eq!(client.stats().fetch_fleet.call_count, 3);
    }

    #[tokio::test]
    async fn dpu_fetch_fleet_records_non_zero_deserialize_time_on_real_fixture() {
        let rows = crate::perf_fixtures::synthesize_fleet(50);
        let client = CountingDpuClient::new(StubDpuClient::new().with_rows(rows));
        client.fetch_fleet().await.unwrap();
        let stats = client.stats().fetch_fleet;
        assert_eq!(stats.call_count, 1);
        assert!(
            stats.deserialize_time_total > Duration::ZERO,
            "deserialize_time_total should be non-zero on a 50-row fixture, got {:?}",
            stats.deserialize_time_total,
        );
    }

    #[tokio::test]
    async fn dpu_fetch_fleet_errored_call_still_counts() {
        let client =
            CountingDpuClient::new(StubDpuClient::new().with_err("forgedb unreachable"));
        let result = client.fetch_fleet().await;
        assert!(result.is_err());
        assert_eq!(client.stats().fetch_fleet.call_count, 1);
    }

    // -- DpuHealthClient tests ----------------------------------------

    struct StubHealthClient {
        rows: StdMutex<Vec<serde_json::Value>>,
    }

    #[async_trait]
    impl DpuHealthClient for StubHealthClient {
        async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HealthSnapshot>> {
            // Real parse work: drive the production parsers over a
            // synthesised `machines` row so `deserialize_time` reflects
            // the same cost path the Sqlx impl pays.
            let rows = self.rows.lock().unwrap().clone();
            let Some(row) = rows.first().cloned() else {
                return Ok(None);
            };
            let agent_report = row.get("dpu_agent_health_report").cloned();
            let _alerts = crate::dpu::parse_health_alerts(agent_report.as_ref());
            let extension_services = crate::dpu::parse_extension_services(
                row.get("network_status_observation")
                    .and_then(|n| n.get("extension_service_observation"))
                    .and_then(|e| e.get("extension_service_statuses")),
            );
            Ok(Some(HealthSnapshot {
                dpu_id: dpu_id.into(),
                agent_version: None,
                agent_version_superseded_at: None,
                alerts: Vec::new(),
                interfaces: Vec::new(),
                client_certificate_expiry: None,
                quarantine_state: None,
                last_seen_at: None,
                registered: true,
                scout_discovery_complete: true,
                hbn_version: String::new(),
                network_config_error: None,
                applied_managed_host_config_version: String::new(),
                desired_managed_host_config_version: String::new(),
                applied_instance_network_config_version: String::new(),
                desired_instance_network_config_version: String::new(),
                bgp_alerts: Vec::new(),
                extension_services_observed_at: None,
                extension_services,
                infiniband_observed_at: None,
                infiniband_ufm_observable: None,
                infiniband_ports: Vec::new(),
                ib_alerts: Vec::new(),
            }))
        }
    }

    #[tokio::test]
    async fn dpu_health_fetch_snapshot_counts_calls_and_records_deserialize_time() {
        let rows = crate::perf_fixtures::synthesize_fleet(1);
        let client = CountingDpuHealthClient::new(StubHealthClient {
            rows: StdMutex::new(rows),
        });
        client.fetch_snapshot("dpu-a").await.unwrap();
        client.fetch_snapshot("dpu-b").await.unwrap();
        let stats = client.stats().fetch_snapshot;
        assert_eq!(stats.call_count, 2);
        assert!(stats.deserialize_time_total > Duration::ZERO);
    }

    // -- DpuServicesClient tests --------------------------------------

    struct StubServicesClient {
        rows: StdMutex<Vec<serde_json::Value>>,
    }

    #[async_trait]
    impl DpuServicesClient for StubServicesClient {
        async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<ServicesSnapshot>> {
            let rows = self.rows.lock().unwrap().clone();
            let Some(row) = rows.first().cloned() else {
                return Ok(None);
            };
            let services = crate::dpu::parse_extension_services(
                row.get("network_status_observation")
                    .and_then(|n| n.get("extension_service_observation"))
                    .and_then(|e| e.get("extension_service_statuses")),
            );
            Ok(Some(ServicesSnapshot {
                dpu_id: dpu_id.into(),
                observed_at: None,
                services,
            }))
        }
    }

    #[tokio::test]
    async fn dpu_services_fetch_snapshot_counts_calls_and_records_deserialize_time() {
        let rows = crate::perf_fixtures::synthesize_fleet(1);
        let client = CountingDpuServicesClient::new(StubServicesClient {
            rows: StdMutex::new(rows),
        });
        client.fetch_snapshot("dpu-a").await.unwrap();
        let stats = client.stats().fetch_snapshot;
        assert_eq!(stats.call_count, 1);
        assert!(stats.deserialize_time_total > Duration::ZERO);
    }

    // -- DpuIsolationClient tests -------------------------------------

    struct StubIsolationClient {
        rows: StdMutex<Vec<serde_json::Value>>,
    }

    #[async_trait]
    impl DpuIsolationClient for StubIsolationClient {
        async fn fetch_snapshot(&self, machine_id: &str) -> Result<IsolationSnapshot> {
            let rows = self.rows.lock().unwrap().clone();
            // Force a serde_json::Value pass to mimic the Sqlx impl's
            // JSON drill-down for quarantine_state.
            let quarantine = rows
                .first()
                .and_then(|r| r.get("network_config"))
                .and_then(|n| n.get("quarantine_state"))
                .and_then(|q| q.get("mode"))
                .and_then(|m| m.as_str())
                .map(str::to_owned);
            Ok(IsolationSnapshot {
                machine_id: machine_id.into(),
                registered: true,
                scout_discovery_complete: true,
                quarantine_state: quarantine,
                last_seen_at: None,
            })
        }
    }

    #[tokio::test]
    async fn dpu_isolation_fetch_snapshot_counts_calls_and_records_deserialize_time() {
        let rows = crate::perf_fixtures::synthesize_fleet(1);
        let client = CountingDpuIsolationClient::new(StubIsolationClient {
            rows: StdMutex::new(rows),
        });
        client.fetch_snapshot("machine-a").await.unwrap();
        client.fetch_snapshot("machine-b").await.unwrap();
        client.fetch_snapshot("machine-c").await.unwrap();
        let stats = client.stats().fetch_snapshot;
        assert_eq!(stats.call_count, 3);
        assert!(stats.deserialize_time_total > Duration::ZERO);
    }

    // -- DpuCertClient tests ------------------------------------------

    struct StubCertClient {
        rows: StdMutex<Vec<serde_json::Value>>,
    }

    #[async_trait]
    impl DpuCertClient for StubCertClient {
        async fn fetch_snapshot(&self, dpu_id: &str) -> Result<CertSnapshot> {
            let rows = self.rows.lock().unwrap().clone();
            // Mirror SqlxDpuCertClient's i64 epoch parse out of the
            // network_status_observation JSON.
            let expiry = rows
                .first()
                .and_then(|r| r.get("network_status_observation"))
                .and_then(|n| n.get("client_certificate_expiry"))
                .and_then(|v| v.as_i64())
                .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0));
            Ok(CertSnapshot {
                dpu_id: dpu_id.into(),
                client_certificate_expiry: expiry,
            })
        }
    }

    #[tokio::test]
    async fn dpu_cert_fetch_snapshot_counts_calls_and_records_deserialize_time() {
        let rows = crate::perf_fixtures::synthesize_fleet(1);
        let client = CountingDpuCertClient::new(StubCertClient {
            rows: StdMutex::new(rows),
        });
        client.fetch_snapshot("dpu-a").await.unwrap();
        let stats = client.stats().fetch_snapshot;
        assert_eq!(stats.call_count, 1);
        assert!(stats.deserialize_time_total > Duration::ZERO);
    }

    // -- HbnClient tests ----------------------------------------------

    struct StubHbnClient {
        rows: StdMutex<Vec<serde_json::Value>>,
    }

    impl StubHbnClient {
        fn build_snapshot(row: &serde_json::Value, dpu_id: &str) -> HbnSnapshot {
            let nco = row.get("network_status_observation");
            let bgp_alerts =
                crate::hbn::parse_bgp_alerts(row.get("dpu_agent_health_report"));
            HbnSnapshot {
                dpu_id: dpu_id.into(),
                hbn_version: String::new(),
                applied_managed_host_config_version: nco
                    .and_then(|v| v.get("network_config_version"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                desired_managed_host_config_version: String::new(),
                applied_instance_network_config_version: String::new(),
                desired_instance_network_config_version: String::new(),
                network_config_error: None,
                bgp_alerts,
                quarantine_state: None,
                last_seen_at: Utc::now(),
            }
        }
    }

    #[async_trait]
    impl HbnClient for StubHbnClient {
        async fn fetch_snapshot(&self, dpu_id: &str) -> Result<Option<HbnSnapshot>> {
            let rows = self.rows.lock().unwrap().clone();
            Ok(rows.first().map(|r| Self::build_snapshot(r, dpu_id)))
        }

        async fn fetch_all_snapshots(&self) -> Result<Vec<HbnSnapshot>> {
            let rows = self.rows.lock().unwrap().clone();
            Ok(rows
                .iter()
                .enumerate()
                .map(|(i, r)| Self::build_snapshot(r, &format!("dpu-{i}")))
                .collect())
        }
    }

    #[tokio::test]
    async fn hbn_fetch_snapshot_counts_calls_and_records_deserialize_time() {
        let rows = crate::perf_fixtures::synthesize_fleet(1);
        let client = CountingHbnClient::new(StubHbnClient {
            rows: StdMutex::new(rows),
        });
        client.fetch_snapshot("dpu-a").await.unwrap();
        let stats = client.stats().fetch_snapshot;
        assert_eq!(stats.call_count, 1);
        assert!(stats.deserialize_time_total > Duration::ZERO);
    }

    #[tokio::test]
    async fn hbn_fetch_all_snapshots_counts_independently_of_per_dpu() {
        let rows = crate::perf_fixtures::synthesize_fleet(5);
        let client = CountingHbnClient::new(StubHbnClient {
            rows: StdMutex::new(rows),
        });
        client.fetch_all_snapshots().await.unwrap();
        client.fetch_snapshot("dpu-x").await.unwrap();
        client.fetch_all_snapshots().await.unwrap();

        let stats = client.stats();
        assert_eq!(stats.fetch_snapshot.call_count, 1);
        assert_eq!(stats.fetch_all_snapshots.call_count, 2);
        assert!(stats.fetch_all_snapshots.deserialize_time_total > Duration::ZERO);
    }

    // -- Composability test -------------------------------------------
    //
    // The Source decorator wraps the per-layer trait; Slice 0b.1's
    // raw-wire `CountingPostgresClient` wraps the underlying `PostgresClient`.
    // A real `SqlxDpuClient` does its own SQL and never goes through
    // `PostgresClient`, but the two decorator families ARE designed to
    // co-exist in a single bench/test harness without their counters
    // bleeding into each other. This test pins that: a stub Source
    // routes its work through a raw-wire-decorated PostgresClient, then
    // we wrap the stub with the Source decorator. The Source decorator
    // captures one fetch_fleet, the raw-wire decorator captures one
    // pool_stats — independent counters, no double-count, both surface
    // their own snapshot.

    use crate::perf_instrument::CountingPostgresClient;
    use crate::postgres::{LockWait, PoolStats, PostgresClient};
    use std::sync::Arc;

    struct InProcPostgres;

    #[async_trait]
    impl PostgresClient for InProcPostgres {
        async fn pool_stats(&self) -> Result<PoolStats> {
            Ok(PoolStats {
                active: 1,
                max_conn: 10,
            })
        }
        async fn lock_waits(&self) -> Result<Vec<LockWait>> {
            Ok(Vec::new())
        }
    }

    struct DpuClientThroughPg {
        pg: Arc<CountingPostgresClient<InProcPostgres>>,
    }

    #[async_trait]
    impl DpuClient for DpuClientThroughPg {
        async fn fetch_fleet(&self) -> Result<Vec<DpuSnapshot>> {
            // Stand-in for the parse work an Sqlx impl would do, with a
            // raw-wire client call mixed in so the composed-decoration
            // path exercises both layers.
            let _ = self.pg.pool_stats().await?;
            Ok(vec![fleet_snap("dpu-x")])
        }
    }

    #[tokio::test]
    async fn source_and_raw_wire_decorators_compose_without_double_counting() {
        let pg = Arc::new(CountingPostgresClient::new(InProcPostgres));
        let dpu = DpuClientThroughPg { pg: pg.clone() };
        let counted_dpu = CountingDpuClient::new(dpu);

        counted_dpu.fetch_fleet().await.unwrap();

        // Source decorator sees exactly one fetch_fleet.
        let source_stats = counted_dpu.stats();
        assert_eq!(source_stats.fetch_fleet.call_count, 1);
        assert!(source_stats.fetch_fleet.deserialize_time_total > Duration::ZERO);

        // Raw-wire decorator (the layer beneath the Source) sees exactly
        // one pool_stats — no double-count.
        let raw_stats = pg.stats();
        assert_eq!(raw_stats.pool_stats.call_count, 1);
        assert_eq!(raw_stats.lock_waits.call_count, 0);
    }

    #[test]
    fn percentiles_empty_returns_zero() {
        assert_eq!(percentiles(&[]), (Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn percentiles_resolves_via_index_rounding() {
        let s: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let (p50, p99) = percentiles(&s);
        assert_eq!(p50, Duration::from_millis(51));
        assert_eq!(p99, Duration::from_millis(99));
    }
}
