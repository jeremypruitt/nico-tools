use super::*;
use crate::action::{Action, Dir};
use crate::model::{LayerSnapshot, PopoverEvent, PopoverSeverity, SourceError};
use chrono::TimeZone;
use nico_common::theme::DEFAULT;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn six_layers() -> Vec<LayerSnapshot> {
    vec![
        LayerSnapshot {
            name: "cluster".into(),
            status: Status::Ok,
            evidence: "3 nodes ready".into(),
            findings: vec![],
            duration_ms: 12,
        },
        LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: "12 errors".into(),
            findings: vec![Finding {
                status: Status::Warn,
                message: "12 ERROR lines in carbide-controller".into(),
                next_command: Some("kubectl logs -n nico carbide-controller".into()),
                link: None,
            }],
            duration_ms: 34,
        },
        LayerSnapshot {
            name: "workflows".into(),
            status: Status::Ok,
            evidence: "no stuck wf".into(),
            findings: vec![],
            duration_ms: 8,
        },
        LayerSnapshot {
            name: "health".into(),
            status: Status::Ok,
            evidence: "4/4 healthy".into(),
            findings: vec![],
            duration_ms: 5,
        },
        LayerSnapshot {
            name: "grpc".into(),
            status: Status::Ok,
            evidence: "reachable".into(),
            findings: vec![],
            duration_ms: 7,
        },
        LayerSnapshot {
            name: "postgres".into(),
            status: Status::Ok,
            evidence: "12ms ping".into(),
            findings: vec![],
            duration_ms: 12,
        },
    ]
}

fn render_to_string(app: &mut App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(app, &DEFAULT, f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        out.push('\n');
    }
    out
}

fn baseline_with(pairs: &[(&str, &str)]) -> nico_doctor::baseline::Baseline {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn scorecard_renders_new_badge_when_layer_regressed_vs_baseline() {
    let mut app = App::new();
    app.set_baseline(Some(baseline_with(&[("logs", "ok")])));
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Warn,
        evidence: "12 errors".into(),
        findings: vec![],
        duration_ms: 0,
    }]));
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("NEW"), "NEW badge missing:\n{s}");
}

#[test]
fn scorecard_renders_fixed_badge_when_layer_recovered_vs_baseline() {
    let mut app = App::new();
    app.set_baseline(Some(baseline_with(&[("logs", "fail")])));
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Ok,
        evidence: "all clear".into(),
        findings: vec![],
        duration_ms: 0,
    }]));
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("FIXED"), "FIXED badge missing:\n{s}");
}

#[test]
fn missing_baseline_renders_no_delta_badges() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let s = render_to_string(&mut app, 120, 24);
    assert!(
        !s.contains("NEW"),
        "NEW unexpectedly present without baseline:\n{s}"
    );
    assert!(
        !s.contains("FIXED"),
        "FIXED unexpectedly present without baseline:\n{s}"
    );
}

#[test]
fn unchanged_delta_renders_no_badge() {
    let mut app = App::new();
    app.set_baseline(Some(baseline_with(&[("logs", "warn")])));
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Warn,
        evidence: "still warn".into(),
        findings: vec![],
        duration_ms: 0,
    }]));
    let s = render_to_string(&mut app, 120, 24);
    assert!(
        !s.contains("NEW"),
        "NEW unexpectedly shown for unchanged layer:\n{s}"
    );
    assert!(
        !s.contains("FIXED"),
        "FIXED unexpectedly shown for unchanged layer:\n{s}"
    );
}

#[test]
fn pulsing_layer_pip_uses_reversed_modifier() {
    use std::time::{Duration, Instant};
    let mut app = App::new();
    let t0 = Instant::now();
    app.handle(Action::Tick(t0));
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Ok,
        evidence: String::new(),
        findings: vec![],
        duration_ms: 0,
    }]));
    app.handle(Action::Tick(t0 + Duration::from_millis(50)));
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Warn,
        evidence: String::new(),
        findings: vec![],
        duration_ms: 0,
    }]));

    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let pip = pip_glyph(&Status::Warn);
    let mut found_reversed = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == pip && cell.modifier.contains(Modifier::REVERSED) {
                found_reversed = true;
                break;
            }
        }
        if found_reversed {
            break;
        }
    }
    assert!(found_reversed, "expected REVERSED modifier on pulsing pip");
}

#[test]
fn settled_layer_pip_does_not_use_reversed_modifier() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Warn,
        evidence: String::new(),
        findings: vec![],
        duration_ms: 0,
    }]));
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let pip = pip_glyph(&Status::Warn);
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == pip {
                assert!(
                    !cell.modifier.contains(Modifier::REVERSED),
                    "non-pulsing pip must not have REVERSED set",
                );
            }
        }
    }
}

#[test]
fn new_badge_paints_in_error_palette() {
    let mut app = App::new();
    app.set_baseline(Some(baseline_with(&[("logs", "ok")])));
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Warn,
        evidence: String::new(),
        findings: vec![],
        duration_ms: 0,
    }]));
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    // Find the 'N' of "NEW" and check fg is theme.error.
    for y in 0..buf.area.height {
        for x in 0..buf.area.width.saturating_sub(2) {
            let n = buf.cell((x, y)).unwrap();
            let e = buf.cell((x + 1, y)).unwrap();
            let w = buf.cell((x + 2, y)).unwrap();
            if n.symbol() == "N" && e.symbol() == "E" && w.symbol() == "W" {
                assert_eq!(n.fg, DEFAULT.error, "NEW badge fg must use theme.error");
                return;
            }
        }
    }
    panic!("NEW badge not found in rendered output");
}

