mod correlate;
mod diagnosis;
mod event;
mod id;
mod source;
mod sources;
mod timeline;
mod tui;

use clap::Parser;
use chrono::Duration;
use serde::Serialize;
use nico_common::config::{Config, ConfigOverrides, ColorMode, OutputFormat, ReachMode};
use nico_common::output::{OutputMode, Status};
use nico_common::reach::ReachManager;
use crate::id::{IdType, detect_id_type};
use crate::source::{Source, SourceKind, SourceResult, StateEntry, UnavailableSource};
use crate::sources::temporal::{TemporalSource, TemporalClient};
use crate::sources::temporal_grpc::GrpcTemporalClient;
use crate::sources::postgres::{PostgresSource, SqlxPostgresClient};
use crate::sources::k8s::{K8sSource, KubeRsK8sClient};
use crate::sources::loki::{LokiSource, LokiClient, K8sLogStreamClient, RealLokiClient, RealK8sLogStreamClient};
use crate::sources::redfish::{RedfishSource, RealRedfishClient};
use crate::timeline::filter_timeline;
use crate::correlate::exit_code;
use crate::diagnosis::diagnose;
use crate::event::Event;
use anyhow::Result;

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

    /// Launch interactive TUI (requires a TTY; mutually exclusive with --json)
    #[arg(long)]
    tui: bool,

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

    // Guard: --tui and --json are mutually exclusive.
    if cli.tui && cli.json {
        eprintln!("error: --tui and --json are mutually exclusive");
        std::process::exit(3);
    }

    // Guard: --tui requires an interactive terminal.
    if cli.tui {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            eprintln!("`--tui` requires an interactive terminal (stdout is not a TTY)");
            std::process::exit(3);
        }
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
            if SourceKind::from_name(s.as_str()).is_none() {
                let valid = SourceKind::ALL.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ");
                eprintln!("error: unknown source {:?}; valid sources: {} or \"all\"", s, valid);
                std::process::exit(1);
            }
        }
    }

    let restricted_names: Vec<&str> = if use_all {
        vec![]
    } else {
        SourceKind::ALL
            .iter()
            .map(|k| k.name())
            .filter(|&name| !cli.sources.iter().any(|s| s == name))
            .collect()
    };

    let attempted_names: Vec<&str> = SourceKind::ALL
        .iter()
        .map(|k| k.name())
        .filter(|&name| !restricted_names.contains(&name))
        .collect();

    let id_type = if let Some(ref t) = cli.r#type {
        match IdType::from_cli_name(t) {
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

    let pg_source: Box<dyn Source> = match SqlxPostgresClient::connect(&postgres_url).await {
        Ok(c) => Box::new(PostgresSource::new(Box::new(c))),
        Err(e) => Box::new(UnavailableSource::new("postgres", format!("connect failed: {e}"))),
    };

    let k8s_source: Box<dyn Source> = match KubeRsK8sClient::try_default().await {
        Ok(c) => Box::new(K8sSource::new(Box::new(c))),
        Err(e) => Box::new(UnavailableSource::new("k8s", format!("kubeconfig unavailable: {e}"))),
    };

    let temporal_client: Box<dyn TemporalClient> = Box::new(GrpcTemporalClient::new(
        temporal_address,
        config.temporal.namespace.clone(),
    ));

    // Loki: explicit env var wins, then reach manager discovery.
    let loki_result: Result<Box<dyn LokiClient>, &'static str> = match std::env::var("LOKI_URL") {
        Ok(url) => Ok(Box::new(RealLokiClient::new(url))),
        Err(_) => {
            if let Some(ref mgr) = reach_mgr {
                match mgr.loki_url().await {
                    Ok((url, guard)) => {
                        _pf_guards.extend(guard);
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
        Ok(loki) => Box::new(LokiSource::new(loki, k8s_log_opt, cli.pod.clone(), since)),
        Err(reason) => Box::new(UnavailableSource::new("loki", reason)),
    };

    // Redfish and its BMC URL are not Config fields — read from env directly.
    let redfish_source: Box<dyn Source> = match std::env::var("REDFISH_BMC_BASE_URL") {
        Ok(bmc_url) => {
            let pg_pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(5))
                .connect(&config.postgres.url)
                .await.ok();
            Box::new(RedfishSource::new(Box::new(RealRedfishClient::new(bmc_url, pg_pool))))
        }
        Err(_) => Box::new(UnavailableSource::new("redfish", "REDFISH_BMC_BASE_URL not set")),
    };

    let all_sources: Vec<(&'static str, Box<dyn Source>)> = vec![
        ("temporal", Box::new(TemporalSource::new(temporal_client))),
        ("postgres", pg_source),
        ("k8s", k8s_source),
        ("loki", loki_source),
        ("redfish", redfish_source),
    ];

    let named_sources: Vec<(&'static str, Box<dyn Source>)> = if use_all {
        all_sources
    } else {
        all_sources
            .into_iter()
            .filter(|(name, _)| cli.sources.iter().any(|s| s == name))
            .collect()
    };

    let mode = OutputMode {
        color: match config.output.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: cli.ascii,
    };

    // --- TUI mode: render immediately, stream source results as they arrive ---
    if cli.tui {
        let source_names: Vec<&'static str> = named_sources.iter().map(|(n, _)| *n).collect();
        let (tx, rx) = std::sync::mpsc::channel::<tui::TuiUpdate>();
        let tui_config = tui::TuiConfig {
            id: cli.id.clone(),
            source_names,
            restricted: restricted_names.iter().map(|s| s.to_string()).collect(),
        };
        let id_clone = cli.id.clone();
        let id_type_clone = id_type.clone();
        let mut join_set = tokio::task::JoinSet::new();
        for (name, source) in named_sources {
            let tx = tx.clone();
            let id = id_clone.clone();
            let idt = id_type_clone.clone();
            join_set.spawn(async move {
                let result = source.collect(&id, &idt).await;
                let _ = tx.send(tui::TuiUpdate::SourceDone { name: name.to_string(), result });
            });
        }
        drop(tx); // rx signals EOF when all tasks have sent their result and dropped their tx clone
        let ctx = tui::TuiContext { mode };
        let tui_exit = tokio::task::block_in_place(|| {
            tui::run_tui_incremental(tui_config, rx, ctx)
        });
        // Wait for any tasks that outlived the TUI (user quit early).
        while join_set.join_next().await.is_some() {}
        drop(_pf_guards);
        std::process::exit(tui_exit);
    }

    // --- Non-TUI path: collect sources sequentially ---
    let mut all_results: Vec<SourceResult> = Vec::new();
    for (_, source) in &named_sources {
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

    if matches!(config.output.format, OutputFormat::Json) {
        let out = JsonOutput {
            version: 1,
            id: &cli.id,
            id_type: id_type.cli_name(),
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
