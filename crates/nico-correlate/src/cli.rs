use clap::{Args, Subcommand};

#[derive(Args, Debug, Clone)]
pub struct CorrelateArgs {
    #[command(subcommand)]
    pub command: Option<CorrelateCommand>,

    /// Entity ID to correlate (workflow, host, DPU, or request ID).
    /// Required when no subcommand is given.
    pub id: Option<String>,

    /// Override auto-detected ID type (workflow|host|dpu|request)
    #[arg(short = 't', long, global = true)]
    pub r#type: Option<String>,

    /// Restrict to specific sources (comma-separated: temporal,postgres,k8s,loki,redfish)
    #[arg(short = 's', long, global = true, value_delimiter = ',')]
    pub sources: Vec<String>,

    /// Limit log search to pods matching this pattern
    #[arg(long, global = true)]
    pub pod: Option<String>,

    /// Look-back window for log sources (e.g. 1h, 30m, 2h30m; default: 1h)
    #[arg(long, global = true, default_value = "1h")]
    pub since: String,

    /// Output JSON
    #[arg(short = 'j', long, global = true)]
    pub json: bool,

    /// Stream new events after the initial dump until Ctrl-C (compatible with --json)
    #[arg(long, global = true)]
    pub tail: bool,

    /// ASCII-only output (no Unicode icons)
    #[arg(long, global = true)]
    pub ascii: bool,

    /// Disable color output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Color theme: default, dracula, nord, gruvbox
    #[arg(long, global = true, env = "NICO_THEME")]
    pub theme: Option<String>,

    /// Config file path (default: ~/.config/nico-tools/config.toml)
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<String>,

    /// Reach mode: port-forward or in-cluster (default: auto-detect from KUBERNETES_SERVICE_HOST)
    #[arg(long, global = true, value_name = "MODE")]
    pub mode: Option<String>,

    /// Postgres connection URL (overrides config file).
    #[arg(long, global = true, value_name = "URL")]
    pub postgres_url: Option<String>,

    /// Override bootstrap-step timeout budgets.
    /// Steps: kube_client, reach_api, preflight, port_forward, postgres_reach.
    /// Example: --timeouts kube_client=10s,port_forward=2s
    #[arg(long, global = true, value_name = "step=Xs,...")]
    pub timeouts: Option<String>,
}

/// Optional subcommand under `nico correlate`. When absent, correlate
/// runs the full multi-source timeline. When present, correlate runs a
/// focused single-purpose join and exits.
#[derive(Subcommand, Debug, Clone)]
pub enum CorrelateCommand {
    /// HBN config-drift correlation: per-axis applied-vs-desired,
    /// drift age, and overlapping probe alerts (issue #208).
    HbnConfigDrift(HbnConfigDriftArgs),
}

#[derive(Args, Debug, Clone)]
pub struct HbnConfigDriftArgs {
    /// Machine / DPU id to inspect.
    pub machine_id: String,

    /// Override the `last_seen_at` freshness threshold (default 90s).
    #[arg(long, value_name = "DURATION")]
    pub freshness: Option<String>,
}
