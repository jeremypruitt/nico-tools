use std::sync::Arc;
use std::time::Duration;

use nico_common::config::{Config, ConfigOverrides, OutputFormat};
use nico_common::output::Status;
use nico_common::theme;

pub mod baseline;
pub mod bootstrap;
pub mod cli;
pub mod dpu;
pub mod dpu_cert;
pub mod dpu_health;
pub mod dpu_isolation;
pub mod dpu_services;
pub mod formatter;
pub mod grpc;
pub mod hbn;
pub mod http;
pub mod layer;
pub mod layers;
pub mod log_collector;
pub mod log_source;
pub mod loki;
pub mod postgres;
pub mod preflight;
pub mod runner;

pub use bootstrap::{bootstrap, prepare_layers, Bootstrapped, BootstrapErr, LayerInputs};
pub use cli::{
    DoctorArgs, DoctorCommand, DpuCertArgs, DpuHealthArgs, DpuIsolationArgs, DpuServicesArgs,
    HbnArgs,
};
pub use runner::Report;

/// Run all layers once, returning a [`Report`]. Equivalent to
/// [`run_once_with_log_collector`] with no collector.
pub async fn run_once(layers: &[Box<dyn layer::Layer>], opts: &layer::RunOpts) -> Report {
    runner::run(layers, opts).await
}

/// Run the per-refresh [`log_collector::LogCollectorStage`] (when one is
/// available) to populate `opts.pod_logs`, then run all layers and return
/// a [`Report`]. The shared cache is what gives us "at most one
/// `pod_logs` call per pod per refresh" (issue #201).
pub async fn run_once_with_log_collector(
    layers: &[Box<dyn layer::Layer>],
    opts: &layer::RunOpts,
    collector: Option<&log_collector::LogCollectorStage>,
) -> Report {
    runner::run_with_log_collector(layers, opts, collector).await
}

/// Run all layers and stream each [`layer::LayerResult`] as it completes.
///
/// Layers run concurrently with the same per-layer timeout policy as
/// [`run_once`]. When a layer times out, an `Unknown` result is sent.
/// The channel is closed when all layers have reported. If `collector`
/// is `Some`, it is run once before the fan-out and its result is
/// installed into `opts.pod_logs` for every layer to read.
pub async fn run_streaming(
    layers: Arc<Vec<Box<dyn layer::Layer>>>,
    opts: layer::RunOpts,
    collector: Option<Arc<log_collector::LogCollectorStage>>,
    tx: tokio::sync::mpsc::Sender<layer::LayerResult>,
) {
    use futures::stream::{FuturesUnordered, StreamExt};

    let opts = runner::with_collected_logs(&opts, collector.as_deref()).await;

    let mut in_flight: FuturesUnordered<_> = (0..layers.len())
        .map(|idx| {
            let layers = layers.clone();
            let opts = opts.clone();
            async move {
                let layer = &layers[idx];
                let timeout = opts.timeout;
                match tokio::time::timeout(timeout, layer.run(&opts)).await {
                    Ok(result) => result,
                    Err(_) => layer::LayerResult {
                        name: layer.name(),
                        status: Status::Unknown,
                        checks: vec![],
                        duration_ms: timeout.as_millis() as u64,
                        skipped_reason: None,
                    },
                }
            }
        })
        .collect();

    while let Some(result) = in_flight.next().await {
        if tx.send(result).await.is_err() {
            break;
        }
    }
}

fn exit_code(report: &Report) -> i32 {
    match report.summary_status() {
        Status::Ok | Status::Skipped => 0,
        Status::Warn => 1,
        Status::Fail => 2,
        Status::Unknown => 3,
    }
}

