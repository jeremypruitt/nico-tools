# ADR-007: Optional TUI — ratatui-based interactive timeline view

- **Status:** Accepted
- **Date:** 2026-05-05

## Context

Both tools currently emit to stdout and exit. The default human output is
designed to fit one screen (ADR-003), which works well for `nico-doctor`
summaries. For `nico-correlate`, however, a real correlation on a busy cluster
can produce 100-300 Timeline events — scrolling that in a terminal is awkward,
and there is no way to inspect the full payload of an individual event without
re-running with `--json` and grepping.

The original PRD listed "Web UI / TUI" as out of scope. That was a v1
simplicity decision, not a permanent one. The Sources are now all wired,
diagnosis patterns exist, and the primary request is for navigation, not
rendering complexity.

The `--tail` mode (issue #13) makes the UX problem acute: a live-streaming
Timeline in a scrolling terminal window is hard to read while it is growing.

This ADR supersedes the "Web UI / TUI" out-of-scope entry in the PRD and
CONTEXT.md for the narrow case described below.

## Decision

Add an **opt-in** TUI mode activated by the `--tui` flag. It is additive:
the default stdout modes (human and `--json`) are unchanged.

**Scope: `nico-correlate` first. `nico-doctor` second, in a separate issue.**

### Activation and guards

- `--tui` requires an interactive terminal. If stdout is not a TTY the tool
  exits immediately with code 3 and message:
  `` `--tui` requires an interactive terminal (stdout is not a TTY) ``
- `--tui` and `--json` are mutually exclusive; passing both is an immediate
  error (code 3).
- `NO_COLOR` / `--no-color` apply inside the TUI (icons remain, ANSI color
  removed). `--ascii` substitutes box-drawing characters with ASCII
  equivalents.
- The TUI is a rendering layer only. It calls the same `correlate()` /
  `run_layers()` functions as the non-TUI path. No business logic lives inside
  the TUI code.
- All existing unit and snapshot tests run without a TTY; `ratatui`'s
  `TestBackend` is used for TUI rendering tests.
- A panic hook must restore the terminal to cooked mode before printing the
  panic message, so a crash does not leave the operator's terminal broken.

### Layout (`nico-correlate --tui`)

```
┌─ Timeline (47 events) ───────────────────┬─ Event detail ────────────────┐
│ 2026-05-05T14:01:03Z  temporal  started  │ source:    temporal            │
│ 2026-05-05T14:01:07Z  postgres  update   │ kind:      activity_failed     │
│▶2026-05-05T14:01:09Z  temporal  failed   │ detail:    ProvisionnActivity  │
│ 2026-05-05T14:01:11Z  k8s       warning  │            attempt 3/3 …       │
│ …                                        │ next:      tctl w describe …   │
├──────────────────────────────────────────┴───────────────────────────────┤
│ Diagnosis: activity_retry_exhaustion  │  ⟳temporal ●postgres ○redfish  ?:help q:quit │
└───────────────────────────────────────────────────────────────────────────┘
```

**Left pane — Timeline list**
- One event per row, severity-coloured per ADR-004 rules.
- `↑`/`↓` move the selection. `PgUp`/`PgDn` for fast scroll.
- `g` jumps to the first row. `G`/`End` jumps to the last row.
- Pane title shows row count: `Timeline (47 events)`. When a filter is
  active: `Timeline (12/47)`.
- Before any events have arrived (skeleton phase): a single dim placeholder
  line `waiting for sources…` fills the pane.

**Right pane — Event detail**
- Shows source, kind, full detail text, and next-command hint for the
  selected event.
- Before any event is selected: dim hint `↑↓ to select an event`.
- When filter returns zero results: `No events match filter`.
- **Collapse threshold:** below 100 columns the right pane is hidden. The
  Timeline pane title gains the hint `(Enter for detail)`. Pressing `Enter`
  opens a full-screen overlay for the selected event, dismissible with `q` or
  `Escape`.

**Bottom bar**
- Left: active Diagnosis label, or blank if none.
- Centre: per-source four-state indicators —
  `⟳` fetching → `●` available → `✗` errored → `○` unavailable/skipped.
- Right: `FOLLOW` / `PAUSED` indicator (tail mode only) + minimal hint
  `?:help  q:quit`.

**Keybindings**
| Key | Action |
|-----|--------|
| `↑` / `↓` | Move selection |
| `PgUp` / `PgDn` | Fast scroll |
| `g` | Jump to first row |
| `G` / `End` | Jump to last row; re-enable auto-follow if in tail mode |
| `f` | Toggle auto-follow (tail mode) |
| `/` | Open inline filter bar |
| `Escape` | Clear active filter / dismiss overlay |
| `Enter` | Open full-screen event detail overlay (always; required below 100 cols) |
| `?` | Open keybindings help overlay |
| `q` / `Ctrl-C` | Clean exit |

**`?` help overlay:** full keybindings list, dismissible with `?`, `q`, or
`Escape`. The bottom bar always shows `?:help  q:quit` so the overlay is
discoverable on first run.

**`/` filter bar**
- Matches substring against source name OR event detail text.
- Filters update in real time as you type. `Escape` clears and restores the
  full Timeline.
- Pane title updates to `Timeline (12/47)` while a filter is active.

### Incremental load and cursor identity

The TUI renders immediately. Sources resolve in parallel; the Timeline
populates as each Source completes. The cursor tracks by **event identity**
(timestamp + source + kind), not row index. When new events are merged into
the sorted Timeline, the cursor follows the selected event to its new row
position. If no event is selected yet, the cursor stays at row 1.

### `--tail` + `--tui`

New events from all Sources append to the Timeline in real time.

**Auto-follow:** when the cursor is on the last row, new events auto-scroll
the list (follow mode). `FOLLOW` is shown in the bottom bar. When the user
scrolls up, follow pauses (`PAUSED`). `G`/`End` jumps to the last row and
re-enables follow. `f` explicitly toggles follow regardless of cursor
position.

**Source errors during tail:** if a Source that was `●` available errors
mid-stream, two things happen simultaneously:
1. Its bottom bar indicator flips `●` → `✗`.
2. A synthetic event is injected into the Timeline at the error timestamp —
   red text, kind `source_error`, detail shows the error message. This marks
   exactly where data stops flowing from that Source in the Timeline itself,
   not just in the status bar.

### Layout (`nico-doctor --tui`) — sketch, details deferred to separate issue

A live-refresh dashboard. Checks re-run on a configurable interval (default
30 s, see config key below). Each layer occupies one row in the left pane;
status icon updates in place. Selecting a layer shows its Findings in the
right pane. `r` forces an immediate refresh.

The same collapse threshold (100 columns), `?` overlay, and keybinding
conventions apply.

### Configuration

A new `[output]` section in `~/.config/nico-tools/config.toml`:

```toml
[output]
tui_refresh = "30s"   # nico-doctor --tui re-run interval
```

`--interval <duration>` flag overrides `tui_refresh` per the existing
flag > env > config > default precedence chain. This key is reserved here
so the `nico-common::config` struct is designed with it in mind; the
`nico-doctor --tui` implementation is a separate issue.

## Consequences

### Positive
- Makes long Timelines navigable without piping to `jq`.
- `--tail --tui` is the intended "watch a workflow in flight" experience.
- `nico-doctor --tui` becomes a useful ops dashboard for low-traffic clusters.
- Opt-in: CI, scripts, and `--json` consumers are completely unaffected.
- Hard-error on non-TTY prevents silent output format surprises in scripts.

### Negative / Trade-offs
- Adds `ratatui` + `crossterm` as dependencies (~500 KB binary size increase).
- Second rendering path to maintain alongside the existing human formatter.
- Terminal size edge cases (very narrow terminals, no-resize events) require
  defensive handling.
- `ratatui` requires raw mode; panic hook is mandatory to avoid leaving the
  operator's terminal broken.

## Alternatives Considered

- **Pipe to `less -R`:** zero dependencies, but no split-pane detail view and
  no live tail. Addresses scrolling only.
- **Web UI (`--serve`):** richer visuals but violates the "no daemons, one
  binary" constraint and requires a browser. Rejected.
- **Always-on TUI (replace default output):** breaks CI and `--json` consumers.
  Opt-in flag is the right boundary.
- **Fallback to human output when not a TTY:** rejected. `--tui` is explicit
  intent; silent format substitution masks misconfiguration. Hard-error is
  clearer.
- **Summary card in right pane during skeleton:** rejected. Adds a second
  layout to maintain for information already visible in the bottom bar.

## Open

- Mouse support (click to select a Timeline row) — defer; arrow keys cover v1.
- `nico-doctor --tui` details — refresh UX, layer drill-down, Findings pane
  layout. Deferred to the separate nico-doctor TUI issue.

## Related

- ADR-001 (exit codes) — non-TTY and `--tui`+`--json` conflicts exit code 3.
- ADR-003 (output format) — `--tui` is a third output mode; `--json` stability
  contract is unaffected.
- ADR-004 (color semantics) — TUI uses the same severity → color mapping.
- ADR-006 (concurrency) — TUI refresh in `nico-doctor` reuses the same bounded
  runner; no new concurrency primitives needed.
- Issue #13 — `--tail` mode; `--tui --tail` is the primary motivating use case.