#[test]
fn pip_glyphs_are_distinct_per_status() {
    assert_ne!(pip_glyph(&Status::Ok), pip_glyph(&Status::Warn));
    assert_ne!(pip_glyph(&Status::Warn), pip_glyph(&Status::Fail));
    assert_ne!(pip_glyph(&Status::Fail), pip_glyph(&Status::Ok));
}

#[test]
fn verdict_word_renders_each_status() {
    assert_eq!(verdict_word(&Status::Ok), "OK");
    assert_eq!(verdict_word(&Status::Warn), "WARN");
    assert_eq!(verdict_word(&Status::Fail), "FAIL");
}

#[test]
fn grid_cols_reflows_with_width() {
    assert_eq!(grid_cols_for_width(40), 1);
    assert_eq!(grid_cols_for_width(70), 2);
    assert_eq!(grid_cols_for_width(120), 3);
}

#[test]
fn render_shows_title_and_all_layer_names() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let s = render_to_string(&mut app, 120, 20);
    assert!(s.contains("nico ops"), "title missing:\n{s}");
    for name in ["cluster", "logs", "workflows", "health", "grpc", "postgres"] {
        assert!(s.contains(name), "layer {name} missing:\n{s}");
    }
}

#[test]
fn render_shows_overall_verdict_word() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let s = render_to_string(&mut app, 120, 20);
    assert!(s.contains("WARN"), "verdict missing:\n{s}");
}

#[test]
fn render_marks_focused_scorecard() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right));
    let s = render_to_string(&mut app, 120, 20);
    // Focus marker is rendered as part of the focused scorecard's title.
    // We expect "▶ logs" but not "▶ cluster".
    assert!(s.contains("▶ logs"), "expected '▶ logs' in render:\n{s}");
    assert!(
        !s.contains("▶ cluster"),
        "did not expect '▶ cluster' (cluster is not focused):\n{s}"
    );
}

#[test]
fn render_drill_shows_findings_for_focused_layer() {
    // Focus `workflows` (idx 2) with a synthetic Finding; the drill
    // panel shows the standard findings list for non-logs layers.
    let mut snaps = six_layers();
    snaps[2].findings.push(Finding {
        status: Status::Warn,
        message: "1 stuck workflow".into(),
        next_command: Some("nico correlate wf-001".into()),
        link: None,
    });
    let mut app = App::new();
    app.handle(Action::Snapshots(snaps));
    app.handle(Action::Focus(Dir::Right));
    app.handle(Action::Focus(Dir::Right));
    let s = render_to_string(&mut app, 120, 24);
    assert!(
        s.contains("findings — workflows"),
        "drill title missing:\n{s}"
    );
    assert!(s.contains("stuck workflow"), "finding text missing:\n{s}");
    assert!(s.contains("next:"), "next-cmd hint missing:\n{s}");
}

fn log_lines_sample() -> Vec<crate::model::LogLine> {
    let ts = chrono::Utc.with_ymd_and_hms(2026, 5, 6, 14, 1, 9).unwrap();
    vec![
        crate::model::LogLine {
            ts,
            pod: "carbide-controller".into(),
            level: Status::Warn,
            message: "ERROR: disk full on /var/lib".into(),
        },
        crate::model::LogLine {
            ts,
            pod: "site-agent-7f3a".into(),
            level: Status::Fail,
            message: "FATAL: oom kill".into(),
        },
    ]
}

fn log_lines_sample_n(n: usize) -> Vec<crate::model::LogLine> {
    let ts = chrono::Utc.with_ymd_and_hms(2026, 5, 6, 14, 1, 9).unwrap();
    (0..n)
        .map(|i| crate::model::LogLine {
            ts,
            pod: format!("pod-{i:03}"),
            level: Status::Warn,
            message: format!("ERROR line {i}"),
        })
        .collect()
}

#[test]
fn render_drill_renders_log_panel_when_logs_focused_and_lines_present() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right)); // focus logs (idx 1)
    app.handle(Action::LogLines(log_lines_sample()));
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("logs — 1–2 of 2"), "panel title missing:\n{s}");
    assert!(s.contains("carbide-controller"), "pod name missing:\n{s}");
    assert!(s.contains("disk full"), "message missing:\n{s}");
    assert!(s.contains("FATAL"), "fail-level message missing:\n{s}");
}

#[test]
fn render_drill_logs_panel_visible_row_count_tracks_inner_height() {
    // ADR-0014: the renderer is the sole cap. With > 20 entries and a
    // tall window, all entries should render — there must be no
    // implicit 20-line cap on the data path. h=120 gives the drill
    // panel ~56 inner rows after layout split, which fits 40 lines.
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right)); // focus logs (idx 1)
    app.handle(Action::LogLines(log_lines_sample_n(40)));
    let s = render_to_string(&mut app, 120, 120);
    assert!(
        s.contains("logs — 1–40 of 40"),
        "title must reflect renderer-side sizing, got:\n{s}"
    );
    assert!(
        s.contains("pod-039"),
        "row 40 (pod-039) must be visible:\n{s}"
    );
    assert!(
        s.contains("pod-020"),
        "row 21 (pod-020) must be visible:\n{s}"
    );
}

