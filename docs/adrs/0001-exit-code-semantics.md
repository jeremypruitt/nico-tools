# ADR-001: Exit code semantics

- **Status:** Accepted
- **Date:** 2026-05-03

## Context

`nico-doctor` is designed to run in CI pipelines and pre-deploy gates. Shell
scripts and CI systems rely on exit codes to make decisions. The tool runs
multiple independent checks (cluster, logs, workflows, health, grpc, postgres),
each of which can succeed, warn, fail, or be unable to complete. We need a
stable, documented contract for what the process exit code means.

## Decision

Exit codes are:

| Code | Meaning |
|------|---------|
| 0 | All checks passed or were skipped. Safe to proceed. |
| 1 | At least one check produced a warning. No failures. |
| 2 | At least one check produced a failure. Do not proceed. |
| 3 | Could not run checks (config error, can't reach cluster, auth failure). |

This contract is part of the public API and is versioned along with `--json`
output. Breaking this contract requires a major version bump.

`unknown` (a check timed out or could not run) does NOT raise the exit code on
its own. It's a separate signal in `--json` output. Treating `unknown` as
`fail` would conflate "broken" with "didn't ask"; they are different and CI
gating may want to handle them differently.

## Consequences

### Positive
- `nico-doctor && deploy.sh` works as a gate.
- Distinct codes for warn vs. fail allow CI to decide its own tolerance.
- Exit code 3 lets ops scripts distinguish "the system is broken" from "the
  tool is broken."

### Negative / Trade-offs
- Conventions vary; some users expect `0/1` only and may not check 2/3.
- Exit code 3 means scripts that gate on `!= 0` will treat tool failure as
  system failure unless they check explicitly.

## Alternatives Considered

- **0/1 only:** insufficient signal for gating policies that differ between
  warn and fail.
- **Bitmask exit codes:** clever but unconventional; hostile to shell users.

## Related

- ADR-003 (output format) — `--json` carries the full per-check status detail
  that the exit code summarizes.
