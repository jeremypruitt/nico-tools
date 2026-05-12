use std::time::Instant;

use crate::model::{
    EntityRef, LayerSnapshot, LogLine, PopoverDiagnosis, PopoverEvent, SourceError,
};

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
    /// Top-N error log lines from a completed refresh round. Powers the
    /// snapshot logs panel (focused-layer drill panel and the Spotlight
    /// `logs` incident card). Issue #158.
    LogLines(Vec<LogLine>),
    /// Left-click at terminal cell `(col, row)`. The reducer hit-tests
    /// against the scorecard regions captured during the last render.
    Click { col: u16, row: u16 },
    /// Mouse-wheel scroll. The reducer routes it to the drill panel when
    /// no overlay is up, otherwise to the open overlay.
    Scroll(ScrollDir),
    /// `M` — toggle terminal mouse capture so the operator can reach
    /// native scrollback when they need to.
    ToggleMouseCapture,
    /// `s` from the scorecard layout — switch to the Spotlight layout
    /// (incident-only 3am page view; only non-green Layers get full
    /// cards).
    ShowSpotlight,
    /// `a` (or `s`, or `Esc`) from Spotlight — return to the show-all
    /// scorecard grid.
    ShowAll,
    /// `y` in Spotlight — copy the focused Finding's next-command to the
    /// system clipboard. Failure (e.g. headless Linux) raises a toast.
    CopyNextCommand,
    /// `o` in Spotlight — open the focused Finding's link via the system
    /// browser. Best-effort; toast on failure (or when no link is set).
    OpenLink,
    /// `c` from any view — extract the entity from the focused row's
    /// finding (workflow / DPU / host / request) and open the correlate
    /// mini-dashboard popup for it. PRD-007 Slice 0 ships the Spotlight
    /// trigger; the underlying handler dispatches through
    /// [`Action::OpenCorrelatePopup`].
    Correlate,
    /// PRD-007: open the correlate mini-dashboard popup for an explicit
    /// entity. The general path the per-surface triggers (Spotlight rows,
    /// log lines, Findings detail, event timeline) all funnel into.
    OpenCorrelatePopup(EntityRef),
    /// Results from a `nico_correlate::collect_all` round, posted by the
    /// host loop. Carries the `entity` so the reducer can drop stale
    /// results when the operator has already closed or re-opened the
    /// popup for a different entity. PRD-007 adds `diagnosis` for the
    /// banner at the top of the popup.
    CorrelateResults {
        entity: EntityRef,
        events: Vec<PopoverEvent>,
        source_errors: Vec<SourceError>,
        diagnosis: Option<PopoverDiagnosis>,
    },
    /// Show a transient toast in the bottom bar (e.g. "clipboard
    /// unavailable"). Auto-clears after `TOAST_TTL`.
    ShowToast(String),
    /// `q` / `Ctrl-C` — exit cleanly.
    Quit,
}
