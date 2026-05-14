//! Pure responsive layout solver for `nico ops`.
//!
//! Today the view layer hand-rolls `Layout::default().split(...)` blocks in
//! every renderer. PRD-006 Slice 2 (issue #368) consolidates that logic
//! behind a pure function: callers pass a [`SolverInput`] (viewport rect,
//! which top-level view is active, focus state, and whether the logs
//! overlay is open) and get back a [`LayoutPlan`] of named pane rects. No
//! I/O, no `App` dependency — testable in isolation.
//!
//! The solver also encodes the three width breakpoints that today are
//! reimplemented at every responsive call site (e.g. `grid_cols_for_width`):
//! small (<100 cols), medium (100..=180), large (>180). Renderers can read
//! the breakpoint off the plan instead of recomputing it.

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::app::Layout as AppLayout;

/// Width breakpoints used across the dashboard. The solver computes the
/// breakpoint once from `viewport.width` and exposes it on the plan so
/// every consumer agrees on which band the terminal falls into.
///
/// - `Small`:  width < 100 cols (e.g. an 80×24 SSH session)
/// - `Medium`: 100 <= width <= 180 cols (default laptop terminal)
/// - `Large`:  width > 180 cols (wide / docked monitor)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Breakpoint {
    Small,
    #[default]
    Medium,
    Large,
}

impl Breakpoint {
    pub fn for_width(width: u16) -> Self {
        if width < 100 {
            Self::Small
        } else if width <= 180 {
            Self::Medium
        } else {
            Self::Large
        }
    }
}

/// What the operator is currently focused on inside the active view.
/// Today only Scorecard reads this; the logs-focused state drives the
/// drill panel's content. The solver itself doesn't change pane sizes
/// based on focus — it's threaded through so consumers can branch
/// without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FocusState {
    #[default]
    Grid,
    Logs,
}

/// Pure inputs to [`solve`]. Same input → same plan, always.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SolverInput {
    pub viewport: Rect,
    pub view: AppLayout,
    pub focus: FocusState,
    pub logs_overlay_open: bool,
}

/// Output of [`solve`] — the named pane rects every renderer needs, plus
/// the derived breakpoint and (when open) the centered logs-overlay rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutPlan {
    pub header: Rect,
    pub body: Rect,
    pub drill: Rect,
    /// One-row severity legend sandwiched between the drill panel and
    /// the hint bar (issue #370). Pure read-only.
    pub legend_bar: Rect,
    pub hint_bar: Rect,
    pub logs_overlay: Option<Rect>,
    pub breakpoint: Breakpoint,
}

/// Solve the layout for the given input. Pure — no I/O, no globals.
pub fn solve(input: SolverInput) -> LayoutPlan {
    let breakpoint = Breakpoint::for_width(input.viewport.width);

    let (header, body, drill, legend_bar, hint_bar) = match input.view {
        AppLayout::Scorecard => split_scorecard(input.viewport),
        AppLayout::Spotlight => split_scorecard(input.viewport),
    };

    let logs_overlay = input
        .logs_overlay_open
        .then(|| centered(input.viewport, logs_overlay_pct(breakpoint)));

    LayoutPlan {
        header,
        body,
        drill,
        legend_bar,
        hint_bar,
        logs_overlay,
        breakpoint,
    }
}

fn split_scorecard(area: Rect) -> (Rect, Rect, Rect, Rect, Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(7),    // body
            Constraint::Min(5),    // drill
            Constraint::Length(1), // severity legend
            Constraint::Length(1), // hint bar
        ])
        .split(area);
    (chunks[0], chunks[1], chunks[2], chunks[3], chunks[4])
}

/// Width × height percentages for the centered logs-overlay modal at
/// each breakpoint. Narrow terminals get a bigger frame (less wasted
/// outer margin); wide terminals get a smaller one so the overlay sits
/// comfortably without spanning the whole monitor.
fn logs_overlay_pct(bp: Breakpoint) -> (u16, u16) {
    match bp {
        Breakpoint::Small => (95, 90),
        Breakpoint::Medium => (85, 80),
        Breakpoint::Large => (75, 70),
    }
}

