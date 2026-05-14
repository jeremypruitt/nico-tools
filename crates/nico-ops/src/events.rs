use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::action::{Action, Dir, ScrollDir};
use crate::app::Layout;

/// Which overlay (if any) is currently obscuring the dashboard. The
/// translator branches on this because most navigation keys should be
/// inert while an overlay is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Detail,
    Help,
    /// Quick-correlate popover (issue #157). Holds no payload itself;
    /// the workflow ID, loading state, events, and source errors live
    /// on `App::correlate_state` so the overlay marker can stay `Copy`.
    Correlate,
    /// PRD-007 Slice 1 (#372): multi-match chooser. Operator picks one
    /// of N entities extracted from the trigger surface; `Enter` opens
    /// the correlate popup for the focused entity, `Esc` cancels.
    /// Entity list + focus index live on `App::chooser_state`.
    CorrelateChooser,
    /// PRD-007 Slice 4 (#377): full-screen correlate view. Operator hit
    /// `Enter` while the condensed correlate popup was open; the same
    /// state and in-flight stream are rendered to fill the viewport
    /// instead of inside the centered modal. `Esc` collapses back to
    /// `Correlate`; `q` and the underlying close path tear down the
    /// stream just like the condensed popup.
    CorrelateFullscreen,
    /// PRD-006 Slice 2 (#368): logs modal overlay. Opens via `l` from
    /// either Scorecard or Spotlight; dismisses via `Esc` / `l` / `q`.
    /// Renders the snapshot logs (`App::log_lines`) inside the popup
    /// primitive at breakpoint-aware percentages from the layout solver.
    Logs,
}

/// Reserved for future input modes (filter bar, etc.). Today only `Normal`
/// exists; the parameter is kept so the translator's contract doesn't have
/// to change when we add modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
}

/// Toast surfaced when the operator presses `m`. Mission Control was
/// removed in PRD-006 slice 1 (issue #367); the keybinding now points the
/// operator at the two remaining views (Scorecard ↔ Spotlight) and the
/// logs drill.
pub const MISSION_CONTROL_REMOVED_TOAST: &str =
    "Mission Control removed; press `s` for Spotlight or `l` for logs";

/// Pure mapping from a crossterm event to an `Action`. No I/O, no state.
/// Returns `None` when the event is uninteresting in the current
/// `(mode, layout, overlay)` context — the caller should ignore it.
pub fn translate(event: &Event, mode: Mode, layout: Layout, overlay: Overlay) -> Option<Action> {
    match event {
        Event::Resize(_, _) => Some(Action::Resize),
        Event::Key(key) => translate_key(key, mode, layout, overlay),
        Event::Mouse(mouse) => translate_mouse(mouse),
        _ => None,
    }
}

fn translate_mouse(mouse: &MouseEvent) -> Option<Action> {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => Some(Action::Click {
            col: mouse.column,
            row: mouse.row,
        }),
        MouseEventKind::ScrollUp => Some(Action::Scroll(ScrollDir::Up)),
        MouseEventKind::ScrollDown => Some(Action::Scroll(ScrollDir::Down)),
        _ => None,
    }
}

fn translate_key(key: &KeyEvent, _mode: Mode, layout: Layout, overlay: Overlay) -> Option<Action> {
    if matches!(key.kind, KeyEventKind::Release) {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Some(Action::Quit);
    }

    match (layout, overlay) {
        (_, Overlay::Detail | Overlay::Help) => translate_overlay(key, overlay),
        (_, Overlay::Correlate) => translate_correlate_overlay(key),
        (_, Overlay::CorrelateFullscreen) => translate_correlate_fullscreen(key),
        (_, Overlay::CorrelateChooser) => translate_correlate_chooser(key),
        (_, Overlay::Logs) => translate_logs_overlay(key),
        (Layout::Scorecard, Overlay::None) => translate_normal(key),
        (Layout::Spotlight, Overlay::None) => translate_spotlight(key),
    }
}

