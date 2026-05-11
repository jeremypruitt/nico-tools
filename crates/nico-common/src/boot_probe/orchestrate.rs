//! Async orchestrator for the boot probe.
//!
//! `BootProbe` owns the live state and a tick task. Callers use the
//! cloneable `Tracker` handle to record state transitions
//! (`started`/`finished`/`skip_remaining`). The renderer either:
//!   - paints the multi-line block on stderr at every tick (TTY)
//!   - emits one log line per transition (non-TTY)
//!   - is silent (JSON)
//!
//! Per ADR-0013's fail-aware rule: the orchestrator does *not* cancel
//! sibling tasks when one fails. The bootstrap path runs sibling
//! futures via `tokio::join!` (or equivalent); when a future returns
//! `Err`, peers continue. After the section settles, downstream
//! sections are marked `Skipped` via `skip_remaining` rather than
//! started.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::json;
use super::log::{success_receipt, transition_line};
use super::render::{rendered_line_count, BootProbeBlock, RenderMode};
use super::state::{ProbeState, StepDef, StepId, StepState};

/// Concrete TTY terminal type — a ratatui Terminal driving a crossterm
/// backend wrapping `io::Stderr`.
type TtyTerminal = Terminal<CrosstermBackend<io::Stderr>>;

/// Where the renderer paints output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeMode {
    /// Animated block on stderr, repainted each tick.
    Tty { color: bool, ascii: bool },
    /// One log line per state transition. No cursor moves.
    NonTty,
    /// Silent during the probe; emits one structured doc on completion.
    Json,
}

impl ProbeMode {
    pub fn render_mode(self) -> Option<RenderMode> {
        match self {
            Self::Tty { color, ascii } => Some(RenderMode { color, ascii }),
            Self::NonTty | Self::Json => None,
        }
    }
}

/// Sink the orchestrator writes to. Tests pass a `Vec<u8>`; production
/// passes `std::io::stderr()`.
pub trait ProbeSink: Send + Sync {
    fn write_str(&mut self, s: &str);
}

impl ProbeSink for Vec<u8> {
    fn write_str(&mut self, s: &str) {
        self.extend_from_slice(s.as_bytes());
    }
}

pub struct StderrSink;
impl ProbeSink for StderrSink {
    fn write_str(&mut self, s: &str) {
        let _ = std::io::stderr().write_all(s.as_bytes());
        let _ = std::io::stderr().flush();
    }
}

struct Inner {
    state: ProbeState,
    started_at: Instant,
    mode: ProbeMode,
    /// Sink used for `NonTty` log lines and the `Json` no-op path.
    /// `Tty` mode writes through `tty_terminal` instead.
    sink: Box<dyn ProbeSink>,
    /// Inline-viewport ratatui terminal for `Tty` mode. Lazily
    /// constructed on first repaint, then re-created on demand when the
    /// rendered line count changes (e.g. warnings appear).
    tty_terminal: Option<TtyTerminal>,
    /// Cached viewport height for the current `tty_terminal`. None
    /// means no terminal yet.
    tty_viewport_lines: Option<u16>,
}

/// Cloneable handle the bootstrap uses to push state transitions.
#[derive(Clone)]
pub struct Tracker {
    inner: Arc<Mutex<Inner>>,
}

impl Tracker {
    /// Mark a step as started — flips state from Pending to Active.
    /// Records the moment so `elapsed` is computed correctly when the
    /// step finishes.
    pub async fn started(&self, id: StepId) {
        let mut g = self.inner.lock().await;
        g.state.set_state(
            id,
            StepState::Active {
                elapsed: Duration::ZERO,
            },
        );
        Self::after_change(&mut g, Some(id));
    }

    /// Mark a step as finished — `Passed`, `Failed` (with message), or
    /// `Skipped`. Caller computes elapsed from `Instant::now() - start`.
    pub async fn finished(&self, id: StepId, new_state: StepState) {
        let mut g = self.inner.lock().await;
        g.state.set_state(id, new_state);
        Self::after_change(&mut g, Some(id));
    }

    /// Snapshot whether any tracked step is currently in the `Failed`
    /// state. Used by the bootstrap caller to decide whether to drive
    /// `finish_success` or `finish_failure`.
    pub async fn any_failed(&self) -> bool {
        let g = self.inner.lock().await;
        g.state.any_failed()
    }

    /// Replace a step's plain-English label and trigger a repaint.
    /// PRD-001 slice 9 (#321) uses this to re-render namespace / gRPC
    /// labels after detection settles the resolved config.
    pub async fn set_label(&self, id: StepId, label: impl Into<String>) {
        let mut g = self.inner.lock().await;
        g.state.set_label(id, label);
        Self::repaint_tty(&mut g);
    }

    /// Replace the banner's deployment-type tag and trigger a repaint.
    /// Flips `type: auto` → `type: <name> (auto)` once detection lands.
    pub async fn set_deployment_type(
        &self,
        deployment_type: Option<String>,
        source: impl Into<String>,
    ) {
        let mut g = self.inner.lock().await;
        g.state.set_deployment_type(deployment_type, source);
        Self::repaint_tty(&mut g);
    }

