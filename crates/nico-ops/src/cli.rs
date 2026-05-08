use clap::{Args, Subcommand};

#[derive(Args, Debug, Clone)]
pub struct OpsArgs {
    #[command(subcommand)]
    pub command: Option<OpsCommand>,

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

    #[arg(
        long,
        value_name = "DURATION",
        help = "Auto-refresh interval (e.g. 5s, 1m). Overrides [output] tui_refresh and NICO_TUI_REFRESH; default 30s"
    )]
    pub interval: Option<String>,
}

impl Default for OpsArgs {
    fn default() -> Self {
        Self {
            command: None,
            namespace: None,
            context: None,
            skip: vec![],
            since: "10m".to_string(),
            timeout: "5s".to_string(),
            postgres_url: None,
            config: None,
            mode: None,
            theme: None,
            interval: None,
        }
    }
}

/// Optional subcommand under `nico ops`. When absent, ops runs the full
/// dashboard. When present, ops opens the focused per-target panel the
/// subcommand selects.
#[derive(Subcommand, Debug, Clone)]
pub enum OpsCommand {
    /// Per-DPU HBN panel — the at-a-glance view for a tenant-onboarding
    /// incident (issue #209).
    Hbn(HbnPanelArgs),
}

/// Args for `nico ops hbn`. Layout is auto-selected by terminal width
/// (Option A wide, Option B narrow); sort defaults to triage-first
/// (Quarantined > Unhealthy > Drift > Healthy).
#[derive(Args, Debug, Clone, Default)]
pub struct HbnPanelArgs {
    /// Sort by `status` (default — worst-first) or `machine` (alphabetical).
    #[arg(long, value_name = "COL", default_value = "status")]
    pub sort: String,
}

impl OpsArgs {
    /// Convert to `DoctorArgs` so `nico-doctor`'s bootstrap path can be
    /// reused without duplicating the cluster-targeting flag surface.
    /// Doctor-only flags (`--json`, `--verbose`, `--spotlight`,
    /// `--ascii`, `--no-color`) are forced to off.
    pub fn to_doctor_args(&self) -> nico_doctor::DoctorArgs {
        nico_doctor::DoctorArgs {
            command: None,
            namespace: self.namespace.clone(),
            context: self.context.clone(),
            skip: self.skip.clone(),
            since: self.since.clone(),
            timeout: self.timeout.clone(),
            json: false,
            verbose: false,
            spotlight: false,
            ascii: false,
            no_color: false,
            postgres_url: self.postgres_url.clone(),
            config: self.config.clone(),
            mode: self.mode.clone(),
            theme: self.theme.clone(),
            timeouts: None,
        }
    }
}