/// Top-level entry point: parse args, build layers, run once, format, and
/// return a process exit code.
pub async fn run_doctor(args: DoctorArgs) -> i32 {
    let _resolved_theme = match theme::resolve_theme(args.theme.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    if let Some(DoctorCommand::Hbn(hbn_args)) = args.command.clone() {
        return run_hbn(&args, hbn_args).await;
    }

    if let Some(DoctorCommand::DpuIsolation(iso_args)) = args.command.clone() {
        return run_dpu_isolation(&args, iso_args).await;
    }

    if let Some(DoctorCommand::DpuCert(cert_args)) = args.command.clone() {
        return run_dpu_cert(&args, cert_args).await;
    }

    if let Some(DoctorCommand::DpuHealth(health_args)) = args.command.clone() {
        return run_dpu_health(&args, health_args).await;
    }

    if let Some(DoctorCommand::DpuServices(svc_args)) = args.command.clone() {
        return run_dpu_services(&args, svc_args).await;
    }

    let bootstrapped = match bootstrap(&args).await {
        Ok(b) => b,
        Err(BootstrapErr::Preflight { human_message: _, json_payload, format }) => {
            if matches!(format, OutputFormat::Json) {
                println!("{}", json_payload);
            }
            // Non-JSON modes already had the failure card painted on
            // stderr by the boot-probe orchestrator (see
            // `BootProbe::finish_failure`); reprinting `human_message`
            // duplicates the same fields with a less polished layout.
            return 3;
        }
        Err(BootstrapErr::Fatal { message, code }) => {
            eprintln!("{}", message);
            return code;
        }
    };

    let baseline_prior = baseline::load();

    let report = run_once_with_log_collector(
        &bootstrapped.layers,
        &bootstrapped.opts,
        bootstrapped.log_collector.as_deref(),
    )
    .await;

    drop(bootstrapped._pf_guards);

    let code = exit_code(&report);

    if code != 3 {
        baseline::save(&report);
    }

    let deltas = baseline::compute_deltas(&report, baseline_prior.as_ref());

    if matches!(bootstrapped.output_format, OutputFormat::Json) {
        println!(
            "{}",
            formatter::format_json(
                &report,
                &bootstrapped.namespace,
                preflight::ok_section(),
                &deltas
            )
        );
    } else {
        print!(
            "{}",
            formatter::format_report(
                &report,
                &bootstrapped.output_mode,
                bootstrapped.verbose,
                &deltas,
                bootstrapped.spotlight
            )
        );
    }

    code
}

/// `nico doctor hbn <dpu-id>` flow. Bypasses the multi-layer ladder and
/// the boot probe — the HBN verdict only needs forgedb (Postgres). It
/// reuses the same config resolution (so `--postgres-url`, the
/// `postgres.url` config key, and the standard output flags all work)
/// and the same headline-vs-detail formatter, so output is consistent
/// with the rest of `nico doctor`.
pub async fn run_hbn(args: &DoctorArgs, hbn_args: HbnArgs) -> i32 {
    let config = match load_minimal_config(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    let output_mode = nico_common::output::OutputMode {
        color: match config.output.color {
            nico_common::config::ColorMode::Always => true,
            nico_common::config::ColorMode::Never => false,
            nico_common::config::ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let freshness = match hbn_args.freshness.as_deref() {
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: invalid --freshness {s:?}: {e}");
                return 1;
            }
        },
        None => hbn::DEFAULT_FRESHNESS_THRESHOLD,
    };

    let layers: Vec<Box<dyn layer::Layer>> = if let Some(skip) =
        layer::forgedb_skip_layer("hbn", config.cluster.deployment_type)
    {
        vec![skip]
    } else {
        let client: Arc<dyn hbn::HbnClient> = match hbn::SqlxHbnClient::new(&config.postgres.url) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("error: invalid postgres URL: {e}");
                eprintln!("  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url");
                return 1;
            }
        };
        let l = layers::hbn::HbnLayer::new(client, hbn_args.dpu_id.clone())
            .with_freshness_threshold(freshness);
        vec![Box::new(l)]
    };

    let opts = layer::RunOpts {
        namespace: config.cluster.namespace.clone(),
        since: Duration::from_secs(600),
        timeout: humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5)),
        ..Default::default()
    };

    let report = run_once(&layers, &opts).await;

    if matches!(config.output.format, OutputFormat::Json) {
        println!(
            "{}",
            formatter::format_json(
                &report,
                &config.cluster.namespace,
                preflight::ok_section(),
                &std::collections::HashMap::new(),
            )
        );
    } else {
        print!(
            "{}",
            formatter::format_report(
                &report,
                &output_mode,
                args.verbose,
                &std::collections::HashMap::new(),
                args.spotlight,
            )
        );
    }

    exit_code(&report)
}

