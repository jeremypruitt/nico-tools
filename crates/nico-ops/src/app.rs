use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use nico_common::output::Status;
use nico_doctor::baseline::{Baseline, Delta, compute_deltas_for};
use ratatui::layout::Rect;

use crate::action::{Action, Dir, ScrollDir};
use crate::cli::FeatureFlags;
use crate::correlate_runner::CorrelateUpdate;
use crate::events::Overlay;
use crate::model::{
    CorrelateState, EntityRef, LayerSnapshot, LogLine, SourceError, SourceProgress, SourceStatus,
    extract_entity_from_finding,
};
use crate::pulse::PulseTimer;
use crate::ringbuffer::{LayerStat, RingBuffer, RunSnapshot};

/// How long a transient toast is shown in the bottom bar before the
/// reducer drops it. Picked to be long enough that the operator can read
/// "clipboard unavailable" but short enough not to linger after a refresh.
pub const TOAST_TTL: Duration = Duration::from_millis(2500);

/// Which top-level layout the dashboard is rendering. The reducer flips
/// between these in response to `Action::ShowSpotlight` (Scorecard →
/// Spotlight) and `Action::ShowAll` (Spotlight → Scorecard); the
/// renderer branches on the value.
///
/// PRD-006 slice 1 (issue #367) shrunk this from three variants to two
/// by removing Mission Control (Layout B). The `m` keybinding is
/// preserved as a one-shot toast — see [`crate::events::translate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// The scorecard grid with drill panel (ADR-010).
    #[default]
    Scorecard,
    /// The "3am page" Spotlight view: tui-big-text headline over
    /// incident cards for non-green Layers, green Layers compressed to a
    /// single footer line.
    Spotlight,
}

/// Default auto-refresh cadence when no flag/env/config override is set.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30);

/// Number of frames in the braille throbber cycle.
const THROBBER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Glyph used in place of the throbber once a refresh has completed.
pub const THROBBER_DONE: &str = "✓";

/// Tick interval the host loop should drive the reducer at; the throbber
/// frame index is `(now - boot) / TICK`.
pub const TICK: Duration = Duration::from_millis(100);

/// Side-effects requested by the reducer that the host loop has to carry
/// out (since `App::handle` is otherwise pure).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Kick off a new collection round.
    StartRefresh,
    /// Turn on terminal mouse capture.
    EnableMouseCapture,
    /// Turn off terminal mouse capture so the operator can use native
    /// terminal scrollback / text selection.
    DisableMouseCapture,
    /// Copy a string to the system clipboard via `arboard`. The host
    /// loop owns the clipboard handle and translates failures into
    /// `Action::ShowToast`.
    CopyToClipboard(String),
    /// Open a URL via the system browser (`$BROWSER` or platform
    /// default). Best-effort; failures translate into
    /// `Action::ShowToast`.
    OpenUrl(String),
    /// Kick off a correlate run for the given entity. The host loop
    /// constructs a [`crate::correlate_runner::CorrelateStream`] and
    /// pumps each [`CorrelateUpdate`] back through the action channel
    /// as `Action::CorrelateUpdate`. PRD-007 Slice 2 — replaces the
    /// one-shot bulk `Action::CorrelateResults` with streaming.
    RunCorrelate(EntityRef),
    /// PRD-007 Slice 2: drop the in-flight correlate stream. The host
    /// loop owns the stream's abort handle; aborting it cancels every
    /// per-Source future via `FuturesUnordered` drop.
    CancelCorrelate,
    /// Tear down and exit cleanly.
    Quit,
}

/// All dashboard state. The reducer (`handle`) is the only mutator.
pub struct App {
    snapshots: Vec<LayerSnapshot>,
    focus: usize,
    overlay: Overlay,
    refreshing: bool,
    last_refreshed: Option<DateTime<Local>>,
    dirty: bool,
    paused: bool,
    interval: Duration,
    next_refresh_at: Option<Instant>,
    boot: Option<Instant>,
    now: Option<Instant>,
    history: RingBuffer,
    baseline: Option<Baseline>,
    deltas: HashMap<String, Delta>,
    prev_status: HashMap<String, Status>,
    pulses: HashMap<String, PulseTimer>,
    mouse_capture: bool,
    drill_scroll: u16,
    overlay_scroll: u16,
    logs_scroll: u16,
    card_regions: Vec<Rect>,
    layout: Layout,
    spotlight_focus: usize,
    toast: Option<Toast>,
    correlate: Option<CorrelateState>,
    chooser: Option<ChooserState>,
    log_lines: Vec<LogLine>,
    features: FeatureFlags,
}

