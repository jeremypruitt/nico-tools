//! `nico ops hbn` — per-DPU HBN panel.
//!
//! At-a-glance HBN view for incident response: one row per DPU, sortable
//! by columns relevant to triage. Layout switches by terminal width:
//! - **Option A** (wide) — full table: machine, hbn version, mh-ver,
//!   inst-ver, drift, quarantine.
//! - **Option B** (narrow) — compact table: machine + composed status
//!   string (e.g. `drift (MH 4m)` / `quarantined`).
//!
//! Aggregation lives in [`nico_doctor::hbn::aggregate_row`]; this module
//! only handles layout selection, sorting, and rendering.

use nico_common::output::Status;
use nico_common::theme::Theme;
use nico_doctor::hbn::{HbnRow, HbnRowStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

/// Minimum terminal width (columns) at which the wide Option A layout is
/// selected. Below this, the panel falls back to Option B (signal-only
/// summary).
pub const OPTION_A_MIN_WIDTH: u16 = 90;

/// Which layout the panel renders for a given terminal width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelLayout {
    /// Wide all-in-one table (machine / versions / drift / quarantine).
    OptionA,
    /// Narrow signal-summary table (machine / status).
    OptionB,
}

/// Pick the layout for a given terminal width.
pub fn select_layout(width: u16) -> PanelLayout {
    if width >= OPTION_A_MIN_WIDTH {
        PanelLayout::OptionA
    } else {
        PanelLayout::OptionB
    }
}

/// Sortable columns for the HBN panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    /// Default: worst-first by row status (Quarantined > Unhealthy >
    /// Drift > Healthy), ties broken by drift age (older drift first),
    /// then machine id ascending. The triage default.
    Status,
    /// Lexicographic by machine id ascending — for "find this DPU in the
    /// list" navigation.
    MachineId,
}

/// Status precedence for sort ordering. Higher = surfaces first when
/// sorting by [`SortColumn::Status`].
fn status_rank(s: HbnRowStatus) -> u8 {
    match s {
        HbnRowStatus::Quarantined => 3,
        HbnRowStatus::Unhealthy => 2,
        HbnRowStatus::Drift => 1,
        HbnRowStatus::Healthy => 0,
    }
}

/// Sort rows in place by `column`. Stable on ties.
pub fn sort_rows(rows: &mut [HbnRow], column: SortColumn) {
    match column {
        SortColumn::Status => rows.sort_by(|a, b| {
            status_rank(b.status)
                .cmp(&status_rank(a.status))
                .then_with(|| b.drift_age.cmp(&a.drift_age))
                .then_with(|| a.machine_id.cmp(&b.machine_id))
        }),
        SortColumn::MachineId => rows.sort_by(|a, b| a.machine_id.cmp(&b.machine_id)),
    }
}

/// Compose the Option B `STATUS` string. Examples:
/// - `healthy`
/// - `drift (MH 4m)` — managed-host axis drifting, age 4m
/// - `drift (INST 12s)` — instance-network axis drifting only
/// - `drift (MH+INST 30s)` — both axes drifting
/// - `quarantined: BlockAllTraffic`
/// - `unhealthy` — container down etc.
pub fn status_string(row: &HbnRow) -> String {
    match row.status {
        HbnRowStatus::Healthy => "healthy".to_string(),
        HbnRowStatus::Quarantined => match row.quarantine_state.as_deref() {
            Some(s) => format!("quarantined: {s}"),
            None => "quarantined".to_string(),
        },
        HbnRowStatus::Unhealthy => "unhealthy".to_string(),
        HbnRowStatus::Drift => {
            let axes = match (row.managed_host_drift, row.instance_network_drift) {
                (true, true) => "MH+INST",
                (true, false) => "MH",
                (false, true) => "INST",
                (false, false) => "drift",
            };
            format!("drift ({axes} {})", humanize_age(row.drift_age))
        }
    }
}

