use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Instant;
use chrono::Duration;
use nico_common::boot_probe::{
    next_command_for, BootProbe, ProbeMode, ProbeState, Section, StderrSink, StepDef, StepId,
    StepState,
};
use nico_common::config::{Config, ConfigOverrides, ColorMode, OutputFormat, ReachMode};
use nico_common::reach::ReachManager;
use nico_common::temporal::{GrpcTemporalClient, TemporalClient};
use nico_common::k8s::KubeRsK8sClient;

use crate::cli::CorrelateArgs;
use crate::id::IdType;
use crate::source::{Source, SourceKind, SourceResult, UnavailableSource};
use crate::sources::temporal::TemporalSource;
use crate::sources::postgres::{PostgresSource, SqlxPostgresClient};
use crate::sources::k8s::K8sSource;
use crate::sources::loki::{LokiSource, LokiClient, K8sLogStreamClient, RealLokiClient, RealK8sLogStreamClient};
use crate::sources::redfish::{RedfishSource, RealRedfishClient};

/// Resolved configuration plus helper data shared by the CLI front end and the
/// future `nico ops` dashboard.
pub struct CorrelateConfig {
    pub config: Config,
    pub id_type: IdType,
    pub since: Duration,
    pub use_all: bool,
    pub restricted_names: Vec<&'static str>,
    pub attempted_names: Vec<&'static str>,
    /// Where the resolved reach mode came from — folded into the boot
    /// probe header (ADR-0013) by `prepare_sources`.
    pub reach_source: &'static str,
    /// Whether the user passed `--no-color` / `NO_COLOR` was set when
    /// the config was resolved. Surfaces here so `prepare_sources` can
    /// configure the boot probe without re-reading env/argv.
    pub color: bool,
}

pub enum BootstrapErr {
    Fatal { message: String, code: i32 },
}

/// Resolve config, normalize CLI args, and detect the ID type.
pub fn resolve_config(args: &CorrelateArgs) -> Result<CorrelateConfig, BootstrapErr> {
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
        color: if args.no_color { Some(ColorMode::Never) } else { None },
        format: if args.json { Some(OutputFormat::Json) } else { None },
        reach_mode: mode_override,
        bootstrap_timeouts_spec: args.timeouts.clone(),
        ..Default::default()
    };

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let config = Config::load(file_toml.as_deref(), &env, &overrides, None).map_err(|e| {
        BootstrapErr::Fatal {
            message: format!("error loading config: {e}"),
            code: 1,
        }
    })?;

    let reach_source: &'static str = if mode_override.is_some() {
        "--mode flag"
    } else if env.contains_key("NICO_REACH_MODE") {
        "NICO_REACH_MODE"
    } else if env.contains_key("KUBERNETES_SERVICE_HOST") {
        "auto-detected: in-cluster"
    } else {
        "auto-detected: no KUBERNETES_SERVICE_HOST"
    };
    let _ = reach_source;

    let since = parse_since(&args.since).map_err(|e| BootstrapErr::Fatal {
        message: format!("error: --since {e}"),
        code: 1,
    })?;

    let use_all = args.sources.is_empty() || args.sources.iter().any(|s| s == "all");

    if !use_all {
        for s in &args.sources {
            if SourceKind::from_name(s.as_str()).is_none() {
                let valid = SourceKind::ALL.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ");
                return Err(BootstrapErr::Fatal {
                    message: format!("error: unknown source {:?}; valid sources: {} or \"all\"", s, valid),
                    code: 1,
                });
            }
        }
    }

    let restricted_names: Vec<&'static str> = if use_all {
        vec![]
    } else {
        SourceKind::ALL
            .iter()
            .map(|k| k.name())
            .filter(|name| !args.sources.iter().any(|s| s == name))
            .collect()
    };

    let attempted_names: Vec<&'static str> = SourceKind::ALL
        .iter()
        .map(|k| k.name())
        .filter(|name| !restricted_names.contains(name))
        .collect();

    let id_str = match args.id.as_deref() {
        Some(s) => s,
        None => {
            return Err(BootstrapErr::Fatal {
                message: "error: missing entity ID; usage: nico correlate <id> [or a subcommand]".into(),
                code: 2,
            });
        }
    };

    let id_type = if let Some(ref t) = args.r#type {
        match IdType::from_cli_name(t) {
            Some(it) => it,
            None => {
                return Err(BootstrapErr::Fatal {
                    message: format!("error: unknown --type {t:?}; use workflow|host|dpu|request"),
                    code: 1,
                });
            }
        }
    } else {
        match crate::id::detect_id_type(id_str) {
            Some(it) => it,
            None => {
                return Err(BootstrapErr::Fatal {
                    message: format!(
                        "error: could not detect ID type for {id_str:?}\nHint: re-run with --type workflow|host|dpu|request"
                    ),
                    code: 1,
                });
            }
        }
    };

    let color = match config.output.color {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
    };

    Ok(CorrelateConfig {
        config,
        id_type,
        since,
        use_all,
        restricted_names,
        attempted_names,
        reach_source,
        color,
    })
}