#[test]
fn render_drill_logs_panel_title_reflects_logs_scroll_offset() {
    // Scroll 20 down on a 200-line dataset; title shifts from
    // "1–{end} of 200" to "21–{end+20} of 200".
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right));
    app.handle(Action::LogLines(log_lines_sample_n(200)));
    // 20 wheel-down events. While dominant (logs focused), each
    // increments logs_scroll.
    for _ in 0..20 {
        app.handle(Action::Scroll(ScrollDir::Down));
    }
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("logs — 21–"), "title must start at 21:\n{s}");
    assert!(s.contains("of 200"), "total must be 200:\n{s}");
    assert!(
        s.contains("pod-020"),
        "row 21 (pod-020) must be visible at top:\n{s}"
    );
}

#[test]
fn render_drill_logs_panel_clamps_offset_when_data_shrinks() {
    // Build up a scroll offset on a big dataset, then replace lines
    // with a tiny dataset directly mutating logs_scroll via the test
    // seam to bypass the LogLines reset (which would zero it).
    // Verifies the renderer's clamp prevents panic on a stale offset.
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right));
    app.handle(Action::LogLines(log_lines_sample_n(200)));
    for _ in 0..50 {
        app.handle(Action::Scroll(ScrollDir::Down));
    }
    // Force a small dataset *without* dispatching Action::LogLines so
    // the renderer sees a stale offset.
    app.set_log_lines_for_test(log_lines_sample_n(3));
    // Render must not panic and must produce a sensible title.
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("of 3"), "total must be 3 after shrink:\n{s}");
}

#[test]
fn render_drill_logs_panel_caps_at_inner_height_when_data_exceeds() {
    // When data exceeds inner.height, the renderer trims and the title
    // shows the visible range vs total.
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right));
    app.handle(Action::LogLines(log_lines_sample_n(200)));
    let s = render_to_string(&mut app, 120, 24);
    // total stays at 200; end varies with the layout's drill-panel
    // share. Assert the shape, not the exact end value.
    assert!(
        s.contains("of 200"),
        "title must carry total=200, got:\n{s}"
    );
    assert!(
        s.contains("logs — 1–"),
        "title must use 1–{{end}} of {{total}} form, got:\n{s}"
    );
}

#[test]
fn render_drill_logs_panel_shows_empty_state_when_no_lines() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right));
    // No LogLines action — log_lines is empty.
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("no errors"), "empty-state copy missing:\n{s}");
}

// Layout B (Mission Control) view tests were removed in PRD-006 slice 1
// (issue #367). The zoomed-logs-quadrant scroll honouring is now covered
// by the scorecard drill panel tests further up.

#[test]
fn truncate_message_returns_input_when_under_budget() {
    assert_eq!(truncate_message("hi", 10), "hi");
}

#[test]
fn truncate_message_appends_ellipsis_when_over_budget() {
    let out = truncate_message("abcdefghij", 5);
    assert_eq!(out.chars().count(), 5);
    assert!(out.ends_with('…'));
}

#[test]
fn truncate_message_handles_zero_budget() {
    assert_eq!(truncate_message("hello", 0), "");
}

#[test]
fn render_drill_uses_findings_panel_for_non_logs_layer() {
    // Sanity-check: focusing cluster (which has no findings) should not
    // render the logs panel header even if log_lines is populated.
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::LogLines(log_lines_sample()));
    // focus stays at idx 0 (cluster).
    let s = render_to_string(&mut app, 120, 24);
    assert!(
        !s.contains("logs — 1–"),
        "logs panel must not appear when cluster is focused:\n{s}"
    );
}

#[test]
fn render_hint_bar_lists_keybinds() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("R:refresh"), "hint missing:\n{s}");
    assert!(s.contains("?:help"), "hint missing:\n{s}");
    assert!(s.contains("q:quit"), "hint missing:\n{s}");
}

#[test]
fn render_help_overlay_shows_keybinds() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::OpenHelp);
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("keybinds"), "overlay title missing:\n{s}");
    assert!(s.contains("refresh"), "overlay body missing:\n{s}");
}

#[test]
fn render_hint_bar_shows_paused_when_paused() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::TogglePause);
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("PAUSED"), "PAUSED indicator missing:\n{s}");
}

#[test]
fn render_hint_bar_omits_paused_when_running() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let s = render_to_string(&mut app, 120, 24);
    assert!(!s.contains("PAUSED"), "PAUSED unexpectedly shown:\n{s}");
}

#[test]
fn render_header_shows_done_glyph_after_completion() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("✓"), "expected ✓ in header after refresh:\n{s}");
}

#[test]
fn render_help_overlay_lists_pause_keybind() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::OpenHelp);
    let s = render_to_string(&mut app, 120, 24);
    assert!(
        s.contains("pause"),
        "help overlay should mention pause keybind:\n{s}"
    );
}

