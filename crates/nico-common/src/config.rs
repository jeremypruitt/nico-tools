use std::collections::HashMap;
use std::fmt;
use std::time::Duration;
use anyhow::{Context, Result};
use serde::Deserialize;

/// Capability-based deployment-type label. α-flat shape: detection
/// resolves to one of the three real shapes, and `Force` is the escape
/// hatch when the operator wants to skip detection entirely. See
/// PRD-001 (`docs/prds/001-deployment-type.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeploymentType {
    /// Full stack: core + rest both deployed (`forge-system` controller ns,
    /// `carbide-api:1079` gRPC, forgedb present).
    Full,
    /// Core-only: carbide-kind without rest (`forge-system` ns, same gRPC,
    /// forgedb present).
    CoreOnly,
    /// Rest-only with mock-core stand-in (`nico-rest` ns,
    /// `nico-rest-mock-core:11079` gRPC, no forgedb).
    RestOnlyMock,
    /// Escape hatch: trust the user's raw config; no detection, no
    /// capability defaults, no contradiction warnings.
    Force,
}

impl DeploymentType {
    /// Stable public label — what `--deployment-type=<…>`, the
    /// `[cluster] deployment_type` config key, the `NICO_DEPLOYMENT_TYPE`
    /// env, and the boot banner all use.
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::CoreOnly => "core-only",
            Self::RestOnlyMock => "rest-only-mock",
            Self::Force => "force",
        }
    }

    /// Parse from the public `label()` vocabulary. `auto` is *not* a
    /// `DeploymentType` value — it's the absence of a resolved type and
    /// is handled at the source-tag layer.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Self::Full),
            "core-only" => Some(Self::CoreOnly),
            "rest-only-mock" => Some(Self::RestOnlyMock),
            "force" => Some(Self::Force),
            _ => None,
        }
    }

    /// Capability: controller namespace ([cluster] namespace) for this
    /// deployment-type. `None` for `Force` (raw config flows through).
    pub fn default_cluster_namespace(self) -> Option<&'static str> {
        match self {
            Self::Full | Self::CoreOnly => Some("forge-system"),
            Self::RestOnlyMock => Some("nico-rest"),
            Self::Force => None,
        }
    }

    /// Capability: gRPC service `host:port`. `None` for `Force`.
    pub fn default_grpc_address(self) -> Option<&'static str> {
        match self {
            Self::Full | Self::CoreOnly => Some("carbide-api.forge-system:1079"),
            Self::RestOnlyMock => Some("nico-rest-mock-core.nico-rest:11079"),
            Self::Force => None,
        }
    }

    /// Capability: postgres namespace. Stubbed in slice 1 — slice 5
    /// (#282) will fill in real values once the capability bundle wiring
    /// lands and the vocabulary is re-grilled.
    pub fn default_postgres_namespace(self) -> Option<&'static str> {
        None
    }

    /// Capability: Temporal frontend address. Stubbed; see
    /// `default_postgres_namespace`.
    pub fn default_temporal_address(self) -> Option<&'static str> {
        None
    }

    /// Capability: kubernetes namespace where `temporal-frontend` runs.
    /// `Full` and `RestOnlyMock` install Temporal in its own `temporal`
    /// namespace; `CoreOnly` doesn't deploy Temporal, so this is `None`.
    /// `Force` returns `None` (raw config flows through).
    ///
    /// Distinct from the Temporal *tenancy* namespace (`temporal.namespace`
    /// in config) which addresses workflow visibility and is unrelated to
    /// k8s tenancy. PRD-001 §"Capability vocabulary".
    pub fn default_temporal_k8s_namespace(self) -> Option<&'static str> {
        match self {
            Self::Full | Self::RestOnlyMock => Some("temporal"),
            Self::CoreOnly | Self::Force => None,
        }
    }

    /// Capability: whether this deployment-type runs the forgedb postgres
    /// schema. The `dpu` layer keys off this — `rest-only-mock` skips the
    /// `dpu` layer because forgedb is absent. `Force` returns `true`
    /// (no enforcement; let the layer try and surface the real error).
    pub fn forgedb_present(self) -> bool {
        match self {
            Self::Full | Self::CoreOnly | Self::Force => true,
            Self::RestOnlyMock => false,
        }
    }

    /// Capability: whether this deployment-type runs Temporal. `core-only`
    /// stops at carbide-kind without rest, so Temporal is never deployed
    /// (see `infra-controller-core/helm-prereqs/setup.sh` phase boundary);
    /// the `workflows` layer and `port-forward: workflows` boot-probe step
    /// skip with reason in that case. `Force` returns `true` (no
    /// enforcement; let the operator's raw config flow through).
    pub fn temporal_present(self) -> bool {
        match self {
            Self::Full | Self::RestOnlyMock | Self::Force => true,
            Self::CoreOnly => false,
        }
    }

    /// Capability: whether InfiniBand fabric is present on any DPU in
    /// the fleet (PRD-004 slice 1). `None` for `Force` — the escape
    /// hatch never probes. For other types the static method also
    /// returns `None`: IB presence is a per-cluster fact, not a
    /// deployment-type-class attribute, so the runtime value is
    /// resolved by the `detect_infiniband_present` boot-probe step
    /// and lives on `ProbeState`, not on the type.
    pub fn infiniband_present(self) -> Option<bool> {
        None
    }
}

impl fmt::Display for DeploymentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Where the resolved `DeploymentType` came from. Drives the
/// `type: <name> (<source>)` tag in the boot banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeploymentTypeSource {
    /// User passed `--deployment-type=auto` (or no flag); detection ran.
    Auto,
    /// User passed an explicit `--deployment-type=<full|core-only|rest-only-mock|force>`.
    Flag,
    /// `[cluster] deployment_type` in `config.toml` or `NICO_DEPLOYMENT_TYPE` env.
    Config,
    /// User passed `--deployment-type=force` (or set it in config/env).
    /// Detection is skipped and capability defaults do not apply.
    Force,
}

impl DeploymentTypeSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Flag => "flag",
            Self::Config => "config",
            Self::Force => "force",
        }
    }
}

impl fmt::Display for DeploymentTypeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

pub struct Config {
    pub cluster: ClusterConfig,
    pub postgres: PostgresConfig,
    pub temporal: TemporalConfig,
    pub output: OutputConfig,
    pub bootstrap: BootstrapConfig,
    pub dpu: DpuConfig,
}

