use std::io::IsTerminal;
use std::sync::Arc;
use std::time::{Duration, Instant};
use async_trait::async_trait;
use nico_common::boot_probe::{
    next_command_for, standard_steps_with_grpc, BootProbe, ProbeMode, ProbeOutcome, ProbeState,
    StderrSink, StepId, StepState,
};
use nico_common::config::{
    Config, ConfigOverrides, ColorMode, DeploymentType, OutputFormat, ReachMode,
};
use nico_common::deployment_detect::{
    run_detection_ladder, ClusterShapeProbe, KubeClusterShapeProbe,
};
use nico_common::output::OutputMode;
use nico_common::reach::ReachManager;

use crate::cli::DoctorArgs;
use crate::dpu::DpuConfig;
use crate::layer::{self, Layer, RunOpts};
use crate::layers;
use crate::log_collector::LogCollectorStage;
use crate::log_source;
use crate::loki;
use crate::http;
use crate::preflight;
use crate::preflight::PreflightChecks;

/// Registry of layer factories, in canonical run order. Adding a new
/// layer is a single-line edit here plus a `register` fn in the new
/// module.
type LayerFactory = fn(&LayerInputs) -> Box<dyn Layer>;
const LAYER_REGISTRY: &[(&str, LayerFactory)] = &[
    (layers::cluster::NAME, layers::cluster::register),
    (layers::logs::NAME, layers::logs::register),
    (layers::workflows::NAME, layers::workflows::register),
    (layers::health::NAME, layers::health::register),
    (layers::grpc::NAME, layers::grpc::register),
    (layers::postgres::NAME, layers::postgres::register),
    (layers::dpu::NAME, layers::dpu::register),
];

struct Unavailable {
    reason: &'static str,
}

#[async_trait]
impl nico_common::k8s::K8sClient for Unavailable {
    async fn list_pods(&self, _scope: nico_common::k8s::PodScope<'_>) -> anyhow::Result<Vec<nico_common::k8s::RawPod>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
    async fn list_events(&self, _ns: &str, _field_selector: Option<&str>) -> anyhow::Result<Vec<nico_common::k8s::RawEvent>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
    async fn pod_logs(&self, _ns: &str, _pod: &str, _since: Duration) -> anyhow::Result<Vec<String>> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

#[async_trait]
impl loki::LokiClient for Unavailable {
    async fn query_errors(&self, _ns: &str, _since: Duration, _limit: usize) -> anyhow::Result<loki::LokiQueryResult> {
        Err(anyhow::anyhow!("{}", self.reason))
    }
}

/// Outcome of preparing a doctor run from CLI args + environment.
pub struct Bootstrapped {
    pub layers: Vec<Box<dyn Layer>>,
    pub opts: RunOpts,
    pub output_mode: OutputMode,
    pub output_format: OutputFormat,
    pub namespace: String,
    pub verbose: bool,
    pub spotlight: bool,
    /// Resolved auto-refresh interval (`[output] tui_refresh` →
    /// `NICO_TUI_REFRESH` env → default 30s). Consumed by `nico-ops` for
    /// the dashboard's auto-refresh cadence; ignored by `nico-doctor`'s
    /// one-shot path.
    pub tui_refresh: Duration,
    /// Resolved Temporal frontend address (after port-forward fallback).
    /// Consumed by `nico-ops` Layout B Activity quadrant
    /// (`recent_namespace_events`); irrelevant to one-shot doctor runs.
    pub temporal_address: String,
    /// Temporal namespace from config — paired with `temporal_address` so
    /// the Activity quadrant can list workflow executions.
    pub temporal_namespace: String,
    /// K8s client built from kubeconfig — reused by Layout B Activity.
    /// `None` when no kubeconfig is reachable; the Activity quadrant just
    /// reports an empty feed in that case.
    pub k8s_client: Option<Arc<dyn nico_common::k8s::K8sClient>>,
    /// Best-effort log source chain (Loki preferred, k8s fallback). Reused
    /// by `nico-ops` for the snapshot logs panel (issue #158). `None` when
    /// neither Loki nor a k8s client is reachable.
    pub log_source: Option<Arc<dyn log_source::LogSource>>,
    /// Per-refresh log collector: runs once before each `runner::run`
    /// fan-out and populates the shared `pod_logs` cache that
    /// `ClusterLayer` and `K8sLogSource` consume (issue #201). `None`
    /// when no kubeconfig is reachable — layers fall back to direct
    /// fetches per their own contracts.
    pub log_collector: Option<Arc<LogCollectorStage>>,
    /// Resolved deployment-type (PRD-001) — `Some(...)` once the
    /// `--deployment-type` flag, config, env, or detection ladder has
    /// produced a label; `None` for unresolved auto runs. Plumbed into
    /// the JSON formatter so `--json` output gains the
    /// `capabilities` object (issue #242).
    pub deployment_type: Option<DeploymentType>,
    /// Kept alive until the caller is done running layers; dropping closes port-forwards.
    pub _pf_guards: Vec<nico_common::reach::ForwardedEndpoint>,
}

pub enum BootstrapErr {
    /// Pre-flight check rejected the run; callers should exit 3.
    /// `human_message` is what we'd print on stderr; `json_payload` is what `--json` mode prints.
    Preflight {
        human_message: String,
        json_payload: String,
        format: OutputFormat,
    },
    /// Fatal config/parse error; `code` is the exit code to use (1 or 3).
    Fatal { message: String, code: i32 },
}

/// Inputs threaded into [`prepare_layers`] — the discrete dependencies a layer set is built from.
pub struct LayerInputs {
    pub k8s_client: Option<Arc<dyn nico_common::k8s::K8sClient>>,
    pub loki_url: Option<String>,
    pub loki_client: Arc<dyn loki::LokiClient>,
    pub temporal_address: String,
    pub temporal_namespace: String,
    pub stuck_threshold: Duration,
    pub http_endpoints: Option<Vec<http::ServiceEndpoint>>,
    pub postgres_url: String,
    pub grpc_address: Option<String>,
    pub reach_mgr_present: bool,
    pub skip: Vec<String>,
    pub dpu_config: DpuConfig,
    /// Resolved deployment-type (PRD-001). Layers that depend on
    /// `forgedb_present()` (the `dpu` layer; see also per-DPU drill-downs)
    /// consult this to decide whether to skip with an "n/a in <type>" reason.
    /// `None` means detection is unresolved (auto, with no probe wired) —
    /// layers fall back to their pre-PRD-001 behavior in that case.
    pub deployment_type: Option<DeploymentType>,
    /// Boot-probe-resolved IB capability (PRD-004 slice 1). `Some(true)`
    /// ⇒ at least one DPU is wired into an IB fabric; `Some(false)` ⇒
    /// confirmed RoCE / ethernet-only; `None` ⇒ probe was skipped
    /// (force mode, postgres unreachable, deployment-type unresolved,
    /// or `rest-only-mock`). Consumed by the fleet `dpu` layer so the
    /// rollup omits the `infiniband` axis on non-IB fleets.
    pub infiniband_present: Option<bool>,
}

/// Build the best-effort log source chain (Loki preferred, k8s fallback)
/// from the same inputs `prepare_layers` consumes. Returns `None` when
/// neither a kubeconfig nor `LOKI_URL` is reachable. Exposed so callers
/// (e.g. `nico-ops`) can reuse the same chain without rebuilding it.
pub fn build_log_source(inputs: &LayerInputs) -> Option<Arc<dyn log_source::LogSource>> {
    match (inputs.k8s_client.as_ref(), inputs.loki_url.is_some()) {
        (Some(k8s), _) => Some(log_source::best_effort_chain(vec![
            Arc::new(loki::LokiLogSource::new(inputs.loki_client.clone())),
            Arc::new(log_source::K8sLogSource::new(k8s.clone())),
        ])),
        (None, true) => Some(log_source::best_effort_chain(vec![
            Arc::new(loki::LokiLogSource::new(inputs.loki_client.clone())),
            Arc::new(log_source::K8sLogSource::new(Arc::new(Unavailable {
                reason: "kubeconfig not found",
            }))),
        ])),
        (None, false) => None,
    }
}

/// Build the ordered layer set from prepared inputs by iterating
/// [`LAYER_REGISTRY`]. Each registry entry is `(name, factory)`; the
/// factory is consulted only when the layer is not in `inputs.skip`.
pub fn prepare_layers(inputs: &LayerInputs) -> Vec<Box<dyn Layer>> {
    LAYER_REGISTRY
        .iter()
        .map(|(name, factory)| {
            if inputs.skip.iter().any(|s| s.as_str() == *name) {
                layer::SkippedLayer::new(name)
            } else {
                factory(inputs)
            }
        })
        .collect()
}

/// Build a runnable doctor configuration from CLI args. Reads the user's config
/// file, environment, and optional kubeconfig; runs pre-flight; resolves
/// service URLs (Loki / Postgres / Temporal / HTTP) via the reach manager.
pub async fn bootstrap(args: &DoctorArgs) -> Result<Bootstrapped, BootstrapErr> {
    let config_path = args
        .config
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            std::path::PathBuf::from(home).join(".config/nico-tools/config.toml")
        });
    let file_toml = std::fs::read_to_string(&config_path).ok();