#[test]
fn render_help_overlay_lists_logs_scroll_keybind() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::OpenHelp);
    let s = render_to_string(&mut app, 120, 24);
    assert!(
        s.contains("scroll logs"),
        "help overlay should document logs scroll:\n{s}"
    );
}

#[test]
fn render_hint_bar_does_not_carry_logs_scroll_hint() {
    // Per ADR-0014, the footer hint bar is intentionally NOT extended
    // with a logs-panel-specific hint. The help overlay is the home
    // for the new keybind.
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right)); // focus logs (dominant)
    let s = render_to_string(&mut app, 120, 24);
    // The hint bar is the last line of output; assert the dominant-
    // view scroll text doesn't leak into it.
    let hint_line = s.lines().last().unwrap_or("");
    assert!(
        !hint_line.contains("scroll logs"),
        "hint bar must not carry logs-scroll hint:\n{hint_line}"
    );
}

#[test]
fn render_detail_overlay_shows_focused_findings() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::Focus(Dir::Right));
    app.handle(Action::OpenDetail);
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("detail — logs"), "overlay title missing:\n{s}");
}

fn drive_runs(app: &mut App, finding_counts: &[usize]) {
    for &fc in finding_counts {
        let findings = (0..fc)
            .map(|i| Finding {
                status: Status::Warn,
                message: format!("warn {i}"),
                next_command: None,
                link: None,
            })
            .collect();
        let mut snaps = six_layers();
        // Inject a varying finding count on the "logs" layer.
        if let Some(logs) = snaps.iter_mut().find(|s| s.name == "logs") {
            logs.findings = findings;
            logs.status = if fc > 0 { Status::Warn } else { Status::Ok };
        }
        app.handle(Action::Snapshots(snaps));
    }
}

#[test]
fn scorecard_sparkline_appears_after_two_or_more_runs() {
    let mut app = App::new();
    drive_runs(&mut app, &[0, 4, 8]);
    let s = render_to_string(&mut app, 120, 24);
    let has_spark = s
        .chars()
        .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
    assert!(has_spark, "expected sparkline glyph in render:\n{s}");
}

#[test]
fn scorecard_sparkline_blank_after_first_run_only() {
    let mut app = App::new();
    drive_runs(&mut app, &[3]);
    let s = render_to_string(&mut app, 120, 24);
    let has_spark = s
        .chars()
        .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
    assert!(!has_spark, "no sparkline expected for <2 runs:\n{s}");
}

#[test]
fn render_does_not_panic_at_narrow_widths_with_history() {
    for (w, h) in [(40u16, 24u16), (60, 24), (90, 24)] {
        let mut app = App::new();
        drive_runs(&mut app, &[0, 4, 8, 2, 6, 1, 0, 7, 3, 5, 4, 6]);
        // No assertion beyond "renders cleanly" — this test guards against
        // panics or layout glitches on the narrow grid reflows.
        let _ = render_to_string(&mut app, w, h);
    }
}

#[test]
fn pre_populated_ring_renders_both_sparkline_and_breadcrumb() {
    let mut app = App::new();
    // Drive enough varied runs to seed both widgets.
    drive_runs(&mut app, &[0, 4, 8, 2, 6]);
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains('■'), "breadcrumb missing:\n{s}");
    let has_spark = s
        .chars()
        .any(|c| matches!(c, '▁' | '▂' | '▃' | '▄' | '▅' | '▆' | '▇' | '█'));
    assert!(has_spark, "sparkline missing:\n{s}");
}

#[test]
fn header_breadcrumb_renders_one_square_per_past_verdict() {
    let mut app = App::new();
    for st in [Status::Warn, Status::Ok, Status::Fail] {
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: st,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }]));
    }
    let s = render_to_string(&mut app, 120, 20);
    let count = s.chars().filter(|c| *c == '■').count();
    assert!(
        count >= 3,
        "expected ≥3 breadcrumb squares, found {count}:\n{s}"
    );
}

#[test]
fn header_breadcrumb_absent_before_any_run() {
    let mut app = App::new();
    let s = render_to_string(&mut app, 120, 20);
    assert!(
        !s.contains('■'),
        "breadcrumb must not paint before any run:\n{s}"
    );
}

#[test]
fn header_breadcrumb_caps_at_breadcrumb_cap() {
    let mut app = App::new();
    for _ in 0..(BREADCRUMB_CAP + 5) {
        app.handle(Action::Snapshots(vec![LayerSnapshot {
            name: "logs".into(),
            status: Status::Ok,
            evidence: String::new(),
            findings: vec![],
            duration_ms: 0,
        }]));
    }
    let s = render_to_string(&mut app, 120, 20);
    let count = s.chars().filter(|c| *c == '■').count();
    assert_eq!(
        count, BREADCRUMB_CAP,
        "breadcrumb must cap at BREADCRUMB_CAP:\n{s}"
    );
}

#[test]
fn loading_header_when_no_snapshots() {
    let mut app = App::new();
    let s = render_to_string(&mut app, 120, 20);
    assert!(s.contains("loading"), "loading hint missing:\n{s}");
}

