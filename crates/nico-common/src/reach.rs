use std::time::Duration;

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::{Api, Client};
use kube::api::ListParams;
use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;

use crate::bootstrap::{run_with_budget, BootstrapStepError};
use crate::config::ReachMode;

/// A live port-forward endpoint backed by a local TCP listener.
/// Dropping this aborts the forwarding task and closes the listener.
pub struct ForwardedEndpoint {
    pub local_port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ForwardedEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Resolves service endpoints based on reach mode: opens in-process kube
/// port-forwards (port-forward mode) or returns cluster-DNS URLs (in-cluster mode).
pub struct ReachManager {
    pub mode: ReachMode,
    client: Client,
    namespace: String,
    postgres_namespace: String,
}

impl ReachManager {
    pub fn new(
        mode: ReachMode,
        client: Client,
        namespace: impl Into<String>,
        postgres_namespace: impl Into<String>,
    ) -> Self {
        Self { mode, client, namespace: namespace.into(), postgres_namespace: postgres_namespace.into() }
    }

    /// Resolve the Temporal gRPC address (`host:port`).
    ///
    /// Returns the address and, in port-forward mode, a guard that keeps the
    /// forward alive for the duration of the tool run. The port-forward
    /// setup is bounded by `budget`; exceeding it surfaces
    /// `BootstrapStepError::TimedOut(budget)`.
    pub async fn temporal_address(
        &self,
        budget: Duration,
    ) -> Result<(String, Option<ForwardedEndpoint>), BootstrapStepError> {
        match self.mode {
            ReachMode::InCluster => {
                let addr = format!("temporal-frontend.{}.svc.cluster.local:7233", self.namespace);
                Ok((addr, None))
            }
            ReachMode::PortForward => {
                let ns = self.namespace.clone();
                let ep = run_with_budget(budget, async {
                    self.forward_by_port(7233, &ns)
                        .await
                        .context("port-forward for Temporal gRPC (port 7233)")
                }).await?;
                Ok((format!("localhost:{}", ep.local_port), Some(ep)))
            }
        }
    }

    /// Resolve the Postgres DSN.
    ///
    /// Credentials come from `base_url`; only host:port is replaced so that
    /// config-supplied user/password/dbname flow through unchanged. The
    /// port-forward setup is bounded by `budget`.
    pub async fn postgres_url(
        &self,
        base_url: &str,
        budget: Duration,
    ) -> Result<(String, Option<ForwardedEndpoint>), BootstrapStepError> {
        match self.mode {
            ReachMode::InCluster => {
                let host = format!("postgresql.{}.svc.cluster.local", self.postgres_namespace);
                let url = replace_pg_host(base_url, &host, 5432)?;
                Ok((url, None))
            }
            ReachMode::PortForward => {
                let ns = self.postgres_namespace.clone();
                let ep = run_with_budget(budget, async {
                    self.forward_by_port(5432, &ns)
                        .await
                        .context("port-forward for Postgres (port 5432)")
                }).await?;
                let url = replace_pg_host(base_url, "localhost", ep.local_port)?;
                Ok((url, Some(ep)))
            }
        }
    }

    /// Resolve the Loki base URL. Port-forward setup is bounded by `budget`.
    pub async fn loki_url(
        &self,
        budget: Duration,
    ) -> Result<(String, Option<ForwardedEndpoint>), BootstrapStepError> {
        match self.mode {
            ReachMode::InCluster => {
                let url = format!("http://loki.{}.svc.cluster.local:3100", self.namespace);
                Ok((url, None))
            }
            ReachMode::PortForward => {
                let ns = self.namespace.clone();
                let ep = run_with_budget(budget, async {
                    self.forward_by_port(3100, &ns)
                        .await
                        .context("port-forward for Loki (port 3100)")
                }).await?;
                Ok((format!("http://localhost:{}", ep.local_port), Some(ep)))
            }
        }
    }