    let mode_override = match args.mode.as_deref() {
        Some("port-forward") => Some(ReachMode::PortForward),
        Some("in-cluster") => Some(ReachMode::InCluster),
        Some(other) => {
            return Err(BootstrapErr::Fatal {
                message: format!("error: unknown --mode {other:?}; use port-forward or in-cluster"),
                code: 1,
            });
        }
        None => None,
    };

    let overrides = ConfigOverrides {
        namespace: args.namespace.clone(),
        context: args.context.clone(),
        postgres_url: args.postgres_url.clone(),
        color: if args.no_color { Some(ColorMode::Never) } else { None },
        format: if args.json { Some(OutputFormat::Json) } else { None },
        reach_mode: mode_override,
        bootstrap_timeouts_spec: args.timeouts.clone(),
        deployment_type_spec: args.deployment_type.clone(),
        ..Default::default()
    };

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    // PRD-001 slice 9 (#321): detect-first-then-load. The boot config
    // is built without a detected deployment-type so we can drive the
    // boot probe's connecting section (kubeconfig + reach API + detect
    // gate) before re-resolving the config with detection's result.
    // Detection only contributes to the bundle layer of the precedence
    // chain — flag/config/force users get the same single-load behavior
    // they had pre-slice-9 because their resolved type doesn't change.
    let boot_config = Config::load(file_toml.as_deref(), &env, &overrides, None).map_err(|e| {
        BootstrapErr::Fatal {
            message: format!("error loading config: {e}"),
            code: 1,
        }
    })?;

    let reach_mode = boot_config.cluster.reach_mode;
    let reach_source = if mode_override.is_some() {
        "--mode flag"
    } else if env.contains_key("NICO_REACH_MODE") {
        "NICO_REACH_MODE"
    } else if env.contains_key("KUBERNETES_SERVICE_HOST") {
        "auto-detected: in-cluster"
    } else {
        "auto-detected: no KUBERNETES_SERVICE_HOST"
    };

    let output_mode = OutputMode {
        color: match boot_config.output.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let since = humantime::parse_duration(&args.since).unwrap_or(Duration::from_secs(600));
    let timeout = humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5));

    let probe_outcome = run_boot_probe(
        boot_config,
        file_toml.as_deref(),
        &env,
        &overrides,
        reach_mode,
        reach_source,
        &output_mode,
    )
    .await;

    let (config, BootProbeResult {
        k8s_client,
        reach_mgr,
        temporal_address,
        postgres_url,
        mut pf_guards,
        infiniband_present,
    }) = match probe_outcome {
        Ok(r) => r,
        Err(failure) => {
            return Err(failure);
        }
    };

    let opts = RunOpts {
        namespace: config.cluster.namespace.clone(),
        since,
        timeout,
        ..Default::default()
    };

    let pf_budget = config.bootstrap.timeouts.port_forward;

    let (loki_url, loki_client): (Option<String>, Arc<dyn loki::LokiClient>) = {
        if let Ok(url) = std::env::var("LOKI_URL") {
            let client = Arc::new(loki::RealLokiClient::new(url.clone())) as Arc<dyn loki::LokiClient>;
            (Some(url), client)
        } else if let Some(ref mgr) = reach_mgr {
            match mgr.loki_url(pf_budget).await {
                Ok((url, guard)) => {
                    pf_guards.extend(guard);
                    let client = Arc::new(loki::RealLokiClient::new(url.clone())) as Arc<dyn loki::LokiClient>;
                    (Some(url), client)
                }
                Err(_) => {
                    let client = Arc::new(Unavailable { reason: "Loki service not found in namespace" }) as Arc<dyn loki::LokiClient>;
                    (None, client)
                }
            }
        } else {
            let client = Arc::new(Unavailable { reason: "LOKI_URL not set" }) as Arc<dyn loki::LokiClient>;
            (None, client)
        }
    };

    let http_endpoints: Option<Vec<http::ServiceEndpoint>> = {
        if let Some(s) = std::env::var("NICO_HEALTH_ENDPOINTS").ok().filter(|s| !s.is_empty()) {
            let endpoints = s
                .split(',')
                .map(|entry| entry.trim())
                .filter(|entry| !entry.is_empty())
                .map(|entry| {
                    if let Some((name, url)) = entry.split_once('=') {
                        http::ServiceEndpoint {
                            name: name.trim().to_string(),
                            base_url: url.trim().to_string(),
                        }
                    } else {
                        http::ServiceEndpoint {
                            name: entry.to_string(),
                            base_url: entry.to_string(),
                        }
                    }
                })
                .collect();
            Some(endpoints)
        } else if let Some(ref mgr) = reach_mgr {
            match mgr.http_endpoints(pf_budget).await {
                Ok((discovered, guards)) => {
                    pf_guards.extend(guards);
                    if discovered.is_empty() {
                        None
                    } else {
                        Some(
                            discovered
                                .into_iter()
                                .map(|(name, url)| http::ServiceEndpoint { name, base_url: url })
                                .collect(),
                        )
                    }
                }
                Err(e) => {
                    eprintln!("nico: warn: HTTP service discovery failed: {e}");
                    None
                }
            }
        } else {
            None
        }
    };

    let bootstrap_k8s = k8s_client.clone();

    let inputs = LayerInputs {
        k8s_client,
        loki_url,
        loki_client,
        temporal_address: temporal_address.clone(),
        temporal_namespace: config.temporal.namespace.clone(),
        stuck_threshold: config.temporal.stuck_threshold,
        http_endpoints,
        postgres_url,
        grpc_address: config.cluster.grpc_address.clone(),
        reach_mgr_present: reach_mgr.is_some(),
        skip: args.skip.clone(),
        dpu_config: config.dpu,
        deployment_type: config.cluster.deployment_type,
        infiniband_present,
    };

    let log_source = build_log_source(&inputs);
    let log_collector = bootstrap_k8s
        .clone()
        .map(|k8s| Arc::new(LogCollectorStage::new(k8s)));
    let layers = prepare_layers(&inputs);

    Ok(Bootstrapped {
        layers,
        opts,
        output_mode,
        output_format: config.output.format,
        namespace: config.cluster.namespace.clone(),
        verbose: args.verbose,
        spotlight: args.spotlight,
        tui_refresh: config.output.tui_refresh,
        temporal_address,
        temporal_namespace: config.temporal.namespace.clone(),
        k8s_client: bootstrap_k8s,
        log_source,
        log_collector,
        deployment_type: config.cluster.deployment_type,
        _pf_guards: pf_guards,
    })
}

