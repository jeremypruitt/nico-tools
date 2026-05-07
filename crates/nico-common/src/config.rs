use std::collections::HashMap;
use std::time::Duration;
use anyhow::{Context, Result};
use serde::Deserialize;

pub struct Config {
    pub cluster: ClusterConfig,
    pub postgres: PostgresConfig,
    pub temporal: TemporalConfig,
    pub output: OutputConfig,
    pub bootstrap: BootstrapConfig,
}

pub struct BootstrapConfig {
    pub timeouts: BootstrapTimeouts,
}

/// Per-step timeout budgets for every awaitable operation in the nico
/// bootstrap path (see ADR-0013 and issue #171). Each duration is a hard
/// upper bound; exceeding it surfaces a `TimedOut` error distinguishable
/// from a non-timeout failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapTimeouts {
    /// Building `kube::Client` from kubeconfig.
    pub kube_client: Duration,
    /// API-server reachability probe (`apiserver_version()`).
    pub reach_api: Duration,
    /// Each preflight sub-check (token, namespace, RBAC).
    pub preflight: Duration,
    /// Each port-forward setup (Temporal, Postgres, Loki, HTTP discovery).
    pub port_forward: Duration,
    /// Postgres reachability probe (TCP connect + handshake).
    pub postgres_reach: Duration,
}

impl Default for BootstrapTimeouts {
    fn default() -> Self {
        Self {
            kube_client: Duration::from_secs(5),
            reach_api: Duration::from_secs(5),
            preflight: Duration::from_secs(5),
            port_forward: Duration::from_secs(3),
            postgres_reach: Duration::from_secs(2),
        }
    }
}

impl BootstrapTimeouts {
    /// Apply a comma-separated list of `step=duration` overrides on top
    /// of `self`. Recognized step names: `kube_client`, `reach_api`,
    /// `preflight`, `port_forward`, `postgres_reach`.
    pub fn apply_overrides(&mut self, spec: &str) -> Result<()> {
        for entry in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, val) = entry.split_once('=').ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid timeouts entry {entry:?}; expected step=duration"
                )
            })?;
            let dur = humantime::parse_duration(val.trim())
                .with_context(|| format!("invalid duration in {entry:?}"))?;
            self.set_step(key.trim(), dur)?;
        }
        Ok(())
    }

    fn set_step(&mut self, name: &str, dur: Duration) -> Result<()> {
        match name {
            "kube_client" => self.kube_client = dur,
            "reach_api" => self.reach_api = dur,
            "preflight" => self.preflight = dur,
            "port_forward" => self.port_forward = dur,
            "postgres_reach" => self.postgres_reach = dur,
            other => {
                return Err(anyhow::anyhow!(
                    "unknown bootstrap timeout step {other:?}; \
                     valid: kube_client, reach_api, preflight, port_forward, postgres_reach"
                ));
            }
        }
        Ok(())
    }
}

pub struct ClusterConfig {
    pub context: Option<String>,
    pub namespace: String,
    pub postgres_namespace: String,
    pub reach_mode: ReachMode,
    pub grpc_address: Option<String>,
}

pub struct PostgresConfig {
    pub url: String,
}

pub struct TemporalConfig {
    pub address: String,
    pub namespace: String,
    pub stuck_threshold: Duration,
}

pub struct OutputConfig {
    pub color: ColorMode,
    pub format: OutputFormat,
    pub tui_refresh: Duration,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ColorMode { Auto, Always, Never }

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum OutputFormat { Human, Json }

/// How the tools reach cluster services.
///
/// Auto-detected from `KUBERNETES_SERVICE_HOST`: present → InCluster, absent → PortForward.
/// Override with `--mode port-forward|in-cluster` or `reach_mode` in `[cluster]` of config.toml.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum ReachMode {
    /// Open in-process kube port-forwards for each service (laptop / CI with kubeconfig).
    PortForward,
    /// Use cluster-DNS `<svc>.<ns>.svc.cluster.local` directly (debug pod / in-cluster).
    InCluster,
}

impl ReachMode {
    pub fn auto_detect(env: &HashMap<String, String>) -> Self {
        if env.contains_key("KUBERNETES_SERVICE_HOST") {
            Self::InCluster
        } else {
            Self::PortForward
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PortForward => "port-forward",
            Self::InCluster => "in-cluster",
        }
    }
}