pub fn parse_since(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<i64>() {
        return Ok(Duration::seconds(secs));
    }
    let mut total = Duration::zero();
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            let n: i64 = num.parse().map_err(|_| format!("invalid duration: {s}"))?;
            num.clear();
            total += match ch {
                'h' => Duration::hours(n),
                'm' => Duration::minutes(n),
                's' => Duration::seconds(n),
                _ => return Err(format!("unknown unit '{ch}' in duration '{s}'; use h, m, or s")),
            };
        }
    }
    if !num.is_empty() {
        return Err(format!("trailing number without unit in duration '{s}'"));
    }
    Ok(total)
}

/// Fully resolved sources ready to be queried. The held port-forward guards
/// keep tunnels alive for the lifetime of this struct.
pub struct PreparedSources {
    pub named_sources: Vec<(&'static str, Box<dyn Source>)>,
    pub _pf_guards: Vec<nico_common::reach::ForwardedEndpoint>,
}

/// Build the source set for a given config, including port-forward setup,
/// Loki/Postgres/Redfish discovery, and `--sources` filtering.
pub async fn prepare_sources(
    args: &CorrelateArgs,
    cfg: &CorrelateConfig,
) -> PreparedSources {
    let probe_mode = if cfg.config.output.format == OutputFormat::Json {
        ProbeMode::Json
    } else if !std::io::stderr().is_terminal() {
        ProbeMode::NonTty
    } else {
        ProbeMode::Tty {
            color: cfg.color,
            ascii: false,
        }
    };
    let probe_state = ProbeState::new(
        correlate_steps(&cfg.config),
        cfg.config.cluster.reach_mode.as_str(),
        cfg.reach_source,
    )
    .with_deployment_type(
        cfg.config
            .cluster
            .deployment_type
            .map(|d| d.label().to_string()),
        cfg.config.cluster.deployment_type_source.label(),
    )
    .with_warnings(cfg.config.override_conflict_warnings());
    let mut probe = BootProbe::new(probe_state, probe_mode, Box::new(StderrSink));
    probe.start_ticking();
    let tracker = probe.tracker();

    let ns = cfg.config.cluster.namespace.clone();
    let timeouts = cfg.config.bootstrap.timeouts;

    // ----- Connecting -----
    tracker.started(StepId::LoadKubeconfig).await;
    let t = Instant::now();
    let kube_with_budget = nico_common::bootstrap::run_with_budget(
        timeouts.kube_client,
        async {
            let c = kube::Client::try_default().await?;
            Ok(c)
        },
    )
    .await;

    let kube_client_result = match kube_with_budget {
        Ok(c) => {
            tracker
                .finished(
                    StepId::LoadKubeconfig,
                    StepState::Passed { elapsed: t.elapsed() },
                )
                .await;
            Ok(c)
        }
        Err(e) => {
            let elapsed = t.elapsed();
            let timed_out = e.is_timed_out();
            tracker
                .finished(
                    StepId::LoadKubeconfig,
                    StepState::Failed {
                        elapsed,
                        message: e.to_string(),
                        timed_out,
                        next_command: next_command_for(StepId::LoadKubeconfig, &ns),
                    },
                )
                .await;
            tracker
                .skip_remaining(&[
                    StepId::ReachApiServer,
                    StepId::PortForwardWorkflows,
                    StepId::PortForwardPostgres,
                    StepId::ReachPostgres,
                ])
                .await;
            Err(anyhow::anyhow!("{}", e))
        }
    };

    let reach_mgr: Option<ReachManager> = if let Ok(ref c) = kube_client_result {
        // Reach API server gate
        tracker.started(StepId::ReachApiServer).await;
        let t = Instant::now();
        let raw = c.clone();
        let r = nico_common::bootstrap::run_with_budget(timeouts.reach_api, async move {
            raw.apiserver_version()
                .await
                .map_err(|e| anyhow::anyhow!("cannot reach API server: {e}"))
                .map(|_| ())
        })
        .await;
        match r {
            Ok(()) => {
                tracker
                    .finished(
                        StepId::ReachApiServer,
                        StepState::Passed { elapsed: t.elapsed() },
                    )
                    .await;
                Some(ReachManager::new(
                    cfg.config.cluster.reach_mode,
                    c.clone(),
                    cfg.config.cluster.namespace.clone(),
                    cfg.config.cluster.postgres_namespace.clone(),
                ))
            }
            Err(e) => {
                let elapsed = t.elapsed();
                let timed_out = e.is_timed_out();
                tracker
                    .finished(
                        StepId::ReachApiServer,
                        StepState::Failed {
                            elapsed,
                            message: e.to_string(),
                            timed_out,
                            next_command: next_command_for(StepId::ReachApiServer, &ns),
                        },
                    )
                    .await;
                tracker
                    .skip_remaining(&[
                        StepId::PortForwardWorkflows,
                        StepId::PortForwardPostgres,
                        StepId::ReachPostgres,
                    ])
                    .await;
                None
            }
        }
    } else {
        None
    };

    let mut pf_guards: Vec<nico_common::reach::ForwardedEndpoint> = vec![];
    let pf_budget = timeouts.port_forward;

    // ----- Serving (parallel) -----
    let (temporal_address, postgres_url) = if let Some(ref mgr) = reach_mgr {
        let temporal_fut = async {
            tracker.started(StepId::PortForwardWorkflows).await;
            let t = Instant::now();
            match mgr.temporal_address(pf_budget).await {
                Ok((addr, guard)) => {
                    tracker
                        .finished(
                            StepId::PortForwardWorkflows,
                            StepState::Passed { elapsed: t.elapsed() },
                        )
                        .await;
                    (addr, guard)
                }
                Err(e) => {
                    let elapsed = t.elapsed();
                    let timed_out = e.is_timed_out();
                    tracker
                        .finished(
                            StepId::PortForwardWorkflows,
                            StepState::Failed {
                                elapsed,
                                message: e.to_string(),
                                timed_out,
                                next_command: next_command_for(StepId::PortForwardWorkflows, &ns),
                            },
                        )
                        .await;
                    (cfg.config.temporal.address.clone(), None)
                }
            }
        };

        let postgres_fut = async {
            tracker.started(StepId::PortForwardPostgres).await;
            let t = Instant::now();
            let (url, guard, ok) =
                match mgr.postgres_url(&cfg.config.postgres.url, pf_budget).await {
                    Ok((url, guard)) => {
                        tracker
                            .finished(
                                StepId::PortForwardPostgres,
                                StepState::Passed { elapsed: t.elapsed() },
                            )
                            .await;
                        (url, guard, true)
                    }
                    Err(e) => {
                        let elapsed = t.elapsed();
                        let timed_out = e.is_timed_out();
                        tracker
                            .finished(
                                StepId::PortForwardPostgres,
                                StepState::Failed {
                                    elapsed,
                                    message: e.to_string(),
                                    timed_out,
                                    next_command: next_command_for(
                                        StepId::PortForwardPostgres,
                                        &ns,
                                    ),
                                },
                            )
                            .await;
                        (cfg.config.postgres.url.clone(), None, false)
                    }
                };

            if !ok {
                tracker
                    .finished(StepId::ReachPostgres, StepState::Skipped)
                    .await;
                return (url, guard);
            }
            tracker.started(StepId::ReachPostgres).await;
            let t = Instant::now();
            match nico_common::bootstrap::probe_postgres(&url, timeouts.postgres_reach).await {
                Ok(()) => {
                    tracker
                        .finished(
                            StepId::ReachPostgres,
                            StepState::Passed { elapsed: t.elapsed() },
                        )
                        .await;
                }
                Err(e) => {
                    let elapsed = t.elapsed();
                    let timed_out = e.is_timed_out();
                    tracker
                        .finished(
                            StepId::ReachPostgres,
                            StepState::Failed {
                                elapsed,
                                message: e.to_string(),
                                timed_out,
                                next_command: next_command_for(StepId::ReachPostgres, &ns),
                            },
                        )
                        .await;
                }
            }
            (url, guard)
        };

        let ((temporal_address, t_guard), (postgres_url, pg_guard)) =
            tokio::join!(temporal_fut, postgres_fut);
        if let Some(g) = t_guard {
            pf_guards.push(g);
        }
        if let Some(g) = pg_guard {
            pf_guards.push(g);
        }
        (temporal_address, postgres_url)
    } else {
        (
            cfg.config.temporal.address.clone(),
            cfg.config.postgres.url.clone(),
        )
    };

    let _ = if tracker.any_failed().await {
        probe.finish_failure(&ns).await
    } else {
        probe.finish_success(&ns).await
    };

    let pg_source: Box<dyn Source> = match SqlxPostgresClient::connect(&postgres_url).await {
        Ok(c) => Box::new(PostgresSource::new(Box::new(c))),
        Err(e) => Box::new(UnavailableSource::new("postgres", format!("connect failed: {e}"))),
    };

    let k8s_source: Box<dyn Source> = match KubeRsK8sClient::try_new(
        cfg.config.cluster.context.as_deref(),
        cfg.config.bootstrap.timeouts.kube_client,
    ).await {
        Ok(c) => Box::new(K8sSource::new(Arc::new(c))),
        Err(e) => Box::new(UnavailableSource::new("k8s", format!("kubeconfig unavailable: {e}"))),
    };

    let temporal_client: Arc<dyn TemporalClient> =
        Arc::new(GrpcTemporalClient::new(temporal_address));

    let loki_result: Result<Box<dyn LokiClient>, &'static str> = match std::env::var("LOKI_URL") {
        Ok(url) => Ok(Box::new(RealLokiClient::new(url))),
        Err(_) => {
            if let Some(ref mgr) = reach_mgr {
                match mgr.loki_url(pf_budget).await {
                    Ok((url, guard)) => {
                        pf_guards.extend(guard);
                        Ok(Box::new(RealLokiClient::new(url)))
                    }
                    Err(_) => Err("Loki service not found in namespace"),
                }
            } else {
                Err("LOKI_URL not set")
            }
        }
    };

