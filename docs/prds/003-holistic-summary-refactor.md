# PRD-003 — Per-DPU + fleet holistic summary refactor

- **Status:** Specced (2026-05-09); awaiting `/to-issues` breakdown.
- **Epic:** #303 (carries `prd-003` label; tracks slice progress).
- **Touches:** ADR candidate (per-axis verdict primitive — created during slice 1 if the abstraction crystallises).
- **Sequenced after:** PRD-002 (refactors against the post-PRD-002 layer surface; doing it before would mean two refactors).
- **Sequenced before:** PRD-004 (IB layer drops into the pattern this PRD establishes).

## Problem

`nico-doctor`'s per-DPU layers each emit their own verdict shape. The
fleet `dpu` rollup hand-rolls counts. As new axes land (`dpu_health`,
`dpu_services` from PRD-002, `infiniband` from PRD-004, eventual
RoCE) the surface grows two ways:

- **Detail and rollup share no abstraction.** Each layer computes its
  own per-DPU verdict locally; the fleet `dpu` rollup re-derives
  similar verdicts from raw fields. Two derivations of the same
  signal can drift.
- **No single answer to "is this DPU OK?"** Operators today read the
  top-of-report layer summary line — `dpu_health: warn`,
  `dpu_cert: ok`, `dpu_isolation: ok`. That works at fleet level but
  per-DPU there's no holistic verdict. As axes proliferate, the
  per-DPU mental model gets harder to hold.

Letting each future layer ad-hoc cross-reference others is the path
that PRD-002's `dpu_health` carve-outs hint at (BGP-typed alerts
stay in `hbn`, not `dpu_health`; IB-typed alerts will carve out
similarly under PRD-004). Without a shared primitive, those
carve-outs accumulate as comments in code rather than a coherent
pattern.

## Personas

- **DPU-fleet operator** running `nico doctor` (no args) — wants the
  fleet rollup to surface all axes consistently with top-N detail.
- **DPU triage operator** running `nico doctor dpu_health <id>` —
  wants a holistic per-DPU summary that includes every per-DPU
  axis as a one-line verdict (with a separate command per axis to
  drill in).

## Goals

- A shared "axis verdict" primitive: each per-DPU axis exports a
  pure function that computes `(Status, message)` for one DPU.
- `dpu_health` becomes the **holistic per-DPU summary** — renders
  one headline per axis using the shared verdicts, plus its own
  agent-health-specific detail (alerts, DHCP staleness,
  agent-version drift) below.
- `dpu` (fleet rollup) becomes the **holistic fleet summary** —
  iterates DPUs, calls the per-axis verdicts, emits one headline
  per axis with rollup count + top-N detail rows linked to the
  drill-down command.
- Per-axis layers (`dpu_cert`, `dpu_isolation`, `hbn`,
  `dpu_services`) keep their drill-down commands; their headline
  becomes the shared verdict. Detail is layer-specific.
- No detail in two places. The shared verdict produces a one-line
  summary; detail lives in exactly one drill-down layer per axis.

## Non-goals

- IB layer (PRD-004 — drops into this pattern).
- Cross-repo coupling: the verdict primitive lives in
  `nico-doctor`; it does not depend on `infra-controller-core`'s
  `api-model` helpers.
- Instance-level lookups for IB config-sync (issue #301 —
  resolved 2026-05-12 via
  `docs/design/ib-config-sync-detection.md`; implementation
  deferred to a future PRD-008 gated on operator demand). Note
  that `hbn`'s instance-network drift rung already does an
  instance lookup — verdicts in this PRD don't add new
  cross-axis instance fetches, but the pattern itself isn't new.
- An ADR up front. If the primitive crystallises into something
  worth documenting separately, write the ADR during/after slice 1.
- Renaming or removing per-DPU drill-down commands. Each axis keeps
  its `nico doctor <axis> <id>` command.

## High-level design