#[derive(Default)]
pub struct ConfigOverrides {
    pub context: Option<String>,
    pub namespace: Option<String>,
    pub postgres_url: Option<String>,
    pub temporal_address: Option<String>,
    pub temporal_namespace: Option<String>,
    pub stuck_threshold: Option<Duration>,
    pub color: Option<ColorMode>,
    pub format: Option<OutputFormat>,
    pub reach_mode: Option<ReachMode>,
    pub tui_refresh: Option<Duration>,
    /// CLI `--timeouts step=Xs,...` spec, applied last (highest precedence).
    pub bootstrap_timeouts_spec: Option<String>,
}

/// Intermediate deserialization shape — all fields optional so missing fields fall back to defaults.
#[derive(Deserialize, Default)]
struct FileConfig {
    cluster: Option<FileClusterConfig>,
    postgres: Option<FilePostgresConfig>,
    temporal: Option<FileTemporalConfig>,
    output: Option<FileOutputConfig>,
    bootstrap: Option<FileBootstrapConfig>,
}

#[derive(Deserialize, Default)]
struct FileBootstrapConfig {
    timeouts: Option<FileBootstrapTimeouts>,
}

#[derive(Deserialize, Default)]
struct FileBootstrapTimeouts {
    kube_client: Option<String>,
    reach_api: Option<String>,
    preflight: Option<String>,
    port_forward: Option<String>,
    postgres_reach: Option<String>,
}

#[derive(Deserialize, Default)]
struct FileClusterConfig {
    context: Option<String>,
    namespace: Option<String>,
    postgres_namespace: Option<String>,
    reach_mode: Option<String>,
    grpc_address: Option<String>,
}

#[derive(Deserialize, Default)]
struct FilePostgresConfig {
    url: Option<String>,
}

#[derive(Deserialize, Default)]
struct FileTemporalConfig {
    address: Option<String>,
    namespace: Option<String>,
    stuck_threshold: Option<String>,
}

#[derive(Deserialize, Default)]
struct FileOutputConfig {
    color: Option<String>,
    format: Option<String>,
    tui_refresh: Option<String>,
}