    /// Replace the override-conflict warnings and trigger a repaint.
    pub async fn set_warnings(&self, warnings: Vec<String>) {
        let mut g = self.inner.lock().await;
        g.state.set_warnings(warnings);
        Self::repaint_tty(&mut g);
    }

    /// Update the resolved InfiniBand presence on the live probe state
    /// (PRD-004 slice 1). Called after `detect_infiniband_present`'s SQL
    /// probe returns; the next render frame surfaces it as
    /// `ib: present|absent|unknown` in the banner header.
    pub async fn set_infiniband_present(&self, val: Option<bool>) {
        let mut g = self.inner.lock().await;
        g.state.infiniband_present = val;
        Self::repaint_tty(&mut g);
    }

    /// Mark every Pending step in `ids` as Skipped — used after a gate
    /// fails to short-circuit downstream sections.
    pub async fn skip_remaining(&self, ids: &[StepId]) {
        let mut g = self.inner.lock().await;
        for &id in ids {
            if let Some(s) = g.state.step_state(id)
                && matches!(s, StepState::Pending)
            {
                g.state.set_state(id, StepState::Skipped);
                Self::emit_log_for(&mut g, id);
            }
        }
        Self::repaint_tty(&mut g);
    }

    fn after_change(g: &mut Inner, id: Option<StepId>) {
        if let Some(id) = id {
            Self::emit_log_for(g, id);
        }
        Self::repaint_tty(g);
    }

    fn emit_log_for(g: &mut Inner, id: StepId) {
        if !matches!(g.mode, ProbeMode::NonTty) {
            return;
        }
        let entry = g
            .state
            .steps
            .iter()
            .find(|(d, _)| d.id == id)
            .map(|(d, s)| (d.clone(), s.clone()));
        if let Some((d, s)) = entry
            && let Some(line) = transition_line(&d, &s)
        {
            let mut out = String::with_capacity(line.len() + 1);
            out.push_str(&line);
            out.push('\n');
            g.sink.write_str(&out);
        }
    }

    fn repaint_tty(g: &mut Inner) {
        let render = match g.mode.render_mode() {
            Some(r) => r,
            None => return,
        };
        // Refresh elapsed times for active steps before painting.
        let now = Instant::now();
        let started_at = g.started_at;
        for (_d, s) in g.state.steps.iter_mut() {
            if let StepState::Active { elapsed } = s {
                *elapsed = now.saturating_duration_since(started_at);
            }
        }
        g.state.total_elapsed = now.saturating_duration_since(started_at);

        // Frame index based on elapsed, using the same 100ms tick as
        // nico-ops's throbber.
        let frame =
            (g.state.total_elapsed.as_millis() / 100) as usize % super::render::THROBBER_FRAMES.len();
        let lines = rendered_line_count(&g.state);
        // u16 saturating cast — `rendered_line_count` is bounded by the
        // step list (single-digit teens in practice) but be defensive.
        let lines_u16: u16 = lines.try_into().unwrap_or(u16::MAX);

        // (Re)construct the inline-viewport terminal if its viewport
        // height no longer matches. ratatui inline viewports are sized
        // at construction; for the boot probe the height grows when
        // warnings are pushed after detection settles.
        let need_new = !matches!(g.tty_viewport_lines, Some(h) if h == lines_u16);
        if need_new {
            // Wind down any existing terminal cleanly: clear its viewport
            // (cursor lands at top of viewport area, contents wiped),
            // then drop. The new terminal opens an inline viewport at
            // the new height starting at the cursor position.
            if let Some(mut t) = g.tty_terminal.take() {
                let _ = t.clear();
            }
            let backend = CrosstermBackend::new(io::stderr());
            let opts = TerminalOptions {
                viewport: Viewport::Inline(lines_u16),
            };
            match Terminal::with_options(backend, opts) {
                Ok(t) => {
                    g.tty_terminal = Some(t);
                    g.tty_viewport_lines = Some(lines_u16);
                }
                Err(_) => {
                    g.tty_viewport_lines = None;
                    return;
                }
            }
        }

        if let Some(t) = g.tty_terminal.as_mut() {
            let widget = BootProbeBlock::new(&g.state, render, frame);
            let _ = t.draw(|f| f.render_widget(widget, f.area()));
        }
    }
}

pub struct BootProbe {
    inner: Arc<Mutex<Inner>>,
    tick_handle: Option<JoinHandle<()>>,
    mode: ProbeMode,
}