/// Render the panel into `area`. Picks Option A or B by area width and
/// renders the appropriate table. Empty input renders a single
/// "no DPUs reported" line so the panel is never blank.
pub fn render_panel(
    rows: &[HbnRow],
    layout: PanelLayout,
    theme: &Theme,
    frame: &mut Frame,
    area: Rect,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(panel_title(rows, layout));

    if rows.is_empty() {
        let p = Paragraph::new(Line::from(Span::styled(
            "no DPUs reported (forgedb returned 0 rows)",
            Style::default().fg(theme.muted),
        )))
        .block(block);
        frame.render_widget(p, area);
        return;
    }

    let inner = block.inner(area);
    frame.render_widget(block, area);

    match layout {
        PanelLayout::OptionA => render_option_a(rows, theme, frame, inner),
        PanelLayout::OptionB => render_option_b(rows, theme, frame, inner),
    }
}

fn panel_title(rows: &[HbnRow], layout: PanelLayout) -> String {
    let layout_tag = match layout {
        PanelLayout::OptionA => "wide",
        PanelLayout::OptionB => "narrow",
    };
    format!(" HBN — {} DPUs ({}) ", rows.len(), layout_tag)
}

fn render_option_a(rows: &[HbnRow], theme: &Theme, frame: &mut Frame, area: Rect) {
    let header = Row::new(vec![
        Cell::from("MACHINE"),
        Cell::from("HBN VER"),
        Cell::from("MH-VER"),
        Cell::from("INST-VER"),
        Cell::from("DRIFT"),
        Cell::from("QUARANTINE"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = rows
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.machine_id.clone()),
                Cell::from(r.hbn_version.clone()),
                Cell::from(format_version_pair(&r.managed_host_applied, &r.managed_host_desired)),
                Cell::from(format_version_pair(
                    &r.instance_network_applied,
                    &r.instance_network_desired,
                )),
                Cell::from(drift_cell(r)),
                Cell::from(quarantine_cell(r)),
            ])
            .style(row_style(r, theme))
        })
        .collect();

    let widths = [
        Constraint::Length(20),
        Constraint::Length(16),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(20),
    ];
    let table = Table::new(body, widths).header(header).column_spacing(1);
    frame.render_widget(table, area);
}

fn render_option_b(rows: &[HbnRow], theme: &Theme, frame: &mut Frame, area: Rect) {
    let header = Row::new(vec![Cell::from("MACHINE"), Cell::from("STATUS")])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = rows
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(r.machine_id.clone()),
                Cell::from(status_string(r)),
            ])
            .style(row_style(r, theme))
        })
        .collect();

    let widths = [Constraint::Length(20), Constraint::Min(20)];
    let table = Table::new(body, widths).header(header).column_spacing(1);
    frame.render_widget(table, area);
}

fn format_version_pair(applied: &str, desired: &str) -> String {
    format!("{applied}/{desired}")
}

fn drift_cell(r: &HbnRow) -> String {
    if !r.managed_host_drift && !r.instance_network_drift {
        "—".to_string()
    } else {
        humanize_age(r.drift_age)
    }
}

fn quarantine_cell(r: &HbnRow) -> String {
    match r.quarantine_state.as_deref() {
        Some(s) => s.to_string(),
        None => "none".to_string(),
    }
}

fn row_style(r: &HbnRow, theme: &Theme) -> Style {
    let color = match status_to_severity(r.status) {
        Status::Fail => theme.error,
        Status::Warn => theme.warn,
        Status::Ok => theme.ok,
        _ => theme.muted,
    };
    Style::default().fg(color)
}

fn status_to_severity(s: HbnRowStatus) -> Status {
    match s {
        HbnRowStatus::Quarantined | HbnRowStatus::Unhealthy => Status::Fail,
        HbnRowStatus::Drift => Status::Warn,
        HbnRowStatus::Healthy => Status::Ok,
    }
}