fn pip_color_for(theme: &Theme, status: Status) -> ratatui::style::Color {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "x".into(),
        status: status.clone(),
        evidence: String::new(),
        findings: vec![],
        duration_ms: 0,
    }]));
    let backend = TestBackend::new(120, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(&mut app, theme, f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let pip = pip_glyph(&status);
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() == pip {
                return cell.fg;
            }
        }
    }
    panic!("pip not found");
}

#[test]
fn dracula_theme_paints_pips_with_dracula_palette() {
    use nico_common::theme::DRACULA;
    let color = pip_color_for(&DRACULA, Status::Warn);
    assert_eq!(color, DRACULA.warn);
}

#[test]
fn nord_theme_paints_pips_with_nord_palette() {
    use nico_common::theme::NORD;
    let color = pip_color_for(&NORD, Status::Fail);
    assert_eq!(color, NORD.error);
}

#[test]
fn gruvbox_theme_paints_pips_with_gruvbox_palette() {
    use nico_common::theme::GRUVBOX;
    let color = pip_color_for(&GRUVBOX, Status::Ok);
    assert_eq!(color, GRUVBOX.ok);
}

// Mission Control (Layout B) view tests were removed in PRD-006 slice 1
// (issue #367). They exercised the 2×3 quadrant grid renderer, the
// `tui-big-text` Mission Control header, the Activity quadrant feed, the
// per-quadrant zoom path, and the bespoke Layout-B hint bar — all of
// which were deleted alongside `Layout::B` and the `Quadrant` type.

#[test]
fn hint_bar_shows_mouse_on_by_default() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("M:mouse(on)"), "mouse hint missing:\n{s}");
}

#[test]
fn hint_bar_reflects_mouse_off_after_toggle() {
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    app.handle(Action::ToggleMouseCapture);
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("M:mouse(off)"), "mouse hint did not flip:\n{s}");
}

#[test]
fn render_publishes_card_regions_for_hit_testing() {
    // Render once to populate card_regions, then locate the "logs"
    // scorecard by scanning cells (column-counted, not byte-indexed,
    // to avoid the multi-byte pip glyphs throwing off positions) and
    // confirm a click on it focuses card #1.
    let mut app = App::new();
    app.handle(Action::Snapshots(six_layers()));
    let backend = TestBackend::new(120, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
    let buf = terminal.backend().buffer().clone();

    let needle: Vec<&str> = vec!["l", "o", "g", "s"];
    let mut hit: Option<(u16, u16)> = None;
    'outer: for y in 0..buf.area.height {
        for x in 0..buf.area.width.saturating_sub(needle.len() as u16) {
            if (0..needle.len() as u16)
                .all(|i| buf.cell((x + i, y)).unwrap().symbol() == needle[i as usize])
            {
                hit = Some((x, y));
                break 'outer;
            }
        }
    }
    let (col, row) = hit.expect("logs scorecard title not found in render");
    app.handle(Action::Click { col, row });
    assert_eq!(
        app.focus(),
        1,
        "click on the logs scorecard at ({col}, {row}) should focus card #1"
    );
}

#[test]
fn drill_scroll_offset_is_applied_to_drill_paragraph() {
    // Use `workflows` so we exercise the standard drill paragraph
    // (the `logs` drill is now backed by the snapshot logs panel,
    // issue #158).
    let mut app = App::new();
    let many = vec![LayerSnapshot {
        name: "workflows".into(),
        status: Status::Warn,
        evidence: "many".into(),
        findings: (0..10)
            .map(|i| Finding {
                status: Status::Warn,
                message: format!("finding number {i:02}"),
                next_command: None,
                link: None,
            })
            .collect(),
        duration_ms: 0,
    }];
    app.handle(Action::Snapshots(many));
    let baseline = render_to_string(&mut app, 120, 24);
    app.handle(Action::Scroll(ScrollDir::Down));
    app.handle(Action::Scroll(ScrollDir::Down));
    let scrolled = render_to_string(&mut app, 120, 24);
    assert_ne!(
        baseline, scrolled,
        "drill should redraw differently when drill_scroll changes"
    );
}

#[test]
fn overlay_scroll_offset_is_applied_to_detail_overlay() {
    let mut app = App::new();
    let many = vec![LayerSnapshot {
        name: "logs".into(),
        status: Status::Warn,
        evidence: "many".into(),
        findings: (0..30)
            .map(|i| Finding {
                status: Status::Warn,
                message: format!("overlay finding {i:02}"),
                next_command: None,
                link: None,
            })
            .collect(),
        duration_ms: 0,
    }];
    app.handle(Action::Snapshots(many));
    app.handle(Action::OpenDetail);
    let baseline = render_to_string(&mut app, 120, 24);
    app.handle(Action::Scroll(ScrollDir::Down));
    app.handle(Action::Scroll(ScrollDir::Down));
    app.handle(Action::Scroll(ScrollDir::Down));
    let scrolled = render_to_string(&mut app, 120, 24);
    assert_ne!(
        baseline, scrolled,
        "detail overlay should redraw differently when overlay_scroll changes"
    );
}

use crate::action::ScrollDir;

// ── Layout C / Spotlight ────────────────────────────────────────────

