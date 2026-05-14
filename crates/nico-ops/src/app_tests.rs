use super::*;
use crate::cli::FeatureFlags;
use crate::model::{Confidence, EntityRef, PopoverEvent, SourceStatus};
use nico_common::output::Status;
use nico_correlate::id::IdType;

fn entity_wf(id: &str) -> EntityRef {
    EntityRef {
        id: id.into(),
        id_type: IdType::Workflow,
        confidence: Confidence::Heuristic,
    }
}

fn entity_dpu(id: &str) -> EntityRef {
    EntityRef {
        id: id.into(),
        id_type: IdType::Dpu,
        confidence: Confidence::Heuristic,
    }
}

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
fn show_logs_opens_logs_overlay() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.clear_dirty();
    app.handle(Action::ShowLogs);
    assert_eq!(app.overlay(), Overlay::Logs);
    assert!(app.dirty(), "opening the overlay marks the frame dirty");
}

#[test]
fn close_overlay_dismisses_logs_overlay() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::ShowLogs);
    app.handle(Action::CloseOverlay);
    assert_eq!(app.overlay(), Overlay::None);
}

#[test]
fn show_logs_inert_when_another_overlay_is_open() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::OpenHelp);
    app.clear_dirty();
    app.handle(Action::ShowLogs);
    assert_eq!(
        app.overlay(),
        Overlay::Help,
        "ShowLogs must not steal an already-active overlay"
    );
    assert!(!app.dirty());
}

#[test]
fn show_logs_preserves_underlying_view_state() {
    // PRD-006 Slice 2 (#368) AC: "preserves underlying-view state".
    // Opening and closing the overlay leaves the scorecard layout and
    // focus untouched.
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(crate::action::Dir::Down));
    let focus_before = app.focus();
    let layout_before = app.layout();
    app.handle(Action::ShowLogs);
    app.handle(Action::CloseOverlay);
    assert_eq!(app.focus(), focus_before);
    assert_eq!(app.layout(), layout_before);
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

// ── Mission Control (Layout B) removed in PRD-006 slice 1 (issue #367).
// The block of tests that exercised the 2×3 quadrant grid, the Activity
// feed, and the per-quadrant zoom path was deleted here. The
// `Layout::Scorecard ↔ Layout::Spotlight` toggle behaviour is covered by
// the existing Spotlight tests further down, and the `m`-toast behaviour
// lives in `events::tests::lower_m_surfaces_mission_control_removed_toast`.

#[test]
fn fresh_app_starts_in_scorecard_layout() {
    let app = App::new();
    assert_eq!(app.layout(), Layout::Scorecard);
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
fn logs_panel_dominant_in_scorecard_layout_when_logs_focused() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right)); // logs at idx 1
    assert!(app.logs_panel_dominant());
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
fn fresh_app_is_in_scorecard_layout() {
    let app = App::new();
    assert_eq!(app.layout(), Layout::Scorecard);
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
fn show_all_returns_to_scorecard_layout_and_marks_dirty() {
    let mut app = App::new();
    app.handle(Action::ShowSpotlight);
    app.clear_dirty();
    app.handle(Action::ShowAll);
    assert_eq!(app.layout(), Layout::Scorecard);
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
fn show_all_when_already_in_scorecard_layout_is_inert() {
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
    // PRD-006 Slice 5 (#371): severity-major ordering puts Fail above
    // Warn, so `grpc` (Fail) precedes `logs` (Warn).
    assert_eq!(names, vec!["grpc", "logs"]);
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
fn copy_next_command_in_scorecard_layout_is_inert() {
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
    // PRD-006 Slice 5 (#371): Fail-major sort puts `grpc` (Fail) at
    // spotlight_focus=0; its next-command comes from `fail_snap`.
    assert_eq!(
        eff,
        Some(Effect::CopyToClipboard("kubectl logs grpc".into()))
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
    // PRD-006 Slice 5 (#371): default focus is `grpc` (Fail-major sort).
    // `grpc` has no link; we navigate down once to reach `logs` whose
    // fixture carries a link, then assert OpenUrl fires.
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_layers()));
    app.handle(Action::ShowSpotlight);
    app.handle(Action::Focus(Dir::Down));
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
    // PRD-007 Slice 4 (#377): the `workflows` fixture carries a
    // `next_command` (`temporal workflow show -w wf-001`); the slice 4
    // NextCommand-first extraction lands a `Parsed` entity rather than
    // the legacy Heuristic message-regex path.
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_warn_snap_with_id(
        "wf-001",
    )]));
    let eff = app.handle(Action::Correlate);
    let expected = EntityRef {
        id: "wf-001".into(),
        id_type: IdType::Workflow,
        confidence: Confidence::Parsed,
    };
    assert_eq!(eff, Some(Effect::RunCorrelate(expected.clone())));
    assert_eq!(app.overlay(), Overlay::Correlate);
    let cs = app.correlate_state().expect("correlate state set");
    assert_eq!(cs.entity, expected);
    assert!(cs.is_loading(), "popup must start in the loading state");
    assert!(cs.sources.is_empty(), "no sources reported yet");
    assert!(cs.events.is_empty());
    assert!(cs.source_errors.is_empty());
    assert!(cs.diagnosis.is_none());
}