### Verdict primitive

A pure function per axis, in `nico-doctor/src/verdicts/<axis>.rs`:

```rust
pub struct AxisSummary {
    pub axis: &'static str,        // "cert", "isolation", "hbn", ...
    pub status: Status,            // Ok | Warn | Fail | Unknown
    pub message: String,           // one-line summary for the headline
}

pub fn cert_verdict(machine: &MachineRow) -> AxisSummary;
pub fn isolation_verdict(machine: &MachineRow) -> AxisSummary;
pub fn hbn_verdict(machine: &MachineRow) -> AxisSummary;
pub fn services_verdict(machine: &MachineRow) -> AxisSummary;
```

`Status::Skipped` is intentionally absent from `AxisSummary` —
skipping happens at the layer level (capability bundle gates), not
per-axis-per-DPU.

### Per-DPU layer shape (post-refactor)

Each per-DPU drill-down layer is a thin shell over the shared
verdict + axis-specific detail:

```rust
async fn collect(&self, opts: &RunOpts) -> LayerOutcome {
    let machine = fetch_machine_row(...).await?;
    let summary = cert_verdict(&machine);

    let mut checks = vec![
        Check::headline(summary.axis, summary.status, summary.message),
    ];
    checks.extend(cert_specific_detail(&machine));
    LayerOutcome::Checks(checks)
}
```

`hbn` is the most complex case: today it emits multiple headlines
(version-drift, network-config-error, BGP, freshness). Post-refactor
the shared `hbn_verdict` collapses to a single rolled-up headline
("hbn: ok" / "hbn: drift on managed-host" / "hbn: 2 BGP alerts");
the per-signal breakdown moves to detail rows. This is the biggest
behavioural change; sliced separately so it can be reviewed on its
own.

### `dpu_health` becomes holistic per-DPU

Pre-refactor: `dpu_health` reads `dpu_agent_health_report` and
emits headlines for non-BGP alerts + agent-version drift + DHCP
staleness.

Post-refactor: `dpu_health` first emits **one headline per per-DPU
axis** using the shared verdicts:

```
dpu_health <id>
  cert         ok        valid for 89 days
  isolation    ok        not quarantined
  hbn          warn      drift on managed_host_config
  services     ok        4/4 ready
  --- agent-health detail ---
  agent_version  ok      0.42.1 (current)
  alerts         warn    2 active (interface)
  dhcp           warn    aa:bb:cc — 5h since last
```

Axis verdicts are the headline section. Agent-health-specific
content (alerts, DHCP, agent version) becomes the detail section,
unchanged in shape from PRD-002.

### `dpu` becomes holistic fleet

Iterate machines, fold per-axis verdicts into a rollup, emit one
headline per axis with rollup count + top-N detail:

```
dpu
  cert         ok        all DPUs valid
  isolation    fail      1 DPU lost connection
  hbn          warn      3 DPUs with drift
  services     ok        no degraded services
  --- fleet detail ---
  • DPU-x lost connection (>5m) (isolation)
    → nico doctor dpu_isolation x
  • DPU-y drift on managed_host_config (hbn)
    → nico doctor hbn y
```

Existing fleet-specific concerns (`probe-stuck` rollup, etc.) stay
as additional headlines beneath the per-axis ones until they migrate
into their own verdicts.

### Capability gating

The per-axis verdicts assume `forgedb_present` (they read machine
rows). When `forgedb_present == false`, `dpu_health` and `dpu` skip
cleanly (existing PRD-001 behaviour). New axes added later
(`infiniband` / future RoCE) carry their own capability gates;
`dpu_health` and `dpu` consult the gate before calling the verdict
helper.

## UX

### Per-DPU summary headlines

Headline rows in `dpu_health <id>` display as:

```
  ✓  cert         valid for 89 days
  ⚠  hbn          drift on managed_host_config
  ✓  isolation    not quarantined
  ✓  services     4/4 ready
```

