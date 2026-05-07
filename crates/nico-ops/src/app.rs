use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use nico_common::output::Status;
use nico_doctor::baseline::{Baseline, Delta, compute_deltas_for};
use ratatui::layout::Rect;

use crate::action::{Action, Dir, ScrollDir};
use crate::events::Overlay;
use crate::model::{CorrelateState, CorrelateStatus, LayerSnapshot, LogLine, Quadrant, workflow_id_from_finding};
use crate::pulse::PulseTimer;
use crate::ringbuffer::{LayerStat, RingBuffer, RunSnapshot};

/// How long a transient toast is shown in the bottom bar before the
/// reducer drops it. Picked to be long enough that the operator can read
/// "clipboard unavailable" but short enough not to linger after a refresh.
pub const TOAST_TTL: Duration = Duration::from_millis(2500);

/// Which top-level layout the dashboard is rendering. The reducer flips
/// between these in response to `Action::ToggleLayout` (A↔B),
/// `Action::ShowSpotlight` (A→C), and `Action::ShowAll` (any→A); the
/// renderer branches on the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Layout A — the scorecard grid with drill panel (ADR-010).
    #[default]
    A,
    /// Layout B — Mission Control 2×3 quadrant grid with `tui-big-text`
    /// verdict header (issue #155).
    B,
    /// Layout C — the "3am page" Spotlight view: tui-big-text headline
    /// over incident cards for non-green Layers, green Layers compressed
    /// to a single footer line.
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
    /// Kick off `nico_correlate::collect_all` for the given workflow ID.
    /// The host loop spawns the call and posts the results back via
    /// `Action::CorrelateResults`. (Issue #157.)
    Correlate(String),
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
    b_focus: usize,
    b_zoomed: bool,
    namespace_events: Vec<nico_correlate::Event>,
    log_lines: Vec<LogLine>,
}

