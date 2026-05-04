# ADR-003: Output format — human-first, JSON-stable

- **Status:** Accepted
- **Date:** 2026-05-03

## Context

The tools have two distinct audiences: operators reading output during an
incident, and CI systems / future correlation tools consuming it
programmatically. These audiences have opposite needs — humans want
compression and signal, machines want completeness and stability.

## Decision

Two output modes:

**Human (default)** — fits one screen. Target ≤20 lines for the summary on a
24-line terminal. One line per layer in the summary. Sections only for layers
with warnings or failures. Every problem points at the next command. Footer
hint reminds about `--verbose` and `--json`.

**JSON (`--json`)** — machine-complete. Includes everything, structured. Has
a top-level `version` field; bumping it is a breaking change requiring a major
version. **Additive changes are safe** (adding new fields or new source entries);
renaming, removing, or changing the type of any existing field is breaking and
requires bumping `version`.

The human format is allowed to evolve freely between minor versions. The JSON
format is a stability contract.

A documented JSON schema lives at `docs/json-schema.md`. There is a snapshot
test that round-trips a recorded fixture to detect accidental schema drift.

## Consequences

### Positive
- Human output stays readable; we can polish it without breaking CI.
- JSON consumers (CI gates, `nico-correlate`, future tooling) have a stable
  contract.
- "If it's not in JSON, downstream tools can't depend on it" is a clear rule.

### Negative / Trade-offs
- Two output paths to maintain.
- Schema discipline required — tempting to add fields ad-hoc.

## Alternatives Considered

- **Single human-only format:** ruled out by CI and correlation use cases.
- **JSON-only with a separate pretty-printer:** complicates the common path.
  Operators want to type one command, not pipe.
- **YAML output:** rejected. Human format already serves the readability case;
  a third format adds maintenance burden with no audience.

## Related

- ADR-001 (exit codes) — exit code is a one-byte summary of the same data
  `--json` carries in full.
- ADR-004 (color) — color rules apply only to the human format; `--json` is
  always plain.
