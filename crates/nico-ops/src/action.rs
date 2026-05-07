use crate::model::LayerSnapshot;

/// Direction for focus navigation across the scorecard grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// All state mutations flow through `App::handle(Action)`. There is no
/// other mutator. (See ADR-010, ADR-012.)
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `R` — kick off a refresh round.
    Refresh,
    /// Move the focused scorecard.
    Focus(Dir),
    /// `Enter` — open the detail overlay for the focused scorecard.
    OpenDetail,
    /// `?` — open the keybinds overlay.
    OpenHelp,
    /// `Esc`, `Enter` (when overlay is open), or repeat-toggle of the
    /// overlay key — dismiss any open overlay.
    CloseOverlay,
    /// Terminal resized — repaint.
    Resize,
    /// Snapshots from a completed (or in-progress) refresh round.
    Snapshots(Vec<LayerSnapshot>),
    /// `q` / `Ctrl-C` — exit cleanly.
    Quit,
}
