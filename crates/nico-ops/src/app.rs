use chrono::{DateTime, Local};

use crate::action::{Action, Dir};
use crate::events::Overlay;
use crate::model::LayerSnapshot;

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
}

impl App {
    pub fn new() -> Self {
        Self {
            snapshots: Vec::new(),
            focus: 0,
            overlay: Overlay::None,
            refreshing: false,
            last_refreshed: None,
            dirty: true,
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
                self.snapshots = snaps;
                self.refreshing = false;
                self.last_refreshed = Some(Local::now());
                self.dirty = true;
                None
            }
            Action::Quit => Some(Effect::Quit),
        }
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
        drive(&mut app, &[Action::Focus(Dir::Right), Action::Focus(Dir::Right)]);
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
        drive(&mut app, &[Action::Focus(Dir::Down), Action::Focus(Dir::Up)]);
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
        drive(&mut app, &[Action::Focus(Dir::Right), Action::Focus(Dir::Right)]);
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
}