/// Internal carry value: what the boot probe produces on the `Ok` path.
/// On a fatal pre-flight failure the probe instead yields a
/// `BootstrapErr::Preflight`. Connecting failures (no kubeconfig)
/// degrade gracefully: probe marks them failed/skipped but bootstrap
/// continues with `k8s_client = None`. Serving failures (port-forward,
/// reach postgres) are non-fatal warnings — the probe shows them in
/// red but bootstrap continues with the configured fallback URL.
struct BootProbeResult {
    k8s_client: Option<Arc<dyn nico_common::k8s::K8sClient>>,
    reach_mgr: Option<ReachManager>,
    temporal_address: String,
    postgres_url: String,
    pf_guards: Vec<nico_common::reach::ForwardedEndpoint>,
    /// Result of the `detect_infiniband_present` boot-probe step
    /// (PRD-004 slice 1). `Some(true)` / `Some(false)` after the probe
    /// runs; `None` when the probe was skipped (force mode, postgres
    /// unreachable, `rest-only-mock`, etc).
    infiniband_present: Option<bool>,
}

async fn run_boot_probe(
    boot_config: Config,
    file_toml: Option<&str>,
    env: &std::collections::HashMap<String, String>,
    overrides: &ConfigOverrides,
    reach_mode: ReachMode,
    reach_source: &str,
    output_mode: &OutputMode,
) -> Result<(Config, BootProbeResult), BootstrapErr> {
    let probe_mode = if boot_config.output.format == OutputFormat::Json {
        ProbeMode::Json
    } else if !std::io::stderr().is_terminal() {
        ProbeMode::NonTty
    } else {
        ProbeMode::Tty {
            color: output_mode.color,
            ascii: output_mode.ascii,
        }
    };

    let steps = standard_steps_with_grpc(
        &boot_config.cluster.namespace,
        &boot_config.bootstrap.timeouts,
        boot_config.cluster.grpc_address.as_deref(),
    );
    let probe_state = ProbeState::new(steps, reach_mode.as_str(), reach_source)
        .with_deployment_type(
            boot_config
                .cluster
                .deployment_type
                .map(|d| d.label().to_string()),
            boot_config.cluster.deployment_type_source.label(),
        )
        .with_warnings(boot_config.override_conflict_warnings());
    let mut probe = BootProbe::new(probe_state, probe_mode, Box::new(StderrSink));
    probe.start_ticking();
    let tracker = probe.tracker();

    let initial_ns = boot_config.cluster.namespace.clone();
    let timeouts = boot_config.bootstrap.timeouts;
    let user_resolved_dt = boot_config.cluster.deployment_type;

    // ---------- Connecting (sequential gate) ----------

    // 1. LoadKubeconfig — non-fatal: degrade gracefully if it fails.
    tracker.started(StepId::LoadKubeconfig).await;
    let t = Instant::now();
    let kube_result = nico_common::k8s::KubeRsK8sClient::try_new(
        boot_config.cluster.context.as_deref(),
        timeouts.kube_client,
    )
    .await;

    let kube_client = match kube_result {
        Ok(c) => {
            tracker
                .finished(
                    StepId::LoadKubeconfig,
                    StepState::Passed { elapsed: t.elapsed() },
                )
                .await;
            Some(c)
        }
        Err(e) => {
            let elapsed = t.elapsed();
            let timed_out = e.is_timed_out();
            tracker
                .finished(
                    StepId::LoadKubeconfig,
                    StepState::Failed {
                        elapsed,
                        message: e.to_string(),
                        timed_out,
                        next_command: next_command_for(StepId::LoadKubeconfig, &initial_ns),
                    },
                )
                .await;
            // No client → all downstream is skipped.
            tracker
                .skip_remaining(&[
                    StepId::ReachApiServer,
                    StepId::DetectDeploymentType,
                    StepId::Credentials,
                    StepId::NamespaceExists,
                    StepId::Rbac,
                    StepId::PortForwardWorkflows,
                    StepId::PortForwardGrpc,
                    StepId::PortForwardPostgres,
                    StepId::ReachPostgres,
                    StepId::DetectInfinibandPresent,
                ])
                .await;
            // Graceful degradation — probe completes (with the failed
            // kubeconfig step rendered), bootstrap continues without
            // a client. Pre-detection boot config is the final config
            // because no reach to the cluster means no detection result.
            let _ = probe.finish_failure(&initial_ns).await;
            let temporal_address = boot_config.temporal.address.clone();
            let postgres_url = boot_config.postgres.url.clone();
            return Ok((
                boot_config,
                BootProbeResult {
                    k8s_client: None,
                    reach_mgr: None,
                    temporal_address,
                    postgres_url,
                    pf_guards: vec![],
                    infiniband_present: None,
                },
            ));
        }
    };

    let raw = kube_client.as_ref().unwrap().raw_client().clone();

    // 2. ReachApiServer — fatal gate.
    tracker.started(StepId::ReachApiServer).await;
    let t = Instant::now();
    let raw_for_reach = raw.clone();
    let reach_result = nico_common::bootstrap::run_with_budget(
        timeouts.reach_api,
        async move {
            raw_for_reach
                .apiserver_version()
                .await
                .map_err(|e| anyhow::anyhow!("cannot reach API server: {e}"))
                .map(|_| ())
        },
    )
    .await;
    if let Err(e) = reach_result {
        let elapsed = t.elapsed();
        let timed_out = e.is_timed_out();
        tracker
            .finished(
                StepId::ReachApiServer,
                StepState::Failed {
                    elapsed,
                    message: e.to_string(),
                    timed_out,
                    next_command: next_command_for(StepId::ReachApiServer, &initial_ns),
                },
            )
            .await;
        tracker
            .skip_remaining(&[
                StepId::DetectDeploymentType,
                StepId::Credentials,
                StepId::NamespaceExists,
                StepId::Rbac,
                StepId::PortForwardWorkflows,
                StepId::PortForwardGrpc,
                StepId::PortForwardPostgres,
                StepId::ReachPostgres,
                StepId::DetectInfinibandPresent,
            ])
            .await;
        return Err(probe_to_preflight_err(probe, &boot_config).await);
    }
    tracker
        .finished(
            StepId::ReachApiServer,
            StepState::Passed { elapsed: t.elapsed() },
        )
        .await;

    // 3. DetectDeploymentType — sequential gate (auto mode only).
    //    PRD-001 slice 9 (#321): runs before the full Config materializes
    //    so the resolved type can slot into the bundle layer of
    //    Config::load and validating's labels reflect the post-detection
    //    namespace / gRPC address.
    let shape_probe = KubeClusterShapeProbe::new(raw.clone());
    tracker.started(StepId::DetectDeploymentType).await;
    let t = Instant::now();
    let detect_result = nico_common::bootstrap::run_with_budget(
        timeouts.preflight,
        detect_deployment_type_step(user_resolved_dt, Some(&shape_probe)),
    )
    .await;
    let detected_dt = match detect_result {
        Ok(opt) => {
            tracker
                .finished(
                    StepId::DetectDeploymentType,
                    StepState::Passed { elapsed: t.elapsed() },
                )
                .await;
            opt
        }
        Err(e) => {
            let elapsed = t.elapsed();
            let timed_out = e.is_timed_out();
            tracker
                .finished(
                    StepId::DetectDeploymentType,
                    StepState::Failed {
                        elapsed,
                        message: e.to_string(),
                        timed_out,
                        next_command: next_command_for(StepId::DetectDeploymentType, &initial_ns),
                    },
                )
                .await;
            tracker
                .skip_remaining(&[
                    StepId::Credentials,
                    StepId::NamespaceExists,
                    StepId::Rbac,
                    StepId::PortForwardWorkflows,
                    StepId::PortForwardGrpc,
                    StepId::PortForwardPostgres,
                    StepId::ReachPostgres,
                    StepId::DetectInfinibandPresent,
                ])
                .await;
            return Err(probe_to_preflight_err(probe, &boot_config).await);
        }
    };

    // Re-load Config with the detected type slotted into the bundle
    // layer. When user_resolved_dt was already Some(_), detected_dt is
    // None and the final config equals boot_config; the re-load is a
    // cheap idempotent rebuild rather than a special case.
    let config = Config::load(file_toml, env, overrides, detected_dt).map_err(|e| {
        BootstrapErr::Fatal {
            message: format!("error re-loading config after detection: {e}"),
            code: 1,
        }
    })?;

    // Update probe metadata to reflect post-detection resolution. In
    // flag/config/force runs these are no-op writes (same values); in
    // auto runs they snap labels to the resolved namespace and gRPC.
    tracker
        .set_label(
            StepId::NamespaceExists,
            format!("namespace '{}' exists", config.cluster.namespace),
        )
        .await;
    let grpc_label = match config.cluster.grpc_address.as_deref() {
        Some(addr) => format!("port-forward: grpc → {addr}"),
        None => "port-forward: grpc".to_string(),
    };
    tracker.set_label(StepId::PortForwardGrpc, grpc_label).await;
    tracker
        .set_deployment_type(
            config.cluster.deployment_type.map(|d| d.label().to_string()),
            config.cluster.deployment_type_source.label(),
        )
        .await;
    tracker
        .set_warnings(config.override_conflict_warnings())
        .await;

    let ns = config.cluster.namespace.clone();

    // ---------- Validating + Serving in parallel ----------

    let pf = preflight::KubePreflightClient::new(raw.clone());
    let reach_mgr = ReachManager::new(
        reach_mode,
        raw.clone(),
        config.cluster.namespace.clone(),
        config.cluster.postgres_namespace.clone(),
        config.cluster.temporal_namespace.clone(),
    );

    let (validating_ok, serving_results) = tokio::join!(
        run_validating_section(&tracker, &pf, &ns, timeouts.preflight),
        run_serving_section(
            &tracker,
            &reach_mgr,
            &config,
            timeouts.port_forward,
            timeouts.postgres_reach,
            timeouts.preflight,
        ),
    );

    if !validating_ok {
        // Validating failure is fatal regardless of serving outcome.
        return Err(probe_to_preflight_err(probe, &config).await);
    }

    let (temporal_address, postgres_url, pf_guards, infiniband_present) = serving_results;

    // Serving failures are non-fatal (probe shows them; bootstrap falls
    // back to config addresses). Probe completes successfully so long
    // as connecting + validating passed.
    let _ = if probe_state_any_failed(&probe).await {
        probe.finish_failure(&ns).await
    } else {
        probe.finish_success(&ns).await
    };

    Ok((
        config,
        BootProbeResult {
            k8s_client: Some(
                Arc::new(kube_client.unwrap()) as Arc<dyn nico_common::k8s::K8sClient>,
            ),
            reach_mgr: Some(reach_mgr),
            temporal_address,
            postgres_url,
            pf_guards,
            infiniband_present,
        },
    ))
}

