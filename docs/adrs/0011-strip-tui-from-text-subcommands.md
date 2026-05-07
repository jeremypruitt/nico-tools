# ADR-011: Strip TUI from text-only subcommands

- **Status:** Accepted
- **Date:** 2026-05-06

## Context

Both `nico-doctor` and `nico-correlate` shipped a `--tui` mode (ADR-007) on
top of their default human and `--json` text outputs. The TUI added a sync
`event::poll` loop, a custom panic hook, and full `ratatui` + `crossterm`
deps to each subcommand crate.

With the move to an umbrella `nico` binary (ADR-009) and a forthcoming
operational dashboard (`nico ops`), the live-dashboard role moves into a
single dedicated crate. Carrying duplicate TUI wiring inside the doctor and
correlate crates means:

- Two TUI implementations to maintain in lockstep with the new dashboard.
- Two more places `ratatui` and `crossterm` get pulled into the dependency
  graph (and their event-loop pattern, which we are intentionally replacing
  in `nico ops` — see ADR-012).
- `nico doctor` and `nico correlate` accumulate TTY-detection and
  panic-hook complexity even though their primary role is to be small,
  scriptable, text-emitting tools.

## Decision

`nico doctor` and `nico correlate` are text-only. They emit human or
`--json` output; they do not have a `--tui` flag.

Concretely:

- `crates/nico-doctor/src/tui.rs` and `crates/nico-correlate/src/tui.rs` are
  deleted.
- `ratatui` and `crossterm` are removed from those crates' `Cargo.toml`.
- The `--tui` and `--interval` flags are dropped from `DoctorArgs`; the
  `--tui` flag is dropped from `CorrelateArgs`.
- Live dashboard semantics live in `nico ops` (ADR-012). That crate composes
  the now-library-first `nico-doctor` and `nico-correlate` APIs (notably
  `prepare_layers`, `run_streaming`, `prepare_sources`, `collect_all`).

### Optional shim policy

For one release we may keep `nico-doctor` and `nico-correlate` thin wrapper
binaries that exec the umbrella with the matching subcommand, to ease muscle
memory. They will be removed in the next major bump. (As of this ADR no shim
is shipped; the names are reserved for that one-release window if we choose
to add them.)

## Consequences

### Positive
- One TUI implementation in the workspace, owned by `nico-ops`.
- Smaller dependency surface and faster build for the text subcommands.
- `DoctorArgs` and `CorrelateArgs` become small, embeddable structs that
  the umbrella binary can compose directly into its `Subcommand` enum.
- Library-first crates mean the dashboard reuses real bootstrap logic
  instead of re-implementing it.

### Negative / Trade-offs
- Anyone driving `--tui` directly from `nico-doctor`/`nico-correlate` must
  switch to `nico ops` once that lands. ADR-007's commitments about TUI
  features (incremental load, filter, follow mode, bottom bar) move
  forward into `nico ops`; they are not lost, just relocated.
- During the gap between this ADR and `nico ops` shipping, there is no
  live dashboard at all. We accept this gap to avoid carrying two
  implementations in parallel.

## Alternatives Considered

- **Keep `--tui` in each subcommand and add a third TUI in `nico-ops`.**
  Rejected: triple the surface area for no gain.
- **Move TUI into a shared `nico-tui` crate and depend on it from all three.**
  Rejected: still pays the dependency-graph cost in the text subcommands and
  preserves the sync `event::poll` pattern we want to replace (ADR-012).

## Related

- ADR-007 — original `--tui` mode design (now scoped to `nico ops`).
- ADR-009 — umbrella binary motivation.
- ADR-012 — async Component-style TUI event loop for the new dashboard.