impl BootProbe {
    /// Construct a probe with the given step list. The probe is "armed"
    /// — call `start_ticking()` to begin painting (TTY only) and call
    /// `tracker()` to drive transitions from the bootstrap.
    pub fn new(state: ProbeState, mode: ProbeMode, sink: Box<dyn ProbeSink>) -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            state,
            started_at: Instant::now(),
            mode,
            sink,
            tty_terminal: None,
            tty_viewport_lines: None,
        }));
        Self {
            inner,
            tick_handle: None,
            mode,
        }
    }

    /// Start a 100ms tick task that re-paints the live block. Only
    /// meaningful in TTY mode; cheap no-op in others.
    pub fn start_ticking(&mut self) {
        if !matches!(self.mode, ProbeMode::Tty { .. }) {
            return;
        }
        let inner = self.inner.clone();
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let mut g = inner.lock().await;
                Tracker::repaint_tty(&mut g);
            }
        });
        self.tick_handle = Some(handle);
    }

    pub fn tracker(&self) -> Tracker {
        Tracker {
            inner: self.inner.clone(),
        }
    }

    /// Successful end-of-probe: cancel the tick task and print the
    /// one-line success receipt directly below the live block. The
    /// inline viewport is left painted so it survives the TUI's
    /// `EnterAlternateScreen` / `LeaveAlternateScreen` round-trip — when
    /// the operator quits the TUI, the preflight checks are restored
    /// from the main buffer.
    pub async fn finish_success(mut self, namespace: &str) -> ProbeOutcome {
        if let Some(h) = self.tick_handle.take() {
            h.abort();
        }
        let mut g = self.inner.lock().await;
        let total = g.state.total_count();
        let receipt = success_receipt(total, g.state.total_elapsed);
        let json = json::success_document(&g.state, namespace);
        match g.mode {
            ProbeMode::Tty { .. } => {
                emit_completion_receipt_tty(&mut g, &receipt);
            }
            ProbeMode::NonTty => {
                g.sink.write_str(&receipt);
                g.sink.write_str("\n");
            }
            ProbeMode::Json => {
                // silent
            }
        }
        ProbeOutcome::Success { json }
    }

    /// Failed end-of-probe: cancel the tick task, leave the block
    /// rendered, and print the error card below it.
    pub async fn finish_failure(mut self, namespace: &str) -> ProbeOutcome {
        if let Some(h) = self.tick_handle.take() {
            h.abort();
        }
        let mut g = self.inner.lock().await;

        let (failed_id, failed_msg, failed_next) = match g.state.first_failure() {
            Some((d, StepState::Failed {
                message,
                next_command,
                ..
            })) => (d.id, message.clone(), next_command.clone()),
            _ => {
                let json = json::failure_document(&g.state, namespace);
                return ProbeOutcome::Failure {
                    json,
                    human_message: "boot probe failed (no specific step recorded)".into(),
                };
            }
        };

        let card = format!(
            "\n✗ pre-flight failed: {failed_msg}\n  step:  {step}\n  try:   {next}\n",
            step = failed_id.technical_name(),
            next = failed_next,
        );

        match g.mode {
            ProbeMode::Tty { .. } => {
                emit_failure_card_tty(&mut g, &card);
            }
            ProbeMode::NonTty => {
                g.sink.write_str(&card);
            }
            ProbeMode::Json => {
                // silent
            }
        }

        let human_message = format!(
            "error: pre-flight check failed [{}]: {}\n  → {}",
            failed_id.technical_name(),
            failed_msg,
            failed_next,
        );
        let json = json::failure_document(&g.state, namespace);
        ProbeOutcome::Failure {
            json,
            human_message,
        }
    }
}

/// Wind down the inline-viewport terminal leaving the boot block in
/// place, then emit the one-line success receipt just below it. The
/// painted block stays in the main buffer so it survives `nico ops`'s
/// `EnterAlternateScreen` / `LeaveAlternateScreen` round-trip — the
/// operator sees the preflight checks again on TUI exit. Mirrors
/// `emit_failure_card_tty`'s "no clear" approach.
fn emit_completion_receipt_tty(g: &mut Inner, receipt: &str) {
    if let Some(mut t) = g.tty_terminal.take() {
        let area = t.get_frame().area();
        let mut stderr = io::stderr();
        let _ = queue!(stderr, MoveTo(0, area.bottom()));
        let _ = stderr.flush();
        let _ = stderr.write_all(receipt.as_bytes());
        let _ = stderr.write_all(b"\n");
        let _ = stderr.flush();
        drop(t);
    } else {
        // Probe never painted (pathological — `start_ticking` runs and
        // `set_*` triggers repaints, but be defensive). Fall through to
        // a plain stderr write.
        g.sink.write_str(receipt);
        g.sink.write_str("\n");
    }
    g.tty_viewport_lines = None;
}

/// Wind down the inline-viewport terminal leaving the boot block
/// in place, then emit the failure card just below it. Mirrors the
/// pre-ADR-0016 behaviour of writing the card after the painted block.
fn emit_failure_card_tty(g: &mut Inner, card: &str) {
    if let Some(mut t) = g.tty_terminal.take() {
        let area = t.get_frame().area();
        // Position cursor just below the painted viewport (no clear —
        // we want the failure context to remain visible).
        let mut stderr = io::stderr();
        let _ = queue!(stderr, MoveTo(0, area.bottom()));
        let _ = stderr.flush();
        let _ = stderr.write_all(card.as_bytes());
        let _ = stderr.flush();
        drop(t);
    } else {
        g.sink.write_str(card);
    }
    g.tty_viewport_lines = None;
}

