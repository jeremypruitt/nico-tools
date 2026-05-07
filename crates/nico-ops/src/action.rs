use std::time::Instant;

use crate::model::LayerSnapshot;

/// Direction for focus navigation across the scorecard grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Mouse-wheel scroll direction. Routed by the reducer to either the drill
/// panel (no overlay) or the detail overlay (when open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDir {
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
    /// `space` — pause/resume the auto-refresh timer. Manual `R` always
    /// works regardless of pause state.
    TogglePause,
    /// Periodic clock tick from the host loop. The reducer compares
    /// `now` against the next-refresh deadline and may emit
    /// `Effect::StartRefresh`. Throbber animation is also driven by
    /// the timestamp on this action.
    Tick(Instant),
    /// Left-click at terminal cell `(col, row)`. The reducer hit-tests
    /// against the scorecard regions captured during the last render.
    Click { col: u16, row: u16 },
    /// Mouse-wheel scroll. The reducer routes it to the drill panel when
    /// no overlay is up, otherwise to the open overlay.
    Scroll(ScrollDir),
    /// `M` — toggle terminal mouse capture so the operator can reach
    /// native scrollback when they need to.
    ToggleMouseCapture,
    /// `q` / `Ctrl-C` — exit cleanly.
    Quit,
}