#[test]
fn correlate_on_non_entity_layer_in_scorecard_is_inert() {
    // Scorecard preserves the issue-#157 contract: silently inert when no
    // entity in the focused finding (no toast). PRD-007 only adds the
    // toast for the Spotlight surface.
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![warn_snap("logs")]));
    let eff = app.handle(Action::Correlate);
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::None);
    assert!(app.correlate_state().is_none());
    assert!(app.toast().is_none());
}

#[test]
fn correlate_on_workflows_layer_with_no_id_is_inert() {
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
    app.handle(Action::Snapshots(vec![
        warn_snap("logs"),
        workflows_warn_snap_with_id("wf-042"),
    ]));
    app.handle(Action::ShowSpotlight);
    // Default focus is 0 (logs) — no entity in its finding ("logs finding"),
    // so PRD-007 raises the toast and stays inert (no overlay).
    assert_eq!(app.handle(Action::Correlate), None);
    assert_eq!(app.overlay(), Overlay::None);
    assert_eq!(
        app.toast().map(|t| t.message.as_str()),
        Some("no entity found in this row")
    );
}

#[test]
fn correlate_on_dpu_finding_in_spotlight_opens_popup_with_dpu_entity() {
    // PRD-007 Slice 0 tracer-bullet path: a non-workflows incident card
    // whose Finding mentions a DPU id should open the correlate popup
    // for that DPU. Before slice 0 this was inert (workflows-layer-only).
    let snap = LayerSnapshot {
        name: "ib".into(),
        status: Status::Warn,
        evidence: "1 dpu down".into(),
        findings: vec![crate::model::Finding {
            status: Status::Warn,
            message: "dpu-r12u5 disconnected at 14:32 (link down 5m)".into(),
            next_command: None,
            link: None,
        }],
        duration_ms: 0,
    };
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![snap]));
    app.handle(Action::ShowSpotlight);
    let eff = app.handle(Action::Correlate);
    assert_eq!(eff, Some(Effect::RunCorrelate(entity_dpu("dpu-r12u5"))));
    assert_eq!(app.overlay(), Overlay::Correlate);
    let cs = app.correlate_state().expect("correlate state set");
    assert_eq!(cs.entity, entity_dpu("dpu-r12u5"));
    assert!(cs.is_loading());
}

#[test]
fn open_correlate_popup_directly_opens_for_any_entity() {
    // Direct path used by the slice-0 tracer bullet's reducer test, and
    // by the per-surface triggers landing in slices 3-5.
    let mut app = App::new();
    let entity = entity_dpu("dpu-r12u5");
    let eff = app.handle(Action::OpenCorrelatePopup(entity.clone()));
    assert_eq!(eff, Some(Effect::RunCorrelate(entity.clone())));
    assert_eq!(app.overlay(), Overlay::Correlate);
    let cs = app.correlate_state().expect("correlate state set");
    assert_eq!(cs.entity, entity);
}

