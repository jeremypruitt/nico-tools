# ADR-003: Output format — human-first, JSON-stable

- **Status:** Accepted
- **Date:** 2026-05-03
- **Amended:** 2026-05-07 (headline vs. detail checks; per-layer detail cap)

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

### Headline vs. detail checks (2026-05-07 amendment)

A single noisy layer (e.g. `logs` with hundreds of pod-error lines) can blow
past the ≤20-line target because every `Check` is rendered as a row. The
≤20-line target is real — the fix is to recognize that not all checks belong
in the summary line.

Each `Check` is one of two kinds:

- **Headline** — summarizes the layer at a glance (e.g. `error_lines`,
  `source`, `pods_ready`). Headline check values are the only values joined
  into a layer's summary line. Bounded in count by layer design.
- **Detail** — one-per-finding evidence (e.g. `pod_error`, `stuck_workflow`).
  Never appears in the summary line. Appears in the findings block, capped.

Default human mode caps the findings block at **N detail bullets per layer**
(initial value: 5), with a trailing `… +M more · --verbose for full list`
elision line when truncated. `--verbose` always shows every detail bullet.
`--json` is unaffected — it remains machine-complete regardless of cap, so
downstream consumers (`nico-correlate`, CI gates) keep getting every finding.

Layers that produce per-finding evidence SHOULD also collapse near-duplicate
findings at their source (e.g. group log errors by pod with a count and a
sample) rather than relying solely on the formatter cap.

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
