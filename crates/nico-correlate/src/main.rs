mod correlate;
mod event;
mod id;
mod source;
mod sources;
mod timeline;

use clap::Parser;
use serde::Serialize;
use crate::id::{IdType, detect_id_type};
use crate::source::{Source, SourceResult};
use crate::sources::temporal::{TemporalSource, TemporalClient, RawTemporalEvent};
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

    /// Output JSON
    #[arg(short = 'j', long)]
    json: bool,
}

// Real Temporal client is wired in issue #14.
struct TodoTemporalClient;

#[async_trait]
impl TemporalClient for TodoTemporalClient {
    async fn get_history(&self, _workflow_id: &str) -> Result<Vec<RawTemporalEvent>> {
        todo!("real Temporal gRPC client — see issue #14")
    }
}

#[derive(Serialize)]
struct JsonOutput<'a> {
    version: u32,
    id: &'a str,
    id_type: &'a str,
    events: Vec<JsonEvent<'a>>,
    sources_unavailable: Vec<&'a str>,
}

#[derive(Serialize)]
struct JsonEvent<'a> {
    ts: String,
    source: &'a str,
    kind: &'a str,
    severity: &'a str,
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

    let sources: Vec<Box<dyn Source>> = vec![
        Box::new(TemporalSource::new(Box::new(TodoTemporalClient))),
    ];

    let mut all_results: Vec<SourceResult> = Vec::new();
    for source in &sources {
        all_results.push(source.collect(&cli.id, &id_type).await);
    }

    let events: Vec<Event> = all_results.iter()
        .filter_map(|r| if let SourceResult::Events(e) = r { Some(e.clone()) } else { None })
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
        };
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    } else {
        println!("Timeline ({} events):", filtered.len());
        for e in &filtered {
            println!("  {}  {}  {}", e.ts.format("%H:%M:%S"), e.source, e.kind);
        }
        for name in &unavailable {
            println!("  [source unavailable: {name}]");
        }
    }

    std::process::exit(code);
}