/// Short human-readable age (`4m`, `30s`, `2h`). Mirrors the styling used
/// by the per-DPU `nico doctor hbn` output so the panel and the verdict
/// render the same units.
pub fn humanize_age(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use nico_doctor::hbn::{HbnSnapshot, aggregate_row};
    use std::time::Duration;

    fn snap(id: &str, mh_app: &str, mh_des: &str) -> HbnSnapshot {
        HbnSnapshot {
            dpu_id: id.into(),
            container_running: true,
            hbn_version: "2.0.0-doca2.5.0".into(),
            applied_managed_host_config_version: mh_app.into(),
            desired_managed_host_config_version: mh_des.into(),
            applied_instance_network_config_version: "v9".into(),
            desired_instance_network_config_version: "v9".into(),
            bgp_alerts: vec![],
            quarantine_state: None,
            last_seen_at: Utc::now(),
        }
    }

    // ── select_layout: width threshold ───────────────────────────────────

    #[test]
    fn select_layout_wide_picks_option_a() {
        assert_eq!(select_layout(120), PanelLayout::OptionA);
        assert_eq!(select_layout(OPTION_A_MIN_WIDTH), PanelLayout::OptionA);
    }

    #[test]
    fn select_layout_narrow_picks_option_b() {
        assert_eq!(select_layout(80), PanelLayout::OptionB);
        assert_eq!(select_layout(OPTION_A_MIN_WIDTH - 1), PanelLayout::OptionB);
    }

    // ── sort_rows: Status (triage default) ───────────────────────────────

    #[test]
    fn sort_rows_status_surfaces_quarantined_first_then_drift_then_healthy() {
        let now = Utc::now();
        let mut rows = vec![
            aggregate_row(&snap("dpu-c", "v17", "v17"), now),
            aggregate_row(&snap("dpu-b", "v16", "v17"), now),
            {
                let mut s = snap("dpu-a", "v17", "v17");
                s.quarantine_state = Some("BlockAllTraffic".into());
                aggregate_row(&s, now)
            },
        ];
        sort_rows(&mut rows, SortColumn::Status);
        assert_eq!(rows[0].machine_id, "dpu-a"); // Quarantined
        assert_eq!(rows[1].machine_id, "dpu-b"); // Drift
        assert_eq!(rows[2].machine_id, "dpu-c"); // Healthy
    }

    #[test]
    fn sort_rows_status_breaks_drift_ties_by_age_oldest_first() {
        let now = Utc::now();
        let mut older = snap("dpu-newer", "v16", "v17");
        older.last_seen_at = now - chrono::Duration::seconds(60);
        let mut oldest = snap("dpu-older", "v16", "v17");
        oldest.last_seen_at = now - chrono::Duration::seconds(600);
        let mut rows = vec![
            aggregate_row(&older, now),
            aggregate_row(&oldest, now),
        ];
        sort_rows(&mut rows, SortColumn::Status);
        assert_eq!(rows[0].machine_id, "dpu-older");
        assert_eq!(rows[1].machine_id, "dpu-newer");
    }

    // ── sort_rows: MachineId ─────────────────────────────────────────────

    #[test]
    fn sort_rows_machine_id_alphabetical_ascending() {
        let now = Utc::now();
        let mut rows = vec![
            aggregate_row(&snap("dpu-c", "v17", "v17"), now),
            aggregate_row(&snap("dpu-a", "v17", "v17"), now),
            aggregate_row(&snap("dpu-b", "v17", "v17"), now),
        ];
        sort_rows(&mut rows, SortColumn::MachineId);
        let ids: Vec<_> = rows.iter().map(|r| r.machine_id.clone()).collect();
        assert_eq!(ids, vec!["dpu-a", "dpu-b", "dpu-c"]);
    }

    // ── status_string: Option B composition ──────────────────────────────

    #[test]
    fn status_string_healthy_plain_word() {
        let row = aggregate_row(&snap("dpu-1", "v17", "v17"), Utc::now());
        assert_eq!(status_string(&row), "healthy");
    }

    #[test]
    fn status_string_drift_managed_host_only() {
        let now = Utc::now();
        let mut s = snap("dpu-1", "v16", "v17");
        s.last_seen_at = now - chrono::Duration::seconds(240);
        let row = aggregate_row(&s, now);
        let out = status_string(&row);
        assert!(out.starts_with("drift (MH "), "got {out}");
        assert!(out.contains("4m"), "got {out}");
    }

    #[test]
    fn status_string_drift_both_axes() {
        let now = Utc::now();
        let mut s = snap("dpu-1", "v16", "v17");
        s.applied_instance_network_config_version = "v8".into();
        s.last_seen_at = now - chrono::Duration::seconds(30);
        let row = aggregate_row(&s, now);
        let out = status_string(&row);
        assert!(out.starts_with("drift (MH+INST "), "got {out}");
    }

    #[test]
    fn status_string_quarantined_includes_state_word() {
        let mut s = snap("dpu-1", "v17", "v17");
        s.quarantine_state = Some("BlockAllTraffic".into());
        let row = aggregate_row(&s, Utc::now());
        assert_eq!(status_string(&row), "quarantined: BlockAllTraffic");
    }

    #[test]
    fn status_string_unhealthy_word() {
        let mut s = snap("dpu-1", "v17", "v17");
        s.container_running = false;
        let row = aggregate_row(&s, Utc::now());
        assert_eq!(status_string(&row), "unhealthy");
    }

    // ── humanize_age: unit boundaries ────────────────────────────────────

    #[test]
    fn humanize_age_under_minute_uses_seconds() {
        assert_eq!(humanize_age(Duration::from_secs(30)), "30s");
        assert_eq!(humanize_age(Duration::from_secs(59)), "59s");
    }

    #[test]
    fn humanize_age_under_hour_uses_minutes() {
        assert_eq!(humanize_age(Duration::from_secs(60)), "1m");
        assert_eq!(humanize_age(Duration::from_secs(3599)), "59m");
    }

    #[test]
    fn humanize_age_at_hour_uses_hours() {
        assert_eq!(humanize_age(Duration::from_secs(3600)), "1h");
        assert_eq!(humanize_age(Duration::from_secs(7200)), "2h");
    }

    // ── render_panel: end-to-end via TestBackend ─────────────────────────

    fn render_to_string(rows: &[HbnRow], layout: PanelLayout, w: u16, h: u16) -> String {
        use nico_common::theme;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = theme::DEFAULT;
        terminal
            .draw(|f| render_panel(rows, layout, &theme, f, Rect::new(0, 0, w, h)))
            .unwrap();
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

    #[test]
    fn render_panel_option_a_includes_machine_and_drift_columns() {
        let now = Utc::now();
        let mut s = snap("dpu-r3-c12", "v17", "v17");
        s.last_seen_at = now;
        let row_ok = aggregate_row(&s, now);
        let mut s2 = snap("dpu-r3-c13", "v16", "v17");
        s2.last_seen_at = now - chrono::Duration::seconds(240);
        let row_drift = aggregate_row(&s2, now);
        let out = render_to_string(&[row_ok, row_drift], PanelLayout::OptionA, 100, 8);
        assert!(out.contains("MACHINE"), "header missing:\n{out}");
        assert!(out.contains("HBN VER"), "header missing:\n{out}");
        assert!(out.contains("DRIFT"), "header missing:\n{out}");
        assert!(out.contains("dpu-r3-c12"), "row missing:\n{out}");
        assert!(out.contains("dpu-r3-c13"), "row missing:\n{out}");
        assert!(out.contains("4m"), "drift age missing:\n{out}");
    }

    #[test]
    fn render_panel_option_b_uses_status_column() {
        let now = Utc::now();
        let mut s = snap("dpu-1", "v17", "v17");
        s.quarantine_state = Some("BlockAllTraffic".into());
        let row = aggregate_row(&s, now);
        let out = render_to_string(&[row], PanelLayout::OptionB, 60, 6);
        assert!(out.contains("STATUS"), "header missing:\n{out}");
        assert!(out.contains("dpu-1"), "row missing:\n{out}");
        assert!(out.contains("quarantined"), "status missing:\n{out}");
    }

    #[test]
    fn render_panel_empty_rows_shows_no_dpus_message() {
        let out = render_to_string(&[], PanelLayout::OptionA, 80, 6);
        assert!(out.contains("no DPUs"), "empty message missing:\n{out}");
    }
}