/// `nico doctor dpu-isolation <machine-id>` flow. Same shape as
/// [`run_hbn`]: bypasses the multi-layer ladder, reuses the standard
/// config resolution and headline-vs-detail formatter, and only depends
/// on Postgres reachability.
pub async fn run_dpu_isolation(args: &DoctorArgs, iso_args: DpuIsolationArgs) -> i32 {
    let config = match load_minimal_config(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    let output_mode = nico_common::output::OutputMode {
        color: match config.output.color {
            nico_common::config::ColorMode::Always => true,
            nico_common::config::ColorMode::Never => false,
            nico_common::config::ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let freshness = match iso_args.freshness.as_deref() {
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: invalid --freshness {s:?}: {e}");
                return 1;
            }
        },
        None => dpu_isolation::DEFAULT_FRESHNESS_THRESHOLD,
    };

    let layers: Vec<Box<dyn layer::Layer>> = if let Some(skip) =
        layer::forgedb_skip_layer("dpu_isolation", config.cluster.deployment_type)
    {
        vec![skip]
    } else {
        let client: Arc<dyn dpu_isolation::DpuIsolationClient> =
            match dpu_isolation::SqlxDpuIsolationClient::new(&config.postgres.url) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    eprintln!("error: invalid postgres URL: {e}");
                    eprintln!("  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url");
                    return 1;
                }
            };
        let l = layers::dpu_isolation::DpuIsolationLayer::new(client, iso_args.machine_id.clone())
            .with_freshness_threshold(freshness);
        vec![Box::new(l)]
    };

    let opts = layer::RunOpts {
        namespace: config.cluster.namespace.clone(),
        since: Duration::from_secs(600),
        timeout: humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5)),
        ..Default::default()
    };

    let report = run_once(&layers, &opts).await;

    if matches!(config.output.format, OutputFormat::Json) {
        println!(
            "{}",
            formatter::format_json(
                &report,
                &config.cluster.namespace,
                preflight::ok_section(),
                &std::collections::HashMap::new(),
            )
        );
    } else {
        print!(
            "{}",
            formatter::format_report(
                &report,
                &output_mode,
                args.verbose,
                &std::collections::HashMap::new(),
                args.spotlight,
            )
        );
    }

    exit_code(&report)
}

/// `nico doctor dpu-cert <dpu-id>` flow. Same shape as
/// [`run_hbn`] / [`run_dpu_isolation`]: bypasses the multi-layer
/// ladder, reuses the standard config resolution and headline-vs-detail
/// formatter, and only depends on Postgres reachability.
pub async fn run_dpu_cert(args: &DoctorArgs, cert_args: DpuCertArgs) -> i32 {
    let config = match load_minimal_config(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    let output_mode = nico_common::output::OutputMode {
        color: match config.output.color {
            nico_common::config::ColorMode::Always => true,
            nico_common::config::ColorMode::Never => false,
            nico_common::config::ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let warn_threshold = match cert_args.warn.as_deref() {
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: invalid --warn {s:?}: {e}");
                return 1;
            }
        },
        None => dpu_cert::DEFAULT_WARN_THRESHOLD,
    };

    let layers: Vec<Box<dyn layer::Layer>> = if let Some(skip) =
        layer::forgedb_skip_layer("dpu_cert", config.cluster.deployment_type)
    {
        vec![skip]
    } else {
        let client: Arc<dyn dpu_cert::DpuCertClient> =
            match dpu_cert::SqlxDpuCertClient::new(&config.postgres.url) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    eprintln!("error: invalid postgres URL: {e}");
                    eprintln!("  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url");
                    return 1;
                }
            };
        let l = layers::dpu_cert::DpuCertLayer::new(client, cert_args.dpu_id.clone())
            .with_warn_threshold(warn_threshold);
        vec![Box::new(l)]
    };

    let opts = layer::RunOpts {
        namespace: config.cluster.namespace.clone(),
        since: Duration::from_secs(600),
        timeout: humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5)),
        ..Default::default()
    };

    let report = run_once(&layers, &opts).await;

    if matches!(config.output.format, OutputFormat::Json) {
        println!(
            "{}",
            formatter::format_json(
                &report,
                &config.cluster.namespace,
                preflight::ok_section(),
                &std::collections::HashMap::new(),
            )
        );
    } else {
        print!(
            "{}",
            formatter::format_report(
                &report,
                &output_mode,
                args.verbose,
                &std::collections::HashMap::new(),
                args.spotlight,
            )
        );
    }

    exit_code(&report)
}

