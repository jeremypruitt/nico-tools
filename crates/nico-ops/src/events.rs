use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::action::{Action, Dir};

/// Which overlay (if any) is currently obscuring the dashboard. The
/// translator branches on this because most navigation keys should be
/// inert while an overlay is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Detail,
    Help,
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
/// `(mode, overlay)` context — the caller should ignore it.
pub fn translate(event: &Event, mode: Mode, overlay: Overlay) -> Option<Action> {
    match event {
        Event::Resize(_, _) => Some(Action::Resize),
        Event::Key(key) => translate_key(key, mode, overlay),
        _ => None,
    }
}

fn translate_key(key: &KeyEvent, _mode: Mode, overlay: Overlay) -> Option<Action> {
    if matches!(key.kind, KeyEventKind::Release) {
        return None;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Some(Action::Quit);
    }

    match overlay {
        Overlay::None => translate_normal(key),
        Overlay::Detail | Overlay::Help => translate_overlay(key, overlay),
    }
}

fn translate_normal(key: &KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('r') | KeyCode::Char('R') => Some(Action::Refresh),
        KeyCode::Char(' ') => Some(Action::TogglePause),
        KeyCode::Char('?') => Some(Action::OpenHelp),
        KeyCode::Enter => Some(Action::OpenDetail),
        KeyCode::Left | KeyCode::Char('h') => Some(Action::Focus(Dir::Left)),
        KeyCode::Right | KeyCode::Char('l') => Some(Action::Focus(Dir::Right)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

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
            translate(&k(KeyCode::Char('q')), Mode::Normal, Overlay::None),
            Some(Action::Quit)
        );
    }

    #[test]
    fn ctrl_c_quits_anywhere() {
        for ov in [Overlay::None, Overlay::Detail, Overlay::Help] {
            assert_eq!(
                translate(&ctrl(KeyCode::Char('c')), Mode::Normal, ov),
                Some(Action::Quit),
                "overlay={:?}",
                ov
            );
        }
    }

    #[test]
    fn r_refreshes_in_normal() {
        assert_eq!(
            translate(&k(KeyCode::Char('R')), Mode::Normal, Overlay::None),
            Some(Action::Refresh)
        );
    }

    #[test]
    fn space_toggles_pause_in_normal() {
        assert_eq!(
            translate(&k(KeyCode::Char(' ')), Mode::Normal, Overlay::None),
            Some(Action::TogglePause)
        );
    }

    #[test]
    fn space_inert_inside_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(translate(&k(KeyCode::Char(' ')), Mode::Normal, ov), None);
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
                translate(&k(code), Mode::Normal, Overlay::None),
                Some(Action::Focus(dir)),
                "code={:?}",
                code
            );
        }
    }

    #[test]
    fn enter_opens_detail_in_normal() {
        assert_eq!(
            translate(&k(KeyCode::Enter), Mode::Normal, Overlay::None),
            Some(Action::OpenDetail)
        );
    }

    #[test]
    fn question_mark_opens_help_in_normal() {
        assert_eq!(
            translate(&k(KeyCode::Char('?')), Mode::Normal, Overlay::None),
            Some(Action::OpenHelp)
        );
    }

    #[test]
    fn esc_closes_open_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(
                translate(&k(KeyCode::Esc), Mode::Normal, ov),
                Some(Action::CloseOverlay),
                "overlay={:?}",
                ov
            );
        }
    }

    #[test]
    fn esc_in_normal_is_inert() {
        assert_eq!(
            translate(&k(KeyCode::Esc), Mode::Normal, Overlay::None),
            None
        );
    }

    #[test]
    fn navigation_inert_inside_overlay() {
        for ov in [Overlay::Detail, Overlay::Help] {
            assert_eq!(translate(&k(KeyCode::Char('h')), Mode::Normal, ov), None);
            assert_eq!(translate(&k(KeyCode::Right), Mode::Normal, ov), None);
            assert_eq!(translate(&k(KeyCode::Char('R')), Mode::Normal, ov), None);
        }
    }

    #[test]
    fn enter_inside_detail_closes_overlay() {
        assert_eq!(
            translate(&k(KeyCode::Enter), Mode::Normal, Overlay::Detail),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn question_mark_inside_help_closes_overlay() {
        assert_eq!(
            translate(&k(KeyCode::Char('?')), Mode::Normal, Overlay::Help),
            Some(Action::CloseOverlay)
        );
    }

    #[test]
    fn resize_event_emits_resize_action() {
        assert_eq!(
            translate(&Event::Resize(80, 24), Mode::Normal, Overlay::None),
            Some(Action::Resize)
        );
    }

    #[test]
    fn key_release_is_ignored() {
        assert_eq!(
            translate(&release(KeyCode::Char('q')), Mode::Normal, Overlay::None),
            None
        );
    }
}