    let k8s_log_opt: Option<Box<dyn K8sLogStreamClient>> = match kube_client_result {
        Ok(c) => Some(Box::new(RealK8sLogStreamClient::new(c))),
        Err(_) => None,
    };

    let loki_source: Box<dyn Source> = match loki_result {
        Ok(loki) => Box::new(LokiSource::new(loki, k8s_log_opt, args.pod.clone(), cfg.since)),
        Err(reason) => Box::new(UnavailableSource::new("loki", reason)),
    };

    let redfish_source: Box<dyn Source> = match std::env::var("REDFISH_BMC_BASE_URL") {
        Ok(bmc_url) => {
            let pg_pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(5))
                .connect(&cfg.config.postgres.url)
                .await
                .ok();
            Box::new(RedfishSource::new(Box::new(RealRedfishClient::new(bmc_url, pg_pool))))
        }
        Err(_) => Box::new(UnavailableSource::new("redfish", "REDFISH_BMC_BASE_URL not set")),
    };

    let all_sources: Vec<(&'static str, Box<dyn Source>)> = vec![
        ("temporal", Box::new(TemporalSource::new(temporal_client, cfg.config.temporal.namespace.clone()))),
        ("postgres", pg_source),
        ("k8s", k8s_source),
        ("loki", loki_source),
        ("redfish", redfish_source),
    ];

    let named_sources: Vec<(&'static str, Box<dyn Source>)> = if cfg.use_all {
        all_sources
    } else {
        all_sources
            .into_iter()
            .filter(|(name, _)| args.sources.iter().any(|s| s == name))
            .collect()
    };

    PreparedSources {
        named_sources,
        _pf_guards: pf_guards,
    }
}