fn apply(app: &mut App, entity: &EntityRef, update: crate::correlate_runner::CorrelateUpdate) {
    app.handle(Action::CorrelateUpdate {
        entity: entity.clone(),
        update,
    });
}

#[test]
fn loading_update_seeds_per_source_progress_strip() {
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_wf("wf-001");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::Loading {
            sources: vec!["temporal", "postgres", "loki"],
        },
    );
    let cs = app.correlate_state().expect("popup open");
    let names: Vec<&str> = cs.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["temporal", "postgres", "loki"]);
    assert!(
        cs.sources.iter().all(|s| s.status == SourceStatus::Pending),
        "every Source must start Pending; got {:?}",
        cs.sources
    );
}

#[test]
fn source_landed_update_flips_dot_to_landed_and_appends_events() {
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_wf("wf-001");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::Loading {
            sources: vec!["temporal"],
        },
    );
    let evt = PopoverEvent {
        ts: chrono::Utc::now(),
        source: "temporal".into(),
        kind: "WorkflowExecutionStarted".into(),
        message: "started".into(),
        severity: crate::model::PopoverSeverity::Info,
    };
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::SourceLanded {
            source: "temporal",
            events: vec![evt.clone()],
        },
    );
    let cs = app.correlate_state().expect("popup open");
    assert_eq!(cs.sources[0].status, SourceStatus::Landed);
    assert_eq!(cs.events.len(), 1);
    assert_eq!(cs.events[0].kind, "WorkflowExecutionStarted");
}

#[test]
fn source_failed_update_flips_dot_to_failed_and_records_source_error() {
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_wf("wf-001");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::Loading {
            sources: vec!["loki"],
        },
    );
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::SourceFailed {
            source: "loki",
            reason: "LOKI_URL not set".into(),
        },
    );
    let cs = app.correlate_state().expect("popup open");
    assert_eq!(cs.sources[0].status, SourceStatus::Failed);
    assert_eq!(cs.source_errors.len(), 1);
    assert_eq!(cs.source_errors[0].name, "loki");
    assert_eq!(cs.source_errors[0].reason, "LOKI_URL not set");
}

#[test]
fn diagnosis_update_lands_in_popup_state_and_done_flips_run_done() {
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_wf("wf-001");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    let diag = crate::model::PopoverDiagnosis {
        pattern: "stuck_workflow".into(),
        error_signature: "47m running".into(),
        next_commands: vec!["nico doctor".into()],
    };
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::Diagnosis {
            diagnosis: Some(diag.clone()),
        },
    );
    assert_eq!(
        app.correlate_state().unwrap().diagnosis.as_ref(),
        Some(&diag)
    );
    assert!(
        app.correlate_state().unwrap().is_loading(),
        "Diagnosis alone shouldn't flip run_done"
    );
    apply(&mut app, &entity, CorrelateUpdate::Done);
    assert!(
        !app.correlate_state().unwrap().is_loading(),
        "Done must flip run_done"
    );
}

#[test]
fn updates_for_stale_entity_are_dropped() {
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_wf("wf-001");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    // Send an update tagged with a different entity — must be ignored.
    apply(&mut app, &entity_wf("wf-OTHER"), CorrelateUpdate::Done);
    let cs = app.correlate_state().unwrap();
    assert!(cs.is_loading(), "stale Done must not flip run_done");
}

#[test]
fn close_overlay_clears_correlate_state_and_emits_cancel() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_warn_snap_with_id(
        "wf-001",
    )]));
    app.handle(Action::Correlate);
    let eff = app.handle(Action::CloseOverlay);
    assert_eq!(app.overlay(), Overlay::None);
    assert!(app.correlate_state().is_none());
    assert_eq!(
        eff,
        Some(Effect::CancelCorrelate),
        "popup dismiss must signal the host loop to abort the runner stream"
    );
}

