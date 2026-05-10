# ADR-0015: Axis verdict primitive (`AxisSummary`)

- **Status:** Accepted
- **Date:** 2026-05-10

## Context

Every per-DPU layer (`dpu_cert`, `dpu_isolation`, `hbn`, `dpu_health`,
`dpu_services`, and the upcoming `infiniband` layer from PRD-004)
reduces a raw observation to a one-line headline plus zero-or-more
detail rows. The reduction has the same shape across every axis —
`(status, message, optional drill-down hint)` — but until PRD-003 it
was inlined separately in each layer. PRD-003 wants holistic per-DPU
and fleet rollups (slices 5 and 6) that consume those reductions; if
each layer keeps the verdict private, the rollup has to reach back
into every layer's renderer to recover the same fields.

PRD-003 Slice 1 (#305) introduces a shared primitive and migrates the
lowest-risk axis (`dpu_cert`) as the first consumer. The slice also
forces resolution of two open questions PRD-003 left open: where
verdict helpers live, and whether the primitive carries the
drill-down command.

## Decision

A new public type `AxisSummary` lives in
`crates/nico-doctor/src/verdicts/`:

```rust
pub struct AxisSummary {
    pub axis: &'static str,        // layer name, joins back to the source
    pub status: Status,            // Ok / Warn / Fail / Unknown
    pub message: String,           // the same one-liner the layer used to render
    pub next_command: Option<String>, // drill-down hint that survives rollups
}
```

Each axis exposes a pure `<axis>_verdict()` function returning
`AxisSummary`. The first instance is `cert_verdict()` in
`verdicts::cert`. Subsequent slices add `isolation_verdict`,
`ib_verdict`, etc.

**Module location.** The verdict helpers live in their own
`verdicts/` module rather than co-located inside each layer module.
Co-locating would re-scatter the same primitive PRD-003 wants to
unify; pulling them together makes the cross-layer convention
discoverable in one place and gives the upcoming `ib_verdict()`
(PRD-004 Slice 2) an obvious home.

**`next_command` field.** Included on the primitive. Downstream
holistic rollups need each axis verdict to carry its own drill-down
hint so the rollup can surface "cert: expired (rotate dpu-agent
client cert)" without reaching back into the per-layer renderer.

**Layer renderer responsibility.** A layer's renderer
(`assemble_checks`) calls its verdict helper, lifts the resulting
`AxisSummary` into a single `Check { kind: Headline }`, and appends
zero-or-more `Check { kind: Detail }` rows that carry layer-specific
raw data the punchy headline elides. JSON output ordering is fixed:
headline first, detail after.

## Alternatives considered

- **Co-locate `*_verdict()` inside each layer module.** Rejected —
  defeats the unification; future readers have to grep across six
  modules to learn the shared shape.
- **Omit `next_command` and let consumers derive it.** Rejected —
  consumers (slices 5 + 6) would have to duplicate the per-axis
  command logic. Carrying it on the primitive keeps the verdict
  self-contained.
- **Make `axis` a free-form `String`.** Rejected — every axis name
  is a compile-time constant equal to the layer's `name()`, so
  `&'static str` matches reality and avoids needless allocation in
  rollup code.

## Consequences

- Every per-DPU layer added in PRD-003 slices 2-5 (`dpu_isolation`,
  `hbn`, `dpu_services`, `dpu_health`) gets migrated to the
  `AxisSummary` shape via its own slice; the `dpu_cert` migration in
  this slice is the template.
- PRD-004 Slice 2's `ib_verdict()` lands directly into `verdicts/`
  with no further design.
- The fleet rollup (PRD-003 slice 6) consumes `Vec<AxisSummary>` per
  DPU instead of reaching into per-layer renderers, which is what
  makes the holistic summary tractable.