/// Thresholds for the seven-layer doctor's `dpu` sub-checks
/// (issue #214). Each field is separately overridable from `[dpu]` in
/// `config.toml` (or via `NICO_DPU_*` env vars) so operators can tune
/// fleet-specific noise floors.
#[derive(Debug, Clone, Copy)]
pub struct DpuConfig {
    pub drift_managed_host_warn: Duration,
    pub drift_managed_host_fail: Duration,
    pub drift_instance_warn: Duration,
    pub drift_instance_fail: Duration,
    pub cert_warn: Duration,
    pub cert_fail: Duration,
    pub lost_connection_warn: Duration,
    pub lost_connection_fail_age: Duration,
    pub lost_connection_fail_pct: f64,
    /// Grace window before a `PostConfigCheckWait` health probe in alert
    /// state trips the `probe-stuck` sub-check (issue #239). Briefly
    /// stuck probes are normal during config rollouts; > grace is the
    /// signal that the agent has not converged.
    pub probe_stuck_grace: Duration,
}

impl Default for DpuConfig {
    fn default() -> Self {
        Self {
            drift_managed_host_warn: Duration::from_secs(15 * 60),
            drift_managed_host_fail: Duration::from_secs(60 * 60),
            drift_instance_warn: Duration::from_secs(2 * 60),
            drift_instance_fail: Duration::from_secs(30 * 60),
            cert_warn: Duration::from_secs(30 * 24 * 60 * 60),
            cert_fail: Duration::from_secs(7 * 24 * 60 * 60),
            lost_connection_warn: Duration::from_secs(5 * 60),
            lost_connection_fail_age: Duration::from_secs(30 * 60),
            lost_connection_fail_pct: 0.05,
            probe_stuck_grace: Duration::from_secs(30),
        }
    }
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
    /// Kubernetes namespace where `temporal-frontend` runs. Distinct from
    /// `temporal.namespace` (the Temporal tenancy namespace). Defaults to
    /// the active deployment-type's `default_temporal_k8s_namespace()`,
    /// or `"temporal"` when no type is resolved.
    pub temporal_namespace: String,
    pub reach_mode: ReachMode,
    pub grpc_address: Option<String>,
    /// Resolved deployment-type. `None` means `auto` — the boot-probe
    /// `detect_deployment_type` step runs the detection ladder. `Some(...)`
    /// means the user (or config / env) pinned a specific type and
    /// detection is skipped.
    pub deployment_type: Option<DeploymentType>,
    /// Where `deployment_type` came from — drives the
    /// `type: <name> (<source>)` tag in the boot banner.
    pub deployment_type_source: DeploymentTypeSource,
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
    /// CLI override for the kubernetes namespace where `temporal-frontend`
    /// runs (PRD-001 slice 10). Distinct from `temporal_namespace` above
    /// (the tenancy namespace). Highest precedence over env, file, and
    /// the deployment-type bundle.
    pub temporal_k8s_namespace: Option<String>,
    pub stuck_threshold: Option<Duration>,
    pub color: Option<ColorMode>,
    pub format: Option<OutputFormat>,
    pub reach_mode: Option<ReachMode>,
    pub tui_refresh: Option<Duration>,
    /// CLI `--timeouts step=Xs,...` spec, applied last (highest precedence).
    pub bootstrap_timeouts_spec: Option<String>,
    /// CLI `--deployment-type=<auto|full|core-only|rest-only-mock|force>`
    /// raw spec. Highest precedence over env (`NICO_DEPLOYMENT_TYPE`) and
    /// file (`[cluster] deployment_type`). `Some("auto")` is meaningful
    /// — it explicitly opts into detection and overrides any config-set
    /// pinned value.
    pub deployment_type_spec: Option<String>,
}

/// Intermediate deserialization shape — all fields optional so missing fields fall back to defaults.
#[derive(Deserialize, Default)]
struct FileConfig {
    cluster: Option<FileClusterConfig>,
    postgres: Option<FilePostgresConfig>,
    temporal: Option<FileTemporalConfig>,
    output: Option<FileOutputConfig>,
    bootstrap: Option<FileBootstrapConfig>,
    dpu: Option<FileDpuConfig>,
}

#[derive(Deserialize, Default)]
struct FileDpuConfig {
    drift_managed_host_warn: Option<String>,
    drift_managed_host_fail: Option<String>,
    drift_instance_warn: Option<String>,
    drift_instance_fail: Option<String>,
    cert_warn: Option<String>,
    cert_fail: Option<String>,
    lost_connection_warn: Option<String>,
    lost_connection_fail_age: Option<String>,
    lost_connection_fail_pct: Option<f64>,
    probe_stuck_grace: Option<String>,
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
    temporal_namespace: Option<String>,
    reach_mode: Option<String>,
    grpc_address: Option<String>,
    deployment_type: Option<String>,
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
        detected_deployment_type: Option<DeploymentType>,
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
        let dpu_file = file.dpu.unwrap_or_default();

        // Deployment-type resolves first — its default-methods feed the
        // five capability-bundle keys below as a layer between
        // hardcoded fallbacks and file (PRD-001 §"Config precedence").
        // `Force` returns `None` from every default-method, so it
        // transparently falls through to the existing hardcoded values.
        // `detected_deployment_type` slots into the bundle layer: it
        // wins only when CLI / env / file are silent (auto mode), and
        // its source tag is `Auto`. PRD-001 slice 9 (#321).
        let (deployment_type, deployment_type_source) = resolve_deployment_type(
            overrides.deployment_type_spec.as_deref(),
            env.get("NICO_DEPLOYMENT_TYPE").map(String::as_str),
            cluster.deployment_type.as_deref(),
            detected_deployment_type,
        )?;
        let dt_namespace = deployment_type.and_then(|dt| dt.default_cluster_namespace());
        let dt_grpc = deployment_type.and_then(|dt| dt.default_grpc_address());
        let dt_postgres_ns = deployment_type.and_then(|dt| dt.default_postgres_namespace());
        let dt_temporal_addr = deployment_type.and_then(|dt| dt.default_temporal_address());
        let dt_temporal_k8s_ns = deployment_type.and_then(|dt| dt.default_temporal_k8s_namespace());