fn mixed_for_spotlight() -> Vec<LayerSnapshot> {
    // 2 non-green (warn, fail), 3 green (ok, ok, skipped).
    vec![
        LayerSnapshot {
            name: "cluster".into(),
            status: Status::Ok,
            evidence: "3 nodes ready".into(),
            findings: vec![],
            duration_ms: 0,
        },
        LayerSnapshot {
            name: "logs".into(),
            status: Status::Warn,
            evidence: "12 errors".into(),
            findings: vec![Finding {
                status: Status::Warn,
                message: "12 ERROR lines".into(),
                next_command: Some("kubectl logs -n nico foo".into()),
                link: Some("https://example.com/logs".into()),
            }],
            duration_ms: 0,
        },
        LayerSnapshot {
            name: "workflows".into(),
            status: Status::Ok,
            evidence: "no stuck wf".into(),
            findings: vec![],
            duration_ms: 0,
        },
        LayerSnapshot {
            name: "grpc".into(),
            status: Status::Fail,
            evidence: "unreachable".into(),
            findings: vec![Finding {
                status: Status::Fail,
                message: "dial tcp: i/o timeout".into(),
                next_command: Some("kubectl describe svc -n nico grpc".into()),
                link: None,
            }],
            duration_ms: 0,
        },
        LayerSnapshot {
            name: "postgres".into(),
            status: Status::Skipped,
            evidence: "skipped".into(),
            findings: vec![],
            duration_ms: 0,
        },
    ]
}

fn enter_spotlight(app: &mut App) {
    app.handle(Action::ShowSpotlight);
}

#[test]
fn spotlight_renders_big_text_headline_for_verdict() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    enter_spotlight(&mut app);
    // tui-big-text doesn't emit literal letters; instead, it paints
    // box-drawing glyphs derived from the 8x8 font. We assert that
    // the Spotlight headline area is non-empty (not just blanks) and
    // styled in the verdict colour.
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|f| render(&mut app, &DEFAULT, f)).unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut found_painted = false;
    for y in 0..SPOTLIGHT_HEADER_HEIGHT.min(buf.area.height) {
        for x in 0..buf.area.width {
            let cell = buf.cell((x, y)).unwrap();
            if cell.symbol() != " " && cell.fg == DEFAULT.error {
                found_painted = true;
                break;
            }
        }
    }
    assert!(found_painted, "expected painted FAIL headline in red");
}

#[test]
fn spotlight_renders_one_card_per_non_green_layer() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    enter_spotlight(&mut app);
    let s = render_to_string(&mut app, 120, 30);
    // The two non-green Layers must each surface as a card title.
    assert!(s.contains("logs"), "logs card missing:\n{s}");
    assert!(s.contains("grpc"), "grpc card missing:\n{s}");
    // Their evidence + next-command lines must show through.
    assert!(s.contains("12 errors"), "logs evidence missing:\n{s}");
    assert!(s.contains("unreachable"), "grpc evidence missing:\n{s}");
    assert!(
        s.contains("next: kubectl logs"),
        "logs next-cmd missing:\n{s}"
    );
    assert!(
        s.contains("[y] copy"),
        "spotlight action keybinds missing:\n{s}"
    );
    assert!(
        s.contains("[o] open"),
        "spotlight action keybinds missing:\n{s}"
    );
    assert!(
        s.contains("[c] correlate"),
        "spotlight action keybinds missing:\n{s}"
    );
}

#[test]
fn spotlight_compresses_green_layers_to_footer_line() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    enter_spotlight(&mut app);
    let s = render_to_string(&mut app, 120, 30);
    for name in ["cluster", "workflows", "postgres"] {
        assert!(
            s.contains(name),
            "green layer {name} should be in footer:\n{s}"
        );
    }
    // The green-strip pip glyph is `●`.
    assert!(s.contains("●"), "green pip missing in footer:\n{s}");
}

#[test]
fn spotlight_does_not_render_layout_a_grid() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    enter_spotlight(&mut app);
    let s = render_to_string(&mut app, 120, 30);
    // Layout A's "nico ops" header title must not appear in
    // Spotlight.
    assert!(
        !s.contains("nico ops"),
        "Layout A header leaked into Spotlight:\n{s}"
    );
    // Layout A's drill panel title must not appear either.
    assert!(
        !s.contains("findings —"),
        "Layout A drill leaked into Spotlight:\n{s}"
    );
}

#[test]
fn spotlight_with_no_incidents_renders_friendly_empty_state() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![LayerSnapshot {
        name: "cluster".into(),
        status: Status::Ok,
        evidence: "ok".into(),
        findings: vec![],
        duration_ms: 0,
    }]));
    enter_spotlight(&mut app);
    let s = render_to_string(&mut app, 120, 24);
    assert!(
        s.contains("no incidents") || s.contains("All layers are green"),
        "expected empty-state hint:\n{s}"
    );
}

#[test]
fn spotlight_toast_renders_in_hint_bar() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    enter_spotlight(&mut app);
    app.handle(Action::ShowToast("clipboard unavailable".into()));
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        s.contains("clipboard unavailable"),
        "toast missing in render:\n{s}"
    );
}

#[test]
fn layout_a_hint_bar_advertises_spotlight_keybind() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    let s = render_to_string(&mut app, 120, 24);
    assert!(s.contains("s:spotlight"), "spotlight hint missing:\n{s}");
}