#[test]
fn close_overlay_without_correlate_does_not_emit_cancel() {
    let mut app = App::new();
    app.handle(Action::OpenHelp);
    let eff = app.handle(Action::CloseOverlay);
    assert_eq!(app.overlay(), Overlay::None);
    assert_eq!(eff, None);
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
fn correlate_updates_when_no_overlay_open_are_dropped() {
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    // Never opened the popup; out-of-band updates must not crash or
    // flip state.
    app.handle(Action::CorrelateUpdate {
        entity: entity_wf("wf-001"),
        update: CorrelateUpdate::SourceFailed {
            source: "loki",
            reason: "x".into(),
        },
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

// ── PRD-007 Slice 1 (#372): multi-match chooser ─────────────────────────

fn entity_host(id: &str) -> EntityRef {
    EntityRef {
        id: id.into(),
        id_type: IdType::Host,
        confidence: Confidence::Heuristic,
    }
}

#[test]
fn show_chooser_opens_overlay_and_seeds_state_focused_on_first() {
    let mut app = App::new();
    app.handle(Action::ShowCorrelateChooser(vec![
        entity_host("host-r12u5"),
        entity_dpu("dpu-bf3-r12u5"),
    ]));
    assert_eq!(app.overlay(), Overlay::CorrelateChooser);
    let state = app.chooser_state().expect("chooser state seeded");
    assert_eq!(state.entities.len(), 2);
    assert_eq!(state.focus, 0);
}

#[test]
fn show_chooser_with_empty_entities_is_inert() {
    let mut app = App::new();
    app.handle(Action::ShowCorrelateChooser(vec![]));
    assert_eq!(app.overlay(), Overlay::None);
    assert!(app.chooser_state().is_none());
}

#[test]
fn show_chooser_inert_when_another_overlay_is_open() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::OpenHelp);
    assert_eq!(app.overlay(), Overlay::Help);
    app.handle(Action::ShowCorrelateChooser(vec![entity_host("host-x")]));
    // Still Help — the chooser cannot overwrite an active overlay.
    assert_eq!(app.overlay(), Overlay::Help);
    assert!(app.chooser_state().is_none());
}

#[test]
fn chooser_navigate_down_advances_focus_then_clamps() {
    let mut app = App::new();
    app.handle(Action::ShowCorrelateChooser(vec![
        entity_host("host-r12u5"),
        entity_dpu("dpu-bf3-r12u5"),
    ]));
    app.handle(Action::ChooserNavigate(Dir::Down));
    assert_eq!(app.chooser_state().unwrap().focus, 1);
    // Down at the end stays put.
    app.handle(Action::ChooserNavigate(Dir::Down));
    assert_eq!(app.chooser_state().unwrap().focus, 1);
}

#[test]
fn chooser_navigate_up_decrements_focus_then_clamps() {
    let mut app = App::new();
    app.handle(Action::ShowCorrelateChooser(vec![
        entity_host("host-r12u5"),
        entity_dpu("dpu-bf3-r12u5"),
    ]));
    app.handle(Action::ChooserNavigate(Dir::Down));
    app.handle(Action::ChooserNavigate(Dir::Up));
    assert_eq!(app.chooser_state().unwrap().focus, 0);
    // Up at index 0 stays put.
    app.handle(Action::ChooserNavigate(Dir::Up));
    assert_eq!(app.chooser_state().unwrap().focus, 0);
}

#[test]
fn chooser_navigate_horizontal_directions_are_inert() {
    let mut app = App::new();
    app.handle(Action::ShowCorrelateChooser(vec![
        entity_host("host-r12u5"),
        entity_dpu("dpu-bf3-r12u5"),
    ]));
    app.handle(Action::ChooserNavigate(Dir::Left));
    app.handle(Action::ChooserNavigate(Dir::Right));
    assert_eq!(app.chooser_state().unwrap().focus, 0);
}

#[test]
fn chooser_confirm_dispatches_open_correlate_for_focused_entity() {
    let mut app = App::new();
    app.handle(Action::ShowCorrelateChooser(vec![
        entity_host("host-r12u5"),
        entity_dpu("dpu-bf3-r12u5"),
    ]));
    app.handle(Action::ChooserNavigate(Dir::Down));
    let effect = app.handle(Action::ChooserConfirm);
    // Chooser closed, correlate popup opened in its place.
    assert_eq!(app.overlay(), Overlay::Correlate);
    assert!(app.chooser_state().is_none());
    let state = app.correlate_state().expect("correlate popup opened");
    assert_eq!(state.entity.id, "dpu-bf3-r12u5");
    assert_eq!(state.entity.id_type, IdType::Dpu);
    // The host loop must see a RunCorrelate effect so it can spawn the
    // collect_all task; this is what makes the popup actually populate.
    assert_eq!(
        effect,
        Some(Effect::RunCorrelate(EntityRef {
            id: "dpu-bf3-r12u5".into(),
            id_type: IdType::Dpu,
            confidence: Confidence::Heuristic,
        }))
    );
}

#[test]
fn chooser_confirm_without_open_chooser_is_inert() {
    let mut app = App::new();
    let effect = app.handle(Action::ChooserConfirm);
    assert!(effect.is_none());
    assert_eq!(app.overlay(), Overlay::None);
}

#[test]
fn chooser_close_overlay_clears_state_no_correlate_dispatched() {
    let mut app = App::new();
    app.handle(Action::ShowCorrelateChooser(vec![
        entity_host("host-r12u5"),
        entity_dpu("dpu-bf3-r12u5"),
    ]));
    app.handle(Action::CloseOverlay);
    assert_eq!(app.overlay(), Overlay::None);
    assert!(app.chooser_state().is_none());
    // No correlate popup was opened — dismissal is a clean no-op.
    assert!(app.correlate_state().is_none());
}

// ── PRD-007 Slice 3 (#376): log-line trigger ────────────────────────────

fn log_line(message: &str) -> LogLine {
    LogLine {
        ts: chrono::Utc::now(),
        pod: "core-abc".into(),
        level: Status::Warn,
        message: message.into(),
    }
}

#[test]
fn correlate_in_logs_overlay_with_no_entity_line_raises_toast_and_keeps_overlay() {
    let mut app = App::new();
    app.handle(Action::LogLines(vec![log_line("nothing useful here")]));
    app.handle(Action::ShowLogs);
    let eff = app.handle(Action::Correlate);
    assert_eq!(eff, None);
    // Logs overlay must stay open — the operator's mental anchor is the
    // log line they were looking at, not the dashboard underneath.
    assert_eq!(app.overlay(), Overlay::Logs);
    assert_eq!(
        app.toast().map(|t| t.message.as_str()),
        Some("no entity found in this row")
    );
}

#[test]
fn correlate_in_logs_overlay_with_single_entity_opens_correlate_popup() {
    let mut app = App::new();
    app.handle(Action::LogLines(vec![log_line(
        "provisioning dpu-r12u5 failed",
    )]));
    app.handle(Action::ShowLogs);
    let eff = app.handle(Action::Correlate);
    // The logs overlay yields to the correlate popup. The host loop
    // sees a RunCorrelate effect so it can spawn the per-Source futures.
    assert_eq!(eff, Some(Effect::RunCorrelate(entity_dpu("dpu-r12u5"))));
    assert_eq!(app.overlay(), Overlay::Correlate);
    let state = app.correlate_state().expect("correlate popup opened");
    assert_eq!(state.entity, entity_dpu("dpu-r12u5"));
    assert!(state.is_loading());
}

#[test]
fn correlate_in_logs_overlay_with_multi_entity_line_opens_chooser() {
    let mut app = App::new();
    app.handle(Action::LogLines(vec![log_line(
        "host-r12u5 had dpu-bf3-r12u5 disconnect",
    )]));
    app.handle(Action::ShowLogs);
    let eff = app.handle(Action::Correlate);
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::CorrelateChooser);
    let chooser = app.chooser_state().expect("chooser opened");
    assert_eq!(chooser.entities.len(), 2);
    assert_eq!(chooser.entities[0].id, "host-r12u5");
    assert_eq!(chooser.entities[1].id, "dpu-bf3-r12u5");
    assert_eq!(chooser.focus, 0);
}

#[test]
fn correlate_in_logs_overlay_with_no_log_lines_is_inert() {
    let mut app = App::new();
    app.handle(Action::ShowLogs);
    let eff = app.handle(Action::Correlate);
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::Logs);
    assert!(app.toast().is_none());
}