        // Env var layer — overrides file, which overrides deployment-type
        // defaults (when present), which overrides hardcoded fallbacks.
        let context = env.get("NICO_CONTEXT").cloned().or(cluster.context);
        let namespace = env.get("NICO_NAMESPACE").cloned()
            .or(cluster.namespace)
            .or(dt_namespace.map(String::from))
            .unwrap_or_else(|| "nico".into());
        let postgres_namespace = env.get("NICO_POSTGRES_NAMESPACE").cloned()
            .or(cluster.postgres_namespace)
            .or(dt_postgres_ns.map(String::from))
            .unwrap_or_else(|| "postgres".into());
        let temporal_k8s_namespace = env.get("NICO_TEMPORAL_K8S_NAMESPACE").cloned()
            .or(cluster.temporal_namespace)
            .or(dt_temporal_k8s_ns.map(String::from))
            .unwrap_or_else(|| "temporal".into());
        let postgres_url = env.get("NICO_POSTGRES_URL").cloned()
            .or(postgres.url)
            .unwrap_or_else(|| "postgres://nico:nico@localhost:5432/nico".into());
        let temporal_address = env.get("NICO_TEMPORAL_ADDRESS").cloned()
            .or(temporal.address)
            .or(dt_temporal_addr.map(String::from))
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

        let grpc_address = env.get("NICO_GRPC_ADDRESS").cloned()
            .or(cluster.grpc_address)
            .or(dt_grpc.map(String::from));

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

        // [dpu] block — file < env. Only known step names are honored.
        let mut dpu = DpuConfig::default();
        for (name, val) in [
            ("drift_managed_host_warn", dpu_file.drift_managed_host_warn),
            ("drift_managed_host_fail", dpu_file.drift_managed_host_fail),
            ("drift_instance_warn", dpu_file.drift_instance_warn),
            ("drift_instance_fail", dpu_file.drift_instance_fail),
            ("cert_warn", dpu_file.cert_warn),
            ("cert_fail", dpu_file.cert_fail),
            ("lost_connection_warn", dpu_file.lost_connection_warn),
            ("lost_connection_fail_age", dpu_file.lost_connection_fail_age),
            ("probe_stuck_grace", dpu_file.probe_stuck_grace),
        ] {
            if let Some(s) = val {
                let d = humantime::parse_duration(&s)
                    .with_context(|| format!("invalid dpu.{name} = {s:?}"))?;
                dpu.set_duration(name, d);
            }
        }
        if let Some(pct) = dpu_file.lost_connection_fail_pct {
            dpu.lost_connection_fail_pct = pct;
        }
        for (name, env_key) in [
            ("drift_managed_host_warn", "NICO_DPU_DRIFT_MANAGED_HOST_WARN"),
            ("drift_managed_host_fail", "NICO_DPU_DRIFT_MANAGED_HOST_FAIL"),
            ("drift_instance_warn", "NICO_DPU_DRIFT_INSTANCE_WARN"),
            ("drift_instance_fail", "NICO_DPU_DRIFT_INSTANCE_FAIL"),
            ("cert_warn", "NICO_DPU_CERT_WARN"),
            ("cert_fail", "NICO_DPU_CERT_FAIL"),
            ("lost_connection_warn", "NICO_DPU_LOST_CONNECTION_WARN"),
            ("lost_connection_fail_age", "NICO_DPU_LOST_CONNECTION_FAIL_AGE"),
            ("probe_stuck_grace", "NICO_DPU_PROBE_STUCK_GRACE"),
        ] {
            if let Some(s) = env.get(env_key) {
                let d = humantime::parse_duration(s)
                    .with_context(|| format!("invalid {env_key} = {s:?}"))?;
                dpu.set_duration(name, d);
            }
        }
        if let Some(s) = env.get("NICO_DPU_LOST_CONNECTION_FAIL_PCT") {
            dpu.lost_connection_fail_pct = s
                .parse::<f64>()
                .with_context(|| format!("invalid NICO_DPU_LOST_CONNECTION_FAIL_PCT = {s:?}"))?;
        }

        // Flag override layer — highest precedence
        let context = overrides.context.clone().or(context);
        let namespace = overrides.namespace.clone().unwrap_or(namespace);
        let postgres_url = overrides.postgres_url.clone().unwrap_or(postgres_url);
        let temporal_address = overrides.temporal_address.clone().unwrap_or(temporal_address);
        let temporal_namespace = overrides.temporal_namespace.clone().unwrap_or(temporal_namespace);
        let temporal_k8s_namespace = overrides
            .temporal_k8s_namespace
            .clone()
            .unwrap_or(temporal_k8s_namespace);
        let stuck_threshold = overrides.stuck_threshold.unwrap_or(stuck_threshold);
        let color = overrides.color.unwrap_or(color);
        let format = overrides.format.unwrap_or(format);
        let reach_mode = overrides.reach_mode.unwrap_or(reach_mode);
        let tui_refresh = overrides.tui_refresh.unwrap_or(tui_refresh);

        Ok(Config {
            cluster: ClusterConfig {
                context,
                namespace,
                postgres_namespace,
                temporal_namespace: temporal_k8s_namespace,
                reach_mode,
                grpc_address,
                deployment_type,
                deployment_type_source,
            },
            postgres: PostgresConfig { url: postgres_url },
            temporal: TemporalConfig {
                address: temporal_address,
                namespace: temporal_namespace,
                stuck_threshold,
            },
            output: OutputConfig { color, format, tui_refresh },
            bootstrap: BootstrapConfig { timeouts },
            dpu,
        })
    }
}

impl Config {
    /// Per PRD-001 §"Capability vocabulary > Override-conflict warning rule":
    /// for each of the five capability-bundle keys, if the resolved value
    /// differs from the active deployment-type's default for that key,
    /// emit one stderr line. `Force` returns `None` from every default,
    /// so it silences all warnings. `auto` (no resolved type yet) also
    /// produces no warnings — there's nothing to compare against.
    pub fn override_conflict_warnings(&self) -> Vec<String> {
        let Some(dt) = self.cluster.deployment_type else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        let dt_label = dt.label();

        // Order is stable (matches the PRD's key list) so callers and
        // tests get deterministic output.
        if let Some(default) = dt.default_cluster_namespace()
            && self.cluster.namespace != default
        {
            warnings.push(format_override_warning(
                "cluster.namespace",
                &self.cluster.namespace,
                dt_label,
                default,
            ));
        }
        if let Some(default) = dt.default_grpc_address()
            && let Some(resolved) = self.cluster.grpc_address.as_deref()
            && resolved != default
        {
            warnings.push(format_override_warning(
                "cluster.grpc_address",
                resolved,
                dt_label,
                default,
            ));
        }
        if let Some(default) = dt.default_postgres_namespace()
            && self.cluster.postgres_namespace != default
        {
            warnings.push(format_override_warning(
                "cluster.postgres_namespace",
                &self.cluster.postgres_namespace,
                dt_label,
                default,
            ));
        }
        if let Some(default) = dt.default_temporal_address()
            && self.temporal.address != default
        {
            warnings.push(format_override_warning(
                "temporal.address",
                &self.temporal.address,
                dt_label,
                default,
            ));
        }
        if let Some(default) = dt.default_temporal_k8s_namespace()
            && self.cluster.temporal_namespace != default
        {
            warnings.push(format_override_warning(
                "cluster.temporal_namespace",
                &self.cluster.temporal_namespace,
                dt_label,
                default,
            ));
        }
        warnings
    }
}