    /// Discover HTTP application services and return `(name, base_url)` pairs.
    ///
    /// In port-forward mode a `ForwardedEndpoint` guard is returned per
    /// service; each individual port-forward is bounded by `budget`. A
    /// timed-out PF is logged as a warning and the service is skipped —
    /// matching the prior best-effort behaviour for non-timeout errors.
    pub async fn http_endpoints(
        &self,
        budget: Duration,
    ) -> Result<(Vec<(String, String)>, Vec<ForwardedEndpoint>)> {
        let svcs = self.discover_http_services().await?;
        let mut endpoints = vec![];
        let mut guards = vec![];

        for (name, port) in svcs {
            match self.mode {
                ReachMode::InCluster => {
                    let url = format!(
                        "http://{}.{}.svc.cluster.local:{}",
                        name, self.namespace, port
                    );
                    endpoints.push((name, url));
                }
                ReachMode::PortForward => {
                    let ns = self.namespace.clone();
                    let result = run_with_budget(budget, async {
                        self.forward_service_port(&name, port, &ns).await
                    }).await;
                    match result {
                        Ok(ep) => {
                            let url = format!("http://localhost:{}", ep.local_port);
                            endpoints.push((name, url));
                            guards.push(ep);
                        }
                        Err(e) => {
                            eprintln!("nico: warn: port-forward for {name}:{port} failed: {e}");
                        }
                    }
                }
            }
        }

        Ok((endpoints, guards))
    }

    /// Find a service in `namespace` that exposes `port`, then open a port-forward for it.
    async fn forward_by_port(&self, port: u16, namespace: &str) -> Result<ForwardedEndpoint> {
        let services: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let svc_list = services
            .list(&ListParams::default())
            .await
            .context("list services")?;

        for svc in svc_list.items {
            let Some(svc_name) = svc.metadata.name.as_deref() else { continue };
            let has_port = svc.spec
                .as_ref()
                .and_then(|s| s.ports.as_ref())
                .map(|ps| ps.iter().any(|p| p.port == port as i32))
                .unwrap_or(false);
            if has_port {
                return self.forward_service_port(svc_name, port, namespace).await;
            }
        }

        anyhow::bail!("no service with port {port} found in namespace {namespace}")
    }

    async fn forward_service_port(&self, service_name: &str, port: u16, namespace: &str) -> Result<ForwardedEndpoint> {
        let pod_name = self
            .find_pod_for_service(service_name, namespace)
            .await
            .with_context(|| format!("find pod for service {service_name}"))?;
        open_port_forward(self.client.clone(), namespace.to_string(), pod_name, port).await
    }

    /// Resolve the pod selector of `service_name` and return a Running pod name.
    async fn find_pod_for_service(&self, service_name: &str, namespace: &str) -> Result<String> {
        let services: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let svc = services
            .get(service_name)
            .await
            .with_context(|| format!("get service {service_name}"))?;

        let selector = svc
            .spec
            .as_ref()
            .and_then(|s| s.selector.as_ref())
            .ok_or_else(|| anyhow::anyhow!("service {service_name} has no pod selector"))?;

        let label_sel = selector
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pod_list = pods
            .list(&ListParams::default().labels(&label_sel))
            .await
            .context("list pods for selector")?;

        pod_list
            .items
            .into_iter()
            .find(|pod| {
                pod.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    == Some("Running")
            })
            .and_then(|pod| pod.metadata.name)
            .ok_or_else(|| anyhow::anyhow!("no running pod found for service {service_name}"))
    }

    /// List services that look like HTTP application services (excluding
    /// well-known non-HTTP ports like Temporal gRPC, Postgres, and Loki).
    async fn discover_http_services(&self) -> Result<Vec<(String, u16)>> {
        let services: Api<Service> = Api::namespaced(self.client.clone(), &self.namespace);
        let svc_list = services
            .list(&ListParams::default())
            .await
            .context("list services")?;

        let mut result = vec![];
        for svc in svc_list.items {
            let Some(name) = svc.metadata.name else { continue };
            let Some(spec) = svc.spec.as_ref() else { continue };

            // Skip headless services.
            if spec.cluster_ip.as_deref() == Some("None") {
                continue;
            }

            let Some(ports) = spec.ports.as_ref() else { continue };

            let http_port = ports
                .iter()
                .find_map(|p| http_port_from(p.port, p.name.as_deref()));

            if let Some(port) = http_port {
                result.push((name, port));
            }
        }

        Ok(result)
    }
}

/// Returns the port number if the given service port looks like an HTTP application
/// port, or `None` if it should be skipped.
///
/// Excluded: well-known non-HTTP ports (Postgres, Temporal, Loki) and sidecar ports
/// (metrics on 1080, profiler on 1081). Also excluded: ports named `grpc` or `proxy`.
fn http_port_from(port: i32, name: Option<&str>) -> Option<u16> {
    const SKIP_PORTS: &[i32] = &[5432, 7233, 3100, 1080, 1081];
    const SKIP_NAMES: &[&str] = &["grpc", "proxy"];

    if SKIP_PORTS.contains(&port) {
        return None;
    }
    if name.map(|n| SKIP_NAMES.contains(&n)).unwrap_or(false) {
        return None;
    }
    let u = port as u16;
    if matches!(u, 80 | 8080 | 8443 | 443) {
        return Some(u);
    }
    if name
        .map(|n| n.contains("http") || n.contains("web"))
        .unwrap_or(false)
    {
        return Some(u);
    }
    None
}

/// Spawn a local TCP listener that tunnels each accepted connection through a
/// fresh kube port-forward to `pod_name:remote_port`.
async fn open_port_forward(
    client: Client,
    namespace: String,
    pod_name: String,
    remote_port: u16,
) -> Result<ForwardedEndpoint> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind local port for port-forward")?;
    let local_port = listener.local_addr()?.port();