fn translate_normal(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(Action::Refresh),
        KeyCode::Char(' ') => Some(Action::TogglePause),
        // `m` used to toggle Mission Control (Layout B). The view was
        // removed in PRD-006 slice 1 (issue #367); the key now surfaces a
        // one-shot toast pointing operators at the two remaining views.
        // `M` (Shift+m) is reserved for terminal mouse-capture toggling.
        KeyCode::Char('m') => Some(Action::ShowToast(MISSION_CONTROL_REMOVED_TOAST.to_string())),
        KeyCode::Char('M') => Some(Action::ToggleMouseCapture),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(Action::ShowSpotlight),
        KeyCode::Char('?') => Some(Action::OpenHelp),
        // `c` from the scorecard layout targets the focused workflow
        // Finding (issue #157). Reducer turns it into a no-op when the
        // focused Layer is not `workflows`.
        KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Correlate),
        // PRD-006 Slice 2 (#368): `l` opens the logs modal. The previous
        // vim-style `l` → Focus(Right) binding is dropped in favour of
        // the logs overlay; arrow keys still cover right-nav.
        KeyCode::Char('l') | KeyCode::Char('L') => Some(Action::ShowLogs),
        KeyCode::Enter => Some(Action::OpenDetail),
        KeyCode::Left | KeyCode::Char('h') => Some(Action::Focus(Dir::Left)),
        KeyCode::Right => Some(Action::Focus(Dir::Right)),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::Focus(Dir::Up)),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::Focus(Dir::Down)),
        _ => None,
    }
}

fn translate_spotlight(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(Action::Refresh),
        KeyCode::Char(' ') => Some(Action::TogglePause),
        KeyCode::Char('M') => Some(Action::ToggleMouseCapture),
        // Mirror the scorecard binding: `m` raises the
        // Mission-Control-removed toast (issue #367).
        KeyCode::Char('m') => Some(Action::ShowToast(MISSION_CONTROL_REMOVED_TOAST.to_string())),
        KeyCode::Char('?') => Some(Action::OpenHelp),
        // s, a, Esc all return to the show-all scorecard layout.
        KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('a') | KeyCode::Char('A') => {
            Some(Action::ShowAll)
        }
        KeyCode::Esc => Some(Action::ShowAll),
        // PRD-006 Slice 5 (#371): Enter drives the drill-down trigger.
        // Today it surfaces a documented stub toast; PRD-007 Slice 4
        // replaces the stub with the correlate-popup launch.
        KeyCode::Enter => Some(Action::SpotlightDrillStub),
        // Spotlight action row: y / o / c.
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Action::CopyNextCommand),
        KeyCode::Char('o') | KeyCode::Char('O') => Some(Action::OpenLink),
        KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Correlate),
        // PRD-006 Slice 2 (#368): `l` opens the logs modal from
        // Spotlight too. The toast pointing at logs has always said
        // "press `l` for logs"; this slice finally implements it.
        KeyCode::Char('l') | KeyCode::Char('L') => Some(Action::ShowLogs),
        // Up/down still navigate cards (Spotlight is a vertical stack).
        KeyCode::Up | KeyCode::Char('k') => Some(Action::Focus(Dir::Up)),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::Focus(Dir::Down)),
        _ => None,
    }
}

fn translate_overlay(key: &KeyEvent, overlay: Overlay) -> Option<Action> {
    match key.code {
        KeyCode::Esc => Some(Action::CloseOverlay),
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('?') if matches!(overlay, Overlay::Help) => Some(Action::CloseOverlay),
        KeyCode::Enter if matches!(overlay, Overlay::Detail) => Some(Action::CloseOverlay),
        _ => None,
    }
}

/// Quick-correlate popover (issue #157). Both `Esc` and `q` dismiss; the
/// usual quit-on-`q` is locally overridden so the operator can return to
/// the dashboard without exiting the process.
///
/// PRD-007 Slice 4 (#377) adds `Enter` — expands the condensed popup to
/// the full-screen correlate view (`Overlay::CorrelateFullscreen`); the
/// in-flight stream and accumulated state are preserved across the flip.
fn translate_correlate_overlay(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::CloseOverlay),
        KeyCode::Enter => Some(Action::ToggleCorrelateFullscreen),
        _ => None,
    }
}