#[test]
fn keypress_c_in_logs_overlay_routes_through_translator_to_popup_open() {
    // Integration: full event-pipeline test. Translator + reducer must
    // agree on the log-line trigger.
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    let mut app = App::new();
    app.handle(Action::LogLines(vec![log_line(
        "provisioning dpu-r12u5 failed",
    )]));
    app.handle(Action::ShowLogs);
    let ev = Event::Key(KeyEvent {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
    let action = crate::events::translate(
        &ev,
        crate::events::Mode::Normal,
        app.layout(),
        app.overlay(),
    )
    .expect("translator must emit an action for `c` in the logs overlay");
    let eff = app.handle(action);
    assert_eq!(eff, Some(Effect::RunCorrelate(entity_dpu("dpu-r12u5"))));
    assert_eq!(app.overlay(), Overlay::Correlate);
}

// ── PRD-007 Slice 5 (#379): event-timeline trigger (stub) ───────────────

fn tag(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}

#[test]
fn correlate_event_row_with_host_id_tag_opens_popup_with_explicit_confidence() {
    let mut app = App::new();
    app.set_features(FeatureFlags {
        events_overlay: true,
    });
    let eff = app.handle(Action::CorrelateEventRow {
        text: "ignored message".into(),
        tags: vec![tag("host_id", "host-r12u5")],
    });
    let expected = EntityRef {
        id: "host-r12u5".into(),
        id_type: IdType::Host,
        confidence: Confidence::Explicit,
    };
    assert_eq!(eff, Some(Effect::RunCorrelate(expected.clone())));
    assert_eq!(app.overlay(), Overlay::Correlate);
    let state = app.correlate_state().expect("correlate popup opened");
    assert_eq!(state.entity, expected);
}

#[test]
fn correlate_event_row_with_no_matching_tag_falls_back_to_message_regex() {
    let mut app = App::new();
    app.set_features(FeatureFlags {
        events_overlay: true,
    });
    let eff = app.handle(Action::CorrelateEventRow {
        text: "stuck workflow hp-7f3a at 14:32".into(),
        tags: vec![],
    });
    let expected = EntityRef {
        id: "hp-7f3a".into(),
        id_type: IdType::Workflow,
        confidence: Confidence::Heuristic,
    };
    assert_eq!(eff, Some(Effect::RunCorrelate(expected)));
    assert_eq!(app.overlay(), Overlay::Correlate);
}

#[test]
fn correlate_event_row_with_no_entity_raises_toast_and_no_overlay() {
    let mut app = App::new();
    app.set_features(FeatureFlags {
        events_overlay: true,
    });
    let eff = app.handle(Action::CorrelateEventRow {
        text: "nothing useful here".into(),
        tags: vec![],
    });
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::None);
    assert_eq!(
        app.toast().map(|t| t.message.as_str()),
        Some("no entity found in this row")
    );
}

#[test]
fn correlate_event_row_with_multi_entity_message_opens_chooser() {
    let mut app = App::new();
    app.set_features(FeatureFlags {
        events_overlay: true,
    });
    let eff = app.handle(Action::CorrelateEventRow {
        text: "host-r12u5 had dpu-bf3-r12u5 disconnect".into(),
        tags: vec![],
    });
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::CorrelateChooser);
    let chooser = app.chooser_state().expect("chooser opened");
    assert_eq!(chooser.entities.len(), 2);
    assert_eq!(chooser.entities[0].id, "host-r12u5");
    assert_eq!(chooser.entities[1].id, "dpu-bf3-r12u5");
}

