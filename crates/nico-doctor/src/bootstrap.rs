use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use nico_common::config::{Config, ConfigOverrides, ColorMode, OutputFormat, ReachMode};
use nico_common::output::OutputMode;
use nico_common::reach::ReachManager;

use crate::cli::DoctorArgs;
use crate::layer::{self, Layer, RunOpts};
use crate::layers;
use crate::log_source;
use crate::loki;
use crate::http;
use crate::grpc;
use crate::postgres;
use crate::preflight;

const LAYER_ORDER: &[&str] = &["cluster", "logs", "workflows", "health", "grpc", "postgres"];

struct Unavailable {
    reason: &'static str,
}

#[async_trait]
impl nico_common::k8s::K8sClient for Unavailable {
    async fn list_pods(&self, _scope: nico_common::k8s::PodScope<'_>) -> anyhow::Result<Vec<nico_common::k8s::RawPod>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
    async fn list_events(&self, _ns: &str, _field_selector: Option<&str>) -> anyhow::Result<Vec<nico_common::k8s::RawEvent>> {
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

/// Outcome of preparing a doctor run from CLI args + environment.
pub struct Bootstrapped {
    pub layers: Vec<Box<dyn Layer>>,
    pub opts: RunOpts,
    pub output_mode: OutputMode,
    pub output_format: OutputFormat,
    pub namespace: String,
    pub verbose: bool,
    pub spotlight: bool,
    /// Resolved auto-refresh interval (`[output] tui_refresh` →
    /// `NICO_TUI_REFRESH` env → default 30s). Consumed by `nico-ops` for
    /// the dashboard's auto-refresh cadence; ignored by `nico-doctor`'s
    /// one-shot path.
    pub tui_refresh: Duration,
    /// Resolved Temporal frontend address (after port-forward fallback).
    /// Consumed by `nico-ops` Layout B Activity quadrant
    /// (`recent_namespace_events`); irrelevant to one-shot doctor runs.
    pub temporal_address: String,
    /// Temporal namespace from config — paired with `temporal_address` so
    /// the Activity quadrant can list workflow executions.
    pub temporal_namespace: String,
    /// K8s client built from kubeconfig — reused by Layout B Activity.
    /// `None` when no kubeconfig is reachable; the Activity quadrant just
    /// reports an empty feed in that case.
    pub k8s_client: Option<Arc<dyn nico_common::k8s::K8sClient>>,
    /// Kept alive until the caller is done running layers; dropping closes port-forwards.
    pub _pf_guards: Vec<nico_common::reach::ForwardedEndpoint>,
}

pub enum BootstrapErr {
    /// Pre-flight check rejected the run; callers should exit 3.
    /// `human_message` is what we'd print on stderr; `json_payload` is what `--json` mode prints.
    Preflight {
        human_message: String,
        json_payload: String,
        format: OutputFormat,
    },
    /// Fatal config/parse error; `code` is the exit code to use (1 or 3).
    Fatal { message: String, code: i32 },
}

/// Inputs threaded into [`prepare_layers`] — the discrete dependencies a layer set is built from.
pub struct LayerInputs {
    pub k8s_client: Option<Arc<dyn nico_common::k8s::K8sClient>>,
    pub loki_url: Option<String>,
    pub loki_client: Arc<dyn loki::LokiClient>,
    pub temporal_address: String,
    pub temporal_namespace: String,
    pub stuck_threshold: Duration,
    pub http_endpoints: Option<Vec<http::ServiceEndpoint>>,
    pub postgres_url: String,
    pub grpc_address: Option<String>,
    pub reach_mgr_present: bool,
    pub skip: Vec<String>,
}

/// Build the ordered layer set from prepared inputs.
pub fn prepare_layers(inputs: &LayerInputs) -> Vec<Box<dyn Layer>> {
    let mut out: Vec<Box<dyn Layer>> = vec![];

    for &name in LAYER_ORDER {
        if inputs.skip.iter().any(|s| s.as_str() == name) {
            out.push(layer::SkippedLayer::new(name));
            continue;
        }
        match name {
            "cluster" => match inputs.k8s_client.as_ref() {
                Some(k8s) => out.push(Box::new(layers::cluster::ClusterLayer::new(k8s.clone()))),
                None => out.push(layer::UnconfiguredLayer::new(
                    "cluster",
                    "kubeconfig not found; set --context or cluster.context in config",
                )),
            },
            "logs" => match (inputs.k8s_client.as_ref(), inputs.loki_url.is_some()) {
                (Some(k8s), _) => {
                    let chain = log_source::best_effort_chain(vec![
                        Arc::new(loki::LokiLogSource::new(inputs.loki_client.clone())),
                        Arc::new(log_source::K8sLogSource::new(k8s.clone())),
                    ]);
                    out.push(Box::new(layers::logs::LogsLayer::new(chain)));
                }
                (None, true) => {
                    let chain = log_source::best_effort_chain(vec![
                        Arc::new(loki::LokiLogSource::new(inputs.loki_client.clone())),
                        Arc::new(log_source::K8sLogSource::new(
                            Arc::new(Unavailable { reason: "kubeconfig not found" }),
                        )),
                    ]);
                    out.push(Box::new(layers::logs::LogsLayer::new(chain)));
                }
                (None, false) => {
                    out.push(layer::UnconfiguredLayer::new(
                        "logs", "set LOKI_URL or ensure kubeconfig is accessible",
                    ));
                }
            },
            "workflows" => {
                out.push(Box::new(layers::workflows::WorkflowsLayer::new(
                    Arc::new(nico_common::temporal::GrpcTemporalClient::new(
                        inputs.temporal_address.clone(),
                    )),
                    inputs.temporal_namespace.clone(),
                    inputs.stuck_threshold,
                )));
            }
            "health" => match inputs.http_endpoints.as_ref() {
                Some(endpoints) if !endpoints.is_empty() => {
                    out.push(Box::new(layers::health::HealthLayer::new(
                        Arc::new(http::ReqwestHttpClient::new()),
                        endpoints.clone(),
                    )));
                }
                _ => {
                    if inputs.reach_mgr_present {
                        out.push(layer::SkippedLayer::new("health"));
                    } else {
                        out.push(layer::UnconfiguredLayer::new(
                            "health",
                            "set NICO_HEALTH_ENDPOINTS=name=http://host:port to enable",
                        ));
                    }
                }
            },
            "grpc" => match inputs.grpc_address.clone() {
                Some(addr) => {
                    out.push(Box::new(layers::grpc::GrpcLayer::new(
                        Arc::new(grpc::TonicGrpcInspector),
                        addr,
                    )));
                }
                None => {
                    out.push(layer::UnconfiguredLayer::new(
                        "grpc",
                        "set NICO_GRPC_ADDRESS or cluster.grpc_address in config to enable",
                    ));
                }
            },
            "postgres" => match postgres::SqlxPostgresClient::new(&inputs.postgres_url) {
                Ok(pg) => out.push(Box::new(layers::postgres::PostgresLayer::new(Arc::new(pg)))),
                Err(e) => {
                    eprintln!("warning: postgres URL invalid: {e}");
                    eprintln!("  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url");
                    out.push(layer::UnconfiguredLayer::new("postgres", "invalid postgres URL"));
                }
            },
            _ => {}
        }
    }

    out
}

/// Build a runnable doctor configuration from CLI args. Reads the user's config
/// file, environment, and optional kubeconfig; runs pre-flight; resolves
/// service URLs (Loki / Postgres / Temporal / HTTP) via the reach manager.
pub async fn bootstrap(args: &DoctorArgs) -> Result<Bootstrapped, BootstrapErr> {
    let config_path = args
        .config
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config/nico-tools/config.toml")
        });
    let file_toml = std::fs::read_to_string(&config_path).ok();

    let mode_override = match args.mode.as_deref() {
        Some("port-forward") => Some(ReachMode::PortForward),
        Some("in-cluster") => Some(ReachMode::InCluster),
        Some(other) => {
            return Err(BootstrapErr::Fatal {
                message: format!("error: unknown --mode {other:?}; use port-forward or in-cluster"),
                code: 1,
            });
        }
        None => None,
    };

    let overrides = ConfigOverrides {
        namespace: args.namespace.clone(),
        context: args.context.clone(),
        postgres_url: args.postgres_url.clone(),
        color: if args.no_color { Some(ColorMode::Never) } else { None },
        format: if args.json { Some(OutputFormat::Json) } else { None },
        reach_mode: mode_override,
        ..Default::default()
    };

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let config = Config::load(file_toml.as_deref(), &env, &overrides).map_err(|e| {
        BootstrapErr::Fatal {
            message: format!("error loading config: {e}"),
            code: 1,
        }
    })?;

    let reach_mode = config.cluster.reach_mode;
    eprintln!(
        "nico: reach mode: {} ({})",
        reach_mode.as_str(),
        if mode_override.is_some() { "--mode flag" }
        else if env.contains_key("NICO_REACH_MODE") { "NICO_REACH_MODE" }
        else if env.contains_key("KUBERNETES_SERVICE_HOST") { "auto-detected: in-cluster" }
        else { "auto-detected: no KUBERNETES_SERVICE_HOST" }
    );

    let output_mode = OutputMode {
        color: match config.output.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let since = humantime::parse_duration(&args.since).unwrap_or(Duration::from_secs(600));
    let timeout = humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5));
    let opts = RunOpts {
        namespace: config.cluster.namespace.clone(),
        since,
        timeout,
    };