/// Sequentially collect every named source into a parallel `Vec<SourceResult>`.
/// Order matches `sources` so callers can zip names back in.
pub async fn collect_all(
    sources: &[(&'static str, Box<dyn Source>)],
    id: &str,
    id_type: &IdType,
) -> Vec<SourceResult> {
    let mut all_results: Vec<SourceResult> = Vec::with_capacity(sources.len());
    for (_, source) in sources {
        all_results.push(source.collect(id, id_type).await);
    }
    all_results
}

/// Step list for the correlate boot probe — same shape as doctor's
/// `standard_steps` but without the validating section (correlate has
/// no preflight RBAC checks) and without `port-forward: grpc` (no
/// gRPC service is wired up here).
fn correlate_steps(config: &Config) -> Vec<StepDef> {
    let t = config.bootstrap.timeouts;
    vec![
        StepDef {
            id: StepId::LoadKubeconfig,
            label: "load kubeconfig".into(),
            section: Section::Connecting,
            budget: t.kube_client,
        },
        StepDef {
            id: StepId::ReachApiServer,
            label: "reach API server".into(),
            section: Section::Connecting,
            budget: t.reach_api,
        },
        StepDef {
            id: StepId::PortForwardWorkflows,
            label: "port-forward: workflows".into(),
            section: Section::Serving,
            budget: t.port_forward,
        },
        StepDef {
            id: StepId::PortForwardPostgres,
            label: "port-forward: postgres".into(),
            section: Section::Serving,
            budget: t.port_forward,
        },
        StepDef {
            id: StepId::ReachPostgres,
            label: "reach postgres".into(),
            section: Section::Serving,
            budget: t.postgres_reach,
        },
    ]
}
