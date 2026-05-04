use clap::Parser;

#[derive(Parser)]
#[command(name = "nico-correlate", about = "Correlate all events for a given entity ID")]
struct Cli {
    /// Entity ID to correlate (workflow, host, DPU, tenant, or request ID)
    id: Option<String>,
}

fn main() {
    let _cli = Cli::parse();
}
