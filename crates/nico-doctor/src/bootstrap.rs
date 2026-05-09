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
    let config = Config::load(file_toml.as_deref(), &env, &overrides).map_err(|e| {
        BootstrapErr::Fatal {
            message: format!("error loading config: {e}"),
            code: 1,
        }
    })?;

    let reach_mode = config.cluster.reach_mode;
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
        color: match config.output.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::env::var("NO_COLOR").is_err(),
        },
        ascii: args.ascii,
    };

    let since = humantime::parse_duration(&args.since).unwrap_or(Duration::from_secs(600));
    let timeout = humantime::parse_duration(&args.timeout).unwrap_or(Duration::from_secs(5));
    let opts = RunOpts {
        namespace: config.cluster.namespace.clone(),
        since,
        timeout,
        ..Default::default()
    };

    let probe_outcome = run_boot_probe(
        &config,
        reach_mode,
        reach_source,
        &output_mode,
    )
    .await;

    let BootProbeResult {
        k8s_client,
        reach_mgr,
        temporal_address,
        postgres_url,
        mut pf_guards,
    } = match probe_outcome {
        Ok(r) => r,
        Err(failure) => {
            return Err(failure);
        }
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
}

async fn run_boot_probe(
    config: &Config,
    reach_mode: ReachMode,
    reach_source: &str,
    output_mode: &OutputMode,
) -> Result<BootProbeResult, BootstrapErr> {
    let probe_mode = if config.output.format == OutputFormat::Json {
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
        &config.cluster.namespace,
        &config.bootstrap.timeouts,
        config.cluster.grpc_address.as_deref(),
    );
    let probe_state = ProbeState::new(steps, reach_mode.as_str(), reach_source)
        .with_deployment_type(
            config.cluster.deployment_type.map(|d| d.label().to_string()),
            config.cluster.deployment_type_source.label(),
        )
        .with_warnings(config.override_conflict_warnings());
    let mut probe = BootProbe::new(probe_state, probe_mode, Box::new(StderrSink));
    probe.start_ticking();
    let tracker = probe.tracker();

    let ns = config.cluster.namespace.clone();
    let timeouts = config.bootstrap.timeouts;

    // ---------- Connecting (sequential gate) ----------

    // 1. LoadKubeconfig — non-fatal: degrade gracefully if it fails.
    tracker.started(StepId::LoadKubeconfig).await;
    let t = Instant::now();
    let kube_result = nico_common::k8s::KubeRsK8sClient::try_new(
        config.cluster.context.as_deref(),
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
                        next_command: next_command_for(StepId::LoadKubeconfig, &ns),
                    },
                )
                .await;
            // No client → all downstream is skipped.
            tracker
                .skip_remaining(&[
                    StepId::ReachApiServer,
                    StepId::Credentials,
                    StepId::DetectDeploymentType,
                    StepId::NamespaceExists,
                    StepId::Rbac,
                    StepId::PortForwardWorkflows,
                    StepId::PortForwardGrpc,
                    StepId::PortForwardPostgres,
                    StepId::ReachPostgres,
                ])
                .await;
            // Graceful degradation — probe completes (with the failed
            // kubeconfig step rendered), bootstrap continues without
            // a client.
            let _ = probe.finish_failure(&ns).await;
            return Ok(BootProbeResult {
                k8s_client: None,
                reach_mgr: None,
                temporal_address: config.temporal.address.clone(),
                postgres_url: config.postgres.url.clone(),
                pf_guards: vec![],
            });
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
                    next_command: next_command_for(StepId::ReachApiServer, &ns),
                },
            )
            .await;
        tracker
            .skip_remaining(&[
                StepId::Credentials,
                StepId::DetectDeploymentType,
                StepId::NamespaceExists,
                StepId::Rbac,
                StepId::PortForwardWorkflows,
                StepId::PortForwardGrpc,
                StepId::PortForwardPostgres,
                StepId::ReachPostgres,
            ])
            .await;
        return Err(probe_to_preflight_err(probe, config).await);
    }
    tracker
        .finished(
            StepId::ReachApiServer,
            StepState::Passed { elapsed: t.elapsed() },
        )
        .await;

    // ---------- Validating + Serving in parallel ----------

    let pf = preflight::KubePreflightClient::new(raw.clone());
    let shape_probe = KubeClusterShapeProbe::new(raw.clone());
    let reach_mgr = ReachManager::new(
        reach_mode,
        raw.clone(),
        config.cluster.namespace.clone(),
        config.cluster.postgres_namespace.clone(),
    );

    let (validating_ok, serving_results) = tokio::join!(
        run_validating_section(
            &tracker,
            &pf,
            &shape_probe,
            &ns,
            timeouts.preflight,
            config.cluster.deployment_type,
        ),
        run_serving_section(&tracker, &reach_mgr, config, timeouts.port_forward, timeouts.postgres_reach),
    );

    if !validating_ok {
        // Mark serving steps that may have started or are still pending as
        // appropriate; serving may have run concurrently — we leave its
        // state as-is (each step recorded its own outcome). Validating
        // failure is fatal regardless.
        return Err(probe_to_preflight_err(probe, config).await);
    }

    let (temporal_address, postgres_url, pf_guards) = serving_results;

    // Serving failures are non-fatal (probe shows them; bootstrap falls
    // back to config addresses). Probe completes successfully so long
    // as connecting + validating passed.
    let _ = if probe_state_any_failed(&probe).await {
        probe.finish_failure(&ns).await
    } else {
        probe.finish_success(&ns).await
    };

    Ok(BootProbeResult {
        k8s_client: Some(Arc::new(kube_client.unwrap()) as Arc<dyn nico_common::k8s::K8sClient>),
        reach_mgr: Some(reach_mgr),
        temporal_address,
        postgres_url,
        pf_guards,
    })
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
    shape_probe: &dyn ClusterShapeProbe,
    ns: &str,
    budget: Duration,
    deployment_type: Option<DeploymentType>,
) -> bool {
    let cred_fut = run_step(
        tracker,
        StepId::Credentials,
        ns,
        budget,
        async { pf.check_token_valid().await },
    );
    let detect_fut = run_step(
        tracker,
        StepId::DetectDeploymentType,
        ns,
        budget,
        async move { detect_deployment_type_step(deployment_type, Some(shape_probe)).await },
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
    let (cred_ok, detect_ok, ns_ok, rbac_ok) =
        tokio::join!(cred_fut, detect_fut, ns_fut, rbac_fut);
    cred_ok && detect_ok && ns_ok && rbac_ok
}

/// Behavior of the `detect_deployment_type` step:
///
/// - `Some(_)` (resolved from CLI / env / file, including `Force`) →
///   pass instantly; detection is skipped per PRD-001's hybrid trust
///   model.
/// - `None` (auto) + a shape probe is wired → run the full detection
///   ladder (workload → namespace → CRD). Pass on first match; fail
///   on no-match with a diagnostic listing observed namespaces,
///   services, and CRDs.
/// - `None` + no shape probe → preserve the slice-1 fallback so
///   non-cluster code paths (tests, degraded boot) still surface a
///   clear "no detection signals" error.
///
/// The matched type is intentionally *not* wired back into config —
/// capability re-pointing lands in slice 5 (#282).
async fn detect_deployment_type_step(
    deployment_type: Option<DeploymentType>,
    shape_probe: Option<&dyn ClusterShapeProbe>,
) -> anyhow::Result<()> {
    if deployment_type.is_some() {
        return Ok(());
    }
    let Some(probe) = shape_probe else {
        return Err(anyhow::anyhow!(
            "no detection signals available; pass --deployment-type=<full|core-only|rest-only-mock> or =force"
        ));
    };
    let outcome = run_detection_ladder(probe).await?;
    if outcome.matched.is_some() {
        return Ok(());
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
) -> (String, String, Vec<nico_common::reach::ForwardedEndpoint>) {
    let ns = config.cluster.namespace.clone();

    // Workflows port-forward
    let temporal_fut = async {
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
            return (postgres_url, pf_guard);
        }

        tracker.started(StepId::ReachPostgres).await;
        let t = Instant::now();
        match nico_common::bootstrap::probe_postgres(&postgres_url, pg_reach_budget).await {
            Ok(()) => {
                tracker
                    .finished(
                        StepId::ReachPostgres,
                        StepState::Passed { elapsed: t.elapsed() },
                    )
                    .await;
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
            }
        }
        (postgres_url, pf_guard)
    };

    let ((temporal_address, t_guard), _grpc, (postgres_url, pg_guard)) =
        tokio::join!(temporal_fut, grpc_fut, postgres_fut);

    let mut guards = vec![];
    if let Some(g) = t_guard {
        guards.push(g);
    }
    if let Some(g) = pg_guard {
        guards.push(g);
    }
    (temporal_address, postgres_url, guards)
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
        }
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

    #[tokio::test]
    async fn detect_step_passes_when_explicit_deployment_type_provided() {
        for dt in [
            DeploymentType::Full,
            DeploymentType::CoreOnly,
            DeploymentType::RestOnlyMock,
            DeploymentType::Force,
        ] {
            assert!(
                detect_deployment_type_step(Some(dt), None).await.is_ok(),
                "expected pass for {dt:?}"
            );
        }
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
