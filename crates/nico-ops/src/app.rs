use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use nico_common::output::Status;
use nico_doctor::baseline::{Baseline, Delta, compute_deltas_for};

use crate::action::{Action, Dir};
use crate::events::Overlay;
use crate::model::LayerSnapshot;
use crate::pulse::PulseTimer;
use crate::ringbuffer::{LayerStat, RingBuffer, RunSnapshot};

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
                if self.move_focus(dir) {
                    self.dirty = true;
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
            Action::Quit => Some(Effect::Quit),
        }
    }

    /// Test seam: lets unit tests drive the throbber without going through
    /// the full Tick handler (which also implicates auto-refresh).
    #[cfg(test)]
    fn force_now(&mut self, boot: Instant, now: Instant) {
        self.boot = Some(boot);
        self.now = Some(now);
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