#[test]
fn correlate_event_row_prefers_tag_even_when_message_carries_other_ids() {
    // The tag is the authoritative source — message regex must not be
    // consulted once a tag hits.
    let mut app = App::new();
    app.set_features(FeatureFlags {
        events_overlay: true,
    });
    let eff = app.handle(Action::CorrelateEventRow {
        text: "host-r12u5 had dpu-bf3-r12u5 disconnect".into(),
        tags: vec![tag("dpu_id", "dpu-r99u9")],
    });
    let expected = EntityRef {
        id: "dpu-r99u9".into(),
        id_type: IdType::Dpu,
        confidence: Confidence::Explicit,
    };
    assert_eq!(eff, Some(Effect::RunCorrelate(expected)));
    assert_eq!(app.overlay(), Overlay::Correlate);
}

#[test]
fn correlate_event_row_is_inert_when_events_overlay_feature_disabled() {
    // Stub gate: with the feature off (default), the action is silently
    // dropped — no extraction, no overlay, no toast. This is the
    // "deferred" contract from issue #379.
    let mut app = App::new();
    let eff = app.handle(Action::CorrelateEventRow {
        text: "stuck workflow hp-7f3a".into(),
        tags: vec![tag("host_id", "host-r12u5")],
    });
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::None);
    assert!(app.toast().is_none());
}