/// `nico doctor dpu-health <machine-id>` flow. Same shape as
/// [`run_hbn`] / [`run_dpu_cert`]: bypasses the multi-layer ladder, reuses
/// the standard config resolution and headline-vs-detail formatter, and
/// only depends on Postgres reachability.
pub async fn run_dpu_health(args: &DoctorArgs, health_args: DpuHealthArgs) -> i32 {
    let config = match load_minimal_config(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    let output_mode = nico_common::output::OutputMode {
        color: match config.output.color {
            nico_common::config::ColorMode::Always => true,
            nico_common::config::ColorMode::Never => false,
            nico_common::config::ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let dhcp_stale_threshold = match health_args.dhcp_stale.as_deref() {
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: invalid --dhcp-stale {s:?}: {e}");
                return 1;
            }
        },
        None => dpu_health::DEFAULT_DHCP_STALE_THRESHOLD,
    };

    let client: Arc<dyn dpu_health::DpuHealthClient> =
        match dpu_health::SqlxDpuHealthClient::new(&config.postgres.url) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                eprintln!("error: invalid postgres URL: {e}");
                eprintln!("  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url");
                return 1;
            }
        };

    let layer = layers::dpu_health::DpuHealthLayer::new(client, health_args.dpu_id.clone())
        .with_dhcp_stale_threshold(dhcp_stale_threshold);
    let layers: Vec<Box<dyn layer::Layer>> = vec![Box::new(layer)];

    let opts = layer::RunOpts {
        namespace: config.cluster.namespace.clone(),
        since: Duration::from_secs(600),
        timeout: humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5)),
        ..Default::default()
    };

    let report = run_once(&layers, &opts).await;

    if matches!(config.output.format, OutputFormat::Json) {
        println!(
            "{}",
            formatter::format_json(
                &report,
                &config.cluster.namespace,
                preflight::ok_section(),
                &std::collections::HashMap::new(),
            )
        );
    } else {
        print!(
            "{}",
            formatter::format_report(
                &report,
                &output_mode,
                args.verbose,
                &std::collections::HashMap::new(),
                args.spotlight,
            )
        );
    }

    exit_code(&report)
}

/// `nico doctor dpu-services <machine-id>` flow. Same shape as
/// [`run_dpu_health`]: bypasses the multi-layer ladder, reuses the
/// standard config resolution and headline-vs-detail formatter, and only
/// depends on Postgres reachability. Forgedb-gated: skips cleanly under
/// `rest-only-mock`.
pub async fn run_dpu_services(args: &DoctorArgs, svc_args: DpuServicesArgs) -> i32 {
    let config = match load_minimal_config(args) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };

    let output_mode = nico_common::output::OutputMode {
        color: match config.output.color {
            nico_common::config::ColorMode::Always => true,
            nico_common::config::ColorMode::Never => false,
            nico_common::config::ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let stale_threshold = match svc_args.stale.as_deref() {
        Some(s) => match humantime::parse_duration(s) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("error: invalid --stale {s:?}: {e}");
                return 1;
            }
        },
        None => dpu_services::DEFAULT_OBSERVATION_STALE_THRESHOLD,
    };

    let layers: Vec<Box<dyn layer::Layer>> = if let Some(skip) =
        layer::forgedb_skip_layer("dpu_services", config.cluster.deployment_type)
    {
        vec![skip]
    } else {
        let client: Arc<dyn dpu_services::DpuServicesClient> =
            match dpu_services::SqlxDpuServicesClient::new(&config.postgres.url) {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    eprintln!("error: invalid postgres URL: {e}");
                    eprintln!("  hint: set postgres.url in ~/.config/nico-tools/config.toml or use --postgres-url");
                    return 1;
                }
            };
        let l = layers::dpu_services::DpuServicesLayer::new(client, svc_args.dpu_id.clone())
            .with_stale_threshold(stale_threshold);
        vec![Box::new(l)]
    };

    let opts = layer::RunOpts {
        namespace: config.cluster.namespace.clone(),
        since: Duration::from_secs(600),
        timeout: humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5)),
        ..Default::default()
    };

    let report = run_once(&layers, &opts).await;

    if matches!(config.output.format, OutputFormat::Json) {
        println!(
            "{}",
            formatter::format_json(
                &report,
                &config.cluster.namespace,
                preflight::ok_section(),
                &std::collections::HashMap::new(),
            )
        );
    } else {
        print!(
            "{}",
            formatter::format_report(
                &report,
                &output_mode,
                args.verbose,
                &std::collections::HashMap::new(),
                args.spotlight,
            )
        );
    }

    exit_code(&report)
}