Status icon + axis name + verdict message. Drilling deeper happens
via `nico doctor <axis> <id>`; each axis verdict's `next_command`
points at its drill-down.

### Fleet rollup headlines

Headline rows in `dpu` display as:

```
  ⚠  hbn          3 DPUs drifted
  ✗  isolation    1 DPU lost
  ✓  cert         no expiring certs
  ✓  services     no degraded
```

Detail section caps at `FINDINGS_CAP` (5) per current convention;
each detail row carries `next_command` linking to the per-DPU axis
drill-down.

### JSON output

`AxisSummary` serialises naturally. `dpu_health.checks` JSON gains
an explicit ordering: per-axis verdicts first (one Check each
flagged `kind: "headline"` and tagged with the axis name), then
agent-health detail. `dpu.checks` follows the same convention.

## Per-axis migration scope

| Axis | Pre-refactor headline shape | Post-refactor headline shape | Detail in drill-down |
|---|---|---|---|
| `dpu_cert` | one mutually-exclusive verdict | unchanged (already shared verdict shape) | cert dates, threshold context |
| `dpu_isolation` | one mutually-exclusive verdict | unchanged (already shared verdict shape) | quarantine source, last-seen |
| `hbn` | multi-headline (drift / config-error / BGP / freshness) | **single rolled-up verdict** (biggest change) | full breakdown of each signal |
| `dpu_services` | (PRD-002, not yet shipped — refactor in a slice that lands after PRD-002 slice 7) | shared verdict shape from inception | per-service rows |

`dpu_health` and `dpu` are consumers, not axes. They re-render the
verdicts from above + their own non-axis content.

## ADR work

A per-axis verdict primitive is a candidate for an ADR if the
abstraction proves load-bearing during slice 1. Defer the decision
to that slice — write ADR-NNNN if the type signature, helper
location, or cross-layer rules need a record beyond what this PRD
captures.

## Domain language to add (CONTEXT.md)

- **Axis verdict** — per-DPU one-line summary `(status, message)`
  computed by a pure function on the machine row, shared between a
  drill-down layer (as its headline) and the holistic summaries
  (`dpu_health` per-DPU and `dpu` fleet).
- **Holistic summary layer** — `dpu_health` per-DPU, `dpu` fleet.
  Render axis verdicts produced elsewhere; do not own per-axis
  detail.

CONTEXT.md updates fold into the PR(s) that introduce them; no
separate cleanup ticket.

## Implementation tracking

The slice breakdown lives in the PRD-003 epic as a tasklist.
Sub-issues are created via `/to-issues` against the epic, all
carrying the `prd-003` label and `Parent: #<epic>` per the
conventions in `docs/agents/issue-tracker.md`.

## Open questions

- Whether the verdict primitive lives in `nico-doctor/src/verdicts/`
  (proposed) or co-located inside each layer's module. Resolve in
  slice 1.
- Whether `AxisSummary` carries an optional `next_command` field
  (so `dpu_health` and `dpu` get the drill-down link for free) or
  the consumers add it. Resolve in slice 1.
- Whether the `hbn` collapse from N headlines to 1 is acceptable or
  if some operators rely on the per-signal headline layout. Decide
  after slice exposing the change ships behind the verdict primitive.

## Related

- PRD-001 (#245) — capability bundle (`forgedb_present`); future axes
  add their own capability flags consumed by the holistic layers.
- PRD-002 (#253) — establishes the post-rewrite layer surface this
  PRD refactors against.
- PRD-004 — infiniband layer; drops into this pattern as the first
  new axis after the refactor lands.
- Issue #301 — IB config-sync detection design spike. Resolved
  2026-05-12 via `docs/design/ib-config-sync-detection.md`:
  config-sync drift will plug into this primitive as a new rung
  in `ib_verdict()` rather than as a separate axis layer when
  PRD-008 lands.
