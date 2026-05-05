use std::process;
use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use clap::Parser;
use nico_common::output::{OutputMode, Status};

mod formatter;
mod grpc;
mod http;
mod k8s;
mod layer;
mod layers;
mod loki;
mod postgres;
mod runner;
mod temporal;

use layer::RunOpts;

const LAYER_ORDER: &[&str] = &["cluster", "logs", "workflows", "health", "grpc", "postgres"];

#[derive(Parser)]
#[command(name = "nico-doctor", about = "Read-only health check for nico/ncx clusters")]
struct Cli {
    #[arg(short, long, help = "Kubernetes namespace", default_value = "nico")]
    namespace: String,

    #[arg(long, env = "NICO_CONTEXT", help = "Kubernetes context")]
    context: Option<String>,

    #[arg(long, value_delimiter = ',', help = "Layers to skip")]
    skip: Vec<String>,

    #[arg(long, default_value = "10m", help = "Look-back window for logs/events")]
    since: String,

    #[arg(long, default_value = "5s", help = "Per-check timeout")]
    timeout: String,

    #[arg(short, long, help = "Output JSON")]
    json: bool,

    #[arg(short, long, help = "Show details for passing checks")]
    verbose: bool,

    #[arg(long, help = "ASCII-only output")]
    ascii: bool,

    #[arg(long, help = "Disable color output")]
    no_color: bool,

    #[arg(long, env = "NICO_POSTGRES_URL", help = "Postgres connection URL")]
    postgres_url: Option<String>,
}

// --- Inactive client stubs ---
// Used when the backing service is absent or unconfigured.