fn format_override_warning(key: &str, resolved: &str, type_label: &str, default: &str) -> String {
    format!(
        "⚠  {key}={resolved} overrides deployment-type {type_label} default ({default})"
    )
}

/// Resolve `(deployment_type, source)` from the precedence chain
/// CLI spec > env (`NICO_DEPLOYMENT_TYPE`) > file (`[cluster] deployment_type`) > detected.
/// `auto` (or absent) with no detected type → `(None, Auto)`.
/// `auto` with detected → `(Some(detected), Auto)` (PRD-001 slice 9).
/// `force` → `(Some(Force), Force)`.
/// Real types → `Some(...)` with `Flag` (CLI) or `Config` (env/file) source.
fn resolve_deployment_type(
    cli_spec: Option<&str>,
    env_val: Option<&str>,
    file_val: Option<&str>,
    detected: Option<DeploymentType>,
) -> Result<(Option<DeploymentType>, DeploymentTypeSource)> {
    // Precedence walk. First non-None spec wins. `auto` is a meaningful
    // value: it forces the auto path even when a lower layer pinned a
    // type, so we don't fall through to env/file once we hit it.
    let (raw, origin): (&str, DeploymentTypeOrigin) = if let Some(s) = cli_spec {
        (s, DeploymentTypeOrigin::Cli)
    } else if let Some(s) = env_val {
        (s, DeploymentTypeOrigin::EnvOrFile)
    } else if let Some(s) = file_val {
        (s, DeploymentTypeOrigin::EnvOrFile)
    } else {
        // No CLI / env / file declaration → bundle layer (detected) feeds
        // the resolved type. `Auto` source either way.
        return Ok((detected, DeploymentTypeSource::Auto));
    };

    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("auto") {
        // Explicit `auto` opts into detection regardless of which layer
        // it came from — the detected type slots into the bundle layer.
        return Ok((detected, DeploymentTypeSource::Auto));
    }

    let dt = DeploymentType::parse(raw).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid deployment-type {:?}; use auto, full, core-only, rest-only-mock, or force",
            raw
        )
    })?;

    let source = match (dt, origin) {
        (DeploymentType::Force, _) => DeploymentTypeSource::Force,
        (_, DeploymentTypeOrigin::Cli) => DeploymentTypeSource::Flag,
        (_, DeploymentTypeOrigin::EnvOrFile) => DeploymentTypeSource::Config,
    };
    Ok((Some(dt), source))
}

#[derive(Clone, Copy)]
enum DeploymentTypeOrigin {
    Cli,
    EnvOrFile,
}