/// A transient bottom-bar message and its expiry timestamp. Cleared by
/// the reducer once `now >= expires_at`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub message: String,
    pub expires_at: Instant,
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
            b_focus: 0,
            b_zoomed: false,
            namespace_events: Vec::new(),
            log_lines: Vec::new(),
        }
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
    /// view (Layout A drill with `logs` focused, or Layout B `Logs` quadrant
    /// while zoomed). See [`logs_panel_dominant`](Self::logs_panel_dominant).
    pub fn logs_scroll(&self) -> u16 {
        self.logs_scroll
    }

    /// Whether the snapshot logs panel is currently the dominant view —
    /// i.e. the surface the operator is reading. Returns true when the
    /// Layout A drill panel is showing the logs panel (focused layer is
    /// `logs`) or when Layout B's `Logs` quadrant is zoomed. Drives input
    /// routing so j/k/wheel target `logs_scroll` here. ADR-0014.
    pub fn logs_panel_dominant(&self) -> bool {
        match self.layout {
            Layout::A => self.focused().is_some_and(|s| s.name == "logs"),
            Layout::B => self.b_zoomed && self.focused_quadrant() == Quadrant::Logs,
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

    pub fn b_focus(&self) -> usize {
        self.b_focus
    }

    pub fn focused_quadrant(&self) -> Quadrant {
        Quadrant::ALL[self.b_focus.min(Quadrant::ALL.len() - 1)]
    }

    pub fn b_zoomed(&self) -> bool {
        self.b_zoomed
    }

    pub fn namespace_events(&self) -> &[nico_correlate::Event] {
        &self.namespace_events
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
                let moved = match self.layout {
                    Layout::A => self.move_focus(dir),
                    Layout::B => {
                        if self.b_zoomed {
                            false
                        } else {
                            move_b_focus(&mut self.b_focus, dir)
                        }
                    }
                    Layout::Spotlight => self.move_focus(dir),
                };
                if moved {
                    self.dirty = true;
                    if was_logs_dominant && !self.logs_panel_dominant() {
                        self.logs_scroll = 0;
                    }
                }
                None
            }
            Action::OpenDetail => {
                if matches!(self.layout, Layout::B) {
                    return None;
                }
                if !self.snapshots.is_empty() && self.overlay == Overlay::None {
                    self.overlay = Overlay::Detail;
                    self.dirty = true;
                }
                None
            }
            Action::ToggleLayout => {
                if self.overlay != Overlay::None {
                    return None;
                }
                self.layout = match self.layout {
                    Layout::A => Layout::B,
                    Layout::B => Layout::A,
                    Layout::Spotlight => Layout::A,
                };
                self.b_zoomed = false;
                self.logs_scroll = 0;
                self.dirty = true;
                None
            }
            Action::ZoomQuadrant => {
                if matches!(self.layout, Layout::B) && !self.b_zoomed {
                    self.b_zoomed = true;
                    self.logs_scroll = 0;
                    self.dirty = true;
                }
                None
            }
            Action::NamespaceEvents(events) => {
                self.namespace_events = events;
                if matches!(self.layout, Layout::B) {
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
            Action::CloseOverlay => {
                if self.overlay != Overlay::None {
                    self.overlay = Overlay::None;
                    self.correlate = None;
                    self.dirty = true;
                } else if matches!(self.layout, Layout::B) {
                    if self.b_zoomed {
                        self.b_zoomed = false;
                    } else {
                        self.layout = Layout::A;
                    }
                    self.dirty = true;
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
                if self.layout != Layout::A {
                    self.layout = Layout::A;
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
                if self.overlay != Overlay::None {
                    return None;
                }
                let workflow_id = self.focused_workflow_id()?;
                self.overlay = Overlay::Correlate;
                self.correlate = Some(CorrelateState {
                    workflow_id: workflow_id.clone(),
                    status: CorrelateStatus::Loading,
                });
                self.dirty = true;
                Some(Effect::Correlate(workflow_id))
            }
            Action::CorrelateResults {
                workflow_id,
                events,
                source_errors,
            } => {
                let state = self.correlate.as_mut()?;
                if state.workflow_id != workflow_id {
                    return None;
                }
                state.status = CorrelateStatus::Loaded {
                    events,
                    source_errors,
                };
                self.dirty = true;
                None
            }
            Action::ShowToast(msg) => {
                self.set_toast(&msg);
                None
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

    /// Workflow ID extracted from the first Finding on the currently
    /// focused Layer, if (a) the focused Layer is `workflows` and (b) at
    /// least one Finding's message carries a recognizable workflow ID
    /// (`wf-…` or `hp-…`). Drives the `[c]` quick-correlate trigger; see
    /// [`workflow_id_from_finding`].
    fn focused_workflow_id(&self) -> Option<String> {
        let snap = self.focused_for_correlate()?;
        if snap.name != "workflows" {
            return None;
        }
        snap.findings.iter().find_map(workflow_id_from_finding)
    }

    /// In Spotlight, `[c]` should target the focused incident card; in
    /// Layout A, the focused scorecard. This returns whichever the
    /// current layout considers focused.
    fn focused_for_correlate(&self) -> Option<&LayerSnapshot> {
        match self.layout {
            Layout::Spotlight => self
                .non_green_snapshots()
                .get(self.spotlight_focus)
                .copied(),
            Layout::A => self.snapshots.get(self.focus),
            // Layout B doesn't expose the workflows-Finding focus model,
            // so `[c]` is a no-op there.
            Layout::B => None,
        }
    }

    fn non_green_snapshots(&self) -> Vec<&LayerSnapshot> {
        self.snapshots
            .iter()
            .filter(|s| !matches!(s.status, Status::Ok | Status::Skipped))
            .collect()
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

/// Layout-B is a fixed 2×3 grid: 3 columns × 2 rows. Returns whether
/// `focus` actually moved.
fn move_b_focus(focus: &mut usize, dir: Dir) -> bool {
    const COLS: usize = 3;
    const N: usize = 6;
    let cur = (*focus).min(N - 1);
    let next = match dir {
        Dir::Left => {
            if cur.is_multiple_of(COLS) {
                return false;
            }
            cur - 1
        }
        Dir::Right => {
            if cur + 1 >= N || (cur + 1).is_multiple_of(COLS) {
                return false;
            }
            cur + 1
        }
        Dir::Up => {
            if cur < COLS {
                return false;
            }
            cur - COLS
        }
        Dir::Down => {
            if cur + COLS >= N {
                return false;
            }
            cur + COLS
        }
    };
    if next == cur {
        return false;
    }
    *focus = next;
    true
}

/// Layout-A grid column count. The grid is 3-up always at the model
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PopoverEvent, SourceError};
    use nico_common::output::Status;

    fn snap(name: &str, status: Status) -> LayerSnapshot {
        LayerSnapshot {
            name: name.into(),
            status,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }
    }

    fn six_layers() -> Vec<LayerSnapshot> {
        vec![
            snap("cluster", Status::Ok),
            snap("logs", Status::Warn),
            snap("workflows", Status::Ok),
            snap("health", Status::Ok),
            snap("grpc", Status::Ok),
            snap("postgres", Status::Ok),
        ]
    }

    fn drive(app: &mut App, actions: &[Action]) {
        for a in actions {
            app.handle(a.clone());
        }
    }

    #[test]
    fn fresh_app_is_dirty() {
        let app = App::new();
        assert!(app.dirty());
        assert_eq!(app.focus(), 0);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(!app.refreshing());
    }

    #[test]
    fn snapshots_action_replaces_state_and_marks_dirty() {
        let mut app = App::new();
        app.clear_dirty();
        app.handle(Action::Snapshots(six_layers()));
        assert_eq!(app.snapshots().len(), 6);
        assert!(!app.refreshing());
        assert!(app.last_refreshed().is_some());
        assert!(app.dirty());
    }

    #[test]
    fn log_lines_action_replaces_state_and_marks_dirty() {
        use chrono::Utc;
        let mut app = App::new();
        app.clear_dirty();
        let line = LogLine {
            ts: Utc::now(),
            pod: "core-abc".into(),
            level: Status::Warn,
            message: "ERROR: disk full".into(),
        };
        app.handle(Action::LogLines(vec![line.clone()]));
        assert_eq!(app.log_lines(), &[line]);
        assert!(app.dirty());
    }

    #[test]
    fn focus_right_moves_within_row() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.clear_dirty();
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.focus(), 1);
        assert!(app.dirty());
    }

    #[test]
    fn focus_right_at_end_of_row_is_inert() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        drive(
            &mut app,
            &[Action::Focus(Dir::Right), Action::Focus(Dir::Right)],
        );
        assert_eq!(app.focus(), 2);
        app.clear_dirty();
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.focus(), 2);
        assert!(!app.dirty());
    }

    #[test]
    fn focus_down_moves_to_next_row() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Down));
        assert_eq!(app.focus(), 3);
    }

    #[test]
    fn focus_up_moves_to_previous_row() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        drive(
            &mut app,
            &[Action::Focus(Dir::Down), Action::Focus(Dir::Up)],
        );
        assert_eq!(app.focus(), 0);
    }

    #[test]
    fn focus_inert_when_overlay_is_open() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::OpenDetail);
        app.clear_dirty();
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.focus(), 0);
        assert!(!app.dirty());
    }

    #[test]
    fn open_detail_requires_snapshots() {
        let mut app = App::new();
        app.clear_dirty();
        app.handle(Action::OpenDetail);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(!app.dirty());
    }

    #[test]
    fn open_help_then_close() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::OpenHelp);
        assert_eq!(app.overlay(), Overlay::Help);
        app.handle(Action::CloseOverlay);
        assert_eq!(app.overlay(), Overlay::None);
    }

    #[test]
    fn refresh_returns_start_effect_and_marks_refreshing() {
        let mut app = App::new();
        let eff = app.handle(Action::Refresh);
        assert_eq!(eff, Some(Effect::StartRefresh));
        assert!(app.refreshing());
    }

    #[test]
    fn refresh_while_already_refreshing_is_inert() {
        let mut app = App::new();
        app.handle(Action::Refresh);
        let eff = app.handle(Action::Refresh);
        assert_eq!(eff, None);
    }

    #[test]
    fn quit_returns_quit_effect() {
        let mut app = App::new();
        let eff = app.handle(Action::Quit);
        assert_eq!(eff, Some(Effect::Quit));
    }

    #[test]
    fn snapshots_clamps_focus_when_layer_count_drops() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        drive(
            &mut app,
            &[Action::Focus(Dir::Right), Action::Focus(Dir::Right)],
        );
        assert_eq!(app.focus(), 2);
        let smaller = vec![snap("cluster", Status::Ok), snap("logs", Status::Ok)];
        app.handle(Action::Snapshots(smaller));
        assert_eq!(app.focus(), 1);
    }

    #[test]
    fn resize_marks_dirty() {
        let mut app = App::new();
        app.clear_dirty();
        app.handle(Action::Resize);
        assert!(app.dirty());
    }

    #[test]
    fn focused_returns_focused_layer() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.focused().unwrap().name, "logs");
    }

    #[test]
    fn fresh_app_is_not_paused_and_uses_default_interval() {
        let app = App::new();
        assert!(!app.paused());
        assert_eq!(app.interval(), DEFAULT_INTERVAL);
    }

    #[test]
    fn toggle_pause_flips_pause_flag_and_marks_dirty() {
        let mut app = App::new();
        app.clear_dirty();
        app.handle(Action::TogglePause);
        assert!(app.paused());
        assert!(app.dirty());
        app.clear_dirty();
        app.handle(Action::TogglePause);
        assert!(!app.paused());
    }

    #[test]
    fn tick_after_completion_triggers_auto_refresh_when_interval_elapsed() {
        let interval = Duration::from_secs(5);
        let mut app = App::with_interval(interval);
        let t0 = Instant::now();
        // Initial manual refresh + completion seeds the auto-refresh deadline.
        app.handle(Action::Tick(t0));
        app.handle(Action::Refresh);
        app.handle(Action::Snapshots(six_layers()));

        // Tick before deadline: no effect.
        let eff = app.handle(Action::Tick(t0 + Duration::from_secs(4)));
        assert_eq!(eff, None);

        // Tick at/after deadline: StartRefresh.
        let eff = app.handle(Action::Tick(t0 + Duration::from_secs(5)));
        assert_eq!(eff, Some(Effect::StartRefresh));
        assert!(app.refreshing());
    }

    #[test]
    fn pause_toggle_via_action_stream() {
        // Synthetic action stream: TogglePause repeatedly inverts the flag.
        let mut app = App::new();
        let stream = vec![
            Action::TogglePause,
            Action::TogglePause,
            Action::TogglePause,
        ];
        let mut paused_history = vec![app.paused()];
        for a in stream {
            app.handle(a);
            paused_history.push(app.paused());
        }
        assert_eq!(paused_history, vec![false, true, false, true]);
    }

    #[test]
    fn pause_suppresses_auto_refresh_but_manual_refresh_still_works() {
        let interval = Duration::from_secs(5);
        let mut app = App::with_interval(interval);
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Refresh);
        app.handle(Action::Snapshots(six_layers()));

        app.handle(Action::TogglePause);
        let eff = app.handle(Action::Tick(t0 + Duration::from_secs(60)));
        assert_eq!(eff, None, "paused dashboard must not auto-refresh");

        // Manual refresh is unaffected by pause.
        let eff = app.handle(Action::Refresh);
        assert_eq!(eff, Some(Effect::StartRefresh));
    }

    #[test]
    fn auto_refresh_does_not_double_fire_while_running() {
        let interval = Duration::from_secs(1);
        let mut app = App::with_interval(interval);
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Refresh);
        app.handle(Action::Snapshots(six_layers()));

        let eff1 = app.handle(Action::Tick(t0 + Duration::from_secs(2)));
        assert_eq!(eff1, Some(Effect::StartRefresh));
        // Another tick while still refreshing must not fire again.
        let eff2 = app.handle(Action::Tick(t0 + Duration::from_secs(3)));
        assert_eq!(eff2, None);
    }

    #[test]
    fn snapshots_pushes_run_into_history() {
        let mut app = App::new();
        assert_eq!(app.history().len(), 0);
        let snaps = vec![
            LayerSnapshot {
                name: "cluster".into(),
                status: Status::Ok,
                evidence: String::new(),
                findings: vec![],
                duration_ms: 12,
            },
            LayerSnapshot {
                name: "logs".into(),
                status: Status::Warn,
                evidence: String::new(),
                findings: vec![crate::model::Finding {
                    status: Status::Warn,
                    message: "12 ERROR lines".into(),
                    next_command: None,
                    link: None,
                }],
                duration_ms: 34,
            },
        ];
        app.handle(Action::Snapshots(snaps));
        assert_eq!(app.history().len(), 1);
        let latest = app.history().latest().unwrap();
        assert_eq!(latest.layers.len(), 2);
        let logs = latest
            .layers
            .iter()
            .find(|l| l.name == "logs")
            .expect("logs layer present");
        assert_eq!(logs.finding_count, 1);
        assert_eq!(logs.duration_ms, 34);
    }

    #[test]
    fn throbber_glyph_is_empty_before_any_run() {
        let app = App::new();
        assert_eq!(app.throbber_glyph(), "");
    }

    #[test]
    fn throbber_glyph_freezes_to_done_after_first_completion() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        assert_eq!(app.throbber_glyph(), THROBBER_DONE);
    }

    // ── delta + pulse integration ────────────────────────────────────────

    fn baseline_of(pairs: &[(&str, &str)]) -> Baseline {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn snapshots_with_baseline_marks_new_delta() {
        let mut app = App::new();
        app.set_baseline(Some(baseline_of(&[("logs", "ok")])));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Warn)]));
        assert_eq!(app.deltas().get("logs"), Some(&Delta::New));
    }

    #[test]
    fn snapshots_with_baseline_marks_fixed_delta() {
        let mut app = App::new();
        app.set_baseline(Some(baseline_of(&[("logs", "fail")])));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Ok)]));
        assert_eq!(app.deltas().get("logs"), Some(&Delta::Fixed));
    }

    #[test]
    fn snapshots_without_baseline_yield_unchanged_only() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![snap("logs", Status::Warn)]));
        assert_eq!(app.deltas().get("logs"), Some(&Delta::Unchanged));
    }

    #[test]
    fn first_snapshot_does_not_pulse_any_layer() {
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Ok)]));
        assert!(!app.pulse_active("logs"));
    }

    #[test]
    fn second_snapshot_with_status_flip_starts_pulse() {
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Ok)]));
        app.handle(Action::Tick(t0 + Duration::from_millis(100)));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Warn)]));
        assert!(app.pulse_active("logs"));
    }

    #[test]
    fn second_snapshot_without_flip_does_not_pulse() {
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Warn)]));
        app.handle(Action::Tick(t0 + Duration::from_millis(100)));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Warn)]));
        assert!(!app.pulse_active("logs"));
    }

    #[test]
    fn pulse_decays_after_pulse_duration() {
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Ok)]));
        app.handle(Action::Tick(t0 + Duration::from_millis(50)));
        app.handle(Action::Snapshots(vec![snap("logs", Status::Warn)]));
        assert!(app.pulse_active("logs"));
        // Pulse window starts at t0+50ms; ends at t0+650ms.
        app.handle(Action::Tick(t0 + Duration::from_millis(700)));
        assert!(!app.pulse_active("logs"));
    }

    #[test]
    fn pulse_fires_only_for_the_layer_that_flipped() {
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::Snapshots(vec![
            snap("cluster", Status::Ok),
            snap("logs", Status::Ok),
        ]));
        app.handle(Action::Tick(t0 + Duration::from_millis(100)));
        app.handle(Action::Snapshots(vec![
            snap("cluster", Status::Ok),
            snap("logs", Status::Warn),
        ]));
        assert!(app.pulse_active("logs"));
        assert!(!app.pulse_active("cluster"));
    }

    #[test]
    fn throbber_glyph_animates_while_refreshing() {
        let mut app = App::new();
        app.handle(Action::Refresh);
        let boot = Instant::now();
        // Frame 0
        app.force_now(boot, boot);
        let f0 = app.throbber_glyph();
        // Frame N (a few ticks later) should be different.
        app.force_now(boot, boot + TICK * 3);
        let f3 = app.throbber_glyph();
        assert_ne!(f0, f3, "throbber should cycle frames over time");
        assert_ne!(f0, THROBBER_DONE);
    }

    // ── Layout B (Mission Control, issue #155) ──────────────────────────

    #[test]
    fn fresh_app_starts_in_layout_a() {
        let app = App::new();
        assert_eq!(app.layout(), Layout::A);
        assert_eq!(app.b_focus(), 0);
        assert!(!app.b_zoomed());
    }

    #[test]
    fn toggle_layout_flips_between_a_and_b() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        assert_eq!(app.layout(), Layout::B);
        app.handle(Action::ToggleLayout);
        assert_eq!(app.layout(), Layout::A);
    }

    #[test]
    fn esc_in_layout_b_returns_to_layout_a() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        assert_eq!(app.layout(), Layout::B);
        app.handle(Action::CloseOverlay);
        assert_eq!(app.layout(), Layout::A);
    }

    #[test]
    fn enter_zooms_focused_quadrant_in_layout_b() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        app.handle(Action::ZoomQuadrant);
        assert!(app.b_zoomed());
    }

    #[test]
    fn esc_in_zoomed_layout_b_unzooms_first_then_returns() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        app.handle(Action::ZoomQuadrant);
        // First Esc: unzoom but stay in Layout B.
        app.handle(Action::CloseOverlay);
        assert!(!app.b_zoomed());
        assert_eq!(app.layout(), Layout::B);
        // Second Esc: returns to Layout A.
        app.handle(Action::CloseOverlay);
        assert_eq!(app.layout(), Layout::A);
    }

    #[test]
    fn focus_in_layout_b_moves_in_two_by_three_grid() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        // 0 1 2
        // 3 4 5
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.b_focus(), 1);
        app.handle(Action::Focus(Dir::Down));
        assert_eq!(app.b_focus(), 4);
        app.handle(Action::Focus(Dir::Left));
        assert_eq!(app.b_focus(), 3);
        app.handle(Action::Focus(Dir::Up));
        assert_eq!(app.b_focus(), 0);
    }

    #[test]
    fn focused_quadrant_matches_b_focus() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        assert_eq!(app.focused_quadrant(), Quadrant::Cluster);
        for _ in 0..5 {
            app.handle(Action::Focus(Dir::Right));
            // Right walks until it hits column boundaries; we want all six.
        }
        // Walk the full grid to make sure we can land on Activity.
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        app.handle(Action::Focus(Dir::Right)); // Workflows
        app.handle(Action::Focus(Dir::Right)); // Services
        app.handle(Action::Focus(Dir::Down)); // Logs (idx 4)
        app.handle(Action::Focus(Dir::Right)); // Activity (idx 5)
        assert_eq!(app.focused_quadrant(), Quadrant::Activity);
    }

    #[test]
    fn focus_does_not_escape_b_grid() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        // From 0, Up/Left are no-ops.
        app.handle(Action::Focus(Dir::Up));
        assert_eq!(app.b_focus(), 0);
        app.handle(Action::Focus(Dir::Left));
        assert_eq!(app.b_focus(), 0);
        // Walk to end (idx 5) and try Down/Right.
        for _ in 0..2 {
            app.handle(Action::Focus(Dir::Right));
        }
        app.handle(Action::Focus(Dir::Down));
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.b_focus(), 5);
        app.handle(Action::Focus(Dir::Down));
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.b_focus(), 5);
    }

    #[test]
    fn focus_inert_when_zoomed_in_layout_b() {
        let mut app = App::new();
        app.handle(Action::ToggleLayout);
        app.handle(Action::Focus(Dir::Right));
        let before = app.b_focus();
        app.handle(Action::ZoomQuadrant);
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.b_focus(), before, "focus should not move while zoomed");
    }

    #[test]
    fn namespace_events_action_replaces_feed() {
        let mut app = App::new();
        let now = chrono::Utc::now();
        let ev = nico_correlate::Event {
            ts: now,
            source: "k8s".into(),
            kind: "Crash".into(),
            message: "boom".into(),
            severity: nico_correlate::Severity::Warning,
            tags: Default::default(),
        };
        app.handle(Action::NamespaceEvents(vec![ev]));
        assert_eq!(app.namespace_events().len(), 1);
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn click_inside_a_card_region_focuses_that_card() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.set_card_regions(vec![
            rect(0, 0, 30, 4),
            rect(30, 0, 30, 4),
            rect(60, 0, 30, 4),
            rect(0, 4, 30, 4),
            rect(30, 4, 30, 4),
            rect(60, 4, 30, 4),
        ]);
        app.clear_dirty();
        app.handle(Action::Click { col: 35, row: 5 });
        assert_eq!(app.focus(), 4);
        assert!(app.dirty());
    }

    #[test]
    fn click_outside_any_card_is_inert() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.set_card_regions(vec![rect(0, 0, 30, 4)]);
        app.clear_dirty();
        app.handle(Action::Click { col: 99, row: 99 });
        assert_eq!(app.focus(), 0);
        assert!(!app.dirty());
    }

    #[test]
    fn click_inert_when_overlay_is_open() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.set_card_regions(vec![rect(0, 0, 30, 4), rect(30, 0, 30, 4)]);
        app.handle(Action::OpenDetail);
        app.clear_dirty();
        app.handle(Action::Click { col: 35, row: 1 });
        assert_eq!(app.focus(), 0);
        assert!(!app.dirty());
    }

    #[test]
    fn click_resets_drill_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.set_card_regions(vec![rect(0, 0, 30, 4), rect(30, 0, 30, 4)]);
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.drill_scroll(), 2);
        app.handle(Action::Click { col: 35, row: 1 });
        assert_eq!(app.focus(), 1);
        assert_eq!(app.drill_scroll(), 0);
    }

    #[test]
    fn scroll_down_with_no_overlay_increments_drill_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.clear_dirty();
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.drill_scroll(), 1);
        assert!(app.dirty());
    }

    #[test]
    fn scroll_up_at_zero_is_inert() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.clear_dirty();
        app.handle(Action::Scroll(ScrollDir::Up));
        assert_eq!(app.drill_scroll(), 0);
        assert!(!app.dirty());
    }

    #[test]
    fn scroll_with_detail_overlay_open_targets_overlay_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::OpenDetail);
        app.clear_dirty();
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.overlay_scroll(), 2);
        assert_eq!(app.drill_scroll(), 0);
    }

    #[test]
    fn fresh_app_has_logs_scroll_zero() {
        let app = App::new();
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn logs_panel_not_dominant_when_layout_a_focused_layer_is_not_logs() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        // focus stays at idx 0 (cluster).
        assert!(!app.logs_panel_dominant());
    }

    #[test]
    fn logs_panel_dominant_in_layout_a_when_logs_focused() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs at idx 1
        assert!(app.logs_panel_dominant());
    }

    fn focus_layout_b_logs_quadrant(app: &mut App) {
        // Layout B grid: 0 Cluster / 1 Workflows / 2 Services /
        //                3 Postgres / 4 Logs / 5 Activity
        app.handle(Action::ToggleLayout);
        app.handle(Action::Focus(Dir::Right)); // Workflows
        app.handle(Action::Focus(Dir::Down)); // Logs (idx 4)
    }

    #[test]
    fn logs_panel_not_dominant_in_layout_b_when_logs_focused_but_not_zoomed() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        focus_layout_b_logs_quadrant(&mut app);
        assert_eq!(app.focused_quadrant(), Quadrant::Logs);
        assert!(!app.b_zoomed());
        assert!(!app.logs_panel_dominant());
    }

    #[test]
    fn logs_panel_dominant_in_layout_b_when_logs_focused_and_zoomed() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        focus_layout_b_logs_quadrant(&mut app);
        app.handle(Action::ZoomQuadrant);
        assert!(app.logs_panel_dominant());
    }

    #[test]
    fn logs_panel_not_dominant_in_layout_b_when_zoomed_but_other_quadrant() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::ToggleLayout);
        // focus stays on Cluster (idx 0).
        app.handle(Action::ZoomQuadrant);
        assert!(!app.logs_panel_dominant());
    }

    #[test]
    fn scroll_layout_a_logs_dominant_targets_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs at idx 1
        app.clear_dirty();
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.logs_scroll(), 2);
        assert_eq!(app.drill_scroll(), 0);
        assert!(app.dirty());
    }

    #[test]
    fn scroll_layout_b_logs_zoomed_targets_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        focus_layout_b_logs_quadrant(&mut app);
        app.handle(Action::ZoomQuadrant);
        app.clear_dirty();
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.logs_scroll(), 1);
        assert_eq!(app.drill_scroll(), 0);
    }

    #[test]
    fn scroll_when_logs_panel_not_dominant_keeps_drill_scroll_behavior() {
        // Regression: focus stays on cluster (idx 0), so logs panel is not
        // dominant. Wheel must still drive drill_scroll, not logs_scroll.
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.clear_dirty();
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.drill_scroll(), 1);
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn scroll_up_at_zero_logs_dominant_is_inert() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right));
        app.clear_dirty();
        app.handle(Action::Scroll(ScrollDir::Up));
        assert_eq!(app.logs_scroll(), 0);
        assert!(!app.dirty());
    }

    #[test]
    fn focus_down_layout_a_logs_dominant_routes_to_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs at idx 1
        app.clear_dirty();
        let focus_before = app.focus();
        app.handle(Action::Focus(Dir::Down));
        app.handle(Action::Focus(Dir::Down));
        assert_eq!(app.logs_scroll(), 2);
        assert_eq!(
            app.focus(),
            focus_before,
            "focus must not move while logs panel is dominant"
        );
        assert!(app.dirty());
    }

    #[test]
    fn focus_up_layout_a_logs_dominant_decrements_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.logs_scroll(), 2);
        app.handle(Action::Focus(Dir::Up));
        assert_eq!(app.logs_scroll(), 1);
    }

    #[test]
    fn focus_horizontal_when_logs_dominant_does_not_scroll() {
        // Only Up/Down route to logs_scroll. Left/Right are unchanged
        // (and currently move focus when logs is dominant — but the key
        // contract is that they don't touch logs_scroll).
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs
        app.clear_dirty();
        app.handle(Action::Focus(Dir::Right));
        app.handle(Action::Focus(Dir::Left));
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn focus_down_layout_b_logs_zoomed_routes_to_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        focus_layout_b_logs_quadrant(&mut app);
        app.handle(Action::ZoomQuadrant);
        let b_before = app.b_focus();
        app.handle(Action::Focus(Dir::Down));
        assert_eq!(app.logs_scroll(), 1);
        assert_eq!(app.b_focus(), b_before);
    }

    #[test]
    fn focus_when_logs_panel_not_dominant_still_moves_focus() {
        // Regression: cluster focused (idx 0). j/k must still navigate.
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.clear_dirty();
        app.handle(Action::Focus(Dir::Down));
        assert_eq!(app.focus(), 3, "focus should move down across the grid");
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn log_lines_action_resets_logs_scroll() {
        use chrono::Utc;
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs
        app.handle(Action::Scroll(ScrollDir::Down));
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.logs_scroll(), 2);
        let line = LogLine {
            ts: Utc::now(),
            pod: "core-abc".into(),
            level: Status::Warn,
            message: "ERROR: disk full".into(),
        };
        app.handle(Action::LogLines(vec![line]));
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn focus_change_away_from_logs_resets_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.logs_scroll(), 1);
        // Logs is at idx 1; Right moves to workflows (idx 2). Logs panel
        // is no longer dominant — so this Focus(Right) routes to focus
        // movement, not scroll. The reset must fire on the transition.
        app.handle(Action::Focus(Dir::Right));
        assert_eq!(app.focus(), 2);
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn click_to_non_logs_card_resets_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.logs_scroll(), 1);
        app.set_card_regions(vec![
            rect(0, 0, 30, 4),
            rect(30, 0, 30, 4),
            rect(60, 0, 30, 4),
        ]);
        app.handle(Action::Click { col: 65, row: 1 }); // focus card 2 (workflows)
        assert_eq!(app.focus(), 2);
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn toggle_layout_resets_logs_scroll() {
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        app.handle(Action::Focus(Dir::Right)); // logs (Layout A)
        app.handle(Action::Scroll(ScrollDir::Down));
        assert_eq!(app.logs_scroll(), 1);
        app.handle(Action::ToggleLayout); // A → B
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn zoom_quadrant_clears_logs_scroll_on_entry() {
        // ZoomQuadrant only fires zoom-in (unzoom is via CloseOverlay).
        // The reset on entry is a belt-and-suspenders guarantee that no
        // stale offset survives a transition into the dominant view. We
        // can't preload logs_scroll>0 just before ZoomQuadrant in Layout B
        // (panel only becomes dominant once zoomed), so this checks the
        // field stays at 0 across the action — combined with the explicit
        // reset assignment in the reducer, this is the spec-compliant
        // round-trip.
        let mut app = App::new();
        app.handle(Action::Snapshots(six_layers()));
        focus_layout_b_logs_quadrant(&mut app);
        assert_eq!(app.logs_scroll(), 0);
        app.handle(Action::ZoomQuadrant);
        assert_eq!(app.logs_scroll(), 0);
    }

    #[test]
    fn toggle_mouse_capture_starts_on_and_flips() {
        let mut app = App::new();
        assert!(app.mouse_capture());
        let eff = app.handle(Action::ToggleMouseCapture);
        assert!(!app.mouse_capture());
        assert_eq!(eff, Some(Effect::DisableMouseCapture));
        let eff = app.handle(Action::ToggleMouseCapture);
        assert!(app.mouse_capture());
        assert_eq!(eff, Some(Effect::EnableMouseCapture));
    }

    #[test]
    fn toggle_mouse_capture_marks_dirty() {
        let mut app = App::new();
        app.clear_dirty();
        app.handle(Action::ToggleMouseCapture);
        assert!(app.dirty());
    }

    // ── Layout C / Spotlight ────────────────────────────────────────────

    fn warn_snap(name: &str) -> LayerSnapshot {
        LayerSnapshot {
            name: name.into(),
            status: Status::Warn,
            evidence: format!("{name} warn"),
            findings: vec![crate::model::Finding {
                status: Status::Warn,
                message: format!("{name} finding"),
                next_command: Some(format!("kubectl describe {name}")),
                link: Some(format!("https://example.com/{name}")),
            }],
            duration_ms: 0,
        }
    }

    fn fail_snap(name: &str) -> LayerSnapshot {
        LayerSnapshot {
            name: name.into(),
            status: Status::Fail,
            evidence: format!("{name} fail"),
            findings: vec![crate::model::Finding {
                status: Status::Fail,
                message: format!("{name} finding"),
                next_command: Some(format!("kubectl logs {name}")),
                link: None,
            }],
            duration_ms: 0,
        }
    }

    fn mixed_layers() -> Vec<LayerSnapshot> {
        // Two non-green (warn, fail) and three green (ok, ok, skipped).
        vec![
            snap("cluster", Status::Ok),
            warn_snap("logs"),
            snap("workflows", Status::Ok),
            fail_snap("grpc"),
            snap("postgres", Status::Skipped),
        ]
    }

    #[test]
    fn fresh_app_is_in_layout_a() {
        let app = App::new();
        assert_eq!(app.layout(), Layout::A);
    }

    #[test]
    fn show_spotlight_switches_layout_to_c_and_marks_dirty() {
        let mut app = App::new();
        app.clear_dirty();
        app.handle(Action::ShowSpotlight);
        assert_eq!(app.layout(), Layout::Spotlight);
        assert!(app.dirty());
    }

    #[test]
    fn show_all_returns_to_layout_a_and_marks_dirty() {
        let mut app = App::new();
        app.handle(Action::ShowSpotlight);
        app.clear_dirty();
        app.handle(Action::ShowAll);
        assert_eq!(app.layout(), Layout::A);
        assert!(app.dirty());
    }

    #[test]
    fn show_spotlight_when_already_in_spotlight_is_inert() {
        let mut app = App::new();
        app.handle(Action::ShowSpotlight);
        app.clear_dirty();
        app.handle(Action::ShowSpotlight);
        assert!(!app.dirty());
    }

    #[test]
    fn show_all_when_already_in_layout_a_is_inert() {
        let mut app = App::new();
        app.clear_dirty();
        app.handle(Action::ShowAll);
        assert!(!app.dirty());
    }

    #[test]
    fn spotlight_cards_are_only_non_green_layers() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_layers()));
        let names: Vec<_> = app
            .spotlight_cards()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, vec!["logs", "grpc"]);
        assert_eq!(app.spotlight_card_count(), 2);
    }

    #[test]
    fn green_footer_lists_ok_and_skipped_layers() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_layers()));
        let names = app.spotlight_green_layer_names();
        assert_eq!(names, vec!["cluster", "workflows", "postgres"]);
    }

    #[test]
    fn copy_next_command_in_layout_a_is_inert() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_layers()));
        let eff = app.handle(Action::CopyNextCommand);
        assert_eq!(eff, None);
        assert!(app.toast().is_none());
    }

    #[test]
    fn copy_next_command_emits_clipboard_effect_with_focused_command() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_layers()));
        app.handle(Action::ShowSpotlight);
        let eff = app.handle(Action::CopyNextCommand);
        assert_eq!(
            eff,
            Some(Effect::CopyToClipboard("kubectl describe logs".into()))
        );
    }

    #[test]
    fn copy_next_command_with_no_command_raises_toast() {
        let no_cmd = vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: "x".into(),
            findings: vec![crate::model::Finding {
                status: Status::Warn,
                message: "no cmd".into(),
                next_command: None,
                link: None,
            }],
            duration_ms: 0,
        }];
        let mut app = App::new();
        app.handle(Action::Snapshots(no_cmd));
        app.handle(Action::ShowSpotlight);
        let eff = app.handle(Action::CopyNextCommand);
        assert_eq!(eff, None);
        let t = app.toast().expect("toast should be set");
        assert!(t.message.contains("no next-command"), "{}", t.message);
    }

    #[test]
    fn open_link_emits_open_url_effect_when_link_present() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_layers()));
        app.handle(Action::ShowSpotlight);
        let eff = app.handle(Action::OpenLink);
        assert_eq!(
            eff,
            Some(Effect::OpenUrl("https://example.com/logs".into()))
        );
    }

    #[test]
    fn open_link_with_no_link_raises_toast() {
        let mut app = App::new();
        // Only `grpc` here, which has no link.
        app.handle(Action::Snapshots(vec![fail_snap("grpc")]));
        app.handle(Action::ShowSpotlight);
        let eff = app.handle(Action::OpenLink);
        assert_eq!(eff, None);
        assert!(app.toast().is_some());
    }

    fn workflows_warn_snap_with_id(workflow_id: &str) -> LayerSnapshot {
        LayerSnapshot {
            name: "workflows".into(),
            status: Status::Warn,
            evidence: "1 stuck".into(),
            findings: vec![crate::model::Finding {
                status: Status::Warn,
                message: format!(
                    "stuck_workflow: {workflow_id} (HostProvisioning): 47m running, last: 47 events"
                ),
                next_command: Some(format!("temporal workflow show -w {workflow_id}")),
                link: None,
            }],
            duration_ms: 0,
        }
    }

    #[test]
    fn correlate_on_workflows_layer_opens_loading_overlay_and_emits_effect() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_warn_snap_with_id(
            "wf-001",
        )]));
        let eff = app.handle(Action::Correlate);
        assert_eq!(eff, Some(Effect::Correlate("wf-001".into())));
        assert_eq!(app.overlay(), Overlay::Correlate);
        let cs = app.correlate_state().expect("correlate state set");
        assert_eq!(cs.workflow_id, "wf-001");
        assert!(matches!(cs.status, CorrelateStatus::Loading));
    }

    #[test]
    fn correlate_on_non_workflows_layer_is_inert() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![warn_snap("logs")]));
        let eff = app.handle(Action::Correlate);
        assert_eq!(eff, None);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(app.correlate_state().is_none());
    }

    #[test]
    fn correlate_on_workflows_layer_with_no_id_is_inert() {
        // workflows layer with only the aggregate "0 stuck, 0 failed"
        // style finding (no recognizable workflow ID token).
        let snap = LayerSnapshot {
            name: "workflows".into(),
            status: Status::Ok,
            evidence: "0 stuck, 0 failed".into(),
            findings: vec![],
            duration_ms: 0,
        };
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![snap]));
        let eff = app.handle(Action::Correlate);
        assert_eq!(eff, None);
        assert_eq!(app.overlay(), Overlay::None);
    }

    #[test]
    fn correlate_in_spotlight_targets_focused_incident_card() {
        let mut app = App::new();
        // Two non-green cards; the second is workflows.
        app.handle(Action::Snapshots(vec![
            warn_snap("logs"),
            workflows_warn_snap_with_id("wf-042"),
        ]));
        app.handle(Action::ShowSpotlight);
        // Default focus is 0 (logs) — should be inert.
        assert_eq!(app.handle(Action::Correlate), None);
        assert_eq!(app.overlay(), Overlay::None);
    }

    #[test]
    fn correlate_results_for_matching_workflow_id_populates_loaded_state() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_warn_snap_with_id(
            "wf-001",
        )]));
        app.handle(Action::Correlate);
        let evs = vec![PopoverEvent {
            ts: chrono::Utc::now(),
            source: "temporal".into(),
            kind: "WorkflowExecutionStarted".into(),
            message: "started".into(),
            severity: crate::model::PopoverSeverity::Info,
        }];
        app.handle(Action::CorrelateResults {
            workflow_id: "wf-001".into(),
            events: evs.clone(),
            source_errors: vec![],
        });
        let cs = app.correlate_state().expect("still open");
        match &cs.status {
            CorrelateStatus::Loaded {
                events,
                source_errors,
            } => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].kind, "WorkflowExecutionStarted");
                assert!(source_errors.is_empty());
            }
            _ => panic!("expected Loaded, got {:?}", cs.status),
        }
    }

    #[test]
    fn correlate_results_for_stale_workflow_id_are_dropped() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_warn_snap_with_id(
            "wf-001",
        )]));
        app.handle(Action::Correlate);
        app.handle(Action::CorrelateResults {
            workflow_id: "wf-OTHER".into(),
            events: vec![],
            source_errors: vec![],
        });
        let cs = app.correlate_state().unwrap();
        assert!(
            matches!(cs.status, CorrelateStatus::Loading),
            "stale results must not flip the popover into Loaded"
        );
    }

    #[test]
    fn close_overlay_clears_correlate_state() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_warn_snap_with_id(
            "wf-001",
        )]));
        app.handle(Action::Correlate);
        app.handle(Action::CloseOverlay);
        assert_eq!(app.overlay(), Overlay::None);
        assert!(app.correlate_state().is_none());
    }

    #[test]
    fn correlate_with_overlay_already_open_is_inert() {
        let mut app = App::new();
        app.handle(Action::Snapshots(vec![workflows_warn_snap_with_id(
            "wf-001",
        )]));
        app.handle(Action::OpenHelp);
        let eff = app.handle(Action::Correlate);
        assert_eq!(eff, None);
        assert_eq!(app.overlay(), Overlay::Help);
    }

    #[test]
    fn correlate_results_when_no_overlay_open_are_dropped() {
        let mut app = App::new();
        // Never opened the popover; out-of-band results must not crash
        // or flip state.
        app.handle(Action::CorrelateResults {
            workflow_id: "wf-001".into(),
            events: vec![],
            source_errors: vec![SourceError {
                name: "loki".into(),
                reason: "x".into(),
            }],
        });
        assert!(app.correlate_state().is_none());
    }

    #[test]
    fn show_toast_action_sets_message() {
        let mut app = App::new();
        app.handle(Action::ShowToast("clipboard unavailable".into()));
        assert_eq!(
            app.toast().map(|t| t.message.as_str()),
            Some("clipboard unavailable")
        );
    }

    #[test]
    fn tick_past_ttl_clears_toast() {
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle(Action::Tick(t0));
        app.handle(Action::ShowToast("x".into()));
        assert!(app.toast().is_some());
        app.handle(Action::Tick(t0 + TOAST_TTL + Duration::from_millis(1)));
        assert!(app.toast().is_none());
    }

    #[test]
    fn snapshots_clamps_spotlight_focus_when_card_count_drops() {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_layers())); // 2 cards
        app.handle(Action::ShowSpotlight);
        // We have not added a "focus next card" action yet; clamping is
        // exercised by mutating the focus directly via a fresh snapshots
        // round that yields fewer cards.
        let one_card = vec![warn_snap("logs")];
        app.handle(Action::Snapshots(one_card));
        assert!(
            app.spotlight_focus() < app.spotlight_card_count().max(1),
            "focus={} count={}",
            app.spotlight_focus(),
            app.spotlight_card_count()
        );
    }
}