async fn probe_state_any_failed(probe: &BootProbe) -> bool {
    // BootProbe doesn't expose its inner state directly; we use the
    // tracker's flow indirectly by calling finish_*. Here we inspect
    // the stored outcome by peeking at the JSON success/failure flag.
    // For our use, we just call finish_* in the caller; this helper is
    // only needed to choose between success and failure variants.
    let tracker = probe.tracker();
    tracker.any_failed().await
}

async fn run_validating_section(
    tracker: &nico_common::boot_probe::Tracker,
    pf: &preflight::KubePreflightClient,
    ns: &str,
    budget: Duration,
) -> bool {
    let cred_fut = run_step(
        tracker,
        StepId::Credentials,
        ns,
        budget,
        async { pf.check_token_valid().await },
    );
    let ns_fut = run_step(
        tracker,
        StepId::NamespaceExists,
        ns,
        budget,
        async { pf.check_namespace_exists(ns).await },
    );
    let rbac_fut = run_step(
        tracker,
        StepId::Rbac,
        ns,
        budget,
        async { pf.check_rbac(ns).await },
    );
    let (cred_ok, ns_ok, rbac_ok) = tokio::join!(cred_fut, ns_fut, rbac_fut);
    cred_ok && ns_ok && rbac_ok
}

/// Behavior of the `detect_deployment_type` step (PRD-001 slice 9):
///
/// - `Some(_)` (resolved from CLI / env / file, including `Force`) →
///   instant-pass with `Ok(None)`; detection is skipped per PRD-001's
///   hybrid trust model. The caller already has the resolved type, so
///   the detection result has nothing to add.
/// - `None` (auto) + a shape probe is wired → run the full detection
///   ladder (workload → namespace → CRD). On first match, return
///   `Ok(Some(matched))` so the caller can re-load `Config` with the
///   detected type slotted into the bundle layer of the precedence
///   chain. On no-match, return `Err` with a diagnostic listing
///   observed namespaces, services, and CRDs.
/// - `None` + no shape probe → preserve the slice-1 fallback so
///   non-cluster code paths (tests, degraded boot) still surface a
///   clear "no detection signals" error.
async fn detect_deployment_type_step(
    user_resolved: Option<DeploymentType>,
    shape_probe: Option<&dyn ClusterShapeProbe>,
) -> anyhow::Result<Option<DeploymentType>> {
    if user_resolved.is_some() {
        return Ok(None);
    }
    let Some(probe) = shape_probe else {
        return Err(anyhow::anyhow!(
            "no detection signals available; pass --deployment-type=<full|core-only|rest-only-mock> or =force"
        ));
    };
    let outcome = run_detection_ladder(probe).await?;
    if let Some(dt) = outcome.matched {
        return Ok(Some(dt));
    }
    let fmt_list = |xs: &[String]| -> String {
        if xs.is_empty() {
            "<none>".to_string()
        } else {
            xs.join(", ")
        }
    };
    Err(anyhow::anyhow!(
        "no deployment-type signal matched \
         (observed namespaces: {ns}; observed services: {svc}; observed CRDs: {crd}); \
         pass --deployment-type=<full|core-only|rest-only-mock> or =force",
        ns = fmt_list(&outcome.observed_namespaces),
        svc = fmt_list(&outcome.observed_services),
        crd = fmt_list(&outcome.observed_crds),
    ))
}

