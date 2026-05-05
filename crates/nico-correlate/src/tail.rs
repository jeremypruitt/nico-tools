use std::collections::HashMap;
use std::sync::Arc;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use serde::Serialize;
use crate::event::{Event, Severity};
use crate::id::IdType;
use crate::source::{Source, SourceResult};
use nico_common::output::{OutputMode, Status};

const POLL_INTERVAL_SECS: u64 = 5;

enum TailMsg {
    Events(Vec<Event>),
    SourceError { source: &'static str, message: String },
}

#[derive(Serialize)]
struct TailJsonEvent {
    ts: String,
    source: String,
    kind: String,
    severity: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    message: String,
}

#[derive(Serialize)]
struct TailJsonError<'a> {
    source: &'a str,
    error: String,
}

/// Poll each source for new events after the initial dump, printing them as they arrive.
/// `initial_events` is the filtered timeline already displayed; used to establish per-source
/// cutoff timestamps so the tail starts exactly where the initial dump ended.
/// Runs until Ctrl-C.
pub async fn run_tail(
    sources: Vec<(&'static str, Box<dyn Source>)>,
    id: String,
    id_type: IdType,
    initial_events: &[Event],
    mode: &OutputMode,
    json: bool,
) {
    let now = Utc::now();

    // Per-source cutoff: newest ts seen in initial dump, or now if source had no events shown.
    let mut cutoffs: HashMap<&'static str, DateTime<Utc>> = HashMap::new();
    for (name, _) in &sources {
        let max_ts = initial_events.iter()
            .filter(|e| e.source.as_str() == *name)
            .map(|e| e.ts)
            .max()
            .unwrap_or(now);
        cutoffs.insert(name, max_ts);
    }

    let (tx, mut rx) = mpsc::channel::<TailMsg>(256);
    let id = Arc::new(id);
    let id_type = Arc::new(id_type);

    for (name, source) in sources {
        let tx = tx.clone();
        let id = Arc::clone(&id);
        let id_type = Arc::clone(&id_type);
        let mut cutoff = cutoffs.get(name).copied().unwrap_or(now);

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
                match source.collect(&id, &id_type).await {
                    SourceResult::Output(o) => {
                        let mut new_events: Vec<Event> = o.events
                            .into_iter()
                            .filter(|e| e.ts > cutoff)
                            .collect();
                        if !new_events.is_empty() {
                            new_events.sort_by_key(|e| e.ts);
                            if let Some(max_ts) = new_events.iter().map(|e| e.ts).max() {
                                cutoff = max_ts;
                            }
                            let _ = tx.send(TailMsg::Events(new_events)).await;
                        }
                    }
                    SourceResult::Unavailable(u) => {
                        let _ = tx.send(TailMsg::SourceError {
                            source: name,
                            message: u.reason.clone(),
                        }).await;
                    }
                }
            }
        });
    }
    drop(tx);

    if !json {
        println!("--- tailing (Ctrl-C to stop) ---");
    }

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            biased;
            _ = &mut ctrl_c => {
                if !json {
                    eprintln!("[tail stopped]");
                }
                return;
            }
            msg = rx.recv() => {
                let Some(msg) = msg else { return };
                match msg {
                    TailMsg::Events(events) => {
                        for e in &events {
                            print_event(e, mode, json);
                        }
                    }
                    TailMsg::SourceError { source, message } => {
                        if json {
                            let je = TailJsonError { source, error: message.clone() };
                            println!("{}", serde_json::to_string(&je).unwrap());
                        } else {
                            eprintln!("[{source} watch error: {message}]");
                        }
                    }
                }
            }
        }
    }
}

fn print_event(e: &Event, mode: &OutputMode, json: bool) {
    if json {
        let sev = match e.severity {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        let je = TailJsonEvent {
            ts: e.ts.to_rfc3339(),
            source: e.source.clone(),
            kind: e.kind.clone(),
            severity: sev.to_string(),
            message: e.message.clone(),
        };
        println!("{}", serde_json::to_string(&je).unwrap());
    } else {
        let status = match e.severity {
            Severity::Info => Status::Ok,
            Severity::Warning => Status::Warn,
            Severity::Error => Status::Fail,
        };
        let icon = status.style(status.icon(mode), mode);
        if e.message.is_empty() {
            println!("  {}  {}  {}  {}", e.ts.format("%H:%M:%S"), icon, e.source, e.kind);
        } else {
            println!("  {}  {}  {}  {}  {}", e.ts.format("%H:%M:%S"), icon, e.source, e.kind, e.message);
        }
    }
}
