//! Capability-based deployment-type detection — PRD-001.
//!
//! When `--deployment-type=auto`, the boot probe runs a signal ladder
//! (workload → namespace inventory → CRD inventory) to resolve the active
//! cluster to a [`DeploymentType`]. This module owns the signal
//! implementations and the thin cluster-shape primitives they call into.
//!
//! Slice 2 lands signal 1 — the workload probe — backed by a small
//! [`ClusterShapeProbe`] trait so the boot probe can drive detection
//! without taking a hard dep on `kube::Client` everywhere. Slice 3 adds
//! signal 2 — the namespace-inventory fallback ([`namespace_inventory_probe`]) —
//! consulted when the workload probe is inconclusive.

use anyhow::Result;
use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Namespace, Service};
use kube::api::Api;
use kube::Client;

use crate::config::DeploymentType;

/// Minimal cluster-shape primitives the detection ladder calls into.
/// Workload probe (slice 2) only uses these two; namespace inventory
/// (slice 3) and CRD inventory (slice 4) extend the trait.
#[async_trait]
pub trait ClusterShapeProbe: Send + Sync {
    async fn service_exists(&self, namespace: &str, name: &str) -> Result<bool>;
    async fn namespace_exists(&self, namespace: &str) -> Result<bool>;
}

/// Outcome of running the workload signal — signal 1 of the detection
/// ladder. `matched` is the resolved type when a known shape matches;
/// `observed_services` is what the probe found, formatted as
/// `<service>@<namespace>` strings (used in the boot-probe step's
/// no-match diagnostic).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadProbeOutcome {
    pub matched: Option<DeploymentType>,
    pub observed_services: Vec<String>,
}

/// Signal 1 of the detection ladder: query the cluster for known
/// Services and resolve to a [`DeploymentType`] when the result matches
/// one of the three documented shapes. Returns `Ok(matched: None, …)`
/// when no shape matches — the caller surfaces the observed-services
/// diagnostic in that case (slice 3 / 4 will fall through to the next
/// signal once they land).
pub async fn workload_probe(
    probe: &dyn ClusterShapeProbe,
) -> Result<WorkloadProbeOutcome> {
    // The three well-known Services we look for. `mock-core` is
    // definitive for `rest-only-mock`; `carbide-api` + `nico-rest-api`
    // means full; `carbide-api` with no `nico-rest` namespace means
    // core-only.
    let mock_core = probe
        .service_exists("nico-rest", "nico-rest-mock-core")
        .await?;
    let carbide = probe
        .service_exists("forge-system", "carbide-api")
        .await?;
    let rest_api = probe
        .service_exists("nico-rest", "nico-rest-api")
        .await?;
    let nico_rest_ns = probe.namespace_exists("nico-rest").await?;

    let mut observed = Vec::new();
    if carbide {
        observed.push("carbide-api@forge-system".to_string());
    }
    if mock_core {
        observed.push("nico-rest-mock-core@nico-rest".to_string());
    }
    if rest_api {
        observed.push("nico-rest-api@nico-rest".to_string());
    }

    let matched = if mock_core {
        Some(DeploymentType::RestOnlyMock)
    } else if carbide && rest_api {
        Some(DeploymentType::Full)
    } else if carbide && !nico_rest_ns {
        Some(DeploymentType::CoreOnly)
    } else {
        None
    };

    Ok(WorkloadProbeOutcome {
        matched,
        observed_services: observed,
    })
}

/// Signal 2 of the detection ladder — namespace inventory fallback.
///
/// Resolves the deployment-type from the presence/absence of
/// `forge-system` and `nico-rest`. Used when slice 2's
/// [`workload_probe`] returns `WorkloadProbeOutcome { matched: None, .. }`.
///
/// - `forge-system` present, `nico-rest` absent → `core-only`
/// - `forge-system` present, `nico-rest` present → `full`
/// - `forge-system` absent, `nico-rest` present → `rest-only-mock`
/// - otherwise → `None` (fall through to slice 4 / CRD inventory)
pub async fn namespace_inventory_probe(
    probe: &dyn ClusterShapeProbe,
) -> Result<Option<DeploymentType>> {
    let forge = probe.namespace_exists("forge-system").await?;
    let rest = probe.namespace_exists("nico-rest").await?;
    Ok(match (forge, rest) {
        (true, false) => Some(DeploymentType::CoreOnly),
        (true, true) => Some(DeploymentType::Full),
        (false, true) => Some(DeploymentType::RestOnlyMock),
        (false, false) => None,
    })
}

/// Real `kube::Client`-backed implementation. `Api::get(...)` returns
/// 404 for missing resources; we map that to `Ok(false)` and surface any
/// other error verbatim.
pub struct KubeClusterShapeProbe {
    client: Client,
}

impl KubeClusterShapeProbe {
    pub fn new(client: Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ClusterShapeProbe for KubeClusterShapeProbe {
    async fn service_exists(&self, namespace: &str, name: &str) -> Result<bool> {
        let api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        match api.get(name).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
            Err(e) => Err(anyhow::anyhow!(
                "failed to check service '{name}' in '{namespace}': {e}"
            )),
        }
    }

    async fn namespace_exists(&self, namespace: &str) -> Result<bool> {
        let api: Api<Namespace> = Api::all(self.client.clone());
        match api.get(namespace).await {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
            Err(e) => Err(anyhow::anyhow!(
                "failed to check namespace '{namespace}': {e}"
            )),
        }
    }
}

/// Test fakes — left always-public (no `#[cfg(test)]`) so downstream
/// crates' tests can share the same `MockClusterShapeProbe`. Mirrors
/// the convention used by `crate::k8s::testing`.
pub mod testing {
    use super::*;
    use std::collections::HashSet;