/// A transient bottom-bar message and its expiry timestamp. Cleared by
/// the reducer once `now >= expires_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub message: String,
    pub expires_at: Instant,
}

/// PRD-007 Slice 1 (#372): state for the multi-match chooser popup.
/// `entities` is the list of candidates the extraction primitive
/// returned; `focus` is the index the operator has highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChooserState {
    pub entities: Vec<EntityRef>,
    pub focus: usize,
}

impl App {
    pub fn new() -> Self {
        Self::with_interval(DEFAULT_INTERVAL)
    }

    pub fn with_interval(interval: Duration) -> Self {
        Self {
            snapshots: Vec::new(),
            focus: 0,
            overlay: Overlay::None,
            refreshing: false,
            last_refreshed: None,
            dirty: true,
            paused: false,
            interval,
            next_refresh_at: None,
            boot: None,
            now: None,
            history: RingBuffer::new(),
            baseline: None,
            deltas: HashMap::new(),
            prev_status: HashMap::new(),
            pulses: HashMap::new(),
            mouse_capture: true,
            drill_scroll: 0,
            overlay_scroll: 0,
            logs_scroll: 0,
            card_regions: Vec::new(),
            layout: Layout::default(),
            spotlight_focus: 0,
            toast: None,
            correlate: None,
            chooser: None,
            log_lines: Vec::new(),
            features: FeatureFlags::default(),
        }
    }

    /// Replace the active set of experimental feature toggles. Host loop
    /// calls this once during startup from the CLI-parsed
    /// [`FeatureFlags`]. The reducer then branches off these (e.g.
    /// `events_overlay` gates [`Action::CorrelateEventRow`]).
    pub fn set_features(&mut self, features: FeatureFlags) {
        self.features = features;
    }

    /// Read-only view of the experimental feature toggles. Renderer /
    /// translator can use this to hide affordances tied to an off-flag
    /// experiment.
    pub fn features(&self) -> FeatureFlags {
        self.features
    }

    /// Seed the baseline used for `NEW` / `FIXED` delta badges. Pass
    /// `None` to clear (e.g. when the baseline file is missing).
    pub fn set_baseline(&mut self, baseline: Option<Baseline>) {
        self.baseline = baseline;
        self.recompute_deltas();
    }

    /// Per-layer `NEW` / `FIXED` / `Unchanged` map computed against the
    /// baseline most recently seeded with [`set_baseline`].
    pub fn deltas(&self) -> &HashMap<String, Delta> {
        &self.deltas
    }

    /// Whether the named layer's pip is currently mid-pulse.
    pub fn pulse_active(&self, layer_name: &str) -> bool {
        match (self.pulses.get(layer_name), self.now) {
            (Some(t), Some(now)) => t.is_active(now),
            _ => false,
        }
    }

    pub fn mouse_capture(&self) -> bool {
        self.mouse_capture
    }

    pub fn drill_scroll(&self) -> u16 {
        self.drill_scroll
    }

    /// Scroll offset for the snapshot logs panel when it is the dominant
    /// view (scorecard drill panel with `logs` focused). See
    /// [`logs_panel_dominant`](Self::logs_panel_dominant). Mission
    /// Control's `Logs` quadrant used to be the second dominant-view
    /// source; it was removed in PRD-006 slice 1 (issue #367).
    pub fn logs_scroll(&self) -> u16 {
        self.logs_scroll
    }

    /// Whether the snapshot logs panel is currently the dominant view —
    /// i.e. the surface the operator is reading. Returns true when the
    /// scorecard drill panel is showing the logs panel (focused layer is
    /// `logs`). Drives input routing so j/k/wheel target `logs_scroll`
    /// here. ADR-0014.
    pub fn logs_panel_dominant(&self) -> bool {
        match self.layout {
            Layout::Scorecard => self.focused().is_some_and(|s| s.name == "logs"),
            Layout::Spotlight => false,
        }
    }