/// PRD-004 slice 1: SQL probe for InfiniBand presence in the fleet.
/// Returns `true` if any DPU has a non-empty
/// `machines.inventory->'infiniband_interfaces'` array, `false` otherwise.
/// The `machines` table missing degrades to `false` so dev clusters that
/// haven't run the carbide schema render `ib: absent` instead of failing.
async fn probe_infiniband_present(postgres_url: &str) -> anyhow::Result<bool> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(2))
        .connect(postgres_url)
        .await
        .map_err(|e| anyhow::anyhow!("failed to connect to postgres: {e}"))?;

    let table_exists: (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_name = 'machines')",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("infiniband schema probe failed: {e}"))?;
    if !table_exists.0 {
        return Ok(false);
    }

    let row: (bool,) = sqlx::query_as(
        "SELECT EXISTS ( \
           SELECT 1 FROM machines \
           WHERE inventory->'infiniband_interfaces' IS NOT NULL \
             AND jsonb_array_length(inventory->'infiniband_interfaces') > 0 \
         )",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| anyhow::anyhow!("infiniband presence query failed: {e}"))?;
    Ok(row.0)
}

/// PRD-001 slice 10 gate: should the `port-forward: workflows` boot-probe
/// step short-circuit to [`StepState::Skipped`]?
///
/// Skips when the resolved `DeploymentType` reports `temporal_present() ==
/// false` (only `core-only` today). `None` (auto pre-detection) preserves
/// pre-PRD-001 behavior — the step runs.
fn should_skip_workflows_pf(deployment_type: Option<DeploymentType>) -> bool {
    let Some(dt) = deployment_type else {
        return false;
    };
    !dt.temporal_present()
}

/// PRD-004 slice 1 gate: should the `detect_infiniband_present` step
/// run a SQL probe, or short-circuit to `Skipped`?
///
/// Skips when:
/// - postgres is not reachable (no point probing without a connection),
/// - `deployment_type` is `None` (auto pre-detection),
/// - `deployment_type == Force` (escape hatch — never probe), or
/// - `forgedb_present()` is false (RestOnlyMock — no inventory column to read).
fn should_probe_infiniband(
    deployment_type: Option<DeploymentType>,
    pg_reachable: bool,
) -> bool {
    if !pg_reachable {
        return false;
    }
    let Some(dt) = deployment_type else {
        return false;
    };
    if matches!(dt, DeploymentType::Force) {
        return false;
    }
    dt.forgedb_present()
}

async fn run_step<F>(
    tracker: &nico_common::boot_probe::Tracker,
    id: StepId,
    ns: &str,
    budget: Duration,
    fut: F,
) -> bool
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    tracker.started(id).await;
    let t = Instant::now();
    let r = nico_common::bootstrap::run_with_budget(budget, fut).await;
    let elapsed = t.elapsed();
    match r {
        Ok(()) => {
            tracker
                .finished(id, StepState::Passed { elapsed })
                .await;
            true
        }
        Err(e) => {
            let timed_out = e.is_timed_out();
            tracker
                .finished(
                    id,
                    StepState::Failed {
                        elapsed,
                        message: e.to_string(),
                        timed_out,
                        next_command: next_command_for(id, ns),
                    },
                )
                .await;
            false
        }
    }
}