#[test]
fn help_overlay_lists_spotlight_keybinds() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    app.handle(Action::OpenHelp);
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        s.contains("spotlight"),
        "help should mention spotlight:\n{s}"
    );
    assert!(
        s.contains("show all"),
        "help should mention show-all return:\n{s}"
    );
}

#[test]
fn spotlight_help_overlay_renders_on_top_of_layout_c() {
    let mut app = App::new();
    app.handle(Action::Snapshots(mixed_for_spotlight()));
    enter_spotlight(&mut app);
    app.handle(Action::OpenHelp);
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        s.contains("keybinds"),
        "help overlay missing in spotlight:\n{s}"
    );
}

#[test]
fn render_in_spotlight_does_not_panic_at_narrow_widths() {
    for (w, h) in [(40u16, 24u16), (60, 24), (90, 24), (120, 30)] {
        let mut app = App::new();
        app.handle(Action::Snapshots(mixed_for_spotlight()));
        enter_spotlight(&mut app);
        let _ = render_to_string(&mut app, w, h);
    }
}

// ── Quick-correlate popover (issue #157) ────────────────────────────

fn workflows_snap_with_id(id: &str) -> LayerSnapshot {
    LayerSnapshot {
        name: "workflows".into(),
        status: Status::Warn,
        evidence: "1 stuck".into(),
        findings: vec![Finding {
            status: Status::Warn,
            message: format!(
                "stuck_workflow: {id} (HostProvisioning): 47m running, last: 47 events"
            ),
            next_command: Some(format!("temporal workflow show -w {id}")),
            link: None,
        }],
        duration_ms: 0,
    }
}

fn open_correlate(app: &mut App) {
    app.handle(Action::Correlate);
}

#[test]
fn correlate_overlay_title_shows_workflow_id_and_throbber_while_loading() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
    open_correlate(&mut app);
    let s = render_to_string(&mut app, 120, 30);
    assert!(s.contains("correlate"), "popover title missing:\n{s}");
    assert!(s.contains("wf-001"), "workflow id missing in title:\n{s}");
    assert!(
        s.contains("collecting"),
        "throbber/collecting indicator missing:\n{s}"
    );
}

#[test]
fn correlate_overlay_body_renders_loaded_timeline_events() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
    open_correlate(&mut app);
    app.handle(Action::CorrelateResults {
        entity: crate::model::EntityRef {
            id: "wf-001".into(),
            id_type: nico_correlate::id::IdType::Workflow,
        },
        events: vec![
            PopoverEvent {
                ts: chrono::Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 5).unwrap(),
                source: "temporal".into(),
                kind: "WorkflowExecutionStarted".into(),
                message: String::new(),
                severity: PopoverSeverity::Info,
            },
            PopoverEvent {
                ts: chrono::Utc.with_ymd_and_hms(2025, 1, 2, 3, 4, 9).unwrap(),
                source: "temporal".into(),
                kind: "WorkflowExecutionFailed".into(),
                message: "deadline exceeded".into(),
                severity: PopoverSeverity::Error,
            },
        ],
        source_errors: vec![],
        diagnosis: None,
    });
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        s.contains("WorkflowExecutionStarted"),
        "first event missing:\n{s}"
    );
    assert!(
        s.contains("WorkflowExecutionFailed"),
        "second event missing:\n{s}"
    );
    assert!(
        s.contains("deadline exceeded"),
        "event message missing:\n{s}"
    );
    assert!(
        !s.contains("collecting"),
        "Loading indicator should disappear after results land:\n{s}"
    );
}

#[test]
fn correlate_overlay_renders_source_errors_inline_as_source_error_rows() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
    open_correlate(&mut app);
    app.handle(Action::CorrelateResults {
        entity: crate::model::EntityRef {
            id: "wf-001".into(),
            id_type: nico_correlate::id::IdType::Workflow,
        },
        events: vec![],
        source_errors: vec![SourceError {
            name: "loki".into(),
            reason: "LOKI_URL not set".into(),
        }],
        diagnosis: None,
    });
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        s.contains("source_error"),
        "synthetic source_error row missing:\n{s}"
    );
    assert!(s.contains("loki"), "failed source name missing:\n{s}");
    assert!(
        s.contains("LOKI_URL not set"),
        "failed source reason missing:\n{s}"
    );
}

#[test]
fn correlate_overlay_renders_empty_state_when_no_events_and_no_source_errors() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
    open_correlate(&mut app);
    app.handle(Action::CorrelateResults {
        entity: crate::model::EntityRef {
            id: "wf-001".into(),
            id_type: nico_correlate::id::IdType::Workflow,
        },
        events: vec![],
        source_errors: vec![],
        diagnosis: None,
    });
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        s.contains("no events found"),
        "empty-state hint missing:\n{s}"
    );
}

#[test]
fn correlate_overlay_does_not_render_when_overlay_is_none() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        !s.contains("correlate —"),
        "popover must not render until `c` opens it:\n{s}"
    );
}

#[test]
fn correlate_overlay_renders_in_spotlight_layout_too() {
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![workflows_snap_with_id("wf-001")]));
    app.handle(Action::ShowSpotlight);
    open_correlate(&mut app);
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        s.contains("correlate"),
        "popover should overlay Spotlight:\n{s}"
    );
    assert!(
        s.contains("wf-001"),
        "wf id missing in Spotlight overlay:\n{s}"
    );
}

