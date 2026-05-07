use nico_common::config::{ColorMode, OutputFormat};
use nico_common::output::{OutputMode, Status};
use nico_common::theme;

pub mod bootstrap;
pub mod cli;
pub mod correlate;
pub mod diagnosis;
pub mod event;
pub mod formatter;
pub mod id;
pub mod namespace;
pub mod source;
pub mod sources;
pub mod tail;
pub mod timeline;

pub use bootstrap::{collect_all, prepare_sources, resolve_config, BootstrapErr, CorrelateConfig, PreparedSources};
pub use cli::CorrelateArgs;
pub use event::{Event, Severity};
pub use namespace::{recent_namespace_events, RECENT_EVENT_CAP};

use crate::correlate::exit_code;
use crate::diagnosis::{diagnose, DiagnosisConfig};
use crate::source::{Source, SourceResult, StateEntry};
use crate::timeline::filter_timeline;

fn severity_to_status(s: &Severity) -> Status {
    match s {
        Severity::Info => Status::Ok,
        Severity::Warning => Status::Warn,
        Severity::Error => Status::Fail,
    }
}

/// Top-level entry point for `nico correlate <id>`. Returns a process exit code.
pub async fn run_correlate(args: CorrelateArgs) -> i32 {
    let _resolved_theme = match theme::resolve_theme(args.theme.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let cfg = match resolve_config(&args) {
        Ok(c) => c,
        Err(BootstrapErr::Fatal { message, code }) => {
            eprintln!("{}", message);
            return code;
        }
    };

    let prepared = prepare_sources(&args, &cfg).await;
    let PreparedSources {
        named_sources,
        _pf_guards,
    } = prepared;

    let mode = OutputMode {
        color: match cfg.config.output.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let all_results = collect_all(&named_sources, &args.id, &cfg.id_type).await;
    drop(_pf_guards);

    let events: Vec<Event> = all_results
        .iter()
        .filter_map(|r| if let SourceResult::Output(o) = r { Some(o.events.clone()) } else { None })
        .flatten()
        .collect();

    let state: Vec<StateEntry> = all_results
        .iter()
        .filter_map(|r| if let SourceResult::Output(o) = r { Some(o.state.clone()) } else { None })
        .flatten()
        .collect();

    let unavailable: Vec<&str> = all_results
        .iter()
        .filter_map(|r| if let SourceResult::Unavailable(u) = r { Some(u.name) } else { None })
        .collect();

    let filtered = filter_timeline(events, 5, 10);
    let diag_config = DiagnosisConfig {
        stuck_threshold: cfg.config.temporal.stuck_threshold,
    };
    let diag = diagnose(&filtered, &state, &diag_config);

    let code = exit_code(Some(&cfg.id_type), &all_results);

    if matches!(cfg.config.output.format, OutputFormat::Json) {
        println!(
            "{}",
            formatter::format_json(
                &args.id,
                cfg.id_type.cli_name(),
                &filtered,
                &cfg.restricted_names,
                &unavailable,
                &state,
                diag.as_ref(),
            )
        );
    } else {
        println!("Timeline ({} events):", filtered.len());
        for e in &filtered {
            let status = severity_to_status(&e.severity);
            let icon = status.style(status.icon(&mode), &mode);
            if e.message.is_empty() {
                println!("  {}  {}  {}  {}", e.ts.format("%H:%M:%S"), icon, e.source, e.kind);
            } else {
                println!(
                    "  {}  {}  {}  {}  {}",
                    e.ts.format("%H:%M:%S"),
                    icon,
                    e.source,
                    e.kind,
                    e.message
                );
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

        let sources_line: Vec<String> = cfg
            .attempted_names
            .iter()
            .map(|name| {
                if unavailable.contains(name) {
                    format!("{name} (unavailable)")
                } else {
                    name.to_string()
                }
            })
            .collect();
        println!("\nSources: {}", sources_line.join("  "));

        for name in &cfg.restricted_names {
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

    if args.tail {
        let tail_sources: Vec<(&'static str, Box<dyn Source>)> = named_sources
            .into_iter()
            .zip(all_results.iter())
            .filter_map(|((name, source), result)| {
                if matches!(result, SourceResult::Output(_)) {
                    Some((name, source))
                } else {
                    None
                }
            })
            .collect();
        tail::run_tail(
            tail_sources,
            args.id.clone(),
            cfg.id_type.clone(),
            &filtered,
            &mode,
            args.json,
        )
        .await;
    }

    code
}