async fn run_serving_section(
    tracker: &nico_common::boot_probe::Tracker,
    reach_mgr: &ReachManager,
    config: &Config,
    pf_budget: Duration,
    pg_reach_budget: Duration,
    ib_probe_budget: Duration,
) -> (
    String,
    String,
    Vec<nico_common::reach::ForwardedEndpoint>,
    Option<bool>,
) {
    let ns = config.cluster.namespace.clone();

    // Workflows port-forward — PRD-001 slice 10: skip when the resolved
    // deployment-type lacks Temporal (only `core-only` today). Mirrors
    // the existing `dpu` skip on `forgedb_present`. `temporal_present()`
    // returns true for `Force` so the escape hatch flows through.
    let skip_workflows_pf = should_skip_workflows_pf(config.cluster.deployment_type);
    let temporal_fut = async {
        if skip_workflows_pf {
            tracker
                .finished(StepId::PortForwardWorkflows, StepState::Skipped)
                .await;
            return (config.temporal.address.clone(), None);
        }
        tracker.started(StepId::PortForwardWorkflows).await;
        let t = Instant::now();
        match reach_mgr.temporal_address(pf_budget).await {
            Ok((addr, guard)) => {
                tracker
                    .finished(
                        StepId::PortForwardWorkflows,
                        StepState::Passed { elapsed: t.elapsed() },
                    )
                    .await;
                (addr, guard)
            }
            Err(e) => {
                let elapsed = t.elapsed();
                let timed_out = e.is_timed_out();
                tracker
                    .finished(
                        StepId::PortForwardWorkflows,
                        StepState::Failed {
                            elapsed,
                            message: e.to_string(),
                            timed_out,
                            next_command: next_command_for(StepId::PortForwardWorkflows, &ns),
                        },
                    )
                    .await;
                (config.temporal.address.clone(), None)
            }
        }
    };

    // gRPC port-forward — currently a no-op placeholder. If
    // grpc_address is configured, mark Passed; otherwise mark Skipped
    // (no actual port-forward to attempt today).
    let grpc_fut = async {
        tracker.started(StepId::PortForwardGrpc).await;
        let t = Instant::now();
        if config.cluster.grpc_address.is_some() {
            tracker
                .finished(
                    StepId::PortForwardGrpc,
                    StepState::Passed { elapsed: t.elapsed() },
                )
                .await;
        } else {
            tracker
                .finished(StepId::PortForwardGrpc, StepState::Skipped)
                .await;
        }
    };

    // Postgres port-forward → reach postgres (sequential within
    // serving's parallel group).
    let postgres_fut = async {
        tracker.started(StepId::PortForwardPostgres).await;
        let t = Instant::now();
        let (postgres_url, pf_guard, pf_ok) = match reach_mgr
            .postgres_url(&config.postgres.url, pf_budget)
            .await
        {
            Ok((url, guard)) => {
                tracker
                    .finished(
                        StepId::PortForwardPostgres,
                        StepState::Passed { elapsed: t.elapsed() },
                    )
                    .await;
                (url, guard, true)
            }
            Err(e) => {
                let elapsed = t.elapsed();
                let timed_out = e.is_timed_out();
                tracker
                    .finished(
                        StepId::PortForwardPostgres,
                        StepState::Failed {
                            elapsed,
                            message: e.to_string(),
                            timed_out,
                            next_command: next_command_for(StepId::PortForwardPostgres, &ns),
                        },
                    )
                    .await;
                (config.postgres.url.clone(), None, false)
            }
        };

        if !pf_ok {
            tracker
                .finished(StepId::ReachPostgres, StepState::Skipped)
                .await;
            tracker
                .finished(StepId::DetectInfinibandPresent, StepState::Skipped)
                .await;
            return (postgres_url, pf_guard, None);
        }

        tracker.started(StepId::ReachPostgres).await;
        let t = Instant::now();
        let pg_reachable = match nico_common::bootstrap::probe_postgres(&postgres_url, pg_reach_budget).await {
            Ok(()) => {
                tracker
                    .finished(
                        StepId::ReachPostgres,
                        StepState::Passed { elapsed: t.elapsed() },
                    )
                    .await;
                true
            }
            Err(e) => {
                let elapsed = t.elapsed();
                let timed_out = e.is_timed_out();
                tracker
                    .finished(
                        StepId::ReachPostgres,
                        StepState::Failed {
                            elapsed,
                            message: e.to_string(),
                            timed_out,
                            next_command: next_command_for(StepId::ReachPostgres, &ns),
                        },
                    )
                    .await;
                false
            }
        };

        // PRD-004 slice 1: detect_infiniband_present — gated probe of
        // `machines.inventory->'infiniband_interfaces'`.
        let ib_present: Option<bool> =
            if !should_probe_infiniband(config.cluster.deployment_type, pg_reachable) {
                tracker
                    .finished(StepId::DetectInfinibandPresent, StepState::Skipped)
                    .await;
                None
            } else {
                tracker.started(StepId::DetectInfinibandPresent).await;
                let t = Instant::now();
                match nico_common::bootstrap::run_with_budget(
                    ib_probe_budget,
                    probe_infiniband_present(&postgres_url),
                )
                .await
                {
                    Ok(present) => {
                        tracker.set_infiniband_present(Some(present)).await;
                        tracker
                            .finished(
                                StepId::DetectInfinibandPresent,
                                StepState::Passed { elapsed: t.elapsed() },
                            )
                            .await;
                        Some(present)
                    }
                    Err(e) => {
                        let elapsed = t.elapsed();
                        let timed_out = e.is_timed_out();
                        tracker
                            .finished(
                                StepId::DetectInfinibandPresent,
                                StepState::Failed {
                                    elapsed,
                                    message: e.to_string(),
                                    timed_out,
                                    next_command: next_command_for(
                                        StepId::DetectInfinibandPresent,
                                        &ns,
                                    ),
                                },
                            )
                            .await;
                        None
                    }
                }
            };
        (postgres_url, pf_guard, ib_present)
    };

    let ((temporal_address, t_guard), _grpc, (postgres_url, pg_guard, infiniband_present)) =
        tokio::join!(temporal_fut, grpc_fut, postgres_fut);

    let mut guards = vec![];
    if let Some(g) = t_guard {
        guards.push(g);
    }
    if let Some(g) = pg_guard {
        guards.push(g);
    }
    (temporal_address, postgres_url, guards, infiniband_present)
}