    /// Which top-level layout the renderer should draw.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// Index of the focused incident card in Spotlight. Bounded by the
    /// number of non-green layers in the current snapshot (clamped on
    /// every refresh).
    pub fn spotlight_focus(&self) -> usize {
        self.spotlight_focus
    }

    /// The active bottom-bar toast (if any). The renderer surfaces it in
    /// the hint bar; the reducer clears it once `Tick` carries it past
    /// `expires_at`.
    pub fn toast(&self) -> Option<&Toast> {
        self.toast.as_ref()
    }

    /// Quick-correlate popover state. `None` when the popover has never
    /// been opened or has been dismissed (`Esc` / `q`). The renderer
    /// only consults this when `overlay() == Overlay::Correlate`.
    pub fn correlate_state(&self) -> Option<&CorrelateState> {
        self.correlate.as_ref()
    }

    /// PRD-007 Slice 1 (#372): multi-match chooser state. `None` outside
    /// of `Overlay::CorrelateChooser`.
    pub fn chooser_state(&self) -> Option<&ChooserState> {
        self.chooser.as_ref()
    }

    pub fn overlay_scroll(&self) -> u16 {
        self.overlay_scroll
    }

    /// The view layer captures the rendered scorecard rectangles here so
    /// that subsequent `Action::Click` events can be hit-tested against
    /// what the operator actually sees on screen.
    pub fn set_card_regions(&mut self, regions: Vec<Rect>) {
        self.card_regions = regions;
    }

