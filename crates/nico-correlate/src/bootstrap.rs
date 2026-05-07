use std::sync::Arc;
use chrono::Duration;
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
        match crate::id::detect_id_type(&args.id) {
            Some(it) => it,
            None => {
                return Err(BootstrapErr::Fatal {
                    message: format!(
                        "error: could not detect ID type for {:?}\nHint: re-run with --type workflow|host|dpu|request",
                        args.id
                    ),
                    code: 1,
                });
            }
        }
    };

    Ok(CorrelateConfig {
        config,
        id_type,
        since,
        use_all,
        restricted_names,
        attempted_names,
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
    let kube_client_result = kube::Client::try_default().await;

    let reach_mgr: Option<ReachManager> = match kube_client_result.as_ref() {
        Ok(c) => Some(ReachManager::new(
            cfg.config.cluster.reach_mode,
            c.clone(),
            cfg.config.cluster.namespace.clone(),
            cfg.config.cluster.postgres_namespace.clone(),
        )),
        Err(_) => None,
    };

    let mut pf_guards: Vec<nico_common::reach::ForwardedEndpoint> = vec![];
    let pf_budget = cfg.config.bootstrap.timeouts.port_forward;

    let temporal_address = if let Some(ref mgr) = reach_mgr {
        match mgr.temporal_address(pf_budget).await {
            Ok((addr, guard)) => {
                pf_guards.extend(guard);
                addr
            }
            Err(e) => {
                eprintln!("nico: warn: temporal port-forward failed ({e}); using config address");
                cfg.config.temporal.address.clone()
            }
        }
    } else {
        cfg.config.temporal.address.clone()
    };

    let postgres_url = if let Some(ref mgr) = reach_mgr {
        match mgr.postgres_url(&cfg.config.postgres.url, pf_budget).await {
            Ok((url, guard)) => {
                pf_guards.extend(guard);
                url
            }
            Err(e) => {
                eprintln!("nico: warn: postgres port-forward failed ({e}); using config URL");
                cfg.config.postgres.url.clone()
            }
        }
    } else {
        cfg.config.postgres.url.clone()
    };

    if let Err(e) = nico_common::bootstrap::probe_postgres(
        &postgres_url,
        cfg.config.bootstrap.timeouts.postgres_reach,
    ).await {
        eprintln!("nico: warn: postgres reach probe: {e}");
    }

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
