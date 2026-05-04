mod correlate;
mod event;
mod id;
mod source;
mod sources;
mod timeline;

use clap::Parser;
use chrono::Duration;
use serde::Serialize;
use crate::id::{IdType, detect_id_type};
use crate::source::{Source, SourceResult, StateEntry};
use crate::sources::temporal::{TemporalSource, TemporalClient, RawTemporalEvent};
use crate::sources::postgres::{PostgresSource, PostgresClient, PgEntityData, SqlxPostgresClient};
use crate::sources::k8s::{K8sSource, K8sClient, K8sPodData};
use crate::sources::loki::{LokiSource, LokiClient, LokiLogLine, K8sLogStreamClient, K8sLogLine};
use crate::sources::redfish::{RedfishSource, RedfishClient, RedfishData};
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

// Real Temporal client is wired in issue #14.
struct TodoTemporalClient;

#[async_trait]
impl TemporalClient for TodoTemporalClient {
    async fn get_history(&self, _workflow_id: &str) -> Result<Vec<RawTemporalEvent>> {
        Err(anyhow::anyhow!("not implemented: real Temporal gRPC client — see issue #14"))
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

// Real k8s client is wired when kube-rs is added.
struct TodoK8sClient;

#[async_trait]
impl K8sClient for TodoK8sClient {
    async fn find_pods_with_events(&self, _id: &str, _id_type: &IdType) -> Result<Vec<K8sPodData>> {
        Err(anyhow::anyhow!("not implemented: real k8s client — uses in-cluster or kubeconfig"))
    }
}

// Real Loki HTTP client is wired when reqwest is added.
struct TodoLokiClient;

#[async_trait]
impl LokiClient for TodoLokiClient {
    async fn query_range(
        &self,
        _id: &str,
        _id_type: &IdType,
        _since: Duration,
        _pod_pattern: Option<&str>,
    ) -> Result<Vec<LokiLogLine>> {
        Err(anyhow::anyhow!("not implemented: real Loki HTTP client — query LOKI_URL with label selectors derived from entity ID"))
    }
}

// Real k8s log streaming client is wired when kube-rs is added.
struct TodoK8sLogStreamClient;

#[async_trait]
impl K8sLogStreamClient for TodoK8sLogStreamClient {
    async fn stream_logs(
        &self,
        _id: &str,
        _id_type: &IdType,
        _since: Duration,
        _pod_pattern: Option<&str>,
    ) -> Result<Vec<K8sLogLine>> {
        Err(anyhow::anyhow!("not implemented: real k8s log streaming client — kubectl logs equivalent"))
    }
}

// Real Redfish client resolves DPU entities via Postgres hosts.dpu_id, then
// queries the host BMC with read-only GETs (ADR-002).
struct TodoRedfishClient;

#[async_trait]
impl RedfishClient for TodoRedfishClient {
    async fn query(&self, _id: &str, _id_type: &IdType) -> Result<RedfishData> {
        Err(anyhow::anyhow!("not implemented: real Redfish client — resolve host via Postgres hosts.dpu_id for DPU entities, query BMC GET endpoints via REDFISH_BMC_BASE_URL"))
    }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    version: u32,
    id: &'a str,
    id_type: &'a str,
    events: Vec<JsonEvent<'a>>,
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

    println!("detected type: {}", id_type_str(&id_type));

    let pg_client: Box<dyn PostgresClient> = match std::env::var("NICO_POSTGRES_URL") {
        Ok(url) => match SqlxPostgresClient::connect(&url).await {
            Ok(c) => Box::new(c),
            Err(e) => Box::new(InactivePostgresClient { reason: format!("connect failed: {e}") }),
        },
        Err(_) => Box::new(InactivePostgresClient { reason: "NICO_POSTGRES_URL not set".into() }),
    };

    let all_sources: Vec<(&str, Box<dyn Source>)> = vec![
        ("temporal", Box::new(TemporalSource::new(Box::new(TodoTemporalClient)))),
        ("postgres", Box::new(PostgresSource::new(pg_client))),
        ("k8s", Box::new(K8sSource::new(Box::new(TodoK8sClient)))),
        ("loki", Box::new(LokiSource::new(
            Box::new(TodoLokiClient),
            Box::new(TodoK8sLogStreamClient),
            cli.pod.clone(),
            since,
        ))),
        ("redfish", Box::new(RedfishSource::new(Box::new(TodoRedfishClient)))),
    ];

    let sources: Vec<Box<dyn Source>> = if cli.sources.is_empty() {
        all_sources.into_iter().map(|(_, s)| s).collect()
    } else {
        all_sources.into_iter()
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
            println!("  {}  {}  {}", e.ts.format("%H:%M:%S"), e.source, e.kind);
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

        for name in &unavailable {
            println!("[source unavailable: {name}]");
        }
    }

    std::process::exit(code);
}