    pub fn snapshots(&self) -> &[LayerSnapshot] {
        &self.snapshots
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    pub fn focused(&self) -> Option<&LayerSnapshot> {
        self.snapshots.get(self.focus)
    }

    pub fn overlay(&self) -> Overlay {
        self.overlay
    }

    pub fn refreshing(&self) -> bool {
        self.refreshing
    }

    pub fn last_refreshed(&self) -> Option<DateTime<Local>> {
        self.last_refreshed
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn history(&self) -> &RingBuffer {
        &self.history
    }

    /// Snapshot logs panel content, populated by the refresh side-effect.
    /// Empty vec means "no errors" (or no log source at all).
    pub fn log_lines(&self) -> &[LogLine] {
        &self.log_lines
    }

    /// Throbber glyph for the current frame: an animated braille spinner
    /// while a refresh is in flight, frozen `✓` once the latest refresh
    /// has completed, or empty when no run has happened yet.
    pub fn throbber_glyph(&self) -> String {
        if self.refreshing {
            let frame = match (self.now, self.boot) {
                (Some(now), Some(boot)) => {
                    let dt = now.saturating_duration_since(boot);
                    (dt.as_millis() / TICK.as_millis()) as usize % THROBBER_FRAMES.len()
                }
                _ => 0,
            };
            THROBBER_FRAMES[frame].to_string()
        } else if self.last_refreshed.is_some() {
            THROBBER_DONE.to_string()
        } else {
            String::new()
        }
    }

    /// The single mutator. Returns an optional `Effect` for the host loop
    /// to carry out (start a fetch, exit, …). Setting `dirty` is the
    /// reducer's job, not the caller's.
    pub fn handle(&mut self, action: Action) -> Option<Effect> {
        match action {
            Action::Refresh => {
                if self.refreshing {
                    return None;
                }
                self.refreshing = true;
                self.dirty = true;
                Some(Effect::StartRefresh)
            }
            Action::Focus(dir) => {
                if self.overlay != Overlay::None {
                    return None;
                }
                // PRD-006 Slice 5 (#371): j/k in Spotlight walks the
                // sorted incident-card list rather than the (invisible)
                // scorecard grid focus.
                if self.layout == Layout::Spotlight && matches!(dir, Dir::Up | Dir::Down) {
                    let n = self.spotlight_card_count();
                    if n == 0 {
                        return None;
                    }
                    let next = match dir {
                        Dir::Up => self.spotlight_focus.saturating_sub(1),
                        Dir::Down => (self.spotlight_focus + 1).min(n - 1),
                        _ => self.spotlight_focus,
                    };
                    if next != self.spotlight_focus {
                        self.spotlight_focus = next;
                        self.dirty = true;
                    }
                    return None;
                }
                if self.logs_panel_dominant() && matches!(dir, Dir::Up | Dir::Down) {
                    let next = match dir {
                        Dir::Up => self.logs_scroll.saturating_sub(1),
                        Dir::Down => self.logs_scroll.saturating_add(1),
                        _ => self.logs_scroll,
                    };
                    if next != self.logs_scroll {
                        self.logs_scroll = next;
                        self.dirty = true;
                    }
                    return None;
                }
                let was_logs_dominant = self.logs_panel_dominant();
                let moved = self.move_focus(dir);
                if moved {
                    self.dirty = true;
                    if was_logs_dominant && !self.logs_panel_dominant() {
                        self.logs_scroll = 0;
                    }
                }
                None
            }
            Action::OpenDetail => {
                if !self.snapshots.is_empty() && self.overlay == Overlay::None {
                    self.overlay = Overlay::Detail;
                    self.dirty = true;
                }
                None
            }
            Action::LogLines(lines) => {
                self.log_lines = lines;
                self.logs_scroll = 0;
                self.dirty = true;
                None
            }
            Action::OpenHelp => {
                if self.overlay == Overlay::None {
                    self.overlay = Overlay::Help;
                    self.dirty = true;
                }
                None
            }
            Action::ShowLogs => {
                if self.overlay == Overlay::None {
                    self.overlay = Overlay::Logs;
                    self.logs_scroll = 0;
                    self.dirty = true;
                }
                None
            }
            Action::CloseOverlay => {
                if self.overlay == Overlay::None {
                    return None;
                }
                let had_correlate = self.correlate.is_some();
                self.overlay = Overlay::None;
                self.correlate = None;
                self.chooser = None;
                self.dirty = true;
                // Slice 2: tearing down the popup must abort the
                // runner's per-Source futures so we never leak tasks
                // (verified by `correlate_runner::tests::dropping_…`).
                if had_correlate {
                    return Some(Effect::CancelCorrelate);
                }
                None
            }
            Action::Resize => {
                self.dirty = true;
                None
            }
            Action::Snapshots(snaps) => {
                if self.focus >= snaps.len() && !snaps.is_empty() {
                    self.focus = snaps.len() - 1;
                }
                self.update_pulses(&snaps);
                self.update_prev_status(&snaps);
                self.history.push(run_snapshot_from(&snaps));
                self.snapshots = snaps;
                self.refreshing = false;
                self.last_refreshed = Some(Local::now());
                if let Some(now) = self.now {
                    self.next_refresh_at = Some(now + self.interval);
                }
                self.recompute_deltas();
                self.clamp_spotlight_focus();
                self.dirty = true;
                None
            }
            Action::TogglePause => {
                self.paused = !self.paused;
                self.dirty = true;
                None
            }
            Action::Tick(now) => {
                if self.boot.is_none() {
                    self.boot = Some(now);
                }
                self.now = Some(now);
                if let Some(t) = &self.toast
                    && now >= t.expires_at
                {
                    self.toast = None;
                    self.dirty = true;
                }
                if self.refreshing {
                    self.dirty = true;
                    return None;
                }
                if !self.paused && self.next_refresh_at.is_some_and(|deadline| now >= deadline) {
                    self.refreshing = true;
                    self.next_refresh_at = None;
                    self.dirty = true;
                    return Some(Effect::StartRefresh);
                }
                None
            }
            Action::Click { col, row } => {
                if self.overlay != Overlay::None {
                    return None;
                }
                if let Some(idx) = self.card_regions.iter().position(|r| contains(r, col, row))
                    && idx < self.snapshots.len()
                    && idx != self.focus
                {
                    self.focus = idx;
                    self.drill_scroll = 0;
                    self.logs_scroll = 0;
                    self.dirty = true;
                }
                None
            }
            Action::Scroll(dir) => {
                let target = if self.overlay == Overlay::Detail {
                    &mut self.overlay_scroll
                } else if self.overlay == Overlay::None {
                    if self.logs_panel_dominant() {
                        &mut self.logs_scroll
                    } else {
                        &mut self.drill_scroll
                    }
                } else {
                    return None;
                };
                let next = match dir {
                    ScrollDir::Up => target.saturating_sub(1),
                    ScrollDir::Down => target.saturating_add(1),
                };
                if next != *target {
                    *target = next;
                    self.dirty = true;
                }
                None
            }
            Action::ToggleMouseCapture => {
                self.mouse_capture = !self.mouse_capture;
                self.dirty = true;
                Some(if self.mouse_capture {
                    Effect::EnableMouseCapture
                } else {
                    Effect::DisableMouseCapture
                })
            }
            Action::ShowSpotlight => {
                if self.layout != Layout::Spotlight {
                    self.layout = Layout::Spotlight;
                    self.spotlight_focus = 0;
                    self.dirty = true;
                }
                None
            }
            Action::ShowAll => {
                if self.layout != Layout::Scorecard {
                    self.layout = Layout::Scorecard;
                    self.dirty = true;
                }
                None
            }
            Action::CopyNextCommand => {
                if self.layout != Layout::Spotlight {
                    return None;
                }
                match self.spotlight_next_command() {
                    Some(cmd) => Some(Effect::CopyToClipboard(cmd)),
                    None => {
                        self.set_toast("no next-command for focused finding");
                        None
                    }
                }
            }
            Action::OpenLink => {
                if self.layout != Layout::Spotlight {
                    return None;
                }
                match self.spotlight_link() {
                    Some(url) => Some(Effect::OpenUrl(url)),
                    None => {
                        self.set_toast("no link for focused finding");
                        None
                    }
                }
            }
            Action::Correlate => {
                if self.overlay == Overlay::Logs {
                    return self.correlate_from_focused_log_line();
                }
                if self.overlay != Overlay::None {
                    return None;
                }
                match self.focused_entity() {
                    Some(entity) => self.handle(Action::OpenCorrelatePopup(entity)),
                    None => {
                        // PRD-007 Slice 0: Spotlight gives operator-visible
                        // feedback when the row carries no entity. Scorecard
                        // stays silent (preserves the issue-#157 contract
                        // that `c` is inert outside the workflows layer).
                        if self.layout == Layout::Spotlight {
                            self.set_toast("no entity found in this row");
                        }
                        None
                    }
                }
            }
            Action::OpenCorrelatePopup(entity) => {
                if self.overlay != Overlay::None {
                    return None;
                }
                self.overlay = Overlay::Correlate;
                self.correlate = Some(CorrelateState {
                    entity: entity.clone(),
                    sources: Vec::new(),
                    events: Vec::new(),
                    source_errors: Vec::new(),
                    diagnosis: None,
                    run_done: false,
                });
                self.dirty = true;
                Some(Effect::RunCorrelate(entity))
            }
            Action::CorrelateUpdate { entity, update } => {
                let state = self.correlate.as_mut()?;
                if state.entity != entity {
                    // Stale update from a previous popup invocation.
                    return None;
                }
                apply_correlate_update(state, update);
                self.dirty = true;
                None
            }
            Action::ShowCorrelateChooser(entities) => {
                if self.overlay != Overlay::None || entities.is_empty() {
                    return None;
                }
                self.overlay = Overlay::CorrelateChooser;
                self.chooser = Some(ChooserState { entities, focus: 0 });
                self.dirty = true;
                None
            }
            Action::ChooserNavigate(dir) => {
                let state = self.chooser.as_mut()?;
                let n = state.entities.len();
                if n == 0 {
                    return None;
                }
                let next = match dir {
                    Dir::Up => state.focus.saturating_sub(1),
                    Dir::Down => (state.focus + 1).min(n - 1),
                    Dir::Left | Dir::Right => state.focus,
                };
                if next != state.focus {
                    state.focus = next;
                    self.dirty = true;
                }
                None
            }
            Action::ChooserConfirm => {
                let state = self.chooser.take()?;
                let entity = state.entities.into_iter().nth(state.focus)?;
                // Close the chooser overlay first so OpenCorrelatePopup's
                // overlay-precondition check sees `Overlay::None`.
                self.overlay = Overlay::None;
                self.dirty = true;
                self.handle(Action::OpenCorrelatePopup(entity))
            }
            Action::ShowToast(msg) => {
                self.set_toast(&msg);
                None
            }
            Action::ToggleCorrelateFullscreen => {
                match self.overlay {
                    Overlay::Correlate => self.overlay = Overlay::CorrelateFullscreen,
                    Overlay::CorrelateFullscreen => self.overlay = Overlay::Correlate,
                    _ => return None,
                }
                self.dirty = true;
                None
            }
            Action::SpotlightDrillStub => {
                if self.layout != Layout::Spotlight || self.overlay != Overlay::None {
                    return None;
                }
                self.set_toast(crate::view::SPOTLIGHT_DRILL_STUB_TOAST);
                None
            }
            Action::CorrelateEventRow { text, tags } => {
                self.correlate_from_event_row(&text, &tags)
            }
            Action::Quit => Some(Effect::Quit),
        }
    }

    fn set_toast(&mut self, msg: &str) {
        let expires_at = self
            .now
            .map(|n| n + TOAST_TTL)
            .unwrap_or_else(|| Instant::now() + TOAST_TTL);
        self.toast = Some(Toast {
            message: msg.to_string(),
            expires_at,
        });
        self.dirty = true;
    }

    /// First entity (DPU / workflow / host / request) extracted from a
    /// Finding on the currently focused row. PRD-007 Slice 0 broadens
    /// from the workflow-only path (issue #157) so `c` works on any row
    /// that mentions an entity ID. PRD-007 Slice 4 (#377) prefers the
    /// Findings detail surface's `next_command` field (NextCommand context,
    /// `Parsed` confidence) over the legacy message regex (Heuristic) so
    /// the detail-row trigger lands ahead of the Slice 0 fallback. Returns
    /// `None` when neither surface yields an entity; the reducer turns
    /// that into a toast in Spotlight or stays silent in Scorecard.
    fn focused_entity(&self) -> Option<EntityRef> {
        use crate::entity_extraction::{ExtractionContext, extract_entities};
        let snap = self.focused_for_correlate()?;
        let from_next_command = snap.findings.iter().find_map(|f| {
            let cmd = f.next_command.as_deref()?;
            extract_entities(cmd, ExtractionContext::NextCommand)
                .into_iter()
                .next()
        });
        if from_next_command.is_some() {
            return from_next_command;
        }
        snap.findings.iter().find_map(extract_entity_from_finding)
    }

    /// PRD-007 Slice 3 (#376): handle `c` while the logs overlay is open.
    /// Extracts entities from the focused log line via the Slice 1
    /// primitive and dispatches to popup / chooser / toast. Returning
    /// here is the only `Action::Correlate` path that may transition out
    /// of `Overlay::Logs` (single or multi-match cases) — the no-entity
    /// case keeps the overlay open so the operator can keep scrolling.
    fn correlate_from_focused_log_line(&mut self) -> Option<Effect> {
        use crate::entity_extraction::{ExtractionContext, extract_entities};
        let message = match self.log_lines.get(self.logs_scroll as usize) {
            Some(line) => line.message.clone(),
            None => return None,
        };
        let entities = extract_entities(&message, ExtractionContext::LogLine);
        match entities.len() {
            0 => {
                self.set_toast("no entity found in this row");
                None
            }
            // OpenCorrelatePopup and ShowCorrelateChooser both reject if
            // an overlay is already up — close the logs overlay first so
            // they see `Overlay::None` and seed cleanly.
            1 => {
                self.overlay = Overlay::None;
                self.dirty = true;
                let entity = entities.into_iter().next().unwrap();
                self.handle(Action::OpenCorrelatePopup(entity))
            }
            _ => {
                self.overlay = Overlay::None;
                self.dirty = true;
                self.handle(Action::ShowCorrelateChooser(entities))
            }
        }
    }

    /// PRD-007 Slice 5 (#379): handle `Action::CorrelateEventRow`. Tag
    /// vocabulary (`host_id`, `dpu_id`, …) is preferred over message-text
    /// regex; tagged hits get `Explicit` confidence, regex fallbacks get
    /// `Heuristic`. Behaves identically to the log-line trigger from
    /// there (popup / chooser / toast).
    ///
    /// Gated by the `events-overlay` feature flag — the events overlay
    /// UI is not a PRD-007 deliverable, so the trigger ships behind the
    /// gate and stays reducer-testable until a future PRD wires a
    /// surface that fires this action.
    fn correlate_from_event_row(
        &mut self,
        text: &str,
        tags: &[(String, String)],
    ) -> Option<Effect> {
        if !self.features.events_overlay {
            return None;
        }
        use crate::entity_extraction::{ExtractionContext, extract_entities};
        // ExtractionContext::EventRow wants `&[(&str, &str)]`; reborrow
        // the owned tag set into that shape for the one call.
        let tag_refs: Vec<(&str, &str)> =
            tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let entities = extract_entities(text, ExtractionContext::EventRow { tags: &tag_refs });
        match entities.len() {
            0 => {
                self.set_toast("no entity found in this row");
                None
            }
            1 => {
                let entity = entities.into_iter().next().unwrap();
                self.handle(Action::OpenCorrelatePopup(entity))
            }
            _ => self.handle(Action::ShowCorrelateChooser(entities)),
        }
    }

    /// In Spotlight, `[c]` should target the focused incident card; in
    /// the scorecard layout, the focused scorecard. This returns
    /// whichever the current layout considers focused.
    fn focused_for_correlate(&self) -> Option<&LayerSnapshot> {
        match self.layout {
            Layout::Spotlight => self
                .non_green_snapshots()
                .get(self.spotlight_focus)
                .copied(),
            Layout::Scorecard => self.snapshots.get(self.focus),
        }
    }

    fn non_green_snapshots(&self) -> Vec<&LayerSnapshot> {
        let mut cards: Vec<&LayerSnapshot> = self
            .snapshots
            .iter()
            .filter(|s| !matches!(s.status, Status::Ok | Status::Skipped))
            .collect();
        // PRD-006 Slice 5 (#371): severity-major, Delta-minor ordering so
        // the most actionable card is on top. Severity rank: Fail > Warn >
        // Unknown. Within a group, NEW > Unchanged > Fixed (Fixed never
        // shows here today — fixed layers are green and live in the
        // footer — but the comparator is total so the contract is stable).
        cards.sort_by_key(|s| {
            let sev = match s.status {
                Status::Fail => 0,
                Status::Warn => 1,
                Status::Unknown => 2,
                _ => 3,
            };
            let delta = match self.deltas.get(&s.name) {
                Some(Delta::New) => 0,
                Some(Delta::Unchanged) | None => 1,
                Some(Delta::Fixed) => 2,
            };
            (sev, delta)
        });
        cards
    }

    fn spotlight_next_command(&self) -> Option<String> {
        let cards = self.non_green_snapshots();
        let card = cards.get(self.spotlight_focus)?;
        card.findings.iter().find_map(|f| f.next_command.clone())
    }

    fn spotlight_link(&self) -> Option<String> {
        let cards = self.non_green_snapshots();
        let card = cards.get(self.spotlight_focus)?;
        card.findings.iter().find_map(|f| f.link.clone())
    }

    fn clamp_spotlight_focus(&mut self) {
        let n = self.non_green_snapshots().len();
        if n == 0 {
            self.spotlight_focus = 0;
        } else if self.spotlight_focus >= n {
            self.spotlight_focus = n - 1;
        }
    }

    /// Number of non-green Layers in the current snapshot — i.e. how
    /// many incident cards Layout C will render.
    pub fn spotlight_card_count(&self) -> usize {
        self.non_green_snapshots().len()
    }

    /// Snapshot of all current incident cards (non-green Layers), in
    /// snapshot order. The renderer uses this to lay out cards; tests
    /// use it to assert which Layers made the cut.
    pub fn spotlight_cards(&self) -> Vec<&LayerSnapshot> {
        self.non_green_snapshots()
    }

    /// Names of the green Layers that should be compressed into the
    /// Layout C footer line, in snapshot order.
    pub fn spotlight_green_layer_names(&self) -> Vec<String> {
        self.snapshots
            .iter()
            .filter(|s| matches!(s.status, Status::Ok | Status::Skipped))
            .map(|s| s.name.clone())
            .collect()
    }

    /// Test seam: lets unit tests drive the throbber without going through
    /// the full Tick handler (which also implicates auto-refresh).
    #[cfg(test)]
    fn force_now(&mut self, boot: Instant, now: Instant) {
        self.boot = Some(boot);
        self.now = Some(now);
    }

    /// Test seam: replace `log_lines` without going through
    /// `Action::LogLines` (which resets `logs_scroll`). Lets the renderer
    /// clamp test verify behavior on a stale scroll offset.
    #[cfg(test)]
    pub(crate) fn set_log_lines_for_test(&mut self, lines: Vec<LogLine>) {
        self.log_lines = lines;
        self.dirty = true;
    }

    fn update_pulses(&mut self, snaps: &[LayerSnapshot]) {
        let Some(now) = self.now else { return };
        for s in snaps {
            if let Some(prev) = self.prev_status.get(&s.name)
                && *prev != s.status
            {
                self.pulses.entry(s.name.clone()).or_default().start(now);
            }
        }
    }

    fn update_prev_status(&mut self, snaps: &[LayerSnapshot]) {
        for s in snaps {
            self.prev_status.insert(s.name.clone(), s.status.clone());
        }
    }

    fn recompute_deltas(&mut self) {
        let baseline_ref = self.baseline.as_ref();
        self.deltas = compute_deltas_for(
            self.snapshots.iter().map(|s| (s.name.as_str(), &s.status)),
            baseline_ref,
        );
    }

    fn move_focus(&mut self, dir: Dir) -> bool {
        let n = self.snapshots.len();
        if n == 0 {
            return false;
        }
        let cols = grid_cols(n);
        let cur = self.focus.min(n - 1);
        let next = match dir {
            Dir::Left => {
                if cur.is_multiple_of(cols) {
                    return false;
                }
                cur - 1
            }
            Dir::Right => {
                if cur + 1 >= n || (cur + 1).is_multiple_of(cols) {
                    return false;
                }
                cur + 1
            }
            Dir::Up => {
                if cur < cols {
                    return false;
                }
                cur - cols
            }
            Dir::Down => {
                if cur + cols >= n {
                    return false;
                }
                cur + cols
            }
        };
        if next == cur {
            return false;
        }
        self.focus = next;
        true
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// Scorecard grid column count. The grid is 3-up always at the model
/// level; the renderer decides whether to reflow to 2-up or 1-up based on
/// terminal width — that's a pure presentation concern. Focus navigation
/// uses 3 columns so that the indices line up with what the operator sees
/// on a normal-width terminal.
fn grid_cols(_n: usize) -> usize {
    3
}

fn run_snapshot_from(snaps: &[LayerSnapshot]) -> RunSnapshot {
    let layers: Vec<LayerStat> = snaps
        .iter()
        .map(|s| LayerStat {
            name: s.name.clone(),
            status: s.status.clone(),
            finding_count: s.findings.len(),
            duration_ms: s.duration_ms,
        })
        .collect();
    let total_ms: u64 = layers.iter().map(|l| l.duration_ms).sum();
    RunSnapshot {
        timestamp: Local::now(),
        total_duration: Duration::from_millis(total_ms),
        layers,
    }
}

fn contains(r: &Rect, col: u16, row: u16) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

/// PRD-007 Slice 2: fold one streamed [`CorrelateUpdate`] into the
/// popup's accumulating state. Pure — easy to unit-test without going
/// through the full reducer.
fn apply_correlate_update(state: &mut CorrelateState, update: CorrelateUpdate) {
    match update {
        CorrelateUpdate::Loading { sources } => {
            // Build the per-Source progress strip in the order the
            // runner declared so the dots row reads left-to-right in
            // a stable layout (temporal · postgres · k8s · loki · …).
            state.sources = sources
                .into_iter()
                .map(|name| SourceProgress {
                    name: name.to_string(),
                    status: SourceStatus::Pending,
                })
                .collect();
        }
        CorrelateUpdate::SourceLanded { source, events } => {
            mark_source(state, source, SourceStatus::Landed);
            state.events.extend(events);
            // Keep the timeline chronologically sorted as events
            // accumulate across Sources.
            state.events.sort_by_key(|e| e.ts);
        }
        CorrelateUpdate::SourceFailed { source, reason } => {
            mark_source(state, source, SourceStatus::Failed);
            state.source_errors.push(SourceError {
                name: source.to_string(),
                reason,
            });
        }
        CorrelateUpdate::Diagnosis { diagnosis } => {
            state.diagnosis = diagnosis;
        }
        CorrelateUpdate::Done => {
            state.run_done = true;
        }
    }
}

fn mark_source(state: &mut CorrelateState, name: &'static str, status: SourceStatus) {
    if let Some(p) = state.sources.iter_mut().find(|p| p.name == name) {
        p.status = status;
    } else {
        // Defensive: a SourceLanded for a Source the Loading event didn't
        // mention shouldn't happen, but if it does append rather than drop
        // so the dots row still reflects what the runner reported.
        state.sources.push(SourceProgress {
            name: name.to_string(),
            status,
        });
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