struct InactiveK8sClient { reason: &'static str }

#[async_trait]
impl k8s::K8sClient for InactiveK8sClient {
    async fn list_pods(&self, _ns: &str) -> anyhow::Result<Vec<k8s::PodInfo>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
    async fn list_events(&self, _ns: &str, _since: Duration) -> anyhow::Result<Vec<k8s::EventInfo>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
    async fn pod_logs(&self, _ns: &str, _pod: &str, _since: Duration) -> anyhow::Result<Vec<String>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactiveLokiClient { reason: &'static str }

#[async_trait]
impl loki::LokiClient for InactiveLokiClient {
    async fn query_errors(&self, _ns: &str, _since: Duration, _limit: usize) -> anyhow::Result<loki::LokiQueryResult> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactiveHttpClient { reason: &'static str }

#[async_trait]
impl http::HttpClient for InactiveHttpClient {
    async fn get_status(&self, _url: &str) -> anyhow::Result<u16> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactiveGrpcInspector { reason: &'static str }

#[async_trait]
impl grpc::GrpcInspector for InactiveGrpcInspector {
    async fn inspect(&self, _addr: &str) -> anyhow::Result<grpc::GrpcInspectResult> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactivePostgresClient { reason: &'static str }

#[async_trait]
impl postgres::PostgresClient for InactivePostgresClient {
    async fn pool_stats(&self) -> anyhow::Result<postgres::PoolStats> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
    async fn lock_waits(&self) -> anyhow::Result<Vec<postgres::LockWait>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

fn exit_code(report: &runner::Report) -> i32 {
    match report.summary_status() {
        Status::Ok | Status::Skipped => 0,
        Status::Warn => 1,
        Status::Fail => 2,
        Status::Unknown => 3,
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let mode = OutputMode {
        color: !cli.no_color && std::env::var("NO_COLOR").is_err(),
        ascii: cli.ascii,
    };

    let since = humantime::parse_duration(&cli.since).unwrap_or(Duration::from_secs(600));
    let timeout = humantime::parse_duration(&cli.timeout).unwrap_or(Duration::from_secs(5));

    let opts = RunOpts { namespace: cli.namespace.clone(), since, timeout };

    // Build k8s client once — uses explicit context or auto-detects kubeconfig/in-cluster.
    let k8s_client: Option<Arc<dyn k8s::K8sClient>> =
        match k8s::KubeRsK8sClient::try_new(cli.context.as_deref()).await {
            Ok(c) => Some(Arc::new(c) as Arc<dyn k8s::K8sClient>),
            Err(_) => None,
        };

    // Build loki client when LOKI_URL is configured.
    let loki_client: Arc<dyn loki::LokiClient> = match std::env::var("LOKI_URL") {
        Ok(url) => Arc::new(loki::RealLokiClient::new(url)) as Arc<dyn loki::LokiClient>,
        Err(_) => Arc::new(InactiveLokiClient { reason: "LOKI_URL not set" }),
    };

    let mut layers: Vec<Box<dyn layer::Layer>> = vec![];

    for &name in LAYER_ORDER {
        if cli.skip.iter().any(|s| s.as_str() == name) {
            layers.push(layer::SkippedLayer::new(name));
            continue;
        }
        match name {
            "cluster" => {
                match k8s_client.as_ref() {
                    Some(k8s) => layers.push(Box::new(layers::cluster::ClusterLayer::new(k8s.clone()))),
                    None => layers.push(layer::UnconfiguredLayer::new(
                        "cluster", "kubeconfig not found; set --context or NICO_CONTEXT",
                    )),
                }
            }
            "logs" => {
                let has_loki = std::env::var("LOKI_URL").is_ok();
                match (k8s_client.as_ref(), has_loki) {
                    (Some(k8s), _) => {
                        layers.push(Box::new(layers::logs::LogsLayer::new(
                            loki_client.clone(),
                            k8s.clone(),
                        )));
                    }
                    (None, true) => {
                        layers.push(Box::new(layers::logs::LogsLayer::new(
                            loki_client.clone(),
                            Arc::new(InactiveK8sClient { reason: "kubeconfig not found" }),
                        )));
                    }
                    (None, false) => {
                        layers.push(layer::UnconfiguredLayer::new(
                            "logs", "set LOKI_URL or ensure kubeconfig is accessible",
                        ));
                    }
                }
            }
            "workflows" => {
                match std::env::var("NICO_TEMPORAL_ADDRESS") {
                    Ok(addr) => {
                        let namespace = std::env::var("NICO_TEMPORAL_NAMESPACE")
                            .unwrap_or_else(|_| "default".to_string());
                        let stuck_threshold = std::env::var("NICO_STUCK_THRESHOLD")
                            .ok()
                            .and_then(|s| humantime::parse_duration(&s).ok())
                            .unwrap_or(Duration::from_secs(30 * 60));
                        layers.push(Box::new(layers::workflows::WorkflowsLayer::new(
                            Arc::new(temporal::RealTemporalClient::new(addr, namespace)),
                            stuck_threshold,
                        )));
                    }
                    Err(_) => layers.push(layer::UnconfiguredLayer::new(
                        "workflows", "set NICO_TEMPORAL_ADDRESS to enable",
                    )),
                }
            }
            "health" => {
                let endpoints_str = std::env::var("NICO_HEALTH_ENDPOINTS").ok();
                match endpoints_str.as_deref() {
                    Some(s) if !s.is_empty() => {
                        let services: Vec<http::ServiceEndpoint> = s.split(',')
                            .map(|u| u.trim().to_string())
                            .filter(|u| !u.is_empty())
                            .map(|url| http::ServiceEndpoint { name: url.clone(), base_url: url })
                            .collect();
                        layers.push(Box::new(layers::health::HealthLayer::new(
                            // TODO: replace with real reqwest-based HttpClient
                            Arc::new(InactiveHttpClient { reason: "http client not yet wired" }),
                            services,
                        )));
                    }
                    _ => layers.push(layer::UnconfiguredLayer::new(
                        "health", "set NICO_HEALTH_ENDPOINTS to enable",
                    )),
                }
            }
            "grpc" => {
                let grpc_addr = std::env::var("NICO_GRPC_ADDRESS").ok()
                    .or_else(|| std::env::var("NICO_TEMPORAL_ADDRESS").ok());
                match grpc_addr {
                    Some(addr) => layers.push(Box::new(layers::grpc::GrpcLayer::new(
                        // TODO #27: replace with real tonic-based GrpcInspector
                        Arc::new(InactiveGrpcInspector { reason: "grpc inspector not yet wired (see #27)" }),
                        addr,
                    ))),
                    None => layers.push(layer::UnconfiguredLayer::new(
                        "grpc", "set NICO_GRPC_ADDRESS or NICO_TEMPORAL_ADDRESS to enable",
                    )),
                }
            }
            "postgres" => {
                match cli.postgres_url.as_deref() {
                    Some(url) => match postgres::SqlxPostgresClient::new(url) {
                        Ok(pg) => layers.push(Box::new(layers::postgres::PostgresLayer::new(Arc::new(pg)))),
                        Err(e) => {
                            eprintln!("warning: postgres URL invalid: {e}");
                            layers.push(Box::new(layers::postgres::PostgresLayer::new(
                                Arc::new(InactivePostgresClient { reason: "invalid postgres URL" }),
                            )));
                        }
                    },
                    None => layers.push(Box::new(layers::postgres::PostgresLayer::new(
                        Arc::new(InactivePostgresClient { reason: "NICO_POSTGRES_URL not set" }),
                    ))),
                }
            }
            _ => {}
        }
    }

    let report = runner::run(&layers, &opts).await;

    if cli.json {
        println!("{}", formatter::format_json(&report, &cli.namespace));
    } else {
        print!("{}", formatter::format_report(&report, &mode, cli.verbose));
    }

    process::exit(exit_code(&report));
}
