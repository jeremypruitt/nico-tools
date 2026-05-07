use std::sync::Arc;

use nico_common::config::OutputFormat;
use nico_common::output::Status;
use nico_common::theme;

pub mod baseline;
pub mod bootstrap;
pub mod cli;
pub mod formatter;
pub mod grpc;
pub mod http;
pub mod layer;
pub mod layers;
pub mod log_source;
pub mod loki;
pub mod postgres;
pub mod preflight;
pub mod runner;

pub use bootstrap::{bootstrap, prepare_layers, Bootstrapped, BootstrapErr, LayerInputs};
pub use cli::DoctorArgs;
pub use runner::Report;

/// Run all layers once, returning a [`Report`].
pub async fn run_once(layers: &[Box<dyn layer::Layer>], opts: &layer::RunOpts) -> Report {
    runner::run(layers, opts).await
}

/// Run all layers and stream each [`layer::LayerResult`] as it completes.
///
/// Layers run concurrently with the same per-layer timeout policy as
/// [`run_once`]. When a layer times out, an `Unknown` result is sent.
/// The channel is closed when all layers have reported.
pub async fn run_streaming(
    layers: Arc<Vec<Box<dyn layer::Layer>>>,
    opts: layer::RunOpts,
    tx: tokio::sync::mpsc::Sender<layer::LayerResult>,
) {
    use futures::stream::{FuturesUnordered, StreamExt};

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

    let bootstrapped = match bootstrap(&args).await {
        Ok(b) => b,
        Err(BootstrapErr::Preflight { human_message, json_payload, format }) => {
            if matches!(format, OutputFormat::Json) {
                println!("{}", json_payload);
            } else {
                eprintln!("{}", human_message);
            }
            return 3;
        }
        Err(BootstrapErr::Fatal { message, code }) => {
            eprintln!("{}", message);
            return code;
        }
    };

    let baseline_prior = baseline::load();

    let report = run_once(&bootstrapped.layers, &bootstrapped.opts).await;

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