/// PRD-007 Slice 4 (#377): full-screen correlate view. `Esc` collapses
/// back to the condensed popup (`Overlay::Correlate`); `q` closes the
/// entire overlay, aborting the in-flight per-Source stream. Ctrl-C is
/// handled by the top-level translator branch before we reach here.
fn translate_correlate_fullscreen(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc => Some(Action::ToggleCorrelateFullscreen),
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::CloseOverlay),
        _ => None,
    }
}

/// PRD-007 Slice 1 (#372): multi-match chooser. Up/Down move focus,
/// Enter confirms the focused entity, Esc cancels. `q` also cancels
/// (consistent with the correlate popup); the underlying-view quit is
/// locally overridden.
fn translate_correlate_chooser(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::CloseOverlay),
        KeyCode::Enter => Some(Action::ChooserConfirm),
        KeyCode::Up | KeyCode::Char('k') => Some(Action::ChooserNavigate(Dir::Up)),
        KeyCode::Down | KeyCode::Char('j') => Some(Action::ChooserNavigate(Dir::Down)),
        _ => None,
    }
}

/// PRD-006 Slice 2 (#368): logs modal overlay. `Esc`, `l`, and `q` all
/// dismiss. Quit-on-`q` is locally overridden so the operator can close
/// the overlay without exiting the process; Ctrl-C still quits because
/// the higher-level translator branch handles it before reaching here.
///
/// PRD-007 Slice 3 (#376) added `c` — the log-line trigger for the
/// correlate drill. The reducer pulls the focused log line out of state,
/// runs the Slice 1 extraction primitive on it, and dispatches to the
/// popup, chooser, or a toast.
fn translate_logs_overlay(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc
        | KeyCode::Char('q')
        | KeyCode::Char('Q')
        | KeyCode::Char('l')
        | KeyCode::Char('L') => Some(Action::CloseOverlay),
        KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Correlate),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };

    fn tr(event: &Event, overlay: Overlay) -> Option<Action> {
        translate(event, Mode::Normal, Layout::Scorecard, overlay)
    }

    fn tr_spotlight(event: &Event) -> Option<Action> {
        translate(event, Mode::Normal, Layout::Spotlight, Overlay::None)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn k(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    fn release(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn q_quits_in_normal() {
        assert_eq!(
            tr(&k(KeyCode::Char('q')), Overlay::None),
            Some(Action::Quit)
        );
    }

    #[test]
    fn ctrl_c_quits_anywhere() {
        for ov in [Overlay::None, Overlay::Detail, Overlay::Help] {
            assert_eq!(
                tr(&ctrl(KeyCode::Char('c')), ov),
                Some(Action::Quit),
                "overlay={:?}",
                ov
            );
        }
    }

    #[test]
    fn r_refreshes_in_normal() {
        assert_eq!(
            tr(&k(KeyCode::Char('R')), Overlay::None),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn space_toggles_pause_in_normal() {
        assert_eq!(
            tr(&k(KeyCode::Char(' ')), Overlay::None),
            Some(Action::TogglePause)
        );
    }

    #[test]
    fn space_inert_inside_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(tr(&k(KeyCode::Char(' ')), ov), None);
        }
    }

    #[test]
    fn arrows_and_hjk_map_to_focus_dirs() {
        // `l` is intentionally absent: PRD-006 Slice 2 (#368) rebinds it
        // to the logs overlay. Vim-style left/up/down still work via
        // h/j/k; right-nav falls back to the arrow key alone.
        for (code, dir) in [
            (KeyCode::Left, Dir::Left),
            (KeyCode::Char('h'), Dir::Left),
            (KeyCode::Right, Dir::Right),
            (KeyCode::Up, Dir::Up),
            (KeyCode::Char('k'), Dir::Up),
            (KeyCode::Down, Dir::Down),
            (KeyCode::Char('j'), Dir::Down),
        ] {
            assert_eq!(
                tr(&k(code), Overlay::None),
                Some(Action::Focus(dir)),
                "code={:?}",
                code
            );
        }
    }

    #[test]
    fn l_opens_logs_overlay_from_scorecard() {
        assert_eq!(
            tr(&k(KeyCode::Char('l')), Overlay::None),
            Some(Action::ShowLogs)
        );
        assert_eq!(
            tr(&k(KeyCode::Char('L')), Overlay::None),
            Some(Action::ShowLogs)
        );
    }

    #[test]
    fn l_opens_logs_overlay_from_spotlight() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('l'))), Some(Action::ShowLogs));
    }

    fn logs_overlay(event: &Event, layout: Layout) -> Option<Action> {
        translate(event, Mode::Normal, layout, Overlay::Logs)
    }

    #[test]
    fn esc_l_q_dismiss_logs_overlay() {
        for layout in [Layout::Scorecard, Layout::Spotlight] {
            for key in [
                KeyCode::Esc,
                KeyCode::Char('l'),
                KeyCode::Char('L'),
                KeyCode::Char('q'),
                KeyCode::Char('Q'),
            ] {
                assert_eq!(
                    logs_overlay(&k(key), layout),
                    Some(Action::CloseOverlay),
                    "key={key:?} layout={layout:?}"
                );
            }
        }
    }

    #[test]
    fn ctrl_c_still_quits_inside_logs_overlay() {
        assert_eq!(
            logs_overlay(&ctrl(KeyCode::Char('c')), Layout::Scorecard),
            Some(Action::Quit)
        );
    }

    #[test]
    fn c_inside_logs_overlay_emits_correlate() {
        // PRD-007 Slice 3 (#376): `c` on a focused log line is the
        // log-line trigger for the correlate drill. The reducer is
        // responsible for extracting entities from the focused line and
        // routing to popup / chooser / toast.
        for layout in [Layout::Scorecard, Layout::Spotlight] {
            for key in [KeyCode::Char('c'), KeyCode::Char('C')] {
                assert_eq!(
                    logs_overlay(&k(key), layout),
                    Some(Action::Correlate),
                    "key={key:?} layout={layout:?}"
                );
            }
        }
    }

    #[test]
    fn enter_opens_detail_in_normal() {
        assert_eq!(
            tr(&k(KeyCode::Enter), Overlay::None),
            Some(Action::OpenDetail)
        );
    }

    #[test]
    fn question_mark_opens_help_in_normal() {
        assert_eq!(
            tr(&k(KeyCode::Char('?')), Overlay::None),
            Some(Action::OpenHelp)
        );
    }

    #[test]
    fn esc_closes_open_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(
                tr(&k(KeyCode::Esc), ov),
                Some(Action::CloseOverlay),
                "overlay={:?}",
                ov
            );
        }
    }

    #[test]
    fn esc_in_normal_is_inert() {
        assert_eq!(tr(&k(KeyCode::Esc), Overlay::None), None);
    }

    #[test]
    fn navigation_inert_inside_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(tr(&k(KeyCode::Char('h')), ov), None);
            assert_eq!(tr(&k(KeyCode::Right), ov), None);
            assert_eq!(tr(&k(KeyCode::Char('R')), ov), None);
        }
    }

    #[test]
    fn enter_inside_detail_closes_overlay() {
        assert_eq!(
            tr(&k(KeyCode::Enter), Overlay::Detail),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn question_mark_inside_help_closes_overlay() {
        assert_eq!(
            tr(&k(KeyCode::Char('?')), Overlay::Help),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn resize_event_emits_resize_action() {
        assert_eq!(
            tr(&Event::Resize(80, 24), Overlay::None),
            Some(Action::Resize)
        );
    }

    #[test]
    fn key_release_is_ignored() {
        assert_eq!(tr(&release(KeyCode::Char('q')), Overlay::None), None);
    }

    #[test]
    fn left_click_emits_click_action_with_coordinates() {
        let ev = mouse(MouseEventKind::Down(MouseButton::Left), 42, 7);
        assert_eq!(
            tr(&ev, Overlay::None),
            Some(Action::Click { col: 42, row: 7 })
        );
    }

    #[test]
    fn scroll_wheel_up_emits_scroll_up() {
        let ev = mouse(MouseEventKind::ScrollUp, 0, 0);
        assert_eq!(tr(&ev, Overlay::None), Some(Action::Scroll(ScrollDir::Up)));
    }

    #[test]
    fn scroll_wheel_down_emits_scroll_down() {
        let ev = mouse(MouseEventKind::ScrollDown, 0, 0);
        assert_eq!(
            tr(&ev, Overlay::None),
            Some(Action::Scroll(ScrollDir::Down))
        );
    }

    #[test]
    fn scroll_works_inside_overlays_too() {
        for ov in [Overlay::Detail, Overlay::Help] {
            let ev = mouse(MouseEventKind::ScrollDown, 0, 0);
            assert_eq!(
                tr(&ev, ov),
                Some(Action::Scroll(ScrollDir::Down)),
                "overlay={:?}",
                ov
            );
        }
    }

    #[test]
    fn other_mouse_events_are_inert() {
        let ev = mouse(MouseEventKind::Moved, 0, 0);
        assert_eq!(tr(&ev, Overlay::None), None);
        let ev = mouse(MouseEventKind::Down(MouseButton::Right), 0, 0);
        assert_eq!(tr(&ev, Overlay::None), None);
    }

    #[test]
    fn upper_m_toggles_mouse_capture_in_normal() {
        assert_eq!(
            tr(&k(KeyCode::Char('M')), Overlay::None),
            Some(Action::ToggleMouseCapture)
        );
    }

    #[test]
    fn lower_m_surfaces_mission_control_removed_toast() {
        // Issue #367: Mission Control was removed; the `m` key now
        // raises a one-shot toast pointing the operator at Spotlight /
        // logs instead of toggling a (now nonexistent) Layout B.
        assert_eq!(
            tr(&k(KeyCode::Char('m')), Overlay::None),
            Some(Action::ShowToast(MISSION_CONTROL_REMOVED_TOAST.to_string()))
        );
    }

    #[test]
    fn lower_m_in_spotlight_also_surfaces_mc_removed_toast() {
        assert_eq!(
            tr_spotlight(&k(KeyCode::Char('m'))),
            Some(Action::ShowToast(MISSION_CONTROL_REMOVED_TOAST.to_string()))
        );
    }

    #[test]
    fn m_inert_inside_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(tr(&k(KeyCode::Char('M')), ov), None);
            assert_eq!(tr(&k(KeyCode::Char('m')), ov), None);
        }
    }

    // ── Spotlight bindings ──────────────────────────────────────────────

    #[test]
    fn s_in_scorecard_layout_switches_to_spotlight() {
        assert_eq!(
            tr(&k(KeyCode::Char('s')), Overlay::None),
            Some(Action::ShowSpotlight)
        );
    }

    #[test]
    fn s_in_spotlight_returns_to_scorecard_layout() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('s'))), Some(Action::ShowAll));
    }

    #[test]
    fn a_in_spotlight_returns_to_scorecard_layout() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('a'))), Some(Action::ShowAll));
    }

    #[test]
    fn esc_in_spotlight_returns_to_scorecard_layout() {
        assert_eq!(tr_spotlight(&k(KeyCode::Esc)), Some(Action::ShowAll));
    }

    #[test]
    fn y_in_spotlight_emits_copy_next_command() {
        assert_eq!(
            tr_spotlight(&k(KeyCode::Char('y'))),
            Some(Action::CopyNextCommand)
        );
    }

    #[test]
    fn o_in_spotlight_emits_open_link() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('o'))), Some(Action::OpenLink));
    }

    #[test]
    fn enter_in_spotlight_emits_drill_stub() {
        // PRD-006 Slice 5 (#371): Enter in Spotlight surfaces the
        // PRD-007 stub toast; the real drill-down primitive lands in
        // PRD-007.
        assert_eq!(
            tr_spotlight(&k(KeyCode::Enter)),
            Some(Action::SpotlightDrillStub)
        );
    }

    #[test]
    fn c_in_spotlight_emits_correlate() {
        assert_eq!(
            tr_spotlight(&k(KeyCode::Char('c'))),
            Some(Action::Correlate)
        );
    }

    #[test]
    fn y_and_o_in_scorecard_layout_do_not_emit_spotlight_actions() {
        // y and o are Spotlight-only; they have no scorecard binding.
        assert_eq!(tr(&k(KeyCode::Char('y')), Overlay::None), None);
        assert_eq!(tr(&k(KeyCode::Char('o')), Overlay::None), None);
    }

    #[test]
    fn c_in_scorecard_layout_emits_correlate_too() {
        // Issue #157: `c` is bound in the scorecard layout as well so the
        // operator can pop the quick-correlate overlay from the grid
        // without first switching to Spotlight.
        assert_eq!(
            tr(&k(KeyCode::Char('c')), Overlay::None),
            Some(Action::Correlate)
        );
    }

    #[test]
    fn esc_closes_correlate_overlay() {
        assert_eq!(
            translate(
                &k(KeyCode::Esc),
                Mode::Normal,
                Layout::Scorecard,
                Overlay::Correlate,
            ),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn enter_in_correlate_overlay_toggles_fullscreen() {
        // PRD-007 Slice 4 (#377): Enter from the condensed correlate
        // popup is the fullscreen expand trigger; quit is still ctrl-c.
        assert_eq!(
            translate(
                &k(KeyCode::Enter),
                Mode::Normal,
                Layout::Scorecard,
                Overlay::Correlate,
            ),
            Some(Action::ToggleCorrelateFullscreen)
        );
    }

    fn correlate_fullscreen(event: &Event) -> Option<Action> {
        translate(
            event,
            Mode::Normal,
            Layout::Scorecard,
            Overlay::CorrelateFullscreen,
        )
    }

    #[test]
    fn esc_in_correlate_fullscreen_collapses_to_condensed_popup() {
        assert_eq!(
            correlate_fullscreen(&k(KeyCode::Esc)),
            Some(Action::ToggleCorrelateFullscreen)
        );
    }

    #[test]
    fn q_in_correlate_fullscreen_closes_overlay_entirely() {
        // Esc collapses; q is the explicit "close the drill" path so the
        // operator can exit the correlate run from fullscreen without a
        // two-step (Esc, then Esc again).
        assert_eq!(
            correlate_fullscreen(&k(KeyCode::Char('q'))),
            Some(Action::CloseOverlay)
        );
        assert_eq!(
            correlate_fullscreen(&k(KeyCode::Char('Q'))),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn ctrl_c_still_quits_inside_correlate_fullscreen() {
        assert_eq!(
            correlate_fullscreen(&ctrl(KeyCode::Char('c'))),
            Some(Action::Quit)
        );
    }

    #[test]
    fn q_closes_correlate_overlay_instead_of_quitting() {
        assert_eq!(
            translate(
                &k(KeyCode::Char('q')),
                Mode::Normal,
                Layout::Scorecard,
                Overlay::Correlate,
            ),
            Some(Action::CloseOverlay)
        );
    }

    // ── Chooser overlay (PRD-007 Slice 1, #372) ─────────────────────────

    fn chooser(event: &Event) -> Option<Action> {
        translate(
            event,
            Mode::Normal,
            Layout::Scorecard,
            Overlay::CorrelateChooser,
        )
    }

    #[test]
    fn esc_dismisses_chooser_overlay() {
        assert_eq!(chooser(&k(KeyCode::Esc)), Some(Action::CloseOverlay));
    }

    #[test]
    fn q_dismisses_chooser_overlay_instead_of_quitting() {
        assert_eq!(chooser(&k(KeyCode::Char('q'))), Some(Action::CloseOverlay));
    }

    #[test]
    fn enter_in_chooser_emits_chooser_confirm() {
        assert_eq!(chooser(&k(KeyCode::Enter)), Some(Action::ChooserConfirm));
    }

    #[test]
    fn up_and_down_in_chooser_emit_chooser_navigate() {
        assert_eq!(
            chooser(&k(KeyCode::Up)),
            Some(Action::ChooserNavigate(Dir::Up))
        );
        assert_eq!(
            chooser(&k(KeyCode::Down)),
            Some(Action::ChooserNavigate(Dir::Down))
        );
        assert_eq!(
            chooser(&k(KeyCode::Char('k'))),
            Some(Action::ChooserNavigate(Dir::Up))
        );
        assert_eq!(
            chooser(&k(KeyCode::Char('j'))),
            Some(Action::ChooserNavigate(Dir::Down))
        );
    }

    #[test]
    fn ctrl_c_still_quits_inside_chooser_overlay() {
        assert_eq!(chooser(&ctrl(KeyCode::Char('c'))), Some(Action::Quit));
    }

    #[test]
    fn ctrl_c_still_quits_inside_correlate_overlay() {
        assert_eq!(
            translate(
                &ctrl(KeyCode::Char('c')),
                Mode::Normal,
                Layout::Scorecard,
                Overlay::Correlate,
            ),
            Some(Action::Quit)
        );
    }

    #[test]
    fn ctrl_c_still_quits_inside_spotlight() {
        assert_eq!(tr_spotlight(&ctrl(KeyCode::Char('c'))), Some(Action::Quit));
    }

    #[test]
    fn r_in_spotlight_still_refreshes() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('r'))), Some(Action::Refresh));
    }

    #[test]
    fn q_in_spotlight_still_quits() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('q'))), Some(Action::Quit));
    }

    #[test]
    fn space_inert_inside_spotlight_overlay_help() {
        // Spotlight-with-help-overlay routes to the overlay table, where
        // space is not bound.
        assert_eq!(
            translate(
                &k(KeyCode::Char(' ')),
                Mode::Normal,
                Layout::Spotlight,
                Overlay::Help
            ),
            None
        );
    }

    #[test]
    fn enter_opens_detail_only_in_scorecard_layout() {
        assert_eq!(
            translate(
                &k(KeyCode::Enter),
                Mode::Normal,
                Layout::Scorecard,
                Overlay::None
            ),
            Some(Action::OpenDetail)
        );
    }

    /// PRD-006 Slice 3 (issue #369). With any popup open, no key that
    /// would normally drive the underlying view may produce an
    /// underlying-view action — the only acceptable outcomes are
    /// `CloseOverlay` (when the key is in the popup's dismiss keymap),
    /// `Quit` (Ctrl-C only), or `None`. This guards against regressions
    /// where a new view-level binding silently leaks through an active
    /// overlay.
    #[test]
    fn no_underlying_view_action_leaks_past_active_overlay() {
        use Action::*;
        let underlying_view_actions: &[Action] = &[
            Refresh,
            TogglePause,
            ShowSpotlight,
            ShowAll,
            OpenHelp,
            OpenDetail,
            Correlate,
            CopyNextCommand,
            OpenLink,
            ToggleMouseCapture,
            ShowLogs,
            ShowToast(MISSION_CONTROL_REMOVED_TOAST.to_string()),
            Focus(Dir::Up),
            Focus(Dir::Down),
            Focus(Dir::Left),
            Focus(Dir::Right),
        ];

        // The full universe of keys that any underlying view (Scorecard
        // or Spotlight) binds. If any of these returns an underlying-view
        // action while an overlay is active, the popup primitive's
        // modal-stack contract is broken.
        let view_keys: &[KeyCode] = &[
            KeyCode::Char('r'),
            KeyCode::Char('R'),
            KeyCode::Char(' '),
            KeyCode::Char('s'),
            KeyCode::Char('S'),
            KeyCode::Char('a'),
            KeyCode::Char('A'),
            KeyCode::Char('m'),
            KeyCode::Char('M'),
            KeyCode::Char('y'),
            KeyCode::Char('Y'),
            KeyCode::Char('o'),
            KeyCode::Char('O'),
            KeyCode::Char('h'),
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('l'),
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
        ];

        for layout in [Layout::Scorecard, Layout::Spotlight] {
            for overlay in [
                Overlay::Detail,
                Overlay::Help,
                Overlay::Correlate,
                Overlay::CorrelateFullscreen,
                Overlay::CorrelateChooser,
                Overlay::Logs,
            ] {
                for key in view_keys {
                    let result = translate(&k(*key), Mode::Normal, layout, overlay);
                    if let Some(action) = &result {
                        assert!(
                            !underlying_view_actions.contains(action),
                            "key {key:?} leaked underlying-view action {action:?} \
                             through overlay {overlay:?} in layout {layout:?}",
                        );
                    }
                }
            }
        }
    }
}