#[test]
fn feature_flags_default_has_events_overlay_off() {
    let f = FeatureFlags::default();
    assert!(!f.events_overlay);
}

#[test]
fn feature_flags_from_cli_names_parses_known_feature() {
    let f = FeatureFlags::from_cli_names(&["events-overlay".to_string()]);
    assert!(f.events_overlay);
}

#[test]
fn feature_flags_from_cli_names_ignores_unknown_feature() {
    let f = FeatureFlags::from_cli_names(&["not-a-real-feature".to_string()]);
    assert!(!f.events_overlay);
}

// ── PRD-007 Slice 4 (#377): Findings detail trigger + Enter-to-fullscreen ─

fn spotlight_card_with_next_command(next: &str) -> LayerSnapshot {
    LayerSnapshot {
        name: "hbn".into(),
        status: Status::Warn,
        evidence: "1 drift".into(),
        findings: vec![crate::model::Finding {
            status: Status::Warn,
            // Detail message intentionally carries no entity ID so we know
            // the Parsed result must have come from `next_command`, not
            // from a Heuristic fallback against the message.
            message: "config drift detected on focused dpu".into(),
            next_command: Some(next.into()),
            link: None,
        }],
        duration_ms: 0,
    }
}

#[test]
fn spotlight_c_prefers_next_command_parsed_entity_over_message() {
    // Acceptance: `c` on a Findings detail row whose `next_command` parses
    // to an entity routes to `OpenCorrelatePopup` with `Parsed` confidence.
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![spotlight_card_with_next_command(
        "nico doctor hbn dpu-r12u5",
    )]));
    app.handle(Action::ShowSpotlight);
    let eff = app.handle(Action::Correlate);
    let expected = EntityRef {
        id: "dpu-r12u5".into(),
        id_type: IdType::Dpu,
        confidence: Confidence::Parsed,
    };
    assert_eq!(eff, Some(Effect::RunCorrelate(expected.clone())));
    assert_eq!(app.overlay(), Overlay::Correlate);
    let cs = app.correlate_state().expect("correlate state set");
    assert_eq!(cs.entity, expected);
}