fn centered(area: Rect, (pct_x, pct_y): (u16, u16)) -> Rect {
    let w = (area.width * pct_x) / 100;
    let h = (area.height * pct_y) / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vp(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn scorecard(viewport: Rect, focus: FocusState, logs_overlay_open: bool) -> LayoutPlan {
        solve(SolverInput {
            viewport,
            view: AppLayout::Scorecard,
            focus,
            logs_overlay_open,
        })
    }

    #[test]
    fn breakpoints_partition_width_range() {
        assert_eq!(Breakpoint::for_width(0), Breakpoint::Small);
        assert_eq!(Breakpoint::for_width(80), Breakpoint::Small);
        assert_eq!(Breakpoint::for_width(99), Breakpoint::Small);
        assert_eq!(Breakpoint::for_width(100), Breakpoint::Medium);
        assert_eq!(Breakpoint::for_width(140), Breakpoint::Medium);
        assert_eq!(Breakpoint::for_width(180), Breakpoint::Medium);
        assert_eq!(Breakpoint::for_width(181), Breakpoint::Large);
        assert_eq!(Breakpoint::for_width(200), Breakpoint::Large);
        assert_eq!(Breakpoint::for_width(u16::MAX), Breakpoint::Large);
    }

    #[test]
    fn scorecard_panes_stack_vertically_and_cover_viewport_at_medium() {
        let viewport = vp(140, 40);
        let plan = scorecard(viewport, FocusState::Grid, false);

        assert_eq!(plan.breakpoint, Breakpoint::Medium);
        assert!(plan.logs_overlay.is_none());

        // Panes share the viewport's x/width and stack from top to bottom
        // covering the full height with no gaps and no overlaps.
        for pane in [plan.header, plan.body, plan.drill, plan.legend_bar, plan.hint_bar] {
            assert_eq!(pane.x, viewport.x);
            assert_eq!(pane.width, viewport.width);
        }
        assert_eq!(plan.header.y, viewport.y);
        assert_eq!(plan.body.y, plan.header.y + plan.header.height);
        assert_eq!(plan.drill.y, plan.body.y + plan.body.height);
        assert_eq!(plan.legend_bar.y, plan.drill.y + plan.drill.height);
        assert_eq!(plan.hint_bar.y, plan.legend_bar.y + plan.legend_bar.height);
        assert_eq!(
            plan.hint_bar.y + plan.hint_bar.height,
            viewport.y + viewport.height,
        );

        // Constraints from the existing renderer: header fixed at 3, legend
        // bar fixed at 1, hint bar fixed at 1. Body and drill flex.
        assert_eq!(plan.header.height, 3);
        assert_eq!(plan.legend_bar.height, 1);
        assert_eq!(plan.hint_bar.height, 1);
        assert!(plan.body.height >= 7);
        assert!(plan.drill.height >= 5);
    }

    #[test]
    fn breakpoint_tracks_viewport_width() {
        assert_eq!(scorecard(vp(80, 24), FocusState::Grid, false).breakpoint, Breakpoint::Small);
        assert_eq!(scorecard(vp(120, 30), FocusState::Grid, false).breakpoint, Breakpoint::Medium);
        assert_eq!(scorecard(vp(220, 60), FocusState::Grid, false).breakpoint, Breakpoint::Large);
    }

    #[test]
    fn logs_overlay_rect_populated_only_when_open() {
        let viewport = vp(140, 40);

        let closed = scorecard(viewport, FocusState::Grid, false);
        assert!(closed.logs_overlay.is_none());

        let open = scorecard(viewport, FocusState::Grid, true);
        let overlay = open.logs_overlay.expect("overlay rect should be populated when open");
        // The overlay sits inside the viewport, is centered, and is
        // strictly smaller than it (so the underlying view shows through
        // the margins).
        assert!(overlay.x >= viewport.x);
        assert!(overlay.y >= viewport.y);
        assert!(overlay.x + overlay.width <= viewport.x + viewport.width);
        assert!(overlay.y + overlay.height <= viewport.y + viewport.height);
        assert!(overlay.width < viewport.width);
        assert!(overlay.height < viewport.height);
        let left_margin = overlay.x - viewport.x;
        let right_margin = (viewport.x + viewport.width) - (overlay.x + overlay.width);
        assert!(left_margin.abs_diff(right_margin) <= 1, "centered horizontally");
        let top_margin = overlay.y - viewport.y;
        let bottom_margin = (viewport.y + viewport.height) - (overlay.y + overlay.height);
        assert!(top_margin.abs_diff(bottom_margin) <= 1, "centered vertically");
    }

    #[test]
    fn logs_overlay_size_grows_at_narrower_breakpoints() {
        // Narrow terminals need a larger relative overlay so logs are
        // legible; wide terminals get a tighter modal.
        let small = scorecard(vp(80, 30), FocusState::Grid, true).logs_overlay.unwrap();
        let medium = scorecard(vp(140, 30), FocusState::Grid, true).logs_overlay.unwrap();
        let large = scorecard(vp(220, 30), FocusState::Grid, true).logs_overlay.unwrap();

        // Compare width-as-percent-of-viewport across breakpoints.
        let pct = |r: Rect, viewport_width: u16| (r.width as u32 * 100) / viewport_width as u32;
        assert!(
            pct(small, 80) > pct(medium, 140),
            "small breakpoint should claim a larger fraction of width than medium"
        );
        assert!(
            pct(medium, 140) > pct(large, 220),
            "medium breakpoint should claim a larger fraction of width than large"
        );
    }

    #[test]
    fn focus_state_does_not_change_pane_geometry() {
        // The drill panel's *content* changes when logs is focused
        // (different consumer renderer), but the solver returns the same
        // rects regardless of focus.
        let viewport = vp(140, 40);
        let grid_focus = scorecard(viewport, FocusState::Grid, false);
        let logs_focus = scorecard(viewport, FocusState::Logs, false);
        assert_eq!(grid_focus, logs_focus);
    }

    #[test]
    fn pure_function_returns_identical_plans_for_identical_inputs() {
        let input = SolverInput {
            viewport: vp(140, 40),
            view: AppLayout::Scorecard,
            focus: FocusState::Grid,
            logs_overlay_open: true,
        };
        assert_eq!(solve(input), solve(input));
    }
}