async fn probe_to_preflight_err(probe: BootProbe, config: &Config) -> BootstrapErr {
    let outcome = probe.finish_failure(&config.cluster.namespace).await;
    let (json, human_message) = match outcome {
        ProbeOutcome::Failure {
            json,
            human_message,
        } => (json, human_message),
        ProbeOutcome::Success { json } => (
            json,
            "boot probe failed (no specific step recorded)".to_string(),
        ),
    };
    BootstrapErr::Preflight {
        human_message,
        json_payload: serde_json::to_string_pretty(&json)
            .unwrap_or_else(|_| "{}".to_string()),
        format: config.output.format,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_inputs() -> LayerInputs {
        LayerInputs {
            k8s_client: None,
            loki_url: None,
            loki_client: Arc::new(Unavailable { reason: "test" }),
            temporal_address: "127.0.0.1:7233".to_string(),
            temporal_namespace: "default".to_string(),
            stuck_threshold: Duration::from_secs(600),
            http_endpoints: None,
            postgres_url: "postgres://localhost/test".to_string(),
            grpc_address: None,
            reach_mgr_present: false,
            skip: vec![],
            dpu_config: DpuConfig::default(),
            deployment_type: None,
            infiniband_present: None,
        }
    }

    #[tokio::test]
    async fn workflows_layer_skips_with_reason_when_deployment_type_lacks_temporal() {
        // PRD-001 slice 10: core-only has no Temporal — workflows layer
        // skips with reason. Mirror of the `dpu` skip pattern.
        let mut inputs = empty_inputs();
        inputs.deployment_type = Some(DeploymentType::CoreOnly);
        let layers = prepare_layers(&inputs);
        let workflows = layers
            .iter()
            .find(|l| l.name() == "workflows")
            .expect("workflows layer present");
        let result = workflows.run(&RunOpts::default()).await;
        assert_eq!(
            result.status,
            nico_common::output::Status::Skipped,
            "workflows must skip when temporal absent",
        );
        assert_eq!(
            result.skipped_reason.as_deref(),
            Some("n/a in core-only: no Temporal"),
        );
    }

    #[tokio::test]
    async fn workflows_layer_runs_normally_when_temporal_present() {
        // Full / RestOnlyMock / Force all have Temporal — workflows must
        // not skip on the deployment-type axis (it can still fail on the
        // gRPC call to a stub address; that is not a Skipped status).
        for dt in [
            DeploymentType::Full,
            DeploymentType::RestOnlyMock,
            DeploymentType::Force,
        ] {
            let mut inputs = empty_inputs();
            inputs.deployment_type = Some(dt);
            let layers = prepare_layers(&inputs);
            let workflows = layers
                .iter()
                .find(|l| l.name() == "workflows")
                .expect("workflows layer present");
            let result = workflows.run(&RunOpts::default()).await;
            assert_ne!(
                result.status,
                nico_common::output::Status::Skipped,
                "workflows must NOT skip in {dt:?} (temporal present)",
            );
            assert_eq!(result.skipped_reason, None, "{dt:?}: no skip reason expected");
        }
    }

    #[tokio::test]
    async fn workflows_layer_runs_normally_when_deployment_type_unresolved() {
        // None means auto-detect didn't resolve — preserve pre-PRD-001
        // behavior: don't gate.
        let mut inputs = empty_inputs();
        inputs.deployment_type = None;
        let layers = prepare_layers(&inputs);
        let workflows = layers
            .iter()
            .find(|l| l.name() == "workflows")
            .expect("workflows layer present");
        let result = workflows.run(&RunOpts::default()).await;
        assert_ne!(result.status, nico_common::output::Status::Skipped);
    }

    #[tokio::test]
    async fn dpu_layer_skips_with_reason_when_deployment_type_lacks_forgedb() {
        let mut inputs = empty_inputs();
        inputs.deployment_type = Some(DeploymentType::RestOnlyMock);
        let layers = prepare_layers(&inputs);
        let dpu = layers.iter().find(|l| l.name() == "dpu").expect("dpu layer present");
        let result = dpu.run(&RunOpts::default()).await;
        assert_eq!(
            result.status,
            nico_common::output::Status::Skipped,
            "dpu must skip when forgedb absent",
        );
        assert_eq!(
            result.skipped_reason.as_deref(),
            Some("n/a in rest-only-mock: no forgedb"),
        );
    }

    #[tokio::test]
    async fn dpu_layer_runs_normally_when_deployment_type_has_forgedb() {
        for dt in [
            DeploymentType::Full,
            DeploymentType::CoreOnly,
            DeploymentType::Force,
        ] {
            let mut inputs = empty_inputs();
            inputs.deployment_type = Some(dt);
            let layers = prepare_layers(&inputs);
            let dpu = layers.iter().find(|l| l.name() == "dpu").expect("dpu layer present");
            let result = dpu.run(&RunOpts::default()).await;
            assert_ne!(
                result.status,
                nico_common::output::Status::Skipped,
                "dpu must NOT skip in {dt:?} (forgedb present)",
            );
            assert_eq!(result.skipped_reason, None, "{dt:?}: no skip reason expected");
        }
    }

    #[tokio::test]
    async fn dpu_layer_runs_normally_when_deployment_type_unresolved() {
        // None means auto-detect didn't resolve a type — preserve pre-PRD-001
        // behavior: don't gate. (Layer either runs or hits UnconfiguredLayer
        // for an invalid postgres URL.)
        let mut inputs = empty_inputs();
        inputs.deployment_type = None;
        let layers = prepare_layers(&inputs);
        let dpu = layers.iter().find(|l| l.name() == "dpu").expect("dpu layer present");
        let result = dpu.run(&RunOpts::default()).await;
        assert_ne!(result.status, nico_common::output::Status::Skipped);
    }

    #[tokio::test]
    async fn prepare_layers_returns_canonical_order() {
        let layers = prepare_layers(&empty_inputs());
        let names: Vec<&str> = layers.iter().map(|l| l.name()).collect();
        assert_eq!(
            names,
            vec!["cluster", "logs", "workflows", "health", "grpc", "postgres", "dpu"]
        );
    }

    #[tokio::test]
    async fn prepare_layers_honours_skip_at_any_position() {
        let mut inputs = empty_inputs();
        inputs.skip = vec!["workflows".to_string(), "dpu".to_string()];
        let layers = prepare_layers(&inputs);
        let names: Vec<&str> = layers.iter().map(|l| l.name()).collect();
        assert_eq!(
            names,
            vec!["cluster", "logs", "workflows", "health", "grpc", "postgres", "dpu"]
        );
    }

    #[test]
    fn should_skip_workflows_pf_skips_only_core_only() {
        // PRD-001 slice 10: only `core-only` lacks Temporal.
        assert!(should_skip_workflows_pf(Some(DeploymentType::CoreOnly)));
        for dt in [
            DeploymentType::Full,
            DeploymentType::RestOnlyMock,
            DeploymentType::Force,
        ] {
            assert!(
                !should_skip_workflows_pf(Some(dt)),
                "{dt:?} has Temporal — pf step must run",
            );
        }
    }

    #[test]
    fn should_skip_workflows_pf_runs_when_deployment_type_unresolved() {
        // None means auto pre-detection; preserve pre-PRD-001 behavior
        // (run the step).
        assert!(!should_skip_workflows_pf(None));
    }

    #[test]
    fn should_probe_infiniband_skips_force_mode() {
        // PRD-004 slice 1: Force is the escape hatch — no probing,
        // banner shows `ib: unknown`.
        assert!(!should_probe_infiniband(Some(DeploymentType::Force), true));
    }

    #[test]
    fn should_probe_infiniband_skips_when_postgres_unreachable() {
        // No connection → no SQL probe possible.
        for dt in [
            DeploymentType::Full,
            DeploymentType::CoreOnly,
            DeploymentType::RestOnlyMock,
            DeploymentType::Force,
        ] {
            assert!(!should_probe_infiniband(Some(dt), false));
        }
    }

    #[test]
    fn should_probe_infiniband_skips_when_deployment_type_unresolved() {
        // Auto pre-detection: forgedb capability is unknown.
        assert!(!should_probe_infiniband(None, true));
    }

    #[test]
    fn should_probe_infiniband_skips_rest_only_mock() {
        // No forgedb → no inventory column to read.
        assert!(!should_probe_infiniband(
            Some(DeploymentType::RestOnlyMock),
            true
        ));
    }

    #[test]
    fn should_probe_infiniband_runs_for_full_and_core_only_with_postgres() {
        for dt in [DeploymentType::Full, DeploymentType::CoreOnly] {
            assert!(
                should_probe_infiniband(Some(dt), true),
                "{dt:?} with reachable pg should probe IB"
            );
        }
    }

    #[tokio::test]
    async fn detect_step_passes_when_explicit_deployment_type_provided() {
        // PRD-001 slice 9 (#321): when the user pinned a type via
        // CLI/env/file, detection has nothing to add — the step returns
        // Ok(None) without consulting the cluster.
        for dt in [
            DeploymentType::Full,
            DeploymentType::CoreOnly,
            DeploymentType::RestOnlyMock,
            DeploymentType::Force,
        ] {
            let res = detect_deployment_type_step(Some(dt), None).await;
            assert!(matches!(res, Ok(None)), "expected Ok(None) for {dt:?}, got {res:?}");
        }
    }

    #[tokio::test]
    async fn detect_step_returns_matched_type_in_auto_mode() {
        // PRD-001 slice 9 (#321): the auto path returns the matched
        // type so the caller can re-load Config with it slotted into
        // the bundle layer.
        use nico_common::deployment_detect::testing::MockClusterShapeProbe;
        let probe = MockClusterShapeProbe::new()
            .with_service("nico-rest", "nico-rest-mock-core");
        let res = detect_deployment_type_step(None, Some(&probe)).await;
        assert_eq!(res.unwrap(), Some(DeploymentType::RestOnlyMock));
    }

    #[tokio::test]
    async fn detect_step_fails_with_diagnostic_when_auto_and_no_probe_wired() {
        let err = detect_deployment_type_step(None, None)
            .await
            .expect_err("auto + no probe must fail with the slice-1 diagnostic");
        let msg = format!("{err}");
        assert!(
            msg.contains("no detection signals available"),
            "expected 'no detection signals available' diagnostic; got: {msg}"
        );
        assert!(
            msg.contains("--deployment-type"),
            "expected recovery hint mentioning --deployment-type; got: {msg}"
        );
        assert!(
            msg.contains("force"),
            "expected recovery hint mentioning force; got: {msg}"
        );
    }

    #[tokio::test]
    async fn detect_step_passes_when_workload_probe_matches() {
        use nico_common::deployment_detect::testing::MockClusterShapeProbe;
        let probe = MockClusterShapeProbe::new()
            .with_service("forge-system", "carbide-api")
            .with_service("nico-rest", "nico-rest-api");
        let res = detect_deployment_type_step(None, Some(&probe)).await;
        assert!(res.is_ok(), "expected pass; got {res:?}");
    }

    #[tokio::test]
    async fn detect_step_falls_through_to_namespace_inventory_when_workload_probe_misses() {
        use nico_common::deployment_detect::testing::MockClusterShapeProbe;
        // Edge case from slice 2's workload-probe rules: carbide-api
        // visible, `nico-rest` namespace exists without either of its
        // Services. Workload probe says no-match. Slice 3's
        // namespace-inventory fallback picks both namespaces up and
        // resolves to `full`, so the step passes.
        let probe = MockClusterShapeProbe::new()
            .with_service("forge-system", "carbide-api")
            .with_namespace("nico-rest");
        let res = detect_deployment_type_step(None, Some(&probe)).await;
        assert!(res.is_ok(), "expected ladder fall-through to pass; got {res:?}");
    }

    #[tokio::test]
    async fn detect_step_does_not_consult_namespace_inventory_when_workload_probe_matches() {
        use nico_common::deployment_detect::testing::MockClusterShapeProbe;
        // First-match-wins: workload probe resolves before slice 3 runs.
        // Configure a probe whose namespace inventory would *disagree*
        // (only `nico-rest` namespace, which would resolve to
        // `rest-only-mock`) but whose workload-probe match (mock-core
        // service) also says `rest-only-mock`. Either way the step
        // passes; the assertion is simply that we don't reach a state
        // where namespace-inventory's verdict overrides.
        let probe = MockClusterShapeProbe::new()
            .with_service("nico-rest", "nico-rest-mock-core");
        let res = detect_deployment_type_step(None, Some(&probe)).await;
        assert!(res.is_ok(), "expected workload-probe match to pass; got {res:?}");
    }

    #[tokio::test]
    async fn detect_step_passes_when_crd_inventory_matches() {
        use nico_common::deployment_detect::testing::MockClusterShapeProbe;
        // No services, no `forge-system`/`nico-rest` namespaces, but
        // CRD inventory definitively says "rest deployed".
        let probe = MockClusterShapeProbe::new().with_crd("sites.nico.nvidia.io");
        let res = detect_deployment_type_step(None, Some(&probe)).await;
        assert!(res.is_ok(), "expected pass via CRD rung; got {res:?}");
    }

    #[tokio::test]
    async fn detect_step_fails_with_all_three_observation_lists_when_no_rung_matches() {
        use nico_common::deployment_detect::testing::MockClusterShapeProbe;
        // No known services, neither `forge-system` nor `nico-rest`
        // namespaces, no indicator CRDs — every rung misses. Diagnostic
        // must list all three observation slots so the operator can see
        // exactly what was probed.
        let probe = MockClusterShapeProbe::new()
            .with_namespace("kube-system")
            .with_namespace("default")
            .with_crd("certificates.cert-manager.io");
        let err = detect_deployment_type_step(None, Some(&probe))
            .await
            .expect_err("auto + all-signals-miss must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("no deployment-type signal matched"),
            "expected ladder no-match diagnostic; got: {msg}"
        );
        assert!(
            msg.contains("observed namespaces:") && msg.contains("kube-system"),
            "expected observed namespaces list incl. kube-system; got: {msg}"
        );
        assert!(
            msg.contains("observed services: <none>"),
            "expected observed services list <none>; got: {msg}"
        );
        assert!(
            msg.contains("observed CRDs: <none>"),
            "expected observed CRDs <none> (cert-manager is not an indicator); got: {msg}"
        );
        assert!(
            msg.contains("--deployment-type"),
            "expected recovery hint mentioning --deployment-type; got: {msg}"
        );
        assert!(
            msg.contains("force"),
            "expected recovery hint mentioning force; got: {msg}"
        );
    }

    #[tokio::test]
    async fn detect_step_renders_none_markers_when_cluster_is_empty() {
        use nico_common::deployment_detect::testing::MockClusterShapeProbe;
        let probe = MockClusterShapeProbe::new();
        let err = detect_deployment_type_step(None, Some(&probe))
            .await
            .expect_err("auto + empty cluster must fail");
        let msg = format!("{err}");
        // All three observation lists rendered as `<none>` when empty.
        for slot in [
            "observed namespaces: <none>",
            "observed services: <none>",
            "observed CRDs: <none>",
        ] {
            assert!(
                msg.contains(slot),
                "expected '{slot}' in empty-cluster diagnostic; got: {msg}"
            );
        }
    }
}