    let k8s_result = nico_common::k8s::KubeRsK8sClient::try_new(config.cluster.context.as_deref()).await;
    let (k8s_client, raw_k8s, reach_mgr): (
        Option<Arc<dyn nico_common::k8s::K8sClient>>,
        Option<kube::Client>,
        Option<ReachManager>,
    ) = match k8s_result {
        Ok(c) => {
            let raw = c.raw_client().clone();
            let mgr = ReachManager::new(
                reach_mode,
                raw.clone(),
                config.cluster.namespace.clone(),
                config.cluster.postgres_namespace.clone(),
            );
            (
                Some(Arc::new(c) as Arc<dyn nico_common::k8s::K8sClient>),
                Some(raw),
                Some(mgr),
            )
        }
        Err(_) => (None, None, None),
    };

    if let Some(raw) = raw_k8s.as_ref() {
        let pf = preflight::KubePreflightClient::new(raw.clone());
        if let preflight::Outcome::Failed(failure) = preflight::run(&pf, &config.cluster.namespace).await {
            let json_payload = preflight::format_failure_json(&failure, &config.cluster.namespace);
            let human_message = format!(
                "error: pre-flight check failed [{}]: {}\n  → {}",
                failure.step.as_str(),
                failure.message,
                failure.next_command
            );
            return Err(BootstrapErr::Preflight {
                human_message,
                json_payload,
                format: config.output.format,
            });
        }
    }