/// Resolve the merged [`Config`] (config file + CLI overrides) for a
/// doctor-style invocation. Exposed so other crates (e.g. `nico-ops`)
/// that share the doctor flag surface but skip the full bootstrap can
/// still read `postgres.url`, `cluster.namespace`, etc. without
/// duplicating the merge logic.
pub fn load_minimal_config(args: &DoctorArgs) -> Result<Config, String> {
    let config_path = args
        .config
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config/nico-tools/config.toml")
        });
    let file_toml = std::fs::read_to_string(&config_path).ok();

    let overrides = doctor_args_to_overrides(args);

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    Config::load(file_toml.as_deref(), &env, &overrides, None)
        .map_err(|e| format!("error loading config: {e}"))
}

fn doctor_args_to_overrides(args: &DoctorArgs) -> ConfigOverrides {
    ConfigOverrides {
        namespace: args.namespace.clone(),
        context: args.context.clone(),
        postgres_url: args.postgres_url.clone(),
        color: if args.no_color {
            Some(nico_common::config::ColorMode::Never)
        } else {
            None
        },
        format: if args.json { Some(OutputFormat::Json) } else { None },
        deployment_type_spec: args.deployment_type.clone(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nico_common::config::DeploymentType;
    use std::collections::HashMap;

    fn args_with_deployment_type(spec: Option<&str>) -> DoctorArgs {
        DoctorArgs {
            command: None,
            namespace: None,
            context: None,
            skip: vec![],
            since: "10m".into(),
            timeout: "5s".into(),
            timeouts: None,
            json: false,
            verbose: false,
            spotlight: false,
            ascii: false,
            no_color: false,
            postgres_url: None,
            config: None,
            mode: None,
            theme: None,
            deployment_type: spec.map(str::to_owned),
        }
    }

    /// Regression for #299. Until this fix, `load_minimal_config` (used by
    /// `run_hbn`, `run_dpu_isolation`, `run_dpu_cert`, `run_dpu_services`,
    /// and `run_dpu_health`) dropped `args.deployment_type` on the floor —
    /// so `--deployment-type=rest-only-mock <id>` ran the postgres path
    /// instead of forgedb-skipping. Verify the CLI flag round-trips into
    /// the resolved `cluster.deployment_type` for every label.
    #[test]
    fn load_minimal_config_round_trips_deployment_type_flag() {
        for label in ["full", "core-only", "rest-only-mock", "force"] {
            let args = args_with_deployment_type(Some(label));
            let overrides = doctor_args_to_overrides(&args);
            let cfg = Config::load(None, &HashMap::new(), &overrides, None)
                .expect("load with synthetic args should not fail");
            assert_eq!(
                cfg.cluster.deployment_type,
                DeploymentType::parse(label),
                "expected --deployment-type={label} to resolve into cluster.deployment_type"
            );
        }
    }

    #[test]
    fn load_minimal_config_auto_flag_round_trips_to_unresolved() {
        let args = args_with_deployment_type(Some("auto"));
        let overrides = doctor_args_to_overrides(&args);
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert!(cfg.cluster.deployment_type.is_none());
    }

    #[test]
    fn load_minimal_config_no_flag_leaves_deployment_type_unresolved() {
        let args = args_with_deployment_type(None);
        let overrides = doctor_args_to_overrides(&args);
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert!(cfg.cluster.deployment_type.is_none());
    }
}
