use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    #[arg(short, long, help = "Kubernetes namespace")]
    pub namespace: Option<String>,

    #[arg(long, help = "Kubernetes context")]
    pub context: Option<String>,

    #[arg(long, value_delimiter = ',', help = "Layers to skip")]
    pub skip: Vec<String>,

    #[arg(long, default_value = "10m", help = "Look-back window for logs/events")]
    pub since: String,

    #[arg(long, default_value = "5s", help = "Per-check timeout")]
    pub timeout: String,

    #[arg(
        long,
        value_name = "step=Xs,...",
        help = "Override bootstrap-step timeout budgets. \
                Steps: kube_client, reach_api, preflight, port_forward, postgres_reach. \
                Example: --timeouts kube_client=10s,port_forward=2s"
    )]
    pub timeouts: Option<String>,

    #[arg(short, long, help = "Output JSON")]
    pub json: bool,

    #[arg(short, long, help = "Show details for passing checks")]
    pub verbose: bool,

    #[arg(long, help = "Show only layers with warn/fail status or a new/fixed delta badge")]
    pub spotlight: bool,

    #[arg(long, help = "ASCII-only output")]
    pub ascii: bool,

    #[arg(long, help = "Disable color output")]
    pub no_color: bool,

    #[arg(long, help = "Postgres connection URL")]
    pub postgres_url: Option<String>,

    #[arg(long, value_name = "PATH", help = "Config file path (default: ~/.config/nico-tools/config.toml)")]
    pub config: Option<String>,

    #[arg(
        long,
        value_name = "MODE",
        help = "Reach mode: port-forward or in-cluster (default: auto-detect from KUBERNETES_SERVICE_HOST)"
    )]
    pub mode: Option<String>,

    #[arg(long, env = "NICO_THEME", value_name = "NAME", help = "Color theme: default, dracula, nord, gruvbox")]
    pub theme: Option<String>,
}
