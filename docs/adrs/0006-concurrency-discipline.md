# ADR-006: Concurrency — bounded parallelism, layered timeouts

- **Status:** Accepted (amended by ADR-0013)
- **Date:** 2026-05-03 (amended 2026-05-07)

## Context

Both `nico-doctor` (six parallel layers) and `nico-correlate` (parallel
Sources) rely on parallel I/O so their output comes back in seconds rather
than minutes. Done naively, this can hammer the apiserver, leak tasks, or
leave the user staring at a hung tool when one check goes slow.

## Decision

Three concurrency rules, all enforced by shared infrastructure:

1. **Bounded parallelism.** All Kubernetes API calls go through a
   `tokio::sync::Semaphore` capped at 8 concurrent permits. This protects the
   apiserver from misuse that fans out to hundreds of pods.

2. **Per-check timeout.** Every individual check has a timeout (default
   5 seconds, configurable via `--timeout`). A timed-out **layer check**
   reports `unknown`, not `fail` — these are distinct signals (see ADR-001).
   This rule is scoped to layer checks. A timed-out **boot probe** step
   reports `fail` (red `✗`), not `unknown`, because a bootstrap I/O
   timeout is conclusive: the boot can't proceed. See ADR-0013.

3. **Global wall-clock timeout.** The whole tool run is wrapped in a single
   `tokio::time::timeout` (default 30 seconds). If the whole run exceeds it,
   anything still pending is reported `unknown` and the tool exits with code
   3 (cannot-run).

Layer execution uses `tokio::join!` (or `FuturesUnordered` if streaming
progress is added later). No unbounded `tokio::spawn` is allowed in
production code paths.

## Consequences

### Positive
- The tool is well-behaved against shared infrastructure.
- One slow check doesn't stall the whole report.
- Timeouts are observable — the user sees `unknown` rather than a hang.

### Negative / Trade-offs
- Choosing the right semaphore cap is a judgment call; 8 is a starting point
  and may need tuning.
- More code than the simplest "just run them all" version.

## Alternatives Considered

- **Unbounded parallelism:** rejected. We have no upper bound on how many
  pods/services exist; we can't safely fan out unconstrained.
- **Fully sequential:** rejected. Defeats the whole purpose; the report
  becomes slow.
- **Per-layer timeout only, no global:** rejected. A pathological case (many
  slow checks) could still exceed any reasonable wall-clock budget.

## Open

- Concrete semaphore cap value (8) is a guess. Revisit after first production
  run.
- Whether to expose the semaphore cap via flag (`--max-concurrency`). Defer
  until someone needs it.

## Related

- ADR-001 (exit codes) — `unknown` vs. `fail` distinction is what makes the
  timeout behavior meaningful.
- ADR-005 (reach mode) — port-forward setup is part of what the per-check
  timeout has to accommodate.
- ADR-0013 (boot probe) — scopes the timeout-as-unknown rule to layer
  checks; boot probe steps treat timeout as failure.
