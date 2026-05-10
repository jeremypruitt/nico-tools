use std::sync::Arc;

use nico_common::config::{ColorMode, Config, ConfigOverrides, DeploymentType, OutputFormat};
use nico_common::output::{OutputMode, Status};
use nico_common::theme;

/// PRD-001 §"Per-layer behavior": when the resolved deployment-type has
/// no forgedb (`rest-only-mock`), forgedb-dependent commands skip with a
/// reason. Mirrors `nico_doctor::layer::forgedb_skip_layer` for the
/// per-DPU non-Layer-trait commands in this crate (currently
/// `hbn-config-drift`).
fn forgedb_skip_reason(deployment_type: Option<DeploymentType>) -> Option<String> {
    let dt = deployment_type?;
    if dt.forgedb_present() {
        return None;
    }
    Some(format!("n/a in {}: no forgedb", dt.label()))
}

pub mod bootstrap;
pub mod cli;
pub mod correlate;
pub mod diagnosis;
pub mod event;
pub mod formatter;
pub mod hbn_drift;
pub mod id;
pub mod namespace;
pub mod source;
pub mod sources;
pub mod tail;
pub mod timeline;

pub use bootstrap::{collect_all, prepare_sources, resolve_config, BootstrapErr, CorrelateConfig, PreparedSources};
pub use cli::{CorrelateArgs, CorrelateCommand, HbnConfigDriftArgs};
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

    if let Some(CorrelateCommand::HbnConfigDrift(hbn_args)) = args.command.clone() {
        return run_hbn_config_drift(&args, hbn_args).await;
    }

    let cfg = match resolve_config(&args) {
        Ok(c) => c,
        Err(BootstrapErr::Fatal { message, code }) => {
            eprintln!("{}", message);
            return code;
        }
    };

    let id_str = args.id.clone().expect("resolve_config rejects missing id");

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

    let all_results = collect_all(&named_sources, &id_str, &cfg.id_type).await;
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
                &id_str,
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
            id_str.clone(),
            cfg.id_type.clone(),
            &filtered,
            &mode,
            args.json,
        )
        .await;
    }

    code
}

/// `nico correlate hbn-config-drift <machine-id>` flow. Bypasses the
/// multi-source bootstrap (boot probe, port-forwards, kube client) — the
/// drift correlation only needs forgedb (Postgres). Reuses the same
/// config resolution so `--postgres-url`, the `postgres.url` config key,
/// and the standard output flags all work.
pub async fn run_hbn_config_drift(args: &CorrelateArgs, hbn_args: HbnConfigDriftArgs) -> i32 {
    let config = match load_minimal_config(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    let freshness = match hbn_args.freshness.as_deref() {
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: invalid --freshness {s:?}: {e}");
                return 1;
            }
        },
        None => hbn_drift::DEFAULT_FRESHNESS_THRESHOLD,
    };

    if let Some(reason) = forgedb_skip_reason(config.cluster.deployment_type) {
        print!("{}", hbn_drift::render_drift_skipped(&hbn_args.machine_id, &reason));
        return 0;
    }

    let client: Arc<dyn hbn_drift::DriftClient> =
        match hbn_drift::SqlxDriftClient::new(&config.postgres.url) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("error: invalid postgres URL: {e}");
                eprintln!(
                    "  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url"
                );
                return 1;
            }
        };

    let now = chrono::Utc::now();
    let snapshot = match client.fetch_drift(&hbn_args.machine_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            print!("{}", hbn_drift::render_drift_no_data(&hbn_args.machine_id));
            return 1;
        }
        Err(e) => {
            eprintln!("error: hbn-config-drift query failed: {e}");
            eprintln!("  hint: check forgedb / postgres connectivity");
            return 1;
        }
    };

    let rows = hbn_drift::assemble_drift_rows(&snapshot, now, freshness);

    if matches!(config.output.format, OutputFormat::Json) {
        println!(
            "{}",
            hbn_drift::render_drift_json(&hbn_args.machine_id, &rows, now, snapshot.last_seen_at)
        );
    } else {
        print!(
            "{}",
            hbn_drift::render_drift_text(&hbn_args.machine_id, &rows, now, snapshot.last_seen_at)
        );
    }

    0
}

fn load_minimal_config(args: &CorrelateArgs) -> Result<Config, String> {
    let config_path = args
        .config
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config/nico-tools/config.toml")
        });
    let file_toml = std::fs::read_to_string(&config_path).ok();

    let overrides = ConfigOverrides {
        postgres_url: args.postgres_url.clone(),
        color: if args.no_color { Some(ColorMode::Never) } else { None },
        format: if args.json { Some(OutputFormat::Json) } else { None },
        ..Default::default()
    };

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    Config::load(file_toml.as_deref(), &env, &overrides, None)
        .map_err(|e| format!("error loading config: {e}"))
}

#[cfg(test)]
mod forgedb_skip_reason_tests {
    use super::*;

    #[test]
    fn rest_only_mock_has_skip_reason() {
        assert_eq!(
            forgedb_skip_reason(Some(DeploymentType::RestOnlyMock)).as_deref(),
            Some("n/a in rest-only-mock: no forgedb"),
        );
    }

    #[test]
    fn forgedb_present_types_have_no_skip_reason() {
        for dt in [DeploymentType::Full, DeploymentType::CoreOnly, DeploymentType::Force] {
            assert_eq!(
                forgedb_skip_reason(Some(dt)),
                None,
                "{dt:?}: forgedb present → no skip",
            );
        }
    }

    #[test]
    fn unresolved_deployment_type_has_no_skip_reason() {
        assert_eq!(forgedb_skip_reason(None), None);
    }
}