impl DpuConfig {
    fn set_duration(&mut self, name: &str, d: Duration) {
        match name {
            "drift_managed_host_warn" => self.drift_managed_host_warn = d,
            "drift_managed_host_fail" => self.drift_managed_host_fail = d,
            "drift_instance_warn" => self.drift_instance_warn = d,
            "drift_instance_fail" => self.drift_instance_fail = d,
            "cert_warn" => self.cert_warn = d,
            "cert_fail" => self.cert_fail = d,
            "lost_connection_warn" => self.lost_connection_warn = d,
            "lost_connection_fail_age" => self.lost_connection_fail_age = d,
            "probe_stuck_grace" => self.probe_stuck_grace = d,
            _ => unreachable!("unknown dpu duration field {name}"),
        }
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
        let config = Config::load(None, &env, &overrides, None).unwrap();
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
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default(), None).unwrap();
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
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.namespace, "prod");
        assert_eq!(config.cluster.context.as_deref(), Some("prod-ctx"));
        assert_eq!(config.postgres.url, "postgres://prod:secret@db:5432/prod");
        // untouched field still has default
        assert_eq!(config.temporal.address, "localhost:7233");
    }

    #[test]
    fn defaults_when_no_sources() {
        let config = Config::load(None, &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
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
        let config = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn reach_mode_flag_override() {
        let overrides = ConfigOverrides {
            reach_mode: Some(ReachMode::InCluster),
            ..Default::default()
        };
        let config = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn reach_mode_file_override() {
        let toml = "[cluster]\nreach_mode = \"in-cluster\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn kubernetes_service_host_selects_in_cluster() {
        let mut env = HashMap::new();
        env.insert("KUBERNETES_SERVICE_HOST".to_string(), "10.0.0.1".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.reach_mode, ReachMode::InCluster);
    }

    #[test]
    fn invalid_reach_mode_errors() {
        let mut env = HashMap::new();
        env.insert("NICO_REACH_MODE".to_string(), "bogus".to_string());
        assert!(Config::load(None, &env, &ConfigOverrides::default(), None).is_err());
    }

    #[test]
    fn tui_refresh_from_file() {
        let toml = "[output]\ntui_refresh = \"10s\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.output.tui_refresh, Duration::from_secs(10));
    }

    #[test]
    fn tui_refresh_env_overrides_file() {
        let toml = "[output]\ntui_refresh = \"10s\"";
        let mut env = HashMap::new();
        env.insert("NICO_TUI_REFRESH".to_string(), "20s".to_string());
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default(), None).unwrap();
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
        let config = Config::load(Some(toml), &env, &overrides, None).unwrap();
        assert_eq!(config.output.tui_refresh, Duration::from_secs(5));
    }

    #[test]
    fn grpc_address_defaults_to_none() {
        let config = Config::load(None, &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert!(config.cluster.grpc_address.is_none());
    }

    #[test]
    fn grpc_address_from_env() {
        let mut env = HashMap::new();
        env.insert("NICO_GRPC_ADDRESS".to_string(), "carbide-api:1079".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.grpc_address.as_deref(), Some("carbide-api:1079"));
    }

    #[test]
    fn grpc_address_from_file() {
        let toml = "[cluster]\ngrpc_address = \"carbide-api:1079\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.grpc_address.as_deref(), Some("carbide-api:1079"));
    }

    #[test]
    fn grpc_address_env_overrides_file() {
        let toml = "[cluster]\ngrpc_address = \"from-file:1079\"";
        let mut env = HashMap::new();
        env.insert("NICO_GRPC_ADDRESS".to_string(), "from-env:1079".to_string());
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.grpc_address.as_deref(), Some("from-env:1079"));
    }

    #[test]
    fn postgres_namespace_defaults_to_postgres() {
        let config = Config::load(None, &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.postgres_namespace, "postgres");
    }

    #[test]
    fn postgres_namespace_from_env() {
        let mut env = HashMap::new();
        env.insert("NICO_POSTGRES_NAMESPACE".to_string(), "db-tier".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.postgres_namespace, "db-tier");
    }

    #[test]
    fn postgres_namespace_from_file() {
        let toml = "[cluster]\npostgres_namespace = \"data\"";
        let config = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(config.cluster.postgres_namespace, "data");
    }

    #[test]
    fn postgres_namespace_env_overrides_file() {
        let toml = "[cluster]\npostgres_namespace = \"from-file\"";
        let mut env = HashMap::new();
        env.insert("NICO_POSTGRES_NAMESPACE".to_string(), "from-env".to_string());
        let config = Config::load(Some(toml), &env, &ConfigOverrides::default(), None).unwrap();
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
        let cfg = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
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
        let cfg = Config::load(Some(toml), &env, &ConfigOverrides::default(), None).unwrap();
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
        let cfg = Config::load(Some(toml), &env, &overrides, None).unwrap();
        assert_eq!(cfg.bootstrap.timeouts.preflight, Duration::from_millis(500));
        assert_eq!(cfg.bootstrap.timeouts.port_forward, Duration::from_millis(250));
    }

    #[test]
    fn bootstrap_timeouts_unknown_step_in_spec_errors() {
        let overrides = ConfigOverrides {
            bootstrap_timeouts_spec: Some("nonsense=1s".to_string()),
            ..Default::default()
        };
        let err = Config::load(None, &HashMap::new(), &overrides, None).err().expect("expected err");
        let msg = format!("{err:#}");
        assert!(msg.contains("nonsense"), "msg = {msg}");
    }

    #[test]
    fn bootstrap_timeouts_invalid_duration_in_spec_errors() {
        let overrides = ConfigOverrides {
            bootstrap_timeouts_spec: Some("preflight=NOTADURATION".to_string()),
            ..Default::default()
        };
        assert!(Config::load(None, &HashMap::new(), &overrides, None).is_err());
    }

    #[test]
    fn grpc_address_independent_from_temporal_address() {
        let mut env = HashMap::new();
        env.insert("NICO_TEMPORAL_ADDRESS".to_string(), "temporal:7233".to_string());
        let config = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert!(config.cluster.grpc_address.is_none());
        assert_eq!(config.temporal.address, "temporal:7233");
    }

    #[test]
    fn deployment_type_labels_match_prd_vocabulary() {
        assert_eq!(DeploymentType::Full.label(), "full");
        assert_eq!(DeploymentType::CoreOnly.label(), "core-only");
        assert_eq!(DeploymentType::RestOnlyMock.label(), "rest-only-mock");
        assert_eq!(DeploymentType::Force.label(), "force");
    }

    #[test]
    fn deployment_type_parse_round_trips() {
        for dt in [
            DeploymentType::Full,
            DeploymentType::CoreOnly,
            DeploymentType::RestOnlyMock,
            DeploymentType::Force,
        ] {
            assert_eq!(DeploymentType::parse(dt.label()), Some(dt));
        }
        assert_eq!(DeploymentType::parse("auto"), None);
        assert_eq!(DeploymentType::parse("nope"), None);
        assert_eq!(DeploymentType::parse(""), None);
    }

    #[test]
    fn deployment_type_capabilities_match_prd_table() {
        // Full / CoreOnly: forge-system + carbide-api + forgedb yes.
        assert_eq!(
            DeploymentType::Full.default_cluster_namespace(),
            Some("forge-system")
        );
        assert_eq!(
            DeploymentType::CoreOnly.default_cluster_namespace(),
            Some("forge-system")
        );
        assert_eq!(
            DeploymentType::Full.default_grpc_address(),
            Some("carbide-api.forge-system:1079")
        );
        assert_eq!(
            DeploymentType::CoreOnly.default_grpc_address(),
            Some("carbide-api.forge-system:1079")
        );
        assert!(DeploymentType::Full.forgedb_present());
        assert!(DeploymentType::CoreOnly.forgedb_present());

        // RestOnlyMock: nico-rest + mock-core + no forgedb.
        assert_eq!(
            DeploymentType::RestOnlyMock.default_cluster_namespace(),
            Some("nico-rest")
        );
        assert_eq!(
            DeploymentType::RestOnlyMock.default_grpc_address(),
            Some("nico-rest-mock-core.nico-rest:11079")
        );
        assert!(!DeploymentType::RestOnlyMock.forgedb_present());
    }

    #[test]
    fn deployment_type_force_returns_none_for_capabilities_and_true_for_forgedb() {
        // Per slice 1 acceptance: methods return None for Force,
        // forgedb_present returns true (no-enforcement semantics).
        assert!(DeploymentType::Force.default_cluster_namespace().is_none());
        assert!(DeploymentType::Force.default_grpc_address().is_none());
        assert!(DeploymentType::Force.default_postgres_namespace().is_none());
        assert!(DeploymentType::Force.default_temporal_address().is_none());
        assert!(DeploymentType::Force.default_temporal_k8s_namespace().is_none());
        assert!(DeploymentType::Force.forgedb_present());
        // PRD-001 slice 10: temporal_present mirrors forgedb_present —
        // Force returns true (no enforcement; let raw config flow).
        assert!(DeploymentType::Force.temporal_present());
    }

    // PRD-001 slice 10: temporal k8s namespace + temporal_present.
    //
    // `default_temporal_k8s_namespace` is the kubernetes namespace where
    // `temporal-frontend` runs. `Full` and `RestOnlyMock` both install
    // Temporal in its own `temporal` namespace. `CoreOnly` doesn't deploy
    // Temporal at all (helm-prereqs phase boundary). `Force` returns
    // `None`. `temporal_present` is the matching feature gate.

    #[test]
    fn deployment_type_default_temporal_k8s_namespace_matches_capability_matrix() {
        assert_eq!(
            DeploymentType::Full.default_temporal_k8s_namespace(),
            Some("temporal")
        );
        assert_eq!(
            DeploymentType::RestOnlyMock.default_temporal_k8s_namespace(),
            Some("temporal")
        );
        assert_eq!(DeploymentType::CoreOnly.default_temporal_k8s_namespace(), None);
        assert_eq!(DeploymentType::Force.default_temporal_k8s_namespace(), None);
    }

    #[test]
    fn deployment_type_temporal_present_matches_capability_matrix() {
        assert!(DeploymentType::Full.temporal_present());
        assert!(DeploymentType::RestOnlyMock.temporal_present());
        assert!(DeploymentType::Force.temporal_present());
        assert!(!DeploymentType::CoreOnly.temporal_present());
    }

    #[test]
    fn deployment_type_infiniband_present_is_none_for_force() {
        // PRD-004 slice 1 AC: `infiniband_present()` returns `None` for
        // `Force`. The static method is a placeholder for the capability;
        // the runtime value is resolved by `detect_infiniband_present`
        // boot-probe step and lives on `ProbeState`, not on the type.
        assert_eq!(DeploymentType::Force.infiniband_present(), None);
    }

    #[test]
    fn deployment_type_infiniband_present_is_none_for_non_force_types() {
        // The type itself never knows IB presence — that's a per-cluster
        // fact resolved at runtime. Static method always reports `None`;
        // boot probe sets the resolved value on ProbeState.
        for dt in [
            DeploymentType::Full,
            DeploymentType::CoreOnly,
            DeploymentType::RestOnlyMock,
        ] {
            assert_eq!(
                dt.infiniband_present(),
                None,
                "{dt:?}::infiniband_present() should return None (resolved at runtime)"
            );
        }
    }

    #[test]
    fn deployment_type_source_labels_are_stable() {
        assert_eq!(DeploymentTypeSource::Auto.label(), "auto");
        assert_eq!(DeploymentTypeSource::Flag.label(), "flag");
        assert_eq!(DeploymentTypeSource::Config.label(), "config");
        assert_eq!(DeploymentTypeSource::Force.label(), "force");
    }

    #[test]
    fn deployment_type_defaults_to_auto_when_no_overrides() {
        let cfg = Config::load(None, &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert!(cfg.cluster.deployment_type.is_none());
        assert_eq!(cfg.cluster.deployment_type_source, DeploymentTypeSource::Auto);
    }

    #[test]
    fn deployment_type_explicit_flag_resolves_to_flag_source() {
        for label in ["full", "core-only", "rest-only-mock"] {
            let overrides = ConfigOverrides {
                deployment_type_spec: Some(label.into()),
                ..Default::default()
            };
            let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
            assert_eq!(cfg.cluster.deployment_type, DeploymentType::parse(label));
            assert_eq!(
                cfg.cluster.deployment_type_source,
                DeploymentTypeSource::Flag,
                "expected Flag source for --deployment-type={label}"
            );
        }
    }

    #[test]
    fn deployment_type_force_flag_resolves_to_force_source() {
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("force".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::Force));
        assert_eq!(cfg.cluster.deployment_type_source, DeploymentTypeSource::Force);
    }

    #[test]
    fn deployment_type_auto_flag_resolves_to_auto_source() {
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("auto".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert!(cfg.cluster.deployment_type.is_none());
        assert_eq!(cfg.cluster.deployment_type_source, DeploymentTypeSource::Auto);
    }

    #[test]
    fn deployment_type_from_env_resolves_to_config_source() {
        let mut env = HashMap::new();
        env.insert(
            "NICO_DEPLOYMENT_TYPE".to_string(),
            "rest-only-mock".to_string(),
        );
        let cfg = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(
            cfg.cluster.deployment_type,
            Some(DeploymentType::RestOnlyMock)
        );
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Config
        );
    }

    #[test]
    fn deployment_type_from_file_resolves_to_config_source() {
        let toml = r#"[cluster]
deployment_type = "core-only"
"#;
        let cfg = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::CoreOnly));
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Config
        );
    }

    #[test]
    fn deployment_type_cli_overrides_env_and_file() {
        let toml = "[cluster]\ndeployment_type = \"core-only\"";
        let mut env = HashMap::new();
        env.insert("NICO_DEPLOYMENT_TYPE".to_string(), "rest-only-mock".to_string());
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &env, &overrides, None).unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::Full));
        assert_eq!(cfg.cluster.deployment_type_source, DeploymentTypeSource::Flag);
    }

    #[test]
    fn deployment_type_env_overrides_file() {
        let toml = "[cluster]\ndeployment_type = \"core-only\"";
        let mut env = HashMap::new();
        env.insert("NICO_DEPLOYMENT_TYPE".to_string(), "full".to_string());
        let cfg = Config::load(Some(toml), &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::Full));
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Config
        );
    }

    #[test]
    fn deployment_type_force_in_config_resolves_to_force_source() {
        // `force` is `force` regardless of where it came from — the
        // banner reads `type: force (force)` for the no-enforcement path.
        let mut env = HashMap::new();
        env.insert("NICO_DEPLOYMENT_TYPE".to_string(), "force".to_string());
        let cfg = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::Force));
        assert_eq!(cfg.cluster.deployment_type_source, DeploymentTypeSource::Force);
    }

    #[test]
    fn deployment_type_invalid_value_errors() {
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("nope".into()),
            ..Default::default()
        };
        let err = Config::load(None, &HashMap::new(), &overrides, None).err().expect("expected err");
        let msg = format!("{err:#}");
        assert!(msg.contains("nope"), "msg = {msg}");
    }

    #[test]
    fn deployment_type_invalid_in_file_errors() {
        let toml = "[cluster]\ndeployment_type = \"weird\"";
        assert!(Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).is_err());
    }

    // PRD-001 slice 5: capability bundle wiring.
    //
    // Precedence chain for the five default-keys becomes
    //   hardcoded < deployment-type < file < env < CLI
    // and when the resolved value contradicts the deployment-type's
    // default, the config builder records a one-line warning per
    // contradicting key. `Force` returns `None` from every default so
    // it both opts out of the new layer and silences all warnings.

    #[test]
    fn deployment_type_default_layer_supplies_cluster_namespace_when_unset_elsewhere() {
        // rest-only-mock from --deployment-type, no file/env/CLI overrides
        // → cluster.namespace resolves to the deployment-type default.
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("rest-only-mock".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.namespace, "nico-rest");
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    #[test]
    fn deployment_type_default_supplies_grpc_address_when_unset_elsewhere() {
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(
            cfg.cluster.grpc_address.as_deref(),
            Some("carbide-api.forge-system:1079")
        );
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    #[test]
    fn override_conflict_warning_emits_when_file_namespace_contradicts_deployment_type() {
        // PRD contradiction matrix: rest-only-mock + file pin to forge-system → warn.
        let toml = "[cluster]\nnamespace = \"forge-system\"";
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("rest-only-mock".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.namespace, "forge-system");
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 1, "expected one warning, got: {warnings:?}");
        assert!(
            warnings[0].contains("cluster.namespace=forge-system"),
            "warning missing key=value: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("rest-only-mock"),
            "warning missing type label: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("nico-rest"),
            "warning missing deployment-type default: {}",
            warnings[0]
        );
    }

    #[test]
    fn override_conflict_warning_emits_for_weird_value() {
        // Weird-but-valid override: user passed a totally unrelated namespace.
        let mut env = HashMap::new();
        env.insert("NICO_NAMESPACE".to_string(), "weird-ns".to_string());
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &env, &overrides, None).unwrap();
        assert_eq!(cfg.cluster.namespace, "weird-ns");
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cluster.namespace=weird-ns"));
        assert!(warnings[0].contains("full"));
        assert!(warnings[0].contains("forge-system"));
    }

    #[test]
    fn override_conflict_warning_no_warn_when_value_matches_deployment_type_default() {
        // file pins to the deployment-type's own default → silent.
        let toml = "[cluster]\nnamespace = \"forge-system\"";
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.namespace, "forge-system");
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    #[test]
    fn override_conflict_warnings_silenced_under_force() {
        // Even if the user overrides every key, force is the no-enforcement
        // escape hatch — no warnings.
        let toml = r#"
[cluster]
namespace = "weird-ns"
grpc_address = "weird:9999"
"#;
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("force".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &HashMap::new(), &overrides, None).unwrap();
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    #[test]
    fn override_conflict_warnings_silenced_under_auto() {
        // No deployment-type resolved yet (auto, pre-detection) → nothing
        // to compare against, so no warnings.
        let toml = "[cluster]\nnamespace = \"weird-ns\"";
        let cfg = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    #[test]
    fn override_conflict_warning_grpc_address_contradiction() {
        // Pin gRPC address via env, deployment-type=rest-only-mock.
        let mut env = HashMap::new();
        env.insert(
            "NICO_GRPC_ADDRESS".to_string(),
            "carbide-api.forge-system:1079".to_string(),
        );
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("rest-only-mock".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &env, &overrides, None).unwrap();
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cluster.grpc_address=carbide-api.forge-system:1079"));
        assert!(warnings[0].contains("rest-only-mock"));
        assert!(warnings[0].contains("nico-rest-mock-core.nico-rest:11079"));
    }

    #[test]
    fn override_conflict_warning_multiple_keys_each_emit() {
        // Two contradictions → two warning lines, in stable key order.
        let toml = r#"
[cluster]
namespace = "weird-ns"
grpc_address = "weird:9999"
"#;
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &HashMap::new(), &overrides, None).unwrap();
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 2, "got: {warnings:?}");
    }

    #[test]
    fn override_conflict_warning_format_matches_prd_spec() {
        // PRD format: `⚠  <key>=<resolved> overrides deployment-type <name> default (<default>)`
        let toml = "[cluster]\nnamespace = \"forge-system\"";
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("rest-only-mock".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &HashMap::new(), &overrides, None).unwrap();
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(
            warnings[0],
            "⚠  cluster.namespace=forge-system overrides deployment-type \
             rest-only-mock default (nico-rest)"
        );
    }

    #[test]
    fn cli_namespace_override_layered_above_deployment_type_default() {
        // Precedence: CLI > deployment-type default. Resolved value comes
        // from CLI; if it contradicts, warning fires.
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("rest-only-mock".into()),
            namespace: Some("forge-system".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.namespace, "forge-system");
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cluster.namespace=forge-system"));
    }

    // PRD-001 slice 9 (#321): detect-first-then-load.
    //
    // `Config::load` accepts a `detected_deployment_type` parameter that
    // slots into the precedence chain at the bundle layer:
    //   hardcoded < detected (bundle) < file < env < CLI
    // Source tag follows the layer that wins:
    //   - detected wins → Auto
    //   - file/env wins → Config
    //   - CLI wins (real type) → Flag
    //   - CLI wins (force) → Force

    #[test]
    fn detected_deployment_type_resolves_when_no_user_declaration() {
        // Auto path: no CLI / env / file declaration; the boot-probe's
        // detection ladder produced `RestOnlyMock`. Source stays `Auto`.
        let cfg = Config::load(
            None,
            &HashMap::new(),
            &ConfigOverrides::default(),
            Some(DeploymentType::RestOnlyMock),
        )
        .unwrap();
        assert_eq!(
            cfg.cluster.deployment_type,
            Some(DeploymentType::RestOnlyMock)
        );
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Auto
        );
        // The bundle layer applies — namespace and grpc come from
        // RestOnlyMock's defaults.
        assert_eq!(cfg.cluster.namespace, "nico-rest");
        assert_eq!(
            cfg.cluster.grpc_address.as_deref(),
            Some("nico-rest-mock-core.nico-rest:11079")
        );
    }

    #[test]
    fn detected_deployment_type_loses_to_cli_flag() {
        // CLI flag pins to full; detection said rest-only-mock.
        // Resolved type follows CLI; source = Flag.
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(
            None,
            &HashMap::new(),
            &overrides,
            Some(DeploymentType::RestOnlyMock),
        )
        .unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::Full));
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Flag
        );
    }

    #[test]
    fn detected_deployment_type_loses_to_env_pin() {
        let mut env = HashMap::new();
        env.insert("NICO_DEPLOYMENT_TYPE".into(), "full".into());
        let cfg = Config::load(
            None,
            &env,
            &ConfigOverrides::default(),
            Some(DeploymentType::RestOnlyMock),
        )
        .unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::Full));
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Config
        );
    }

    #[test]
    fn detected_deployment_type_loses_to_file_pin() {
        let toml = "[cluster]\ndeployment_type = \"core-only\"";
        let cfg = Config::load(
            Some(toml),
            &HashMap::new(),
            &ConfigOverrides::default(),
            Some(DeploymentType::RestOnlyMock),
        )
        .unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::CoreOnly));
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Config
        );
    }

    #[test]
    fn explicit_auto_cli_falls_through_to_detected() {
        // `--deployment-type=auto` explicitly opts into detection. Even
        // with a file pin to `core-only`, the auto override means the
        // detected type wins; source = Auto.
        let toml = "[cluster]\ndeployment_type = \"core-only\"";
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("auto".into()),
            ..Default::default()
        };
        let cfg = Config::load(
            Some(toml),
            &HashMap::new(),
            &overrides,
            Some(DeploymentType::RestOnlyMock),
        )
        .unwrap();
        assert_eq!(
            cfg.cluster.deployment_type,
            Some(DeploymentType::RestOnlyMock)
        );
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Auto
        );
    }

    #[test]
    fn auto_with_no_detection_yields_unresolved() {
        // Auto path with no detection (e.g., probe failed) → no resolved
        // type; banner reads `auto`.
        let cfg = Config::load(None, &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert!(cfg.cluster.deployment_type.is_none());
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Auto
        );
    }

    #[test]
    fn detected_rest_only_mock_drives_capability_bundle_with_file_override_warning() {
        // Closure case: legacy file pin to `forge-system` with no
        // deployment-type declaration. Detection resolves to
        // RestOnlyMock; the resolved namespace comes from the file
        // (higher layer than bundle), but the override-conflict warning
        // fires because the file value contradicts the bundle's default.
        let toml = "[cluster]\nnamespace = \"forge-system\"";
        let cfg = Config::load(
            Some(toml),
            &HashMap::new(),
            &ConfigOverrides::default(),
            Some(DeploymentType::RestOnlyMock),
        )
        .unwrap();
        assert_eq!(cfg.cluster.namespace, "forge-system");
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("cluster.namespace=forge-system"));
        assert!(warnings[0].contains("rest-only-mock"));
        assert!(warnings[0].contains("nico-rest"));
    }

    #[test]
    fn cli_force_silences_detection_and_warnings() {
        // `--deployment-type=force` short-circuits the bundle layer
        // entirely (Force returns None from every default-method) and
        // silences override warnings. Detection result is ignored.
        let toml = "[cluster]\nnamespace = \"weird-ns\"";
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("force".into()),
            ..Default::default()
        };
        let cfg = Config::load(
            Some(toml),
            &HashMap::new(),
            &overrides,
            Some(DeploymentType::RestOnlyMock),
        )
        .unwrap();
        assert_eq!(cfg.cluster.deployment_type, Some(DeploymentType::Force));
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Force
        );
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    // PRD-001 slice 10: precedence chain for `cluster.temporal_namespace`
    //   hardcoded ("temporal") < bundle (deployment-type) < file < env < CLI

    #[test]
    fn temporal_k8s_namespace_defaults_to_temporal_when_no_sources() {
        let cfg = Config::load(None, &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "temporal");
    }

    #[test]
    fn temporal_k8s_namespace_bundle_layer_supplies_value_for_full() {
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "temporal");
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    #[test]
    fn temporal_k8s_namespace_bundle_layer_falls_through_for_core_only() {
        // CoreOnly returns None from default_temporal_k8s_namespace —
        // falls through to the hardcoded "temporal" fallback.
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("core-only".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "temporal");
        // CoreOnly's None means override-conflict warning never fires
        // for this key, even when the resolved value differs from a
        // hypothetical default.
        assert!(cfg.override_conflict_warnings().is_empty());
    }

    #[test]
    fn temporal_k8s_namespace_from_file() {
        let toml = "[cluster]\ntemporal_namespace = \"my-temporal\"";
        let cfg = Config::load(Some(toml), &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "my-temporal");
    }

    #[test]
    fn temporal_k8s_namespace_from_env() {
        let mut env = HashMap::new();
        env.insert(
            "NICO_TEMPORAL_K8S_NAMESPACE".to_string(),
            "env-temporal".to_string(),
        );
        let cfg = Config::load(None, &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "env-temporal");
    }

    #[test]
    fn temporal_k8s_namespace_env_overrides_file() {
        let toml = "[cluster]\ntemporal_namespace = \"file-temporal\"";
        let mut env = HashMap::new();
        env.insert(
            "NICO_TEMPORAL_K8S_NAMESPACE".to_string(),
            "env-temporal".to_string(),
        );
        let cfg = Config::load(Some(toml), &env, &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "env-temporal");
    }

    #[test]
    fn temporal_k8s_namespace_cli_overrides_env_and_file() {
        let toml = "[cluster]\ntemporal_namespace = \"file-temporal\"";
        let mut env = HashMap::new();
        env.insert(
            "NICO_TEMPORAL_K8S_NAMESPACE".to_string(),
            "env-temporal".to_string(),
        );
        let overrides = ConfigOverrides {
            temporal_k8s_namespace: Some("cli-temporal".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &env, &overrides, None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "cli-temporal");
    }

    #[test]
    fn temporal_k8s_namespace_file_overrides_bundle() {
        // file pin to a non-default namespace beats deployment-type
        // bundle's default. Override-conflict warning fires.
        let toml = "[cluster]\ntemporal_namespace = \"temporal-prod\"";
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "temporal-prod");
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("cluster.temporal_namespace=temporal-prod"));
        assert!(warnings[0].contains("full"));
        assert!(warnings[0].contains("temporal"));
    }

    #[test]
    fn temporal_k8s_namespace_no_warn_when_default_method_returns_none() {
        // CoreOnly returns None — no warning ever fires for this key
        // regardless of resolved value, mirroring the existing Option
        // handling for the other capability defaults.
        let toml = "[cluster]\ntemporal_namespace = \"weird-ns\"";
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("core-only".into()),
            ..Default::default()
        };
        let cfg = Config::load(Some(toml), &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.temporal_namespace, "weird-ns");
        let warnings = cfg.override_conflict_warnings();
        // None of the warnings should mention temporal_namespace.
        assert!(
            warnings.iter().all(|w| !w.contains("temporal_namespace")),
            "expected no temporal_namespace warning under core-only; got: {warnings:?}"
        );
    }

    #[test]
    fn deployment_type_default_only_applies_when_no_higher_layer_set() {
        // Hardcoded default for namespace is "nico". Without a
        // deployment-type, that's the resolved value.
        let cfg = Config::load(None, &HashMap::new(), &ConfigOverrides::default(), None).unwrap();
        assert_eq!(cfg.cluster.namespace, "nico");
        // With deployment-type=full, the deployment-type default replaces
        // the hardcoded fallback (no file/env/CLI in play).
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("full".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &HashMap::new(), &overrides, None).unwrap();
        assert_eq!(cfg.cluster.namespace, "forge-system");
    }
}
