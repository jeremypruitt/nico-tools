mod correlate;
mod diagnosis;
mod event;
mod id;
mod source;
mod sources;
mod timeline;

use clap::Parser;
use chrono::Duration;
use serde::Serialize;
use nico_common::config::{Config, ConfigOverrides, ColorMode, OutputFormat, ReachMode};
use nico_common::output::{OutputMode, Status};
use nico_common::reach::ReachManager;
use crate::id::{IdType, detect_id_type};
use crate::source::{Source, SourceResult, StateEntry};
use crate::sources::temporal::{TemporalSource, TemporalClient};
use crate::sources::temporal_grpc::GrpcTemporalClient;
use crate::sources::postgres::{PostgresSource, PostgresClient, PgEntityData, SqlxPostgresClient};
use crate::sources::k8s::{K8sSource, K8sClient, K8sPodData, KubeRsK8sClient};
use crate::sources::loki::{LokiSource, LokiClient, LokiLogLine, K8sLogStreamClient, K8sLogLine, RealLokiClient, RealK8sLogStreamClient};
use crate::sources::redfish::{RedfishSource, RedfishClient, RedfishData, RealRedfishClient};
use crate::timeline::filter_timeline;
use crate::correlate::exit_code;
use crate::diagnosis::diagnose;
use crate::event::Event;
use anyhow::Result;
use async_trait::async_trait;

#[derive(Parser)]
#[command(name = "nico-correlate", about = "Correlate all events for a given entity ID")]
struct Cli {
    /// Entity ID to correlate (workflow, host, DPU, or request ID)
    id: String,

    /// Override auto-detected ID type (workflow|host|dpu|request)
    #[arg(short = 't', long)]
    r#type: Option<String>,

    /// Restrict to specific sources (comma-separated: temporal,postgres,k8s,loki,redfish)
    #[arg(short = 's', long, value_delimiter = ',')]
    sources: Vec<String>,

    /// Limit log search to pods matching this pattern
    #[arg(long)]
    pod: Option<String>,

    /// Look-back window for log sources (e.g. 1h, 30m, 2h30m; default: 1h)
    #[arg(long, default_value = "1h")]
    since: String,

    /// Output JSON
    #[arg(short = 'j', long)]
    json: bool,

    /// ASCII-only output (no Unicode icons)
    #[arg(long)]
    ascii: bool,

    /// Disable color output
    #[arg(long)]
    no_color: bool,

    /// Config file path (default: ~/.config/nico-tools/config.toml)
    #[arg(long, value_name = "PATH")]
    config: Option<String>,

    /// Reach mode: port-forward or in-cluster (default: auto-detect from KUBERNETES_SERVICE_HOST)
    #[arg(long, value_name = "MODE")]
    mode: Option<String>,
}