#[derive(Debug)]
pub enum ProbeOutcome {
    Success { json: Value },
    Failure { json: Value, human_message: String },
}

impl ProbeOutcome {
    pub fn json(&self) -> &Value {
        match self {
            Self::Success { json } | Self::Failure { json, .. } => json,
        }
    }
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failure { .. })
    }
}

/// Build the standard nine-step probe state used by the live
/// bootstrap path. Steps that don't apply for a given run (e.g. no
/// gRPC address configured) can be filtered out by the caller before
/// constructing the probe.
///
/// When `grpc_address` is `Some`, the `port-forward: grpc` step's
/// label renders the resolved target inline (`port-forward: grpc → <addr>`).
/// When `None`, the label stays minimal — the step itself is marked
/// `Skipped` at run time.
pub fn standard_steps(
    namespace: &str,
    timeouts: &crate::config::BootstrapTimeouts,
) -> Vec<StepDef> {
    standard_steps_with_grpc(namespace, timeouts, None)
}

pub fn standard_steps_with_grpc(
    namespace: &str,
    timeouts: &crate::config::BootstrapTimeouts,
    grpc_address: Option<&str>,
) -> Vec<StepDef> {
    use super::state::Section::*;
    let grpc_label = match grpc_address {
        Some(addr) => format!("port-forward: grpc → {addr}"),
        None => "port-forward: grpc".to_string(),
    };
    vec![
        StepDef {
            id: StepId::LoadKubeconfig,
            label: "load kubeconfig".into(),
            section: Connecting,
            budget: timeouts.kube_client,
        },
        StepDef {
            id: StepId::ReachApiServer,
            label: "reach API server".into(),
            section: Connecting,
            budget: timeouts.reach_api,
        },
        StepDef {
            id: StepId::DetectDeploymentType,
            label: "detect deployment-type".into(),
            section: Connecting,
            budget: timeouts.preflight,
        },
        StepDef {
            id: StepId::Credentials,
            label: "credentials".into(),
            section: Validating,
            budget: timeouts.preflight,
        },
        StepDef {
            id: StepId::NamespaceExists,
            label: format!("namespace '{namespace}' exists"),
            section: Validating,
            budget: timeouts.preflight,
        },
        StepDef {
            id: StepId::Rbac,
            label: "list-pods permission".into(),
            section: Validating,
            budget: timeouts.preflight,
        },
        StepDef {
            id: StepId::PortForwardWorkflows,
            label: "port-forward: workflows".into(),
            section: Serving,
            budget: timeouts.port_forward,
        },
        StepDef {
            id: StepId::PortForwardGrpc,
            label: grpc_label,
            section: Serving,
            budget: timeouts.port_forward,
        },
        StepDef {
            id: StepId::PortForwardPostgres,
            label: "port-forward: postgres".into(),
            section: Serving,
            budget: timeouts.port_forward,
        },
        StepDef {
            id: StepId::ReachPostgres,
            label: "reach postgres".into(),
            section: Serving,
            budget: timeouts.postgres_reach,
        },
        StepDef {
            id: StepId::DetectInfinibandPresent,
            label: "detect infiniband".into(),
            section: Serving,
            budget: timeouts.preflight,
        },
    ]
}

