# ADR-012: Async Component-style TUI event loop for `nico ops`

- **Status:** Proposed
- **Date:** 2026-05-06

## Context

The TUI in `nico-doctor` and `nico-correlate` (ADR-007) was built around a
sync `crossterm::event::poll(timeout)` loop that drives a single render
function with a single growing state struct. Inside that loop the rest of the
async world (data fetches, port-forwards, log tails) gets bridged through
`std::sync::mpsc` channels. That pattern got us to a working dashboard, but
it has wear marks:

- The render path has grown a long ad-hoc match on key events with no
  natural place to scope state per visual region (timeline, detail pane,
  bottom bar, filter, help overlay).
- Async work in tail mode is started on a `tokio::task::JoinSet` from
  outside the loop, while UI state ages inside the loop — so cancellation
  and "what happens to in-flight requests when the user filters" are
  awkward.
- Adding new screens (like `nico ops` will need) means more branches in the
  same render-and-event monolith.

`nico ops` is the natural place to fix this once, before there's a dashboard
shape we'd be migrating away from.

## Decision

`nico ops` will use an async Component-style TUI event loop. Properties:

- **Single async event loop**, driven by a `tokio::select!` on:
  - keyboard / resize events (via `crossterm::event::EventStream`),
  - tick timer for refresh cadence,
  - `mpsc::Receiver` of typed `AppEvent`s emitted by data tasks.
- **Components are first-class.** Each visual region (timeline, detail
  pane, status bar, filter, help overlay) implements a small trait with
  `handle_event(&mut self, event) -> Option<Action>` and `render(&self,
  area, frame)`. Components own their local state.
- **Actions are bubbled up** from components and reduced by an `App`
  reducer that owns global state and dispatches data fetches as further
  async tasks. This is the same shape as the `tui-realm` / Elm-architecture
  approach but using only `ratatui` + `crossterm` + `tokio` (no extra TUI
  framework dep, per ADR-007).
- **Data work runs as tokio tasks** with `JoinSet` cancellation tied to the
  app lifetime. Each task sends `AppEvent`s back through the channel; no
  blocking calls on the event loop.

This explicitly **replaces** the sync `event::poll` pattern that was used in
the now-deleted `nico-doctor` and `nico-correlate` TUIs (ADR-011). New TUI
code must follow the Component pattern. We are not retrofitting the old
pattern anywhere.

### Forward-looking

This ADR is "Proposed" rather than "Accepted" because `nico ops` itself is
still a stub (`pub fn run_ops() -> i32 { eprintln!("not yet"); 3 }`). The
ADR records the architectural commitment we have made when designing the
umbrella restructure (ADR-009, ADR-011); concrete trait signatures and a
worked example will land with the first real `nico ops` implementation, at
which point this ADR moves to Accepted.

## Consequences

### Positive
- Local state stays local; new screens are new components, not new branches
  in a monolith.
- Async data work, cancellation, and the UI loop share one tokio runtime
  instead of being bridged through sync channels.
- The pattern matches the future `nico ops` requirements (multiple panes,
  per-pane filters, on-demand drill-downs) much better than the old loop.

### Negative / Trade-offs
- Slightly more upfront machinery (a Component trait, an Action enum, an
  App reducer) than a single `loop { poll; render; }` body.
- Async event loops are more debuggable with structured logging than ad-hoc
  `eprintln!`; a small tracing setup will be needed.

## Alternatives Considered

- **Keep the sync `event::poll` loop in `nico ops`.** Rejected: see Context.
  We are about to grow the surface, and that pattern doesn't scale.
- **Adopt `tui-realm` or another framework.** Rejected for now: the ratatui
  + crossterm baseline (ADR-007) is small and well-understood; bringing in a
  framework adds a moving target without solving anything we can't solve in
  ~200 lines of Component plumbing.

## Related

- ADR-007 — original TUI design (ratatui+crossterm baseline preserved).
- ADR-009 — umbrella binary `nico` and library-first subcommand crates.
- ADR-011 — TUI moved out of text-only subcommands; new TUI lives in
  `nico-ops` and uses this pattern.
