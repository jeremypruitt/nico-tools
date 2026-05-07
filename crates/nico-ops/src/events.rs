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
}

/// Reserved for future input modes (filter bar, etc.). Today only `Normal`
/// exists; the parameter is kept so the translator's contract doesn't have
/// to change when we add modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
}

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
        (Layout::A, Overlay::None) => translate_normal(key),
        (Layout::Spotlight, Overlay::None) => translate_spotlight(key),
    }
}

fn translate_normal(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(Action::Refresh),
        KeyCode::Char(' ') => Some(Action::TogglePause),
        KeyCode::Char('m') | KeyCode::Char('M') => Some(Action::ToggleMouseCapture),
        KeyCode::Char('s') | KeyCode::Char('S') => Some(Action::ShowSpotlight),
        KeyCode::Char('?') => Some(Action::OpenHelp),
        // `c` from Layout A targets the focused workflow Finding (issue
        // #157). Reducer turns it into a no-op when the focused Layer is
        // not `workflows`.
        KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Correlate),
        KeyCode::Enter => Some(Action::OpenDetail),
        KeyCode::Left | KeyCode::Char('h') => Some(Action::Focus(Dir::Left)),
        KeyCode::Right | KeyCode::Char('l') => Some(Action::Focus(Dir::Right)),
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
        KeyCode::Char('m') | KeyCode::Char('M') => Some(Action::ToggleMouseCapture),
        KeyCode::Char('?') => Some(Action::OpenHelp),
        // s, a, Esc all return to the show-all Layout A.
        KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('a') | KeyCode::Char('A') => {
            Some(Action::ShowAll)
        }
        KeyCode::Esc => Some(Action::ShowAll),
        // Spotlight action row: y / o / c.
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Action::CopyNextCommand),
        KeyCode::Char('o') | KeyCode::Char('O') => Some(Action::OpenLink),
        KeyCode::Char('c') | KeyCode::Char('C') => Some(Action::Correlate),
        // Up/down still navigate cards (Layout C is a vertical stack).
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
fn translate_correlate_overlay(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::CloseOverlay),
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
        translate(event, Mode::Normal, Layout::A, overlay)
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
    fn arrow_and_hjkl_map_to_focus_dirs() {
        for (code, dir) in [
            (KeyCode::Left, Dir::Left),
            (KeyCode::Char('h'), Dir::Left),
            (KeyCode::Right, Dir::Right),
            (KeyCode::Char('l'), Dir::Right),
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
    fn lower_m_toggles_mouse_capture_in_normal() {
        assert_eq!(
            tr(&k(KeyCode::Char('m')), Overlay::None),
            Some(Action::ToggleMouseCapture)
        );
    }

    #[test]
    fn m_inert_inside_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(tr(&k(KeyCode::Char('M')), ov), None);
        }
    }

    // ── Layout C / Spotlight bindings ───────────────────────────────────

    #[test]
    fn s_in_layout_a_switches_to_spotlight() {
        assert_eq!(
            tr(&k(KeyCode::Char('s')), Overlay::None),
            Some(Action::ShowSpotlight)
        );
    }

    #[test]
    fn s_in_spotlight_returns_to_layout_a() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('s'))), Some(Action::ShowAll));
    }

    #[test]
    fn a_in_spotlight_returns_to_layout_a() {
        assert_eq!(tr_spotlight(&k(KeyCode::Char('a'))), Some(Action::ShowAll));
    }

    #[test]
    fn esc_in_spotlight_returns_to_layout_a() {
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
    fn c_in_spotlight_emits_correlate() {
        assert_eq!(
            tr_spotlight(&k(KeyCode::Char('c'))),
            Some(Action::Correlate)
        );
    }

    #[test]
    fn y_and_o_in_layout_a_do_not_emit_spotlight_actions() {
        // y and o are Spotlight-only; they have no Layout A binding.
        assert_eq!(tr(&k(KeyCode::Char('y')), Overlay::None), None);
        assert_eq!(tr(&k(KeyCode::Char('o')), Overlay::None), None);
    }

    #[test]
    fn c_in_layout_a_emits_correlate_too() {
        // Issue #157: `c` is bound in Layout A as well so the operator
        // can pop the quick-correlate overlay from the scorecard grid
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
                Layout::A,
                Overlay::Correlate,
            ),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn q_closes_correlate_overlay_instead_of_quitting() {
        assert_eq!(
            translate(
                &k(KeyCode::Char('q')),
                Mode::Normal,
                Layout::A,
                Overlay::Correlate,
            ),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn ctrl_c_still_quits_inside_correlate_overlay() {
        assert_eq!(
            translate(
                &ctrl(KeyCode::Char('c')),
                Mode::Normal,
                Layout::A,
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
}
