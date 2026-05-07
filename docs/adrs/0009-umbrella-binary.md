# ADR-009: Umbrella binary `nico` with clap subcommand dispatch

- **Status:** Accepted
- **Date:** 2026-05-06

## Context

The toolkit has shipped two independent binaries (`nico-doctor`,
`nico-correlate`), each with its own argument-parsing entry point and its own
bootstrap. A third capability — a live ops dashboard — is on the way, and a
fourth is plausible. Adding a dashboard as a third top-level binary
multiplies install paths, brand surfaces, shell-completion sources, and the
"which command was that?" cost for the operator. It also means three places
where reach-manager / config wiring must agree.

The operator's mental model is one tool, several capabilities, all rooted in
the same cluster context. The shipped layout works against that.

## Decision

Ship a single umbrella binary `nico` that dispatches subcommands:

- `nico ops` — live operational dashboard (default subcommand). Placeholder
  for now; see ADR-012.
- `nico doctor [...args]` — what `nico-doctor [...args]` does today.
- `nico correlate <id> [...args]` — what `nico-correlate <id> [...args]` does
  today.

The dispatcher is a thin clap binary. Each subcommand's argument struct
(`DoctorArgs`, `CorrelateArgs`) lives in its own crate's public API and is
embedded in the umbrella `Subcommand` enum via clap derive. The umbrella does
no work of its own; it parses and calls `nico_doctor::run_doctor(args).await`,
`nico_correlate::run_correlate(args).await`, or `nico_ops::run_ops()`.

The subcommand crates become library-first (see ADR-011): all bootstrap and
flow logic lives in `lib.rs` and is independently exercisable, including by
the future `nico ops` dashboard which will compose these libraries directly.

## Consequences

### Positive
- One install path, one shell completion, one `--help` surface.
- A new capability is a new subcommand crate, not a new binary.
- The dashboard can compose `prepare_layers` / `prepare_sources` from the
  doctor and correlate libraries without shelling out.
- Cluster context, reach mode, and config resolution can converge across
  capabilities because all paths flow through the same library APIs.

### Negative / Trade-offs
- Operators with the old binary names in their muscle memory or scripts must
  switch. We can ship optional thin-shim binaries (`nico-doctor`,
  `nico-correlate`) for one release that delegate to `nico doctor` /
  `nico correlate`; see ADR-011 for the policy.
- Building `nico` pulls in all subcommand dependencies. Acceptable: the
  current dependency closure is dominated by the same shared crates anyway.

## Alternatives Considered

- **Keep three independent binaries.** Rejected: see Context — multiplies
  surfaces, fragments bootstrap, and the dashboard would need to spawn
  subprocesses or duplicate wiring.
- **One binary, no subcommands, behavior modes via flags.** Rejected: the
  three modes have meaningfully different argument shapes (`<id>` is
  required for correlate, not for the others). Subcommands are the natural
  fit.

## Related

- ADR-011 — drop TUI from text-only subcommands; library-first crates.
- ADR-012 — async Component-style TUI event loop for `nico ops`.
