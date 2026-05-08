use clap::{Args, Subcommand};

#[derive(Args, Debug, Clone)]
pub struct DoctorArgs {
    #[command(subcommand)]
    pub command: Option<DoctorCommand>,


    #[arg(short, long, global = true, help = "Kubernetes namespace")]
    pub namespace: Option<String>,

    #[arg(long, global = true, help = "Kubernetes context")]
    pub context: Option<String>,

    #[arg(long, global = true, value_delimiter = ',', help = "Layers to skip")]
    pub skip: Vec<String>,

    #[arg(long, default_value = "10m", global = true, help = "Look-back window for logs/events")]
    pub since: String,

    #[arg(long, default_value = "5s", global = true, help = "Per-check timeout")]
    pub timeout: String,

    #[arg(
        long,
        global = true,
        value_name = "step=Xs,...",
        help = "Override bootstrap-step timeout budgets. \
                Steps: kube_client, reach_api, preflight, port_forward, postgres_reach. \
                Example: --timeouts kube_client=10s,port_forward=2s"
    )]
    pub timeouts: Option<String>,

    #[arg(short, long, global = true, help = "Output JSON")]
    pub json: bool,

    #[arg(short, long, global = true, help = "Show details for passing checks")]
    pub verbose: bool,

    #[arg(long, global = true, help = "Show only layers with warn/fail status or a new/fixed delta badge")]
    pub spotlight: bool,

    #[arg(long, global = true, help = "ASCII-only output")]
    pub ascii: bool,

    #[arg(long, global = true, help = "Disable color output")]
    pub no_color: bool,

    #[arg(long, global = true, help = "Postgres connection URL")]
    pub postgres_url: Option<String>,

    #[arg(long, global = true, value_name = "PATH", help = "Config file path (default: ~/.config/nico-tools/config.toml)")]
    pub config: Option<String>,

    #[arg(
        long,
        global = true,
        value_name = "MODE",
        help = "Reach mode: port-forward or in-cluster (default: auto-detect from KUBERNETES_SERVICE_HOST)"
    )]
    pub mode: Option<String>,

    #[arg(long, global = true, env = "NICO_THEME", value_name = "NAME", help = "Color theme: default, dracula, nord, gruvbox")]
    pub theme: Option<String>,
}

/// Optional subcommand under `nico doctor`. When absent, doctor runs the
/// full multi-layer ladder (cluster, logs, workflows, health, grpc,
/// postgres). When present, doctor runs the focused single-target check
/// the subcommand specifies.
#[derive(Subcommand, Debug, Clone)]
pub enum DoctorCommand {
    /// Single-DPU HBN (Host-Based Networking) verdict (issue #205).
    ///
    /// Resolves the given DPU ID to its most recent
    /// `DpuNetworkStatus` row plus desired-config peer in forgedb,
    /// runs the fixed HBN check set, and renders a headline-vs-detail
    /// report. Read-only.
    Hbn(HbnArgs),

    /// Single-DPU isolation verdict (issue #207).
    ///
    /// "DPU has no traffic" has three very different root causes —
    /// `not-yet-known`, `quarantined`, and `lost-connection`. This
    /// command does that triage in one step, reading the machine
    /// registration row, scout-discovery state,
    /// `MachineQuarantineState`, and most recent
    /// `DpuNetworkStatus.last_seen_at` to pick exactly one verdict
    /// (or `healthy`).
    DpuIsolation(DpuIsolationArgs),

    /// Single-DPU client-certificate days-to-expiry verdict (issue
    /// #206).
    ///
    /// Reads `client_certificate_expiry_unix_epoch_secs` from the
    /// most recent `DpuNetworkStatus` row and reports `expired`,
    /// `expiring-soon`, `healthy`, or `no-recent-status`. Default
    /// warning threshold is 30 days.
    DpuCert(DpuCertArgs),
}

#[derive(Args, Debug, Clone)]
pub struct HbnArgs {
    /// DPU ID to inspect.
    pub dpu_id: String,

    /// Override the `last_seen_at` freshness threshold (default 90s).
    #[arg(long, value_name = "DURATION")]
    pub freshness: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DpuIsolationArgs {
    /// Machine ID to inspect.
    pub machine_id: String,

    /// Override the `last_seen_at` freshness threshold (default 90s).
    #[arg(long, value_name = "DURATION")]
    pub freshness: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DpuCertArgs {
    /// DPU ID to inspect.
    pub dpu_id: String,

    /// Override the cert-expiry warning window (default 30 days).
    /// Accepts any humantime duration, e.g. `7d`, `336h`, `60m`.
    #[arg(long, value_name = "DURATION")]
    pub warn: Option<String>,
}
