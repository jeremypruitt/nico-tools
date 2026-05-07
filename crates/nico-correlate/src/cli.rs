use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct CorrelateArgs {
    /// Entity ID to correlate (workflow, host, DPU, or request ID)
    pub id: String,

    /// Override auto-detected ID type (workflow|host|dpu|request)
    #[arg(short = 't', long)]
    pub r#type: Option<String>,

    /// Restrict to specific sources (comma-separated: temporal,postgres,k8s,loki,redfish)
    #[arg(short = 's', long, value_delimiter = ',')]
    pub sources: Vec<String>,

    /// Limit log search to pods matching this pattern
    #[arg(long)]
    pub pod: Option<String>,

    /// Look-back window for log sources (e.g. 1h, 30m, 2h30m; default: 1h)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Output JSON
    #[arg(short = 'j', long)]
    pub json: bool,

    /// Stream new events after the initial dump until Ctrl-C (compatible with --json)
    #[arg(long)]
    pub tail: bool,

    /// ASCII-only output (no Unicode icons)
    #[arg(long)]
    pub ascii: bool,

    /// Disable color output
    #[arg(long)]
    pub no_color: bool,

    /// Color theme: default, dracula, nord, gruvbox
    #[arg(long, env = "NICO_THEME")]
    pub theme: Option<String>,

    /// Config file path (default: ~/.config/nico-tools/config.toml)
    #[arg(long, value_name = "PATH")]
    pub config: Option<String>,

    /// Reach mode: port-forward or in-cluster (default: auto-detect from KUBERNETES_SERVICE_HOST)
    #[arg(long, value_name = "MODE")]
    pub mode: Option<String>,

    /// Override bootstrap-step timeout budgets.
    /// Steps: kube_client, reach_api, preflight, port_forward, postgres_reach.
    /// Example: --timeouts kube_client=10s,port_forward=2s
    #[arg(long, value_name = "step=Xs,...")]
    pub timeouts: Option<String>,
}
