use std::collections::HashMap;
use std::time::Duration;
use anyhow::{Context, Result};
use serde::Deserialize;

pub struct Config {
    pub cluster: ClusterConfig,
    pub postgres: PostgresConfig,
    pub temporal: TemporalConfig,
    pub output: OutputConfig,
}

pub struct ClusterConfig {
    pub context: Option<String>,
    pub namespace: String,
    pub reach_mode: ReachMode,
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
}

/// Intermediate deserialization shape — all fields optional so missing fields fall back to defaults.
#[derive(Deserialize, Default)]
struct FileConfig {
    cluster: Option<FileClusterConfig>,
    postgres: Option<FilePostgresConfig>,
    temporal: Option<FileTemporalConfig>,
    output: Option<FileOutputConfig>,
}

#[derive(Deserialize, Default)]
struct FileClusterConfig {
    context: Option<String>,
    namespace: Option<String>,
    reach_mode: Option<String>,
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

        // Env var layer — overrides file values
        let context = env.get("NICO_CONTEXT").cloned().or(cluster.context);
        let namespace = env.get("NICO_NAMESPACE").cloned()
            .or(cluster.namespace)
            .unwrap_or_else(|| "nico".into());
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
            cluster: ClusterConfig { context, namespace, reach_mode },
            postgres: PostgresConfig { url: postgres_url },
            temporal: TemporalConfig {
                address: temporal_address,
                namespace: temporal_namespace,
                stuck_threshold,
            },
            output: OutputConfig { color, format, tui_refresh },
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
}
