use clap::Parser;

#[derive(Parser)]
#[command(name = "nico-doctor", about = "Read-only health check for nico/ncx clusters")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