impl Config {
    pub fn load(
        file_toml: Option<&str>,
        env: &HashMap<String, String>,
        overrides: &ConfigOverrides,
    ) -> Result<Config> {
        let file: FileConfig = match file_toml {
            Some(s) => toml::from_str(s).context("failed to parse config file")?,
            None => FileConfig::default(),
        };

        let cluster = file.cluster.unwrap_or_default();
        let postgres = file.postgres.unwrap_or_default();
        let temporal = file.temporal.unwrap_or_default();
        let output = file.output.unwrap_or_default();
        let bootstrap_file = file.bootstrap.unwrap_or_default();

        // Env var layer — overrides file values
        let context = env.get("NICO_CONTEXT").cloned().or(cluster.context);
        let namespace = env.get("NICO_NAMESPACE").cloned()
            .or(cluster.namespace)
            .unwrap_or_else(|| "nico".into());
        let postgres_namespace = env.get("NICO_POSTGRES_NAMESPACE").cloned()
            .or(cluster.postgres_namespace)
            .unwrap_or_else(|| "postgres".into());
        let postgres_url = env.get("NICO_POSTGRES_URL").cloned()
            .or(postgres.url)
            .unwrap_or_else(|| "postgres://nico:nico@localhost:5432/nico".into());
        let temporal_address = env.get("NICO_TEMPORAL_ADDRESS").cloned()
            .or(temporal.address)
            .unwrap_or_else(|| "localhost:7233".into());
        let temporal_namespace = env.get("NICO_TEMPORAL_NAMESPACE").cloned()
            .or(temporal.namespace)
            .unwrap_or_else(|| "default".into());

        let stuck_threshold_str = env.get("NICO_STUCK_THRESHOLD").cloned()
            .or(temporal.stuck_threshold);
        let stuck_threshold = match stuck_threshold_str.as_deref() {
            Some(s) => humantime::parse_duration(s)
                .context(format!("invalid stuck_threshold {:?}", s))?,
            None => Duration::from_secs(30 * 60),
        };

        let color = match output.color.as_deref() {
            Some("always") => ColorMode::Always,
            Some("never") => ColorMode::Never,
            _ => ColorMode::Auto,
        };

        let format = match output.format.as_deref() {
            Some("json") => OutputFormat::Json,
            _ => OutputFormat::Human,
        };

        let tui_refresh_str = env.get("NICO_TUI_REFRESH").cloned().or(output.tui_refresh);
        let tui_refresh = match tui_refresh_str.as_deref() {
            Some(s) => humantime::parse_duration(s)
                .context(format!("invalid tui_refresh {:?}", s))?,
            None => Duration::from_secs(30),
        };

        let reach_mode_str = env.get("NICO_REACH_MODE").cloned()
            .or(cluster.reach_mode);
        let reach_mode = match reach_mode_str.as_deref() {
            Some("port-forward") => ReachMode::PortForward,
            Some("in-cluster") => ReachMode::InCluster,
            Some(other) => return Err(anyhow::anyhow!(
                "invalid reach_mode {:?}; use port-forward or in-cluster", other
            )),
            None => ReachMode::auto_detect(env),
        };

        let grpc_address = env.get("NICO_GRPC_ADDRESS").cloned().or(cluster.grpc_address);

        // Bootstrap timeouts — defaults < file < env < CLI override spec.
        let mut timeouts = BootstrapTimeouts::default();
        let file_t = bootstrap_file.timeouts.unwrap_or_default();
        for (name, val) in [
            ("kube_client", file_t.kube_client),
            ("reach_api", file_t.reach_api),
            ("preflight", file_t.preflight),
            ("port_forward", file_t.port_forward),
            ("postgres_reach", file_t.postgres_reach),
        ] {
            if let Some(s) = val {
                let dur = humantime::parse_duration(&s).with_context(|| {
                    format!("invalid bootstrap.timeouts.{name} = {s:?}")
                })?;
                timeouts.set_step(name, dur)?;
            }
        }
        for (name, env_key) in [
            ("kube_client", "NICO_TIMEOUT_KUBE_CLIENT"),
            ("reach_api", "NICO_TIMEOUT_REACH_API"),
            ("preflight", "NICO_TIMEOUT_PREFLIGHT"),
            ("port_forward", "NICO_TIMEOUT_PORT_FORWARD"),
            ("postgres_reach", "NICO_TIMEOUT_POSTGRES_REACH"),
        ] {
            if let Some(s) = env.get(env_key) {
                let dur = humantime::parse_duration(s).with_context(|| {
                    format!("invalid {env_key} = {s:?}")
                })?;
                timeouts.set_step(name, dur)?;
            }
        }
        if let Some(spec) = overrides.bootstrap_timeouts_spec.as_deref() {
            timeouts.apply_overrides(spec)?;
        }

        // Flag override layer — highest precedence
        let context = overrides.context.clone().or(context);
        let namespace = overrides.namespace.clone().unwrap_or(namespace);
        let postgres_url = overrides.postgres_url.clone().unwrap_or(postgres_url);
        let temporal_address = overrides.temporal_address.clone().unwrap_or(temporal_address);
        let temporal_namespace = overrides.temporal_namespace.clone().unwrap_or(temporal_namespace);
        let stuck_threshold = overrides.stuck_threshold.unwrap_or(stuck_threshold);
        let color = overrides.color.unwrap_or(color);
        let format = overrides.format.unwrap_or(format);
        let reach_mode = overrides.reach_mode.unwrap_or(reach_mode);
        let tui_refresh = overrides.tui_refresh.unwrap_or(tui_refresh);

        Ok(Config {
            cluster: ClusterConfig { context, namespace, postgres_namespace, reach_mode, grpc_address },
            postgres: PostgresConfig { url: postgres_url },
            temporal: TemporalConfig {
                address: temporal_address,
                namespace: temporal_namespace,
                stuck_threshold,
            },
            output: OutputConfig { color, format, tui_refresh },
            bootstrap: BootstrapConfig { timeouts },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_overrides_env() {
        let mut env = HashMap::new();
        env.insert("NICO_NAMESPACE".to_string(), "from-env".to_string());
        env.insert("NICO_POSTGRES_URL".to_string(), "postgres://env/db".to_string());
        let overrides = ConfigOverrides {
            namespace: Some("from-flag".to_string()),
            postgres_url: Some("postgres://flag/db".to_string()),
            ..Default::default()
        };
        let config = Config::load(None, &env, &overrides).unwrap();
        assert_eq!(config.cluster.namespace, "from-flag");
        assert_eq!(config.postgres.url, "postgres://flag/db");
        // env value wins where no flag override exists
        assert_eq!(config.temporal.address, "localhost:7233");
    }

    #[test]
    fn env_overrides_file() {
        let toml = "[cluster]\nnamespace = \"from-file\"";
        let mut env = HashMap::new();
        env.insert("NICO_NAMESPACE".to_string(), "from-env".to_string());
        env.insert("NICO_POSTGRES_URL".to_string(), "postgres://env:pw@host:5432/db".to_string());
        env.insert("NICO_CONTEXT".to_string(), "env-context".to_string());
        env.insert("NICO_TEMPORAL_ADDRESS".to_string(), "env-temporal:7233".to_string());
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.namespace, "from-env");
        assert_eq!(config.cluster.context.as_deref(), Some("env-context"));
        assert_eq!(config.postgres.url, "postgres://env:pw@host:5432/db");
        assert_eq!(config.temporal.address, "env-temporal:7233");
    }

    #[test]
    fn file_overrides_defaults() {
        let toml = r#"
[cluster]
namespace = "prod"
context = "prod-ctx"
[postgres]
url = "postgres://prod:secret@db:5432/prod"
"#;
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.namespace, "prod");
        assert_eq!(config.cluster.context.as_deref(), Some("prod-ctx"));
        assert_eq!(config.postgres.url, "postgres://prod:secret@db:5432/prod");
        // untouched field still has default
        assert_eq!(config.temporal.address, "localhost:7233");
    }

    #[test]
    fn defaults_when_no_sources() {
        let config = Config::load(None, &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.namespace, "nico");
        assert!(config.cluster.context.is_none());
        assert_eq!(config.postgres.url, "postgres://nico:nico@localhost:5432/nico");
        assert_eq!(config.temporal.address, "localhost:7233");
        assert_eq!(config.temporal.namespace, "default");
        assert_eq!(config.temporal.stuck_threshold, Duration::from_secs(30 * 60));
        assert_eq!(config.output.color, ColorMode::Auto);
        assert_eq!(config.output.format, OutputFormat::Human);
        assert_eq!(config.output.tui_refresh, Duration::from_secs(30));
        // no KUBERNETES_SERVICE_HOST in test env → PortForward
        assert_eq!(config.cluster.reach_mode, ReachMode::PortForward);
    }

    #[test]
    fn reach_mode_env_override() {
        let mut env = HashMap::new();
        env.insert("NICO_REACH_MODE".to_string(), "in-cluster".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn reach_mode_flag_override() {
        let overrides = ConfigOverrides {
            reach_mode: Some(ReachMode::InCluster),
            ..Default::default()
        };
        let config = Config::load(None, &HashMap::new(), &overrides).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn reach_mode_file_override() {
        let toml = "[cluster]\nreach_mode = \"in-cluster\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn kubernetes_service_host_selects_in_cluster() {
        let mut env = HashMap::new();
        env.insert("KUBERNETES_SERVICE_HOST".to_string(), "10.0.0.1".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn invalid_reach_mode_errors() {
        let mut env = HashMap::new();
        env.insert("NICO_REACH_MODE".to_string(), "bogus".to_string());
        assert!(Config::load(None, &env, &ConfigOverrides::default()).is_err());
    }

    #[test]
    fn tui_refresh_from_file() {
        let toml = "[output]\ntui_refresh = \"10s\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.output.tui_refresh, Duration::from_secs(10));
    }

    #[test]
    fn tui_refresh_env_overrides_file() {
        let toml = "[output]\ntui_refresh = \"10s\"";
        let mut env = HashMap::new();
        env.insert("NICO_TUI_REFRESH".to_string(), "20s".to_string());
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.output.tui_refresh, Duration::from_secs(20));
    }

    #[test]
    fn tui_refresh_flag_overrides_env_and_file() {
        let toml = "[output]\ntui_refresh = \"10s\"";
        let mut env = HashMap::new();
        env.insert("NICO_TUI_REFRESH".to_string(), "20s".to_string());
        let overrides = ConfigOverrides {
            tui_refresh: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let config = Config::load(Some(toml), &env, &overrides).unwrap();
        assert_eq!(config.output.tui_refresh, Duration::from_secs(5));
    }

    #[test]
    fn grpc_address_defaults_to_none() {
        let config = Config::load(None, &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert!(config.cluster.grpc_address.is_none());
    }

    #[test]
    fn grpc_address_from_env() {
        let mut env = HashMap::new();
        env.insert("NICO_GRPC_ADDRESS".to_string(), "carbide-api:1079".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.grpc_address.as_deref(), Some("carbide-api:1079"));
    }

    #[test]
    fn grpc_address_from_file() {
        let toml = "[cluster]\ngrpc_address = \"carbide-api:1079\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.grpc_address.as_deref(), Some("carbide-api:1079"));
    }

    #[test]
    fn grpc_address_env_overrides_file() {
        let toml = "[cluster]\ngrpc_address = \"from-file:1079\"";
        let mut env = HashMap::new();
        env.insert("NICO_GRPC_ADDRESS".to_string(), "from-env:1079".to_string());
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.grpc_address.as_deref(), Some("from-env:1079"));
    }

    #[test]
    fn postgres_namespace_defaults_to_postgres() {
        let config = Config::load(None, &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.postgres_namespace, "postgres");
    }

    #[test]
    fn postgres_namespace_from_env() {
        let mut env = HashMap::new();
        env.insert("NICO_POSTGRES_NAMESPACE".to_string(), "db-tier".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.postgres_namespace, "db-tier");
    }

    #[test]
    fn postgres_namespace_from_file() {
        let toml = "[cluster]\npostgres_namespace = \"data\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.postgres_namespace, "data");
    }

    #[test]
    fn postgres_namespace_env_overrides_file() {
        let toml = "[cluster]\npostgres_namespace = \"from-file\"";
        let mut env = HashMap::new();
        env.insert("NICO_POSTGRES_NAMESPACE".to_string(), "from-env".to_string());
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(config.cluster.postgres_namespace, "from-env");
    }

    #[test]
    fn bootstrap_timeouts_defaults_match_adr_0013_table() {
        let t = BootstrapTimeouts::default();
        assert_eq!(t.kube_client, Duration::from_secs(5));
        assert_eq!(t.reach_api, Duration::from_secs(5));
        assert_eq!(t.preflight, Duration::from_secs(5));
        assert_eq!(t.port_forward, Duration::from_secs(3));
        assert_eq!(t.postgres_reach, Duration::from_secs(2));
    }

    #[test]
    fn bootstrap_timeouts_loaded_from_file() {
        let toml = r#"
[bootstrap.timeouts]
kube_client = "9s"
preflight = "2s"
port_forward = "1s"
"#;
        let cfg = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default()).unwrap();
        assert_eq!(cfg.bootstrap.timeouts.kube_client, Duration::from_secs(9));
        assert_eq!(cfg.bootstrap.timeouts.preflight, Duration::from_secs(2));
        assert_eq!(cfg.bootstrap.timeouts.port_forward, Duration::from_secs(1));
        // unspecified fields keep defaults
        assert_eq!(cfg.bootstrap.timeouts.reach_api, Duration::from_secs(5));
        assert_eq!(cfg.bootstrap.timeouts.postgres_reach, Duration::from_secs(2));
    }

    #[test]
    fn bootstrap_timeouts_env_overrides_file() {
        let toml = "[bootstrap.timeouts]\npreflight = \"7s\"";
        let mut env = HashMap::new();
        env.insert("NICO_TIMEOUT_PREFLIGHT".to_string(), "1s".to_string());
        let cfg = Config::load(Some(toml), &env, &ConfigOverrides::default()).unwrap();
        assert_eq!(cfg.bootstrap.timeouts.preflight, Duration::from_secs(1));
    }

    #[test]
    fn bootstrap_timeouts_cli_spec_overrides_env_and_file() {
        let toml = "[bootstrap.timeouts]\npreflight = \"7s\"";
        let mut env = HashMap::new();
        env.insert("NICO_TIMEOUT_PREFLIGHT".to_string(), "6s".to_string());
        let overrides = ConfigOverrides {
            bootstrap_timeouts_spec: Some("preflight=500ms,port_forward=250ms".to_string()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &env, &overrides).unwrap();
        assert_eq!(cfg.bootstrap.timeouts.preflight, Duration::from_millis(500));
        assert_eq!(cfg.bootstrap.timeouts.port_forward, Duration::from_millis(250));
    }

    #[test]
    fn bootstrap_timeouts_unknown_step_in_spec_errors() {
        let overrides = ConfigOverrides {
            bootstrap_timeouts_spec: Some("nonsense=1s".to_string()),
            ..Default::default()
        };
        let err = Config::load(None, &HashMap::new(), &overrides).err().expect("expected err");
        let msg = format!("{err:#}");
        assert!(msg.contains("nonsense"), "msg = {msg}");
    }

    #[test]
    fn bootstrap_timeouts_invalid_duration_in_spec_errors() {
        let overrides = ConfigOverrides {
            bootstrap_timeouts_spec: Some("preflight=NOTADURATION".to_string()),
            ..Default::default()
        };
        assert!(Config::load(None, &HashMap::new(), &overrides).is_err());
    }

    #[test]
    fn grpc_address_independent_from_temporal_address() {
        let mut env = HashMap::new();
        env.insert("NICO_TEMPORAL_ADDRESS".to_string(), "temporal:7233".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default()).unwrap();
        assert!(config.cluster.grpc_address.is_none());
        assert_eq!(config.temporal.address, "temporal:7233");
    }
}