// ── PRD-007 Slice 0 — DPU correlate mini-dashboard popup ───────────

fn dpu_warn_snap(message: &str) -> LayerSnapshot {
    LayerSnapshot {
        name: "ib".into(),
        status: Status::Warn,
        evidence: "1 dpu down".into(),
        findings: vec![Finding {
            status: Status::Warn,
            message: message.into(),
            next_command: None,
            link: None,
        }],
        duration_ms: 0,
    }
}

#[test]
fn correlate_popup_for_dpu_renders_diagnosis_banner_and_timeline_at_120_cols() {
    // PRD-007 Slice 0 acceptance: TestBackend snapshot of the popup at
    // medium width (120 cols) against a fixture correlate result. Asserts
    // the Diagnosis banner sits above the chronologically-sorted timeline
    // — the "killer feature" shape promised by the slice spec.
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![dpu_warn_snap(
        "dpu-r12u5 disconnected at 14:32 (link down 5m)",
    )]));
    app.handle(Action::ShowSpotlight);
    open_correlate(&mut app);
    let dpu_entity = crate::model::EntityRef {
        id: "dpu-r12u5".into(),
        id_type: nico_correlate::id::IdType::Dpu,
    };
    app.handle(Action::CorrelateResults {
        entity: dpu_entity,
        events: vec![
            PopoverEvent {
                ts: chrono::Utc.with_ymd_and_hms(2025, 1, 2, 14, 30, 0).unwrap(),
                source: "postgres".into(),
                kind: "provision_fail".into(),
                message: "BMC unreachable after 3 retries".into(),
                severity: PopoverSeverity::Warning,
            },
            PopoverEvent {
                ts: chrono::Utc.with_ymd_and_hms(2025, 1, 2, 14, 32, 0).unwrap(),
                source: "redfish".into(),
                kind: "NetworkAdapterFailed".into(),
                message: "link state down".into(),
                severity: PopoverSeverity::Error,
            },
        ],
        source_errors: vec![],
        diagnosis: Some(crate::model::PopoverDiagnosis {
            pattern: "k8s_crash_loop".into(),
            error_signature: "pod worker-xyz in CrashLoopBackOff (6 restarts)".into(),
            next_commands: vec!["kubectl describe pod worker-xyz".into()],
        }),
    });
    let s = render_to_string(&mut app, 120, 30);
    // Title pins the entity by id, not "workflow-id"-flavored suffix.
    assert!(
        s.contains("dpu-r12u5"),
        "DPU id should appear in popup title:\n{s}"
    );
    // Diagnosis banner above the timeline.
    assert!(
        s.contains("diagnosis:"),
        "Diagnosis banner label missing:\n{s}"
    );
    assert!(
        s.contains("k8s_crash_loop"),
        "Diagnosis pattern missing in banner:\n{s}"
    );
    assert!(
        s.contains("CrashLoopBackOff"),
        "Diagnosis error signature missing in banner:\n{s}"
    );
    // Timeline events render below the banner.
    assert!(
        s.contains("provision_fail"),
        "first timeline event missing:\n{s}"
    );
    assert!(
        s.contains("NetworkAdapterFailed"),
        "second timeline event missing:\n{s}"
    );
    // Banner-then-timeline ordering: diagnosis label appears earlier in
    // the rendered string than the first event kind.
    let diag_pos = s
        .find("diagnosis:")
        .expect("diagnosis label should exist in render output");
    let event_pos = s
        .find("provision_fail")
        .expect("first event kind should exist in render output");
    assert!(
        diag_pos < event_pos,
        "Diagnosis banner must render above the timeline; \
         got diag@{diag_pos} event@{event_pos}:\n{s}"
    );
}

#[test]
fn correlate_popup_omits_diagnosis_section_when_no_diagnosis_present() {
    // Slice spec: "Diagnosis banner (top, omitted if none)".
    let mut app = App::new();
    app.handle(Action::Snapshots(vec![dpu_warn_snap(
        "dpu-r12u5 disconnected at 14:32 (link down 5m)",
    )]));
    app.handle(Action::ShowSpotlight);
    open_correlate(&mut app);
    let dpu_entity = crate::model::EntityRef {
        id: "dpu-r12u5".into(),
        id_type: nico_correlate::id::IdType::Dpu,
    };
    app.handle(Action::CorrelateResults {
        entity: dpu_entity,
        events: vec![PopoverEvent {
            ts: chrono::Utc.with_ymd_and_hms(2025, 1, 2, 14, 30, 0).unwrap(),
            source: "postgres".into(),
            kind: "provision_fail".into(),
            message: "BMC unreachable".into(),
            severity: PopoverSeverity::Warning,
        }],
        source_errors: vec![],
        diagnosis: None,
    });
    let s = render_to_string(&mut app, 120, 30);
    assert!(
        !s.contains("diagnosis:"),
        "Diagnosis banner must not render when no diagnosis matched:\n{s}"
    );
    assert!(
        s.contains("provision_fail"),
        "Timeline still renders without diagnosis:\n{s}"
    );
}
