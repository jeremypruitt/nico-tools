use std::io::IsTerminal;
use std::process;
use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use clap::Parser;
use nico_common::config::{Config, ConfigOverrides, ColorMode, OutputFormat, ReachMode};
use nico_common::output::{OutputMode, Status};
use nico_common::reach::ReachManager;

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
mod tui;

use layer::RunOpts;

const LAYER_ORDER: &[&str] = &["cluster", "logs", "workflows", "health", "grpc", "postgres"];

#[derive(Parser)]
#[command(name = "nico-doctor", about = "Read-only health check for nico/ncx clusters")]
struct Cli {
    #[arg(short, long, help = "Kubernetes namespace")]
    namespace: Option<String>,

    #[arg(long, help = "Kubernetes context")]
    context: Option<String>,

    #[arg(long, value_delimiter = ',', help = "Layers to skip")]
    skip: Vec<String>,

    #[arg(long, default_value = "10m", help = "Look-back window for logs/events")]
    since: String,

    #[arg(long, default_value = "5s", help = "Per-check timeout")]
    timeout: String,

    #[arg(long, help = "Launch interactive TUI dashboard (requires a TTY; mutually exclusive with --json)")]
    tui: bool,

    #[arg(long, value_name = "DURATION", help = "TUI refresh interval (default: 30s, or [output] tui_refresh in config)")]
    interval: Option<String>,

    #[arg(short, long, help = "Output JSON")]
    json: bool,

    #[arg(short, long, help = "Show details for passing checks")]
    verbose: bool,

    #[arg(long, help = "ASCII-only output")]
    ascii: bool,

    #[arg(long, help = "Disable color output")]
    no_color: bool,

    #[arg(long, help = "Postgres connection URL")]
    postgres_url: Option<String>,

    #[arg(long, value_name = "PATH", help = "Config file path (default: ~/.config/nico-tools/config.toml)")]
    config: Option<String>,

    #[arg(
        long,
        value_name = "MODE",
        help = "Reach mode: port-forward or in-cluster (default: auto-detect from KUBERNETES_SERVICE_HOST)"
    )]
    mode: Option<String>,
}

struct Unavailable { reason: &'static str }

#[async_trait]
impl k8s::K8sClient for Unavailable {
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

#[async_trait]
impl loki::LokiClient for Unavailable {
    async fn query_errors(&self, _ns: &str, _since: Duration, _limit: usize) -> anyhow::Result<loki::LokiQueryResult> {
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

    // Guard: --tui and --json are mutually exclusive.
    if cli.tui && cli.json {
        eprintln!("error: --tui and --json are mutually exclusive");
        process::exit(3);
    }

    // Guard: --tui requires an interactive terminal.
    if cli.tui && !std::io::stdout().is_terminal() {
        eprintln!("`--tui` requires an interactive terminal (stdout is not a TTY)");
        process::exit(3);
    }

    if cli.tui {
        tui::install_panic_hook();
    }

    // Load config file from --config or the default path.
    let config_path = cli.config.as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config/nico-tools/config.toml")
        });
    let file_toml = std::fs::read_to_string(&config_path).ok();

    // Parse --mode flag into a ReachMode override.
    let mode_override = match cli.mode.as_deref() {
        Some("port-forward") => Some(ReachMode::PortForward),
        Some("in-cluster") => Some(ReachMode::InCluster),
        Some(other) => {
            eprintln!("error: unknown --mode {other:?}; use port-forward or in-cluster");
            process::exit(1);
        }
        None => None,
    };