    let mut pf_guards: Vec<nico_common::reach::ForwardedEndpoint> = vec![];

    let temporal_address = if let Some(ref mgr) = reach_mgr {
        match mgr.temporal_address().await {
            Ok((addr, guard)) => {
                pf_guards.extend(guard);
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

    let postgres_url = if let Some(ref mgr) = reach_mgr {
        match mgr.postgres_url(&config.postgres.url).await {
            Ok((url, guard)) => {
                pf_guards.extend(guard);
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

    let (loki_url, loki_client): (Option<String>, Arc<dyn loki::LokiClient>) = {
        if let Ok(url) = std::env::var("LOKI_URL") {
            let client = Arc::new(loki::RealLokiClient::new(url.clone())) as Arc<dyn loki::LokiClient>;
            (Some(url), client)
        } else if let Some(ref mgr) = reach_mgr {
            match mgr.loki_url().await {
                Ok((url, guard)) => {
                    pf_guards.extend(guard);
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

    let http_endpoints: Option<Vec<http::ServiceEndpoint>> = {
        if let Some(s) = std::env::var("NICO_HEALTH_ENDPOINTS").ok().filter(|s| !s.is_empty()) {
            let endpoints = s
                .split(',')
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
                    pf_guards.extend(guards);
                    if discovered.is_empty() {
                        None
                    } else {
                        Some(
                            discovered
                                .into_iter()
                                .map(|(name, url)| http::ServiceEndpoint { name, base_url: url })
                                .collect(),
                        )
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

    let bootstrap_k8s = k8s_client.clone();

    let inputs = LayerInputs {
        k8s_client,
        loki_url,
        loki_client,
        temporal_address: temporal_address.clone(),
        temporal_namespace: config.temporal.namespace.clone(),
        stuck_threshold: config.temporal.stuck_threshold,
        http_endpoints,
        postgres_url,
        grpc_address: config.cluster.grpc_address.clone(),
        reach_mgr_present: reach_mgr.is_some(),
        skip: args.skip.clone(),
    };

    let layers = prepare_layers(&inputs);

    Ok(Bootstrapped {
        layers,
        opts,
        output_mode,
        output_format: config.output.format,
        namespace: config.cluster.namespace.clone(),
        verbose: args.verbose,
        spotlight: args.spotlight,
        tui_refresh: config.output.tui_refresh,
        temporal_address,
        temporal_namespace: config.temporal.namespace.clone(),
        k8s_client: bootstrap_k8s,
        _pf_guards: pf_guards,
    })
}