    let task = tokio::spawn(async move {
        let pods: Api<Pod> = Api::namespaced(client, &namespace);
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else { break };
            let pods = pods.clone();
            let pod = pod_name.clone();
            tokio::spawn(async move {
                let Ok(mut pf) = pods.portforward(&pod, &[remote_port]).await else { return };
                let Some(mut pf_stream) = pf.take_stream(remote_port) else { return };
                let _ = copy_bidirectional(&mut tcp, &mut pf_stream).await;
                drop(pf);
            });
        }
    });

    Ok(ForwardedEndpoint { local_port, task })
}

/// Rewrite host:port in a Postgres DSN while preserving credentials and dbname.
///
/// Accepts `postgres[ql]://user:pass@host:port/db[?options]`.
pub fn replace_pg_host(base_url: &str, new_host: &str, new_port: u16) -> Result<String> {
    let (scheme, rest) = base_url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("invalid postgres URL: missing ://"))?;
    let (user_info, host_and_rest) = rest
        .split_once('@')
        .ok_or_else(|| anyhow::anyhow!("invalid postgres URL: missing @"))?;
    let path_and_query = host_and_rest
        .find('/')
        .map(|i| &host_and_rest[i..])
        .unwrap_or("");
    Ok(format!("{scheme}://{user_info}@{new_host}:{new_port}{path_and_query}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_port_1080_is_excluded() {
        assert_eq!(http_port_from(1080, Some("http")), None);
    }

    #[test]
    fn sidecar_port_1081_is_excluded() {
        assert_eq!(http_port_from(1081, Some("http")), None);
    }

    #[test]
    fn grpc_named_port_is_excluded() {
        assert_eq!(http_port_from(8080, Some("grpc")), None);
    }

    #[test]
    fn proxy_named_port_is_excluded() {
        assert_eq!(http_port_from(8080, Some("proxy")), None);
    }

    #[test]
    fn standard_http_port_by_number_is_discovered() {
        assert_eq!(http_port_from(8080, Some("http")), Some(8080));
    }

    #[test]
    fn nonstandard_port_with_http_name_is_discovered() {
        // e.g. carbide-api on port 1079 named "http"
        assert_eq!(http_port_from(1079, Some("http")), Some(1079));
    }

    #[test]
    fn port_with_web_name_is_discovered() {
        assert_eq!(http_port_from(9090, Some("web")), Some(9090));
    }

    #[test]
    fn replace_pg_host_basic() {
        let url = "postgres://nico:nico@localhost:5432/nico";
        let result = replace_pg_host(url, "127.0.0.1", 12345).unwrap();
        assert_eq!(result, "postgres://nico:nico@127.0.0.1:12345/nico");
    }

    #[test]
    fn replace_pg_host_with_options() {
        let url = "postgres://user:pass@db.example.com:5432/mydb?sslmode=require";
        let result = replace_pg_host(url, "localhost", 9999).unwrap();
        assert_eq!(result, "postgres://user:pass@localhost:9999/mydb?sslmode=require");
    }

    #[test]
    fn replace_pg_host_cluster_dns() {
        let url = "postgres://nico:nico@localhost:5432/nico";
        let host = "postgresql.nico.svc.cluster.local";
        let result = replace_pg_host(url, host, 5432).unwrap();
        assert_eq!(result, "postgres://nico:nico@postgresql.nico.svc.cluster.local:5432/nico");
    }
}
