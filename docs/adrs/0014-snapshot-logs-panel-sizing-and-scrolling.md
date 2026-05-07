# ADR-0014: Snapshot logs panel — sizing and scrolling

- **Status:** Proposed
- **Date:** 2026-05-07

## Context

The snapshot logs panel (introduced in #158) renders the top error log
lines from the most recent refresh round. It appears in two places in
`nico ops`:

- **Layout A drill panel** when the `logs` layer is focused
  (`view.rs:441`)
- **Layout B `Logs` quadrant** (always visible alongside three others)

Today the data path uses two caps:

- `LOG_PANEL_FETCH_LIMIT = 500` — passed to `LogSource::collect`, matching
  what `LogsLayer` asks for.
- `LOG_PANEL_TOP_N = 20` — applied by `log_lines_from_entries` after
  fetch, before the lines reach `App::log_lines`.

The renderer (`render_logs_panel` in `view.rs:476`) takes
`inner.height` lines from those 20 and titles the panel `" logs — top
{lines.len()} "`. Two visible problems follow:

1. On any terminal where the panel's inner height exceeds 20, the body
   ends at row 20 and the rest of the area is blank — the title says
   "top 20" but the panel reserved space for ~35.
2. Once `lines.len()` exceeds the inner height (e.g. on a short
   terminal), the surplus lines are unreachable. There is no scroll
   affordance on the logs panel — only the drill panel's findings have
   one (`drill_scroll`, `view.rs:467`).

This ADR resolves both with a single, minimal pattern: render-side
trimming for the sizing fix, plus a panel-local scroll state with
reset-on-state-transition semantics.

## Decision

### Sizing — render-side trim, no fetch coupling

`LOG_PANEL_TOP_N` is **deleted**. `log_lines_from_entries` returns
every entry the source produced (still capped upstream by
`LOG_PANEL_FETCH_LIMIT = 500`).

`render_logs_panel` becomes the sole arbiter of how many rows to show.
With `lines.len()` available data and `inner.height` rows of canvas:

- The body renders `lines[logs_scroll .. logs_scroll + visible]`, where
  `visible = (lines.len() - logs_scroll).min(inner.height as usize)`.
- The title is `format!(" logs — {start}–{end} of {total} ", ...)` when
  non-empty, where `start = logs_scroll + 1`,
  `end = logs_scroll + visible`, `total = lines.len()`. When
  `lines.is_empty()`, the title is `" logs "` and the body keeps the
  existing `"no errors"` line.
- A taller window with more data than rows simply shows more rows.
  When the data is thinner than the window, the extra space below the
  last entry is left blank — there is no padding/centering to invent.

The fetch path is **not** coupled to `inner.height`. Resizing does not
re-trigger `spawn_logs_refresh`. The next refresh tick (or `R` press)
delivers a dataset matching the new geometry; until then the panel
operates on the data it already has, which is correct just at the
"old" cardinality.

### Scrolling — dedicated `logs_scroll` field on `App`

`App` gains a `logs_scroll: u16` alongside the existing `drill_scroll`
and `overlay_scroll`. The reducer routes scroll input to it whenever
the logs panel is the **dominant view** for the current layout:

- **Layout A:** dominant when the focused layer is `logs` (the drill
  panel below the strip is rendering the logs panel).
- **Layout B:** dominant when the focused quadrant is `Logs` **and**
  it is currently zoomed (`Action::ZoomQuadrant` engaged).

When the logs panel is dominant:

- `Action::Scroll(Up/Down)` (mouse wheel) targets `logs_scroll`
  instead of `drill_scroll`.
- `Action::Focus(Up/Down)` (j/k or arrow keys) is **rerouted** to the
  same scroll path. The user is already in the dominant log view; the
  vertical focus directions have no useful target there, so j/k/↑/↓
  scroll the panel.

When the logs panel is **not** dominant, all four inputs keep their
current behavior: mouse wheel → `drill_scroll`, j/k/↑/↓ → focus
movement.

`logs_scroll` resets to `0` on every state transition that isn't "user
scrolled while looking at logs":

- `Action::LogLines` (new refresh data arrives) — stale offset against
  fresh data is more confusing than helpful.
- Focus change away from logs.
- `Action::ToggleLayout` (A ↔ B).
- `Action::ZoomQuadrant` (zoom toggled either direction).

`render_logs_panel` clamps the offset on use to
`logs_scroll.min(lines.len().saturating_sub(inner.height))` so a
post-refresh dataset shrink can't render past the end before the next
state transition fires the reset.

### Help overlay

The `?` overlay gains one line documenting the dominant-view scroll:

```
j/k or wheel — scroll logs (when logs panel is zoomed/focused)
```

The footer hint bar is **not** modified. Per-panel context-sensitive
hints there would be a slippery slope; the help overlay is the
documented place for keybind discovery.

## Consequences

### Positive

- Tall windows fill: the body always renders as many rows as the data
  has, up to `inner.height`. No more dead space below row 20.
- The title carries honest cardinality: `1–30 of 200` tells the user
  there's more data, where they are in it, and how much more there is.
- Long log streams become reachable from inside `nico ops`. Users no
  longer have to leave the dashboard for the underlying `LogsLayer` to
  see entries past the visible window.
- The pattern is reusable. `logs_scroll` is the second instance of
  "panel-local scroll state with reset-on-transition semantics"
  (`drill_scroll` is the first); a third panel that needs scroll can
  follow the same shape.

### Negative / Trade-offs

- One more `u16` in `App` and one more reset rule per state
  transition. The reset surface (4 actions) is small but real new
  surface.
- The "logs panel dominant" predicate is a layout-aware method on
  `App`. Adding a third layout with logs visibility (e.g. a future
  Layout C) means updating the predicate.
- Mouse wheel in Layout A no longer scrolls the drill panel when the
  focused layer is `logs` — it scrolls the logs panel instead. This is
  the intended substitution: the "drill panel" *is* the logs panel in
  that focus.
- Resizing larger leaves blanks below the last log row until the next
  refresh delivers more data. Accepted as a known artifact: avoiding
  it would require re-triggering `spawn_logs_refresh` on every resize.

### Net effect on existing caps

| Constant                  | Before   | After   |
|---------------------------|----------|---------|
| `LOG_PANEL_FETCH_LIMIT`   | 500      | 500     |
| `LOG_PANEL_TOP_N`         | 20       | deleted |

The fetch firehose stays at 500 to match `LogsLayer`. The post-fetch
cap is gone; the renderer's `inner.height` is the new effective cap
on visible rows.

## Alternatives Considered

- **Couple fetch limit to panel height.** Pass `inner.height` into
  `LogsCtx` and have `spawn_logs_refresh` request exactly that many
  lines. Rejected: the fetch is async and runs on a refresh cadence,
  not on resize. The first fetch happens before any height is known
  (chicken-and-egg), and re-fetching on every resize would hammer the
  log source. The 500-line firehose is already in flight and matches
  what `LogsLayer` asks for; trimming render-side is free.
- **Keep `LOG_PANEL_TOP_N` and raise it to 500.** Belt-and-suspenders.
  Rejected: redundant with `LOG_PANEL_FETCH_LIMIT` plus the
  render-side `inner.height` trim. Two caps that always agree are
  one cap and a confusing constant.
- **Scroll j/k always (independent of focus).** Rejected: would break
  Layout A's horizontal card focus and Layout B's quadrant focus when
  the user wants to navigate, not scroll. Scope j/k scroll to the
  dominant-view rule.
- **Add a scroll mode (e.g. `i` to enter, `Esc` to leave).**
  Rejected as overengineered for a two-key affordance.
- **Show up/down chevrons in the title** when content is above/below
  the window. Rejected as visual noise; the `start–end of total`
  format already conveys both directions of scrollability.
- **Add a per-panel hint in the footer** when the logs panel is
  dominant. Rejected: per-panel hints in the global hint bar don't
  scale; the help overlay is the right home for keybind discovery.
- **Reuse `drill_scroll` in Layout A only.** Rejected: Layout B has no
  drill panel at all, so reusing `drill_scroll` there would mean
  "`drill_scroll` happens to also be the logs scroll when no drill
  exists" — a confusing overload. Dedicated field is clearer.

## Related

- ADR-010 — `nico-ops` dashboard architecture (the panel sits inside
  Layout A's drill area and Layout B's logs quadrant; mouse wheel
  routing was established there).
- ADR-012 — Async Component-style TUI event loop (the `Action` /
  reducer plumbing this ADR extends).
- Issue #158 — original snapshot logs panel.