    /// In-memory probe that only reports presence for explicitly-set
    /// services / namespaces. Mirrors the tiny shape `workload_probe`
    /// queries — set the tuples you want present and the rest reports
    /// absent.
    #[derive(Default)]
    pub struct MockClusterShapeProbe {
        services: HashSet<(String, String)>,
        namespaces: HashSet<String>,
    }

    impl MockClusterShapeProbe {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn with_service(mut self, namespace: &str, name: &str) -> Self {
            self.services
                .insert((namespace.to_string(), name.to_string()));
            // Treat the namespace as present whenever we install a
            // Service into it — production has the same invariant.
            self.namespaces.insert(namespace.to_string());
            self
        }

        pub fn with_namespace(mut self, namespace: &str) -> Self {
            self.namespaces.insert(namespace.to_string());
            self
        }
    }

    #[async_trait]
    impl ClusterShapeProbe for MockClusterShapeProbe {
        async fn service_exists(&self, namespace: &str, name: &str) -> Result<bool> {
            Ok(self
                .services
                .contains(&(namespace.to_string(), name.to_string())))
        }

        async fn namespace_exists(&self, namespace: &str) -> Result<bool> {
            Ok(self.namespaces.contains(namespace))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::MockClusterShapeProbe;
    use super::*;

    #[tokio::test]
    async fn workload_probe_resolves_rest_only_mock_when_mock_core_service_present() {
        let probe = MockClusterShapeProbe::new()
            .with_service("nico-rest", "nico-rest-mock-core");
        let outcome = workload_probe(&probe).await.unwrap();
        assert_eq!(outcome.matched, Some(DeploymentType::RestOnlyMock));
        assert!(outcome
            .observed_services
            .iter()
            .any(|s| s == "nico-rest-mock-core@nico-rest"));
    }

    #[tokio::test]
    async fn workload_probe_resolves_full_when_carbide_and_rest_api_both_present() {
        let probe = MockClusterShapeProbe::new()
            .with_service("forge-system", "carbide-api")
            .with_service("nico-rest", "nico-rest-api");
        let outcome = workload_probe(&probe).await.unwrap();
        assert_eq!(outcome.matched, Some(DeploymentType::Full));
        assert!(outcome
            .observed_services
            .iter()
            .any(|s| s == "carbide-api@forge-system"));
        assert!(outcome
            .observed_services
            .iter()
            .any(|s| s == "nico-rest-api@nico-rest"));
    }

    #[tokio::test]
    async fn workload_probe_resolves_core_only_when_carbide_present_and_no_nico_rest_namespace() {
        let probe = MockClusterShapeProbe::new()
            .with_service("forge-system", "carbide-api");
        let outcome = workload_probe(&probe).await.unwrap();
        assert_eq!(outcome.matched, Some(DeploymentType::CoreOnly));
        assert_eq!(
            outcome.observed_services,
            vec!["carbide-api@forge-system".to_string()]
        );
    }

    #[tokio::test]
    async fn workload_probe_returns_none_when_no_known_services_present() {
        let probe = MockClusterShapeProbe::new();
        let outcome = workload_probe(&probe).await.unwrap();
        assert_eq!(outcome.matched, None);
        assert!(outcome.observed_services.is_empty());
    }

    #[tokio::test]
    async fn workload_probe_does_not_match_core_only_when_nico_rest_namespace_present_without_rest_api() {
        // Edge case: carbide-api visible, `nico-rest` namespace exists
        // (e.g., partial rollout), but neither `nico-rest-mock-core` nor
        // `nico-rest-api` Services are up. Per PRD signal-1 rules, this
        // is *not* core-only — fall through to slices 3/4.
        let probe = MockClusterShapeProbe::new()
            .with_service("forge-system", "carbide-api")
            .with_namespace("nico-rest");
        let outcome = workload_probe(&probe).await.unwrap();
        assert_eq!(outcome.matched, None);
        assert!(outcome
            .observed_services
            .iter()
            .any(|s| s == "carbide-api@forge-system"));
    }

    #[tokio::test]
    async fn namespace_inventory_resolves_core_only_when_only_forge_system_present() {
        let probe = MockClusterShapeProbe::new().with_namespace("forge-system");
        let got = namespace_inventory_probe(&probe).await.unwrap();
        assert_eq!(got, Some(DeploymentType::CoreOnly));
    }

    #[tokio::test]
    async fn namespace_inventory_resolves_full_when_both_namespaces_present() {
        let probe = MockClusterShapeProbe::new()
            .with_namespace("forge-system")
            .with_namespace("nico-rest");
        let got = namespace_inventory_probe(&probe).await.unwrap();
        assert_eq!(got, Some(DeploymentType::Full));
    }

    #[tokio::test]
    async fn namespace_inventory_resolves_rest_only_mock_when_only_nico_rest_present() {
        let probe = MockClusterShapeProbe::new().with_namespace("nico-rest");
        let got = namespace_inventory_probe(&probe).await.unwrap();
        assert_eq!(got, Some(DeploymentType::RestOnlyMock));
    }

    #[tokio::test]
    async fn namespace_inventory_returns_none_when_neither_namespace_present() {
        // Ambiguous-no-match: cluster has unrelated namespaces only.
        // Caller falls through to slice 4 (CRD inventory).
        let probe = MockClusterShapeProbe::new()
            .with_namespace("kube-system")
            .with_namespace("default");
        let got = namespace_inventory_probe(&probe).await.unwrap();
        assert_eq!(got, None);
    }
}
