# ADR-010: `nico-ops` dashboard architecture

- **Status:** Accepted
- **Date:** 2026-05-06

## Context

`nico ops` is the umbrella binary's live operational dashboard subcommand
(ADR-009). Until this ADR, `nico-ops::run_ops()` was a placeholder that
printed "not yet" and exited 3. This is the first real slice — what the
operator sees on launch.

We need a layout opinion before we wire in real data. The constraints from
upstream ADRs:

- Read-only by design (ADR-002): the dashboard inspects, never remediates.
- Output format contract (ADR-003): the screen still has to fit one terminal
  height and earn vertical space — pip strip + scorecards meet that bar.
- Color semantics (ADR-004): pip / scorecard / drill colors come from
  `nico_common::theme` (`--theme` / `NICO_THEME`).
- Concurrency discipline (ADR-006): per-layer collection runs concurrently
  via the existing `nico_doctor::run_streaming` API.
- Async Component event loop (ADR-012): event handling is a single
  `tokio::select!` over `crossterm::event::EventStream` + a tick interval +
  an `mpsc` of typed `AppEvent`s; the screen has Components, an `Action`
  enum, an `App` reducer, and a pure `events::translate` function.

Three layout candidates were on the table:

- **Layout A — Status Strip + Scorecards + Drill.** Header pip strip + 3-up
  reflowing scorecards + drill panel. Scannable, drillable, glanceable.
- **Layout B — Tabs by Layer.** One tab per Layer; arrow-key tab switcher.
  Conserves space but hides 5/6 of the system at any moment.
- **Layout C — Stream + Sidebar.** Live event stream on the left,
  sidebar of layer states. Useful for incident timelines, less useful for
  steady-state health.

Layout A wins for the launch slice because the primary `nico ops` user is an
operator triaging an incident: they need (1) a one-glance verdict, (2) a
scannable surface for which Layer is unhappy, and (3) a way to drill into
the unhappy Layer's findings without losing the global view. B hides the
global view; C buries the verdict.

## Decision

`nico ops` ships **Layout A** as the launch default and the only layout in
this slice. ADR placeholders for Layouts B and C are not produced; if either
is built later, this ADR is superseded for the layout choice and a new ADR
covers that layout. ADR-010 also supersedes the `nico-doctor --tui` layout
section of ADR-007: that subcommand is text-only now (ADR-011) and the
dashboard role lives here.

### Layout A — Status Strip + Scorecards + Drill

```
┌─ nico ops ────────────────────────────────────────────  refreshed 14:01:09 ┐
│ ● ● ! ● ● ●     OK                                                          │
├──────────────────────────────────────────────────────────────────────────────┤
│ ┌─ cluster ─────┐ ┌─ logs ───────┐ ┌─ workflows ───┐                         │
│ │ ● 3 nodes     │ │ ! 12 errors  │ │ ● no stuck wf │                         │
│ └───────────────┘ └──────────────┘ └───────────────┘                         │
│ ┌─ health ──────┐ ┌─ grpc ───────┐ ┌─ postgres ────┐                         │
│ │ ● 4/4 healthy │ │ ● reachable  │ │ ● 12ms ping   │                         │
│ └───────────────┘ └──────────────┘ └───────────────┘                         │
├──────────────────────────────────────────────────────────────────────────────┤
│ Findings — logs                                                              │
│  ! 12 ERROR lines in carbide-controller (last 10m)                           │
│      next: kubectl logs -n nico carbide-controller --since=10m | grep ERROR  │
│                                                                              │
├──────────────────────────────────────────────────────────────────────────────┤
│ R:refresh  hjkl/arrows:focus  Enter:detail  ?:help  q:quit                   │
└──────────────────────────────────────────────────────────────────────────────┘
```

**Header.** A pip strip — one Unicode pip per Layer, colored by aggregate
Status (`●` ok, `▲` warn, `✖` fail, `○` unknown/skipped). Followed by an
overall verdict word (`OK` / `WARN` / `FAIL`) and a `refreshed HH:MM:SS`
timestamp.

**Body.** A reflowing grid of scorecards, one per Layer. The grid is 3-up
on wide terminals, 2-up on medium, 1-up on narrow. Each card shows a status
pip, the layer's name, and a single line of evidence (e.g. "12 errors",
"no stuck wf"). Sparkline and delta-badge slots are reserved here; they ship
in later slices.

**Drill panel.** Below the grid: the Findings of the focused scorecard,
each line accompanied by a dim `next:` command-hint that points at the
underlying tool the operator should run. Read-only; we never run the
suggested commands for them.

**Overlays.**
- `Enter` opens a full-screen detail overlay for the focused Layer
  (full Findings list, no truncation). `Esc` dismisses.
- `?` opens a keybinds cheat sheet. Same dismissal.

**Keys.** `R` triggers a manual refresh. `↑↓←→` and `hjkl` move focus
across scorecards. `q` / `Ctrl-C` exits cleanly.

### Cross-cutting infrastructure (lands in this slice)

- **Async event loop.** `tokio::select!` over `EventStream`, a render-tick
  interval, and an `mpsc::Receiver<AppEvent>`. (ADR-012)
- **Single `Action` enum.** Every state change goes through
  `App::handle(Action) -> Option<Effect>`. No ad-hoc mutators.
- **Pure `events::translate(CrosstermEvent, mode, overlay) -> Option<Action>`.**
  Unit-testable without spinning up a terminal.
- **Dirty-flag rendering.** The render path is gated on a `dirty: bool`
  set by the reducer; ticks alone do not redraw.
- **Panic hook.** Restores cooked mode and leaves the alt-screen before
  printing the panic message, so a crash never strands the operator's
  terminal.
- **TTY guard.** Non-TTY stdout exits 3 with the message
  `nico ops requires an interactive terminal (stdout is not a TTY)`. This
  matches ADR-007's TTY-guard wording for the (now-removed) `--tui` flag.
- **Theme integration.** `--theme` flag and `NICO_THEME` env var map
  through `nico_common::theme::resolve_theme`.

## Consequences

### Positive
- One opinion shipped end-to-end before the surface grows. New screens
  become new components, not new branches in a monolith.
- `events::translate` and `App::handle` are pure and unit-testable; the
  render layer is snapshot-tested via `ratatui::backend::TestBackend`.
- The pip strip + verdict word satisfies the "one-glance" requirement
  without truncating any Layer's evidence on a normal-width terminal.

### Negative / Trade-offs
- This slice ships placeholder data: the dashboard is structurally complete
  but is not yet wired to `nico_doctor::run_streaming`. The follow-up slice
  replaces the placeholder layers with the real bootstrap path.
- Sparklines and delta badges are deferred. The grid leaves room for them.

## Alternatives Considered

- **Layout B — Tabs by Layer.** Rejected for the launch default: hides
  five-sixths of the system at any moment; bad for at-a-glance triage.
- **Layout C — Stream + Sidebar.** Rejected for the launch default:
  optimizes for incident timelines, not steady-state health.
- **Synchronous render loop (ADR-007's pattern).** Rejected per ADR-012;
  the new TUI uses the async Component pattern.

## Related

- ADR-007 — original `--tui` design. This ADR supersedes its
  `nico-doctor --tui` layout section.
- ADR-009 — umbrella binary `nico` and library-first subcommand crates.
- ADR-011 — TUI removed from text subcommands; live dashboard moves here.
- ADR-012 — async Component-style TUI event loop (the engine ADR-010 sits on).