fn parse_since(s: &str) -> Result<Duration, String> {
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

struct InactivePostgresClient {
    reason: String,
}

#[async_trait]
impl PostgresClient for InactivePostgresClient {
    async fn query_entity(&self, _id: &str, _id_type: &IdType) -> Result<PgEntityData> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactiveK8sClient {
    reason: String,
}

#[async_trait]
impl K8sClient for InactiveK8sClient {
    async fn find_pods_with_events(&self, _id: &str, _id_type: &IdType) -> Result<Vec<K8sPodData>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactiveLokiClient {
    reason: String,
}

#[async_trait]
impl LokiClient for InactiveLokiClient {
    async fn query_range(
        &self,
        _id: &str,
        _id_type: &IdType,
        _since: Duration,
        _pod_pattern: Option<&str>,
    ) -> Result<Vec<LokiLogLine>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactiveK8sLogStreamClient {
    reason: String,
}

#[async_trait]
impl K8sLogStreamClient for InactiveK8sLogStreamClient {
    async fn stream_logs(
        &self,
        _id: &str,
        _id_type: &IdType,
        _since: Duration,
        _pod_pattern: Option<&str>,
    ) -> Result<Vec<K8sLogLine>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

struct InactiveRedfishClient {
    reason: String,
}

#[async_trait]
impl RedfishClient for InactiveRedfishClient {
    async fn query(&self, _id: &str, _id_type: &IdType) -> Result<RedfishData> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

#[derive(Serialize)]
struct JsonDiagnosis {
    pattern: String,
    activity: String,
    error_signature: String,
    next_commands: Vec<String>,
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    version: u32,
    id: &'a str,
    id_type: &'a str,
    events: Vec<JsonEvent<'a>>,
    sources_restricted: Vec<&'a str>,
    sources_unavailable: Vec<&'a str>,
    state: Vec<JsonStateEntry<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnosis: Option<JsonDiagnosis>,
}

#[derive(Serialize)]
struct JsonEvent<'a> {
    ts: String,
    source: &'a str,
    kind: &'a str,
    severity: &'a str,
}

#[derive(Serialize)]
struct JsonStateEntry<'a> {
    source: &'a str,
    key: &'a str,
    value: &'a str,
}

const KNOWN_SOURCES: &[&str] = &["temporal", "postgres", "k8s", "loki", "redfish"];

fn id_type_str(t: &IdType) -> &'static str {
    match t {
        IdType::Workflow => "workflow",
        IdType::Host => "host",
        IdType::Dpu => "dpu",
        IdType::Request => "request",
    }
}

fn parse_id_type(s: &str) -> Option<IdType> {
    match s {
        "workflow" => Some(IdType::Workflow),
        "host" => Some(IdType::Host),
        "dpu" => Some(IdType::Dpu),
        "request" => Some(IdType::Request),
        _ => None,
    }
}

fn severity_to_status(s: &crate::event::Severity) -> Status {
    match s {
        crate::event::Severity::Info => Status::Ok,
        crate::event::Severity::Warning => Status::Warn,
        crate::event::Severity::Error => Status::Fail,
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

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
            std::process::exit(1);
        }
        None => None,
    };

    let overrides = ConfigOverrides {
        color: if cli.no_color { Some(ColorMode::Never) } else { None },
        format: if cli.json { Some(OutputFormat::Json) } else { None },
        reach_mode: mode_override,
        ..Default::default()
    };

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let config = match Config::load(file_toml.as_deref(), &env, &overrides) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error loading config: {e}");
            std::process::exit(1);
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

    let since = match parse_since(&cli.since) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: --since {e}");
            std::process::exit(1);
        }
    };

    let use_all = cli.sources.is_empty() || cli.sources.iter().any(|s| s == "all");

    if !use_all {
        for s in &cli.sources {
            if !KNOWN_SOURCES.contains(&s.as_str()) {
                eprintln!(
                    "error: unknown source {:?}; valid sources: {} or \"all\"",
                    s,
                    KNOWN_SOURCES.join(", ")
                );
                std::process::exit(1);
            }
        }
    }

    let restricted_names: Vec<&str> = if use_all {
        vec![]
    } else {
        KNOWN_SOURCES
            .iter()
            .copied()
            .filter(|&name| !cli.sources.iter().any(|s| s == name))
            .collect()
    };

    let attempted_names: Vec<&str> = KNOWN_SOURCES
        .iter()
        .copied()
        .filter(|&name| !restricted_names.contains(&name))
        .collect();

    let id_type = if let Some(ref t) = cli.r#type {
        match parse_id_type(t) {
            Some(it) => Some(it),
            None => {
                eprintln!("error: unknown --type {t:?}; use workflow|host|dpu|request");
                std::process::exit(1);
            }
        }
    } else {
        detect_id_type(&cli.id)
    };

    if id_type.is_none() {
        eprintln!(
            "error: could not detect ID type for {:?}\nHint: re-run with --type workflow|host|dpu|request",
            cli.id
        );
        std::process::exit(1);
    }
    let id_type = id_type.unwrap();

    // Build raw kube client once; share across k8s source, log stream, and reach manager.
    let kube_client_result = kube::Client::try_default().await;

    let reach_mgr: Option<ReachManager> = match kube_client_result.as_ref() {
        Ok(c) => Some(ReachManager::new(reach_mode, c.clone(), config.cluster.namespace.clone())),
        Err(_) => None,
    };

    // Port-forward guards — kept alive until after all sources have been collected.
    let mut _pf_guards: Vec<nico_common::reach::ForwardedEndpoint> = vec![];

    // --- Temporal address ---
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

    // --- Postgres URL ---
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

    let pg_client: Box<dyn PostgresClient> = match SqlxPostgresClient::connect(&postgres_url).await {
        Ok(c) => Box::new(c),
        Err(e) => Box::new(InactivePostgresClient { reason: format!("connect failed: {e}") }),
    };

    let k8s_client: Box<dyn K8sClient> = match KubeRsK8sClient::try_default().await {
        Ok(c) => Box::new(c),
        Err(e) => Box::new(InactiveK8sClient { reason: format!("kubeconfig unavailable: {e}") }),
    };

    let temporal_client: Box<dyn TemporalClient> = Box::new(GrpcTemporalClient::new(
        temporal_address,
        config.temporal.namespace.clone(),
    ));

    // Loki: explicit env var wins, then reach manager discovery.
    let loki_client: Box<dyn LokiClient> = match std::env::var("LOKI_URL") {
        Ok(url) => Box::new(RealLokiClient::new(url)),
        Err(_) => {
            if let Some(ref mgr) = reach_mgr {
                match mgr.loki_url().await {
                    Ok((url, guard)) => {
                        _pf_guards.extend(guard);
                        Box::new(RealLokiClient::new(url))
                    }
                    Err(_) => Box::new(InactiveLokiClient {
                        reason: "Loki service not found in namespace".into(),
                    }),
                }
            } else {
                Box::new(InactiveLokiClient { reason: "LOKI_URL not set".into() })
            }
        }
    };

    let k8s_log_client: Box<dyn K8sLogStreamClient> = match kube_client_result {
        Ok(c) => Box::new(RealK8sLogStreamClient::new(c)),
        Err(e) => Box::new(InactiveK8sLogStreamClient { reason: format!("kubeconfig unavailable: {e}") }),
    };

    // Redfish and its BMC URL are not Config fields — read from env directly.
    let redfish_client: Box<dyn RedfishClient> = match std::env::var("REDFISH_BMC_BASE_URL") {
        Ok(bmc_url) => {
            let pg_pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(5))
                .connect(&config.postgres.url)
                .await.ok();
            Box::new(RealRedfishClient::new(bmc_url, pg_pool))
        }
        Err(_) => Box::new(InactiveRedfishClient { reason: "REDFISH_BMC_BASE_URL not set".into() }),
    };

    let all_sources: Vec<(&str, Box<dyn Source>)> = vec![
        ("temporal", Box::new(TemporalSource::new(temporal_client))),
        ("postgres", Box::new(PostgresSource::new(pg_client))),
        ("k8s", Box::new(K8sSource::new(k8s_client))),
        ("loki", Box::new(LokiSource::new(
            loki_client,
            k8s_log_client,
            cli.pod.clone(),
            since,
        ))),
        ("redfish", Box::new(RedfishSource::new(redfish_client))),
    ];

    let sources: Vec<Box<dyn Source>> = if use_all {
        all_sources.into_iter().map(|(_, s)| s).collect()
    } else {
        all_sources
            .into_iter()
            .filter(|(name, _)| cli.sources.iter().any(|s| s == name))
            .map(|(_, s)| s)
            .collect()
    };

    let mut all_results: Vec<SourceResult> = Vec::new();
    for source in &sources {
        all_results.push(source.collect(&cli.id, &id_type).await);
    }

    // Port-forward guards can be dropped once all source data has been collected.
    drop(_pf_guards);

    let events: Vec<Event> = all_results.iter()
        .filter_map(|r| if let SourceResult::Output(o) = r { Some(o.events.clone()) } else { None })
        .flatten()
        .collect();

    let state: Vec<StateEntry> = all_results.iter()
        .filter_map(|r| if let SourceResult::Output(o) = r { Some(o.state.clone()) } else { None })
        .flatten()
        .collect();

    let unavailable: Vec<&str> = all_results.iter()
        .filter_map(|r| if let SourceResult::Unavailable(u) = r { Some(u.name) } else { None })
        .collect();

    let filtered = filter_timeline(events, 5, 10);
    let diag = diagnose(&filtered, &state);

    let code = exit_code(Some(&id_type), &all_results);

    let mode = OutputMode {
        color: match config.output.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: cli.ascii,
    };

    if matches!(config.output.format, OutputFormat::Json) {
        let out = JsonOutput {
            version: 1,
            id: &cli.id,
            id_type: id_type_str(&id_type),
            events: filtered.iter().map(|e| JsonEvent {
                ts: e.ts.to_rfc3339(),
                source: &e.source,
                kind: &e.kind,
                severity: match e.severity {
                    crate::event::Severity::Info => "info",
                    crate::event::Severity::Warning => "warning",
                    crate::event::Severity::Error => "error",
                },
            }).collect(),
            sources_restricted: restricted_names.clone(),
            sources_unavailable: unavailable.clone(),
            state: state.iter().map(|s| JsonStateEntry {
                source: s.source,
                key: &s.key,
                value: &s.value,
            }).collect(),
            diagnosis: diag.map(|d| JsonDiagnosis {
                pattern: d.pattern,
                activity: d.activity,
                error_signature: d.error_signature,
                next_commands: d.next_commands,
            }),
        };
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!("Timeline ({} events):", filtered.len());
        for e in &filtered {
            let status = severity_to_status(&e.severity);
            let icon = status.style(status.icon(&mode), &mode);
            if e.message.is_empty() {
                println!("  {}  {}  {}  {}", e.ts.format("%H:%M:%S"), icon, e.source, e.kind);
            } else {
                println!("  {}  {}  {}  {}  {}", e.ts.format("%H:%M:%S"), icon, e.source, e.kind, e.message);
            }
        }

        let pg_state: Vec<&StateEntry> = state.iter().filter(|s| s.source == "postgres").collect();
        if !pg_state.is_empty() {
            println!("\nPostgres state (current):");
            for s in &pg_state {
                println!("  {}: {}", s.key, s.value);
            }
        }

        let redfish_state: Vec<&StateEntry> = state.iter().filter(|s| s.source == "redfish").collect();
        if !redfish_state.is_empty() {
            println!("\nRedfish state (current):");
            for s in &redfish_state {
                println!("  {}: {}", s.key, s.value);
            }
        }

        let k8s_state: Vec<&StateEntry> = state.iter().filter(|s| s.source == "k8s").collect();
        if !k8s_state.is_empty() {
            println!("\nK8s pods touched:");
            for s in &k8s_state {
                println!("  {}  {}", s.key, s.value);
            }
        }

        for s in state.iter().filter(|s| s.source == "loki") {
            println!("{}", s.value);
        }

        let sources_line: Vec<String> = attempted_names.iter().map(|name| {
            if unavailable.contains(name) {
                format!("{name} (unavailable)")
            } else {
                name.to_string()
            }
        }).collect();
        println!("\nSources: {}", sources_line.join("  "));

        for name in &restricted_names {
            println!("[source restricted: {name}]");
        }
        for name in &unavailable {
            println!("[source unavailable: {name}]");
        }

        if let Some(d) = diag {
            println!("\nLikely diagnosis:");
            println!("  Pattern:  {}", d.pattern);
            println!("  Activity: {}", d.activity);
            println!("  Error:    {}", d.error_signature);
            println!("  Confirm with:");
            for cmd in &d.next_commands {
                println!("    {cmd}");
            }
        }
    }

    std::process::exit(code);
}