    // Parse --interval flag into a Duration override.
    let interval_override = match cli.interval.as_deref() {
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("error: invalid --interval {s:?}: {e}");
                process::exit(1);
            }
        },
        None => None,
    };

    // CLI flags are highest precedence; env and file layers are handled by Config::load.
    let overrides = ConfigOverrides {
        namespace: cli.namespace.clone(),
        context: cli.context.clone(),
        postgres_url: cli.postgres_url.clone(),
        color: if cli.no_color { Some(ColorMode::Never) } else { None },
        format: if cli.json { Some(OutputFormat::Json) } else { None },
        reach_mode: mode_override,
        tui_refresh: interval_override,
        ..Default::default()
    };

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let config = match Config::load(file_toml.as_deref(), &env, &overrides) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading config: {e}");
            process::exit(1);
        }
    };

    let reach_mode = config.cluster.reach_mode;
    eprintln!(
        "nico: reach mode: {} ({})",
        reach_mode.as_str(),
        if mode_override.is_some() { "--mode flag" }
        else if env.contains_key("NICO_REACH_MODE") { "NICO_REACH_MODE" }
        else if env.contains_key("KUBERNETES_SERVICE_HOST") { "auto-detected: in-cluster" }
        else { "auto-detected: no KUBERNETES_SERVICE_HOST" }
    );

    let mode = OutputMode {
        color: match config.output.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: cli.ascii,
    };

    let since = humantime::parse_duration(&cli.since).unwrap_or(Duration::from_secs(600));
    let timeout = humantime::parse_duration(&cli.timeout).unwrap_or(Duration::from_secs(5));

    let opts = RunOpts { namespace: config.cluster.namespace.clone(), since, timeout };

    // Build k8s client using context from Config (flag > env > file > default).
    let k8s_result = k8s::KubeRsK8sClient::try_new(config.cluster.context.as_deref()).await;
    let (k8s_client, reach_mgr): (Option<Arc<dyn k8s::K8sClient>>, Option<ReachManager>) =
        match k8s_result {
            Ok(c) => {
                let raw = c.raw_client().clone();
                let mgr = ReachManager::new(reach_mode, raw, config.cluster.namespace.clone());
                (Some(Arc::new(c) as Arc<dyn k8s::K8sClient>), Some(mgr))
            }
            Err(_) => (None, None),
        };

    // Resolve service URLs via reach manager; keep port-forward guards alive until run completes.
    // Guards are collected here and dropped after runner::run() returns.
    let mut _pf_guards: Vec<nico_common::reach::ForwardedEndpoint> = vec![];

    // --- Temporal ---
    let temporal_address = if let Some(ref mgr) = reach_mgr {
        match mgr.temporal_address().await {
            Ok((addr, guard)) => {
                _pf_guards.extend(guard);
                addr
            }
            Err(e) => {
                eprintln!("nico: warn: temporal port-forward failed ({e}); using config address");
                config.temporal.address.clone()
            }
        }
    } else {
        config.temporal.address.clone()
    };

    // --- Postgres ---
    let postgres_url = if let Some(ref mgr) = reach_mgr {
        match mgr.postgres_url(&config.postgres.url).await {
            Ok((url, guard)) => {
                _pf_guards.extend(guard);
                url
            }
            Err(e) => {
                eprintln!("nico: warn: postgres port-forward failed ({e}); using config URL");
                config.postgres.url.clone()
            }
        }
    } else {
        config.postgres.url.clone()
    };

    // --- Loki ---
    let (loki_url, loki_client): (Option<String>, Arc<dyn loki::LokiClient>) = {
        // Explicit env var takes precedence over reach-manager discovery.
        if let Ok(url) = std::env::var("LOKI_URL") {
            let client = Arc::new(loki::RealLokiClient::new(url.clone())) as Arc<dyn loki::LokiClient>;
            (Some(url), client)
        } else if let Some(ref mgr) = reach_mgr {
            match mgr.loki_url().await {
                Ok((url, guard)) => {
                    _pf_guards.extend(guard);
                    let client = Arc::new(loki::RealLokiClient::new(url.clone())) as Arc<dyn loki::LokiClient>;
                    (Some(url), client)
                }
                Err(_) => {
                    let client = Arc::new(Unavailable { reason: "Loki service not found in namespace" }) as Arc<dyn loki::LokiClient>;
                    (None, client)
                }
            }
        } else {
            let client = Arc::new(Unavailable { reason: "LOKI_URL not set" }) as Arc<dyn loki::LokiClient>;
            (None, client)
        }
    };

    // --- HTTP health endpoints ---
    let http_endpoints: Option<Vec<http::ServiceEndpoint>> = {
        // Explicit env var takes precedence.
        if let Some(s) = std::env::var("NICO_HEALTH_ENDPOINTS").ok().filter(|s| !s.is_empty()) {
            let endpoints = s.split(',')
                .map(|entry| entry.trim())
                .filter(|entry| !entry.is_empty())
                .map(|entry| {
                    if let Some((name, url)) = entry.split_once('=') {
                        http::ServiceEndpoint {
                            name: name.trim().to_string(),
                            base_url: url.trim().to_string(),
                        }
                    } else {
                        http::ServiceEndpoint {
                            name: entry.to_string(),
                            base_url: entry.to_string(),
                        }
                    }
                })
                .collect();
            Some(endpoints)
        } else if let Some(ref mgr) = reach_mgr {
            match mgr.http_endpoints().await {
                Ok((discovered, guards)) => {
                    _pf_guards.extend(guards);
                    if discovered.is_empty() {
                        None
                    } else {
                        Some(discovered.into_iter().map(|(name, url)| {
                            http::ServiceEndpoint { name, base_url: url }
                        }).collect())
                    }
                }
                Err(e) => {
                    eprintln!("nico: warn: HTTP service discovery failed: {e}");
                    None
                }
            }
        } else {
            None
        }
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
                        "cluster",
                        "kubeconfig not found; set --context or cluster.context in config",
                    )),
                }
            }
            "logs" => {
                match (k8s_client.as_ref(), loki_url.is_some()) {
                    (Some(k8s), _) => {
                        layers.push(Box::new(layers::logs::LogsLayer::new(
                            loki_client.clone(),
                            k8s.clone(),
                        )));
                    }
                    (None, true) => {
                        layers.push(Box::new(layers::logs::LogsLayer::new(
                            loki_client.clone(),
                            Arc::new(Unavailable { reason: "kubeconfig not found" }),
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
                layers.push(Box::new(layers::workflows::WorkflowsLayer::new(
                    Arc::new(temporal::RealTemporalClient::new(
                        temporal_address.clone(),
                        config.temporal.namespace.clone(),
                    )),
                    config.temporal.stuck_threshold,
                )));
            }
            "health" => {
                match http_endpoints.as_ref() {
                    Some(endpoints) if !endpoints.is_empty() => {
                        layers.push(Box::new(layers::health::HealthLayer::new(
                            Arc::new(http::ReqwestHttpClient::new()),
                            endpoints.clone(),
                        )));
                    }
                    _ => {
                        // In port-forward mode: no services found → skip rather than Unknown.
                        // In unconfigured mode: use UnconfiguredLayer so the user sees the hint.
                        if reach_mgr.is_some() {
                            layers.push(layer::SkippedLayer::new("health"));
                        } else {
                            layers.push(layer::UnconfiguredLayer::new(
                                "health",
                                "set NICO_HEALTH_ENDPOINTS=name=http://host:port to enable",
                            ));
                        }
                    }
                }
            }
            "grpc" => {
                // Prefer explicit NICO_GRPC_ADDRESS; fall back to resolved temporal address.
                let grpc_addr = std::env::var("NICO_GRPC_ADDRESS")
                    .unwrap_or_else(|_| temporal_address.clone());
                layers.push(Box::new(layers::grpc::GrpcLayer::new(
                    Arc::new(grpc::TonicGrpcInspector),
                    grpc_addr,
                )));
            }
            "postgres" => {
                match postgres::SqlxPostgresClient::new(&postgres_url) {
                    Ok(pg) => layers.push(Box::new(layers::postgres::PostgresLayer::new(Arc::new(pg)))),
                    Err(e) => {
                        eprintln!("warning: postgres URL invalid: {e}");
                        eprintln!("  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url");
                        layers.push(layer::UnconfiguredLayer::new("postgres", "invalid postgres URL"));
                    }
                }
            }
            _ => {}
        }
    }

    if cli.tui {
        let layers: std::sync::Arc<Vec<Box<dyn layer::Layer>>> = std::sync::Arc::new(layers);
        let (refresh_tx, mut refresh_rx) = tokio::sync::mpsc::channel::<()>(4);
        let (result_tx, result_rx) = std::sync::mpsc::channel::<tui::TuiUpdate>();

        let namespace = config.cluster.namespace.clone();

        tokio::spawn({
            let layers = layers.clone();
            let result_tx = result_tx.clone();
            async move {
                run_layers_tui(layers.clone(), namespace.clone(), since, timeout, result_tx.clone()).await;
                while refresh_rx.recv().await.is_some() {
                    run_layers_tui(layers.clone(), namespace.clone(), since, timeout, result_tx.clone()).await;
                }
            }
        });

        let tui_cfg = tui::TuiConfig {
            refresh_interval: config.output.tui_refresh,
            layer_names: LAYER_ORDER.to_vec(),
        };
        let ctx = tui::TuiContext { mode };

        let trigger_refresh: Box<dyn Fn() + Send> = Box::new(move || {
            let _ = refresh_tx.try_send(());
        });

        let tui_exit = tokio::task::block_in_place(|| {
            tui::run_tui(tui_cfg, result_rx, ctx, trigger_refresh)
        });

        drop(_pf_guards);
        process::exit(tui_exit);
    }

    let report = runner::run(&layers, &opts).await;

    // Port-forward guards are dropped here, cleanly closing all forwards.
    drop(_pf_guards);

    if matches!(config.output.format, OutputFormat::Json) {
        println!("{}", formatter::format_json(&report, &config.cluster.namespace));
    } else {
        print!("{}", formatter::format_report(&report, &mode, cli.verbose));
    }

    process::exit(exit_code(&report));
}

async fn run_layers_tui(
    layers: std::sync::Arc<Vec<Box<dyn layer::Layer>>>,
    namespace: String,
    since: Duration,
    timeout: Duration,
    tx: std::sync::mpsc::Sender<tui::TuiUpdate>,
) {
    let opts = layer::RunOpts { namespace, since, timeout };
    let report = runner::run(&layers, &opts).await;
    for result in report.layers {
        let name = result.name.to_string();
        let _ = tx.send(tui::TuiUpdate::LayerDone { name, result });
    }
    let _ = tx.send(tui::TuiUpdate::RunComplete);
}
