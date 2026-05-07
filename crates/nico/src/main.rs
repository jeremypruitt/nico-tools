use clap::{Parser, Subcommand};
use std::process;

#[derive(Parser)]
#[command(
    name = "nico",
    about = "nico — diagnostic CLI for nico/carbide/ncx clusters",
    version,
    arg_required_else_help = false
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Live ops dashboard.
    Ops(nico_ops::OpsArgs),
    /// Read-only health check across cluster, logs, workflows, gRPC, postgres.
    Doctor(nico_doctor::DoctorArgs),
    /// Correlate every event for a given entity ID across all sources.
    Correlate(nico_correlate::CorrelateArgs),
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let code = match cli.command.unwrap_or_else(|| Command::Ops(nico_ops::OpsArgs::default())) {
        Command::Ops(args) => nico_ops::run_ops(args).await,
        Command::Doctor(args) => nico_doctor::run_doctor(args).await,
        Command::Correlate(args) => nico_correlate::run_correlate(args).await,
    };

    process::exit(code);
}
