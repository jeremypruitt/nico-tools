use std::process;
use std::time::Duration;
use clap::Parser;
use nico_common::output::{OutputMode, Status};

mod grpc;
mod http;
mod k8s;
mod layer;
mod layers;
mod loki;
mod runner;
mod temporal;

use layer::RunOpts;
use runner::Report;

#[derive(Parser)]
#[command(name = "nico-doctor", about = "Read-only health check for nico/ncx clusters")]
struct Cli {
    #[arg(short, long, help = "Kubernetes namespace", default_value = "nico")]
    namespace: String,

    #[arg(long, help = "Kubernetes context [env: NICO_CONTEXT]")]
    context: Option<String>,

    #[arg(long, value_delimiter = ',', help = "Layers to skip")]
    skip: Vec<String>,

    #[arg(long, default_value = "10m", help = "Look-back window for logs/events")]
    since: String,

    #[arg(long, default_value = "5s", help = "Per-check timeout")]
    timeout: String,

    #[arg(short, long, help = "Output JSON")]
    json: bool,

    #[arg(short, long, help = "Show details for passing checks")]
    verbose: bool,

    #[arg(long, help = "ASCII-only output")]
    ascii: bool,

    #[arg(long, help = "Disable color output")]
    no_color: bool,
}

fn print_report(report: &Report, mode: &OutputMode) {
    for layer in &report.layers {
        let icon = layer.status.icon(mode);
        let styled_icon = layer.status.style(icon, mode);
        let summary = layer.checks.iter()
            .map(|c| c.value.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {styled_icon} {:<12} {summary}", layer.name);
    }

    let has_findings = report.layers.iter().any(|l| {
        l.checks.iter().any(|c| c.status != Status::Ok)
    });

    if has_findings {
        println!();
        for layer in &report.layers {
            let bad: Vec<_> = layer.checks.iter()
                .filter(|c| c.status != Status::Ok)
                .collect();
            if bad.is_empty() { continue; }
            println!("{}:", layer.name);
            for check in bad {
                println!("  • {} ({})", check.value, check.name);
                if let Some(cmd) = &check.next_command {
                    println!("    → {cmd}");
                }
            }
        }
    }

    println!();
    let status = report.summary_status();
    let icon = status.icon(mode);
    let warn_count = report.layers.iter()
        .flat_map(|l| &l.checks)
        .filter(|c| c.status == Status::Warn)
        .count();
    let fail_count = report.layers.iter()
        .flat_map(|l| &l.checks)
        .filter(|c| c.status == Status::Fail)
        .count();
    println!("Summary: {icon}  {warn_count} warnings, {fail_count} failures");
    println!("Hint: --verbose for details on passing checks, --json for machine output");
}

fn exit_code(report: &Report) -> i32 {
    match report.summary_status() {
        Status::Ok | Status::Skipped => 0,
        Status::Warn => 1,
        Status::Fail => 2,
        Status::Unknown => 3,
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let mode = OutputMode {
        color: !cli.no_color && std::env::var("NO_COLOR").is_err(),
        ascii: cli.ascii,
    };

    let since = humantime::parse_duration(&cli.since).unwrap_or(Duration::from_secs(600));
    let timeout = humantime::parse_duration(&cli.timeout).unwrap_or(Duration::from_secs(5));

    let opts = RunOpts { namespace: cli.namespace.clone(), since, timeout };

    // TODO(#3): wire real k8s client; for now exit cleanly with no layers
    let layers: Vec<Box<dyn layer::Layer>> = vec![];
    let report = runner::run(&layers, &opts).await;

    if cli.json {
        println!("{}", serde_json::json!({
            "version": 1,
            "namespace": cli.namespace,
            "summary": {
                "ok": report.layers.iter().filter(|l| l.status == Status::Ok).count(),
                "warn": report.layers.iter().filter(|l| l.status == Status::Warn).count(),
                "fail": report.layers.iter().filter(|l| l.status == Status::Fail).count(),
                "unknown": report.layers.iter().filter(|l| l.status == Status::Unknown).count(),
            },
            "layers": report.layers.iter().map(|l| serde_json::json!({
                "name": l.name,
                "status": format!("{:?}", l.status).to_lowercase(),
                "duration_ms": l.duration_ms,
                "checks": l.checks.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "status": format!("{:?}", c.status).to_lowercase(),
                    "value": c.value,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }));
    } else {
        print_report(&report, &mode);
    }

    process::exit(exit_code(&report));
}
