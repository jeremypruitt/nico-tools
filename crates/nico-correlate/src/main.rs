mod correlate;
mod event;
mod id;
mod source;
mod sources;
mod timeline;

use clap::Parser;
use chrono::Duration;
use serde::Serialize;
use nico_common::output::{OutputMode, Status};
use crate::id::{IdType, detect_id_type};
use crate::source::{Source, SourceResult, StateEntry};
use crate::sources::temporal::{TemporalSource, TemporalClient, RawTemporalEvent};
use crate::sources::temporal_grpc::GrpcTemporalClient;
use crate::sources::postgres::{PostgresSource, PostgresClient, PgEntityData, SqlxPostgresClient};
use crate::sources::k8s::{K8sSource, K8sClient, K8sPodData, KubeRsK8sClient};
use crate::sources::loki::{LokiSource, LokiClient, LokiLogLine, K8sLogStreamClient, K8sLogLine, RealLokiClient, RealK8sLogStreamClient};
use crate::sources::redfish::{RedfishSource, RedfishClient, RedfishData, RealRedfishClient};
use crate::timeline::filter_timeline;
use crate::correlate::exit_code;
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

struct InactiveTemporalClient {
    reason: String,
}

#[async_trait]
impl TemporalClient for InactiveTemporalClient {
    async fn get_history(&self, _workflow_id: &str) -> Result<Vec<RawTemporalEvent>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
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
struct JsonOutput<'a> {
    version: u32,
    id: &'a str,
    id_type: &'a str,
    events: Vec<JsonEvent<'a>>,
    sources_restricted: Vec<&'a str>,
    sources_unavailable: Vec<&'a str>,
    state: Vec<JsonStateEntry<'a>>,
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

    let pg_client: Box<dyn PostgresClient> = match std::env::var("NICO_POSTGRES_URL") {
        Ok(url) => match SqlxPostgresClient::connect(&url).await {
            Ok(c) => Box::new(c),
            Err(e) => Box::new(InactivePostgresClient { reason: format!("connect failed: {e}") }),
        },
        Err(_) => Box::new(InactivePostgresClient { reason: "NICO_POSTGRES_URL not set".into() }),
    };

    let k8s_client: Box<dyn K8sClient> = match KubeRsK8sClient::try_default().await {
        Ok(c) => Box::new(c),
        Err(e) => Box::new(InactiveK8sClient { reason: format!("kubeconfig unavailable: {e}") }),
    };

    let temporal_client: Box<dyn TemporalClient> = match std::env::var("NICO_TEMPORAL_ADDRESS") {
        Ok(addr) => {
            let ns = std::env::var("NICO_TEMPORAL_NAMESPACE").unwrap_or_else(|_| "default".into());
            Box::new(GrpcTemporalClient::new(addr, ns))
        }
        Err(_) => Box::new(InactiveTemporalClient { reason: "NICO_TEMPORAL_ADDRESS not set".into() }),
    };

    let loki_client: Box<dyn LokiClient> = match std::env::var("LOKI_URL") {
        Ok(url) => Box::new(RealLokiClient::new(url)),
        Err(_) => Box::new(InactiveLokiClient { reason: "LOKI_URL not set".into() }),
    };

    let k8s_log_client: Box<dyn K8sLogStreamClient> = match kube::Client::try_default().await {
        Ok(c) => Box::new(RealK8sLogStreamClient::new(c)),
        Err(e) => Box::new(InactiveK8sLogStreamClient { reason: format!("kubeconfig unavailable: {e}") }),
    };

    let redfish_client: Box<dyn RedfishClient> = match std::env::var("REDFISH_BMC_BASE_URL") {
        Ok(bmc_url) => {
            let pg_pool = match std::env::var("NICO_POSTGRES_URL") {
                Ok(pg_url) => sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .acquire_timeout(std::time::Duration::from_secs(5))
                    .connect(&pg_url)
                    .await.ok(),
                Err(_) => None,
            };
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

    let code = exit_code(Some(&id_type), &all_results);

    let mode = OutputMode {
        color: !cli.no_color && std::env::var("NO_COLOR").is_err(),
        ascii: cli.ascii,
    };

    if cli.json {
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
    }

    std::process::exit(code);
}