#[test]
fn enter_in_correlate_overlay_expands_to_fullscreen_preserving_state() {
    // Acceptance: pressing Enter while the condensed correlate popup is
    // open switches to the full-screen correlate view (overlay flips to
    // `CorrelateFullscreen`) without disturbing the in-flight stream.
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_dpu("dpu-r12u5");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::Loading {
            sources: vec!["temporal", "postgres"],
        },
    );
    assert_eq!(app.overlay(), Overlay::Correlate);
    let before = app.correlate_state().cloned();

    let eff = app.handle(Action::ToggleCorrelateFullscreen);
    assert_eq!(eff, None, "fullscreen toggle has no side effect");
    assert_eq!(app.overlay(), Overlay::CorrelateFullscreen);
    // Stream state must survive the toggle so the in-flight per-Source
    // futures keep landing into the same popup.
    let after = app.correlate_state().cloned();
    assert_eq!(after, before, "correlate state must be preserved");
}

#[test]
fn esc_in_fullscreen_collapses_to_condensed_popup_preserving_state() {
    // Acceptance: Esc from the full-screen view collapses back to the
    // condensed popup; the in-flight stream is preserved.
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_dpu("dpu-r12u5");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::Loading {
            sources: vec!["temporal"],
        },
    );
    app.handle(Action::ToggleCorrelateFullscreen);
    assert_eq!(app.overlay(), Overlay::CorrelateFullscreen);
    let before = app.correlate_state().cloned();

    let eff = app.handle(Action::ToggleCorrelateFullscreen);
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::Correlate);
    let after = app.correlate_state().cloned();
    assert_eq!(after, before);
}

#[test]
fn fullscreen_correlate_updates_still_land_in_state() {
    // Streaming updates that arrive while the operator is in fullscreen
    // must continue to land. The stale-update guard keys on the entity
    // (not on the overlay variant), so this is a sanity test that the
    // CorrelateFullscreen overlay does not accidentally inhibit the
    // existing CorrelateUpdate handler.
    use crate::correlate_runner::CorrelateUpdate;
    let mut app = App::new();
    let entity = entity_dpu("dpu-r12u5");
    app.handle(Action::OpenCorrelatePopup(entity.clone()));
    app.handle(Action::ToggleCorrelateFullscreen);
    assert_eq!(app.overlay(), Overlay::CorrelateFullscreen);
    apply(
        &mut app,
        &entity,
        CorrelateUpdate::Loading {
            sources: vec!["temporal"],
        },
    );
    let cs = app.correlate_state().expect("state still set");
    let names: Vec<&str> = cs.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["temporal"]);
}

#[test]
fn close_overlay_from_fullscreen_cancels_stream_and_clears_state() {
    // Closing from fullscreen must abort the per-Source futures and tear
    // down state — same contract as closing from the condensed popup.
    let mut app = App::new();
    app.handle(Action::OpenCorrelatePopup(entity_dpu("dpu-r12u5")));
    app.handle(Action::ToggleCorrelateFullscreen);
    let eff = app.handle(Action::CloseOverlay);
    assert_eq!(eff, Some(Effect::CancelCorrelate));
    assert_eq!(app.overlay(), Overlay::None);
    assert!(app.correlate_state().is_none());
}

#[test]
fn toggle_fullscreen_outside_correlate_overlay_is_inert() {
    // Fullscreen only applies while the correlate popup is open; firing
    // the toggle from any other state (None, Help, Detail) must be inert
    // so a stray keybind does not flip the dashboard into a half-state.
    let mut app = App::new();
    assert_eq!(app.handle(Action::ToggleCorrelateFullscreen), None);
    assert_eq!(app.overlay(), Overlay::None);

    app.handle(Action::OpenHelp);
    assert_eq!(app.handle(Action::ToggleCorrelateFullscreen), None);
    assert_eq!(app.overlay(), Overlay::Help);
}

#[test]
fn spotlight_c_with_unparseable_next_command_raises_toast() {
    // Acceptance: detail row with `next_command` set but no parseable
    // entity → "no entity found in this row" toast, no overlay.
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![spotlight_card_with_next_command(
        "nico doctor --json",
    )]));
    app.handle(Action::ShowSpotlight);
    let eff = app.handle(Action::Correlate);
    assert_eq!(eff, None);
    assert_eq!(app.overlay(), Overlay::None);
    assert_eq!(
        app.toast().map(|t| t.message.as_str()),
        Some("no entity found in this row")
    );
}