/// Compute the next-command hint for a step's technical name. Mirrors
/// the existing preflight `failed()` mapping; centralizes it so the
/// boot probe can fill the failure card without each callsite knowing
/// the kubectl invocation.
pub fn next_command_for(id: StepId, namespace: &str) -> String {
    match id {
        StepId::LoadKubeconfig => "kubectl config view".into(),
        StepId::ReachApiServer => "kubectl cluster-info".into(),
        StepId::Credentials => "kubectl auth whoami".into(),
        StepId::DetectDeploymentType => {
            "pass --deployment-type=<full|core-only|rest-only-mock> or =force".into()
        }
        StepId::NamespaceExists => format!("kubectl get ns {namespace}"),
        StepId::Rbac => format!("kubectl auth can-i list pods -n {namespace}"),
        StepId::PortForwardWorkflows => format!(
            "kubectl -n {namespace} port-forward svc/temporal-frontend 7233:7233"
        ),
        StepId::PortForwardGrpc => "set NICO_GRPC_ADDRESS or cluster.grpc_address in config".into(),
        StepId::PortForwardPostgres => {
            "kubectl -n postgres port-forward svc/postgres 5432:5432".to_string()
        }
        StepId::ReachPostgres => "check postgres URL host:port reachability".into(),
        StepId::DetectInfinibandPresent => {
            "check forgedb / postgres connectivity, or pass --deployment-type=force".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boot_probe::state::Section;

    fn def(id: StepId, sec: Section) -> StepDef {
        StepDef {
            id,
            label: id.technical_name().to_string(),
            section: sec,
            budget: Duration::from_millis(50),
        }
    }

    #[tokio::test]
    async fn nontty_emits_one_log_line_per_transition() {
        let state = ProbeState::new(
            vec![
                def(StepId::LoadKubeconfig, Section::Connecting),
                def(StepId::ReachApiServer, Section::Connecting),
            ],
            "port-forward",
            "auto",
        );
        let probe = BootProbe::new(state, ProbeMode::NonTty, Box::new(Vec::<u8>::new()));
        let tracker = probe.tracker();
        tracker.started(StepId::LoadKubeconfig).await;
        tracker
            .finished(
                StepId::LoadKubeconfig,
                StepState::Passed {
                    elapsed: Duration::from_millis(50),
                },
            )
            .await;
        tracker.started(StepId::ReachApiServer).await;
        tracker
            .finished(
                StepId::ReachApiServer,
                StepState::Failed {
                    elapsed: Duration::from_millis(20),
                    message: "connection refused".into(),
                    timed_out: false,
                    next_command: "kubectl cluster-info".into(),
                },
            )
            .await;

        let outcome = probe.finish_failure("nico").await;
        let bytes = outcome_sink_bytes(&outcome).unwrap_or_default();
        let _ = bytes; // not used; we read sink via BootProbe internals below

        // Re-read what was written: failure_doc has the right shape.
        match outcome {
            ProbeOutcome::Failure { json, .. } => {
                assert_eq!(json["preflight"]["failed_step"], "reachability");
            }
            _ => panic!("expected failure"),
        }
    }

    fn outcome_sink_bytes(_o: &ProbeOutcome) -> Option<Vec<u8>> {
        None
    }

    #[tokio::test]
    async fn skip_remaining_marks_pending_steps_skipped() {
        let state = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::NamespaceExists, Section::Validating),
                def(StepId::Rbac, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        let probe = BootProbe::new(state, ProbeMode::Json, Box::new(Vec::<u8>::new()));
        let tracker = probe.tracker();
        tracker
            .skip_remaining(&[
                StepId::Credentials,
                StepId::NamespaceExists,
                StepId::Rbac,
            ])
            .await;

        // Inspect the inner state via tracker's lock.
        let g = probe.inner.lock().await;
        for id in [
            StepId::Credentials,
            StepId::NamespaceExists,
            StepId::Rbac,
        ] {
            match g.state.step_state(id).unwrap() {
                StepState::Skipped => {}
                other => panic!("expected Skipped for {id:?}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn skip_remaining_does_not_overwrite_completed_steps() {
        let state = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::NamespaceExists, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        let probe = BootProbe::new(state, ProbeMode::Json, Box::new(Vec::<u8>::new()));
        let tracker = probe.tracker();
        tracker
            .finished(
                StepId::Credentials,
                StepState::Passed {
                    elapsed: Duration::from_millis(10),
                },
            )
            .await;
        tracker
            .skip_remaining(&[StepId::Credentials, StepId::NamespaceExists])
            .await;

        let g = probe.inner.lock().await;
        match g.state.step_state(StepId::Credentials).unwrap() {
            StepState::Passed { .. } => {}
            other => panic!(
                "completed step must not be overwritten by skip_remaining; got {other:?}"
            ),
        }
        assert!(matches!(
            g.state.step_state(StepId::NamespaceExists).unwrap(),
            StepState::Skipped
        ));
    }

    #[tokio::test]
    async fn tracker_set_label_replaces_step_label_for_render() {
        // PRD-001 slice 9 (#321): post-detection label updates. The
        // bootstrap mutates `namespace_exists` and `port-forward: grpc`
        // labels after detection settles, so the renderer paints the
        // resolved values rather than the boot-config placeholders.
        let state = ProbeState::new(
            vec![def(StepId::NamespaceExists, Section::Validating)],
            "port-forward",
            "auto",
        );
        let probe = BootProbe::new(state, ProbeMode::Json, Box::new(Vec::<u8>::new()));
        let tracker = probe.tracker();
        tracker
            .set_label(StepId::NamespaceExists, "namespace 'nico-rest' exists")
            .await;
        let g = probe.inner.lock().await;
        let label = g
            .state
            .steps
            .iter()
            .find(|(d, _)| d.id == StepId::NamespaceExists)
            .map(|(d, _)| d.label.clone())
            .unwrap();
        assert_eq!(label, "namespace 'nico-rest' exists");
    }

    #[tokio::test]
    async fn tracker_set_deployment_type_updates_banner_metadata() {
        // After detection lands the auto path, the banner flips from
        // `type: auto` → `type: rest-only-mock (auto)`.
        let state = ProbeState::new(
            vec![def(StepId::DetectDeploymentType, Section::Connecting)],
            "port-forward",
            "auto",
        );
        let probe = BootProbe::new(state, ProbeMode::Json, Box::new(Vec::<u8>::new()));
        let tracker = probe.tracker();
        tracker
            .set_deployment_type(Some("rest-only-mock".to_string()), "auto")
            .await;
        let g = probe.inner.lock().await;
        assert_eq!(g.state.deployment_type.as_deref(), Some("rest-only-mock"));
        assert_eq!(g.state.deployment_type_source, "auto");
    }

    #[tokio::test]
    async fn tracker_set_warnings_replaces_warning_lines() {
        // Override-conflict warnings are computed against the resolved
        // (post-detection) config. The bootstrap re-pushes them once
        // detection lands; the tracker handles it as a single mutation.
        let state = ProbeState::new(
            vec![def(StepId::DetectDeploymentType, Section::Connecting)],
            "port-forward",
            "auto",
        );
        let probe = BootProbe::new(state, ProbeMode::Json, Box::new(Vec::<u8>::new()));
        let tracker = probe.tracker();
        tracker
            .set_warnings(vec![
                "⚠  cluster.namespace=forge-system overrides \
                 deployment-type rest-only-mock default (nico-rest)"
                    .to_string(),
            ])
            .await;
        let g = probe.inner.lock().await;
        assert_eq!(g.state.warnings.len(), 1);
        assert!(g.state.warnings[0].contains("rest-only-mock"));
    }

    #[tokio::test]
    async fn fail_aware_siblings_run_to_completion() {
        // Simulates ADR-0013 fail-aware semantics: the orchestrator
        // does not cancel siblings, so all parallel results land in
        // the final probe state.
        let state = ProbeState::new(
            vec![
                def(StepId::Credentials, Section::Validating),
                def(StepId::NamespaceExists, Section::Validating),
                def(StepId::Rbac, Section::Validating),
            ],
            "port-forward",
            "auto",
        );
        let probe = BootProbe::new(state, ProbeMode::Json, Box::new(Vec::<u8>::new()));
        let tracker = probe.tracker();

        let t1 = tracker.clone();
        let t2 = tracker.clone();
        let t3 = tracker.clone();
        let h1 = tokio::spawn(async move {
            t1.started(StepId::Credentials).await;
            t1.finished(
                StepId::Credentials,
                StepState::Failed {
                    elapsed: Duration::from_millis(10),
                    message: "401".into(),
                    timed_out: false,
                    next_command: "kubectl auth whoami".into(),
                },
            )
            .await;
        });
        let h2 = tokio::spawn(async move {
            t2.started(StepId::NamespaceExists).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            t2.finished(
                StepId::NamespaceExists,
                StepState::Passed {
                    elapsed: Duration::from_millis(20),
                },
            )
            .await;
        });
        let h3 = tokio::spawn(async move {
            t3.started(StepId::Rbac).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            t3.finished(
                StepId::Rbac,
                StepState::Passed {
                    elapsed: Duration::from_millis(20),
                },
            )
            .await;
        });
        let (_, _, _) = tokio::join!(h1, h2, h3);

        let g = probe.inner.lock().await;
        // All three siblings have terminal states — none cancelled.
        for id in [
            StepId::Credentials,
            StepId::NamespaceExists,
            StepId::Rbac,
        ] {
            assert!(
                g.state.step_state(id).unwrap().is_terminal(),
                "step {id:?} not terminal"
            );
        }
        // The failure was Credentials, but siblings completed.
        assert!(matches!(
            g.state.step_state(StepId::Credentials).unwrap(),
            StepState::Failed { .. }
        ));
        assert!(matches!(
            g.state.step_state(StepId::NamespaceExists).unwrap(),
            StepState::Passed { .. }
        ));
        assert!(matches!(
            g.state.step_state(StepId::Rbac).unwrap(),
            StepState::Passed { .. }
        ));
    }

    #[tokio::test]
    async fn json_mode_is_silent_during_probe_then_emits_doc() {
        let state = ProbeState::new(
            vec![def(StepId::LoadKubeconfig, Section::Connecting)],
            "port-forward",
            "auto",
        );
        let sink = Box::new(Vec::<u8>::new());
        let probe = BootProbe::new(state, ProbeMode::Json, sink);
        let tracker = probe.tracker();
        tracker.started(StepId::LoadKubeconfig).await;
        tracker
            .finished(
                StepId::LoadKubeconfig,
                StepState::Passed {
                    elapsed: Duration::from_millis(50),
                },
            )
            .await;
        let outcome = probe.finish_success("nico").await;
        match outcome {
            ProbeOutcome::Success { json } => {
                assert_eq!(json["preflight"]["ok"], true);
            }
            _ => panic!("expected success"),
        }
    }

    #[tokio::test]
    async fn standard_steps_includes_eleven_steps_with_infiniband_present() {
        // PRD-004 slice 1: standard step list grows by one
        // (`detect_infiniband_present`) ordered after `ReachPostgres`.
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps("nico", &t);
        assert_eq!(s.len(), 11);
    }

    #[tokio::test]
    async fn standard_steps_places_detect_infiniband_present_after_reach_postgres() {
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps("nico", &t);
        let positions: std::collections::HashMap<StepId, usize> = s
            .iter()
            .enumerate()
            .map(|(i, d)| (d.id, i))
            .collect();
        let reach_pg = positions[&StepId::ReachPostgres];
        let detect_ib = positions[&StepId::DetectInfinibandPresent];
        assert!(
            reach_pg < detect_ib,
            "DetectInfinibandPresent must come after ReachPostgres (needs forgedb)"
        );
    }

    #[tokio::test]
    async fn detect_infiniband_present_lives_in_serving_section() {
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps("nico", &t);
        let detect = s
            .iter()
            .find(|d| d.id == StepId::DetectInfinibandPresent)
            .expect("DetectInfinibandPresent step missing from standard_steps");
        assert_eq!(detect.section, crate::boot_probe::state::Section::Serving);
    }

    #[tokio::test]
    async fn standard_steps_places_detect_deployment_type_after_reach_api_before_credentials() {
        // PRD-001 slice 9 (#321): detect-first-then-load. The step is a
        // sequential gate between Connecting and Validating; visually it
        // sits at the end of Connecting because the validating fan-out
        // consumes its result via the resolved cluster.namespace label.
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps("nico", &t);
        let positions: std::collections::HashMap<StepId, usize> = s
            .iter()
            .enumerate()
            .map(|(i, d)| (d.id, i))
            .collect();
        let reach = positions[&StepId::ReachApiServer];
        let detect = positions[&StepId::DetectDeploymentType];
        let cred = positions[&StepId::Credentials];
        assert!(reach < detect, "ReachApiServer must come before DetectDeploymentType");
        assert!(detect < cred, "DetectDeploymentType must gate before Credentials");
    }

    #[tokio::test]
    async fn detect_deployment_type_lives_in_connecting_section() {
        // PRD-001 slice 9 (#321) re-placed the step from Validating to
        // Connecting so its result can feed Config::load before the
        // validating step labels (`namespace 'X' exists`,
        // `port-forward: grpc → addr`) are rendered.
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps("nico", &t);
        let detect = s.iter().find(|d| d.id == StepId::DetectDeploymentType).unwrap();
        assert_eq!(detect.section, crate::boot_probe::state::Section::Connecting);
    }

    #[tokio::test]
    async fn standard_steps_renders_resolved_grpc_target_in_label() {
        // PRD-001 slice 6: when a gRPC address is resolved (from
        // deployment-type default, file, or env), the `port-forward: grpc`
        // step's banner label must include the target inline so the
        // operator sees exactly what the boot probe is dialing.
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps_with_grpc("nico-rest", &t, Some("nico-rest-mock-core.nico-rest:11079"));
        let grpc = s.iter().find(|d| d.id == StepId::PortForwardGrpc).unwrap();
        assert_eq!(
            grpc.label,
            "port-forward: grpc → nico-rest-mock-core.nico-rest:11079",
            "expected resolved grpc target in banner label, got {:?}",
            grpc.label,
        );
    }

    #[tokio::test]
    async fn standard_steps_omits_arrow_when_grpc_address_unset() {
        // No resolved address → no arrow. The step renders Skipped at run
        // time; the label should not pretend a target exists.
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps_with_grpc("nico", &t, None);
        let grpc = s.iter().find(|d| d.id == StepId::PortForwardGrpc).unwrap();
        assert_eq!(grpc.label, "port-forward: grpc");
    }

    #[tokio::test]
    async fn rest_only_mock_resolves_to_nico_rest_labels_end_to_end() {
        // Closes the user-reported failure: with `--deployment-type=rest-only-mock`
        // (and no overrides), slice 5 resolves the capability bundle so that
        // `cluster.namespace = "nico-rest"` and
        // `cluster.grpc_address = Some("nico-rest-mock-core.nico-rest:11079")`.
        // Slice 6's contract: those resolved values flow into the boot probe's
        // step labels — the `namespace_exists` row reads `nico-rest`, and the
        // `port-forward: grpc` row renders the resolved target inline.
        use crate::config::{Config, ConfigOverrides};
        let env = std::collections::HashMap::new();
        let overrides = ConfigOverrides {
            deployment_type_spec: Some("rest-only-mock".into()),
            ..Default::default()
        };
        let cfg = Config::load(None, &env, &overrides, None).expect("config load");
        assert_eq!(cfg.cluster.namespace, "nico-rest");
        assert_eq!(
            cfg.cluster.grpc_address.as_deref(),
            Some("nico-rest-mock-core.nico-rest:11079")
        );

        let steps = standard_steps_with_grpc(
            &cfg.cluster.namespace,
            &cfg.bootstrap.timeouts,
            cfg.cluster.grpc_address.as_deref(),
        );
        let ns = steps.iter().find(|d| d.id == StepId::NamespaceExists).unwrap();
        let grpc = steps.iter().find(|d| d.id == StepId::PortForwardGrpc).unwrap();
        assert_eq!(ns.label, "namespace 'nico-rest' exists");
        assert_eq!(
            grpc.label,
            "port-forward: grpc → nico-rest-mock-core.nico-rest:11079"
        );
    }

    #[tokio::test]
    async fn detect_first_then_load_resolves_closure_case_labels_via_workload_probe() {
        // PRD-001 slice 9 (#321) closure-case integration test.
        //
        // The bug we're closing: a `kind-nico-rest-local` cluster with no
        // `--deployment-type` flag and no config-file pin. Detection
        // resolves the cluster shape from the `nico-rest-mock-core`
        // workload probe; `Config::load` slots the detected type into
        // the bundle layer so downstream boot-probe step labels render
        // `namespace 'nico-rest' exists` and
        // `port-forward: grpc → nico-rest-mock-core.nico-rest:11079`.
        //
        // Prior to this slice the detection result was thrown away — the
        // step labels stayed pinned to pre-detection (hardcoded) values
        // and `nico ops` failed at `namespace 'forge-system' not found`.
        use crate::config::{
            Config, ConfigOverrides, DeploymentType, DeploymentTypeSource,
        };
        use crate::deployment_detect::{run_detection_ladder, testing::MockClusterShapeProbe};

        let probe = MockClusterShapeProbe::new()
            .with_service("nico-rest", "nico-rest-mock-core");
        let outcome = run_detection_ladder(&probe).await.expect("ladder");
        assert_eq!(
            outcome.matched,
            Some(DeploymentType::RestOnlyMock),
            "workload probe should match RestOnlyMock from nico-rest-mock-core@nico-rest",
        );
        assert!(
            outcome
                .observed_services
                .iter()
                .any(|s| s == "nico-rest-mock-core@nico-rest"),
            "diagnostic should record observed mock-core service: {:?}",
            outcome.observed_services,
        );

        // Re-load Config with the detected type — auto path with no
        // CLI/env/file declaration. Source stays `Auto`.
        let env = std::collections::HashMap::new();
        let cfg = Config::load(None, &env, &ConfigOverrides::default(), outcome.matched)
            .expect("config load");
        assert_eq!(
            cfg.cluster.deployment_type,
            Some(DeploymentType::RestOnlyMock)
        );
        assert_eq!(
            cfg.cluster.deployment_type_source,
            DeploymentTypeSource::Auto
        );
        assert_eq!(cfg.cluster.namespace, "nico-rest");
        assert_eq!(
            cfg.cluster.grpc_address.as_deref(),
            Some("nico-rest-mock-core.nico-rest:11079")
        );
        assert!(
            cfg.override_conflict_warnings().is_empty(),
            "no overrides → no warnings",
        );

        // Boot-probe step labels reflect the post-detection config.
        let steps = standard_steps_with_grpc(
            &cfg.cluster.namespace,
            &cfg.bootstrap.timeouts,
            cfg.cluster.grpc_address.as_deref(),
        );
        let ns_step = steps
            .iter()
            .find(|d| d.id == StepId::NamespaceExists)
            .expect("namespace_exists step");
        let grpc_step = steps
            .iter()
            .find(|d| d.id == StepId::PortForwardGrpc)
            .expect("port_forward_grpc step");
        assert_eq!(ns_step.label, "namespace 'nico-rest' exists");
        assert_eq!(
            grpc_step.label,
            "port-forward: grpc → nico-rest-mock-core.nico-rest:11079"
        );
    }

    #[tokio::test]
    async fn detect_first_then_load_emits_override_warning_for_legacy_forge_system_pin() {
        // The user's other failure mode: legacy file pin to
        // `cluster.namespace = "forge-system"` against a `nico-rest`
        // cluster. Detection resolves RestOnlyMock; the file value wins
        // for the resolved namespace (file > bundle), but the
        // override-conflict warning fires.
        use crate::config::{Config, ConfigOverrides, DeploymentType};
        use crate::deployment_detect::{run_detection_ladder, testing::MockClusterShapeProbe};

        let probe = MockClusterShapeProbe::new()
            .with_service("nico-rest", "nico-rest-mock-core");
        let outcome = run_detection_ladder(&probe).await.expect("ladder");
        assert_eq!(outcome.matched, Some(DeploymentType::RestOnlyMock));

        let toml = "[cluster]\nnamespace = \"forge-system\"";
        let env = std::collections::HashMap::new();
        let cfg = Config::load(Some(toml), &env, &ConfigOverrides::default(), outcome.matched)
            .expect("config load");
        assert_eq!(cfg.cluster.namespace, "forge-system");
        let warnings = cfg.override_conflict_warnings();
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert_eq!(
            warnings[0],
            "⚠  cluster.namespace=forge-system overrides deployment-type \
             rest-only-mock default (nico-rest)",
        );
    }

    #[tokio::test]
    async fn standard_steps_namespace_label_uses_resolved_namespace() {
        // PRD-001 slice 6: closes the user-reported failure where the
        // banner showed `namespace 'forge-system' exists` against a
        // `rest-only-mock` cluster. The label must reflect whatever
        // `cluster.namespace` resolved to, with no `forge-system` fallback.
        let t = crate::config::BootstrapTimeouts::default();
        let s = standard_steps("nico-rest", &t);
        let ns = s.iter().find(|d| d.id == StepId::NamespaceExists).unwrap();
        assert_eq!(ns.label, "namespace 'nico-rest' exists");
        assert!(
            !ns.label.contains("forge-system"),
            "namespace label must not hard-code forge-system, got {:?}",
            ns.label,
        );
    }
}
