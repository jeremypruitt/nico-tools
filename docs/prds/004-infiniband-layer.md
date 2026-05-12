# PRD-004 — `infiniband` layer: per-DPU IB drill-down

- **Status:** Specced (2026-05-09); awaiting `/to-issues` breakdown.
- **Epic:** #304 (carries `prd-004` label; tracks slice progress).
- **Touches:** ADR-0013 (boot probe — adds `detect_infiniband_present`
  step alongside `detect_deployment_type`); PRD-001 capability bundle
  (adds `infiniband_present` flag); PRD-002 `dpu_health` carve-out
  (excludes IB-typed alerts); CONTEXT.md (new `infiniband` layer entry,
  `infiniband_present` capability vocab entry).
- **Sequenced after:** PRD-002 (uses the post-rewrite layer surface);
  PRD-003 (drops into the holistic verdict primitive).
- **Spawned from:** Issue #264 (IB verdict-shape design spike under
  PRD-002). Grew beyond a PRD-002 amendment after the verdict-shape
  decision triggered a wider d-broad refactor (PRD-003).

## Problem

NICo IB clusters today have no per-port doctor visibility. PRD-002
named `infiniband` as in scope but explicitly deferred verdict shape.
The verdict-shape grilling concluded:

- IB belongs as a per-DPU drill-down layer (parallel to `hbn` /
  `dpu_cert` / `dpu_isolation`).
- Fleet-IB visibility is best surfaced as a headline in the holistic
  `dpu` rollup (PRD-003), not as a parallel fleet layer.
- Some clusters have no IB at all (RoCE-only, ethernet-only). The
  layer must skip cleanly when IB hardware isn't present.
- IB-typed HealthReport alerts (`IbPortDown`, `IbCleanupPending`)
  currently flow through `dpu_health` (per PRD-002). Those move to
  this layer; PRD-002's `dpu_health` carve-out grows to exclude
  IB-typed alerts (parallel to BGP-typed alerts staying in `hbn`).

IB config-sync detection — comparing observed pkeys/partition_ids
against the expected `InstanceInfinibandConfig` — is out of scope
here to keep PRD-004 observation-only and shippable. Design
decisions resolved in `docs/design/ib-config-sync-detection.md`
(2026-05-12, closes spike #301): config-drift rung lands inside
`ib_verdict()` (per-layer, not a new axis layer); comparison
replicated locally; implementation deferred to a future PRD-008
gated on operator demand.

## Personas

- **GPU/IB-cluster operator** running NICo on H100/GB300/etc. IB
  fabric is typically more operationally critical than HBN. Wants
  per-port visibility from `nico doctor infiniband <dpu-id>` and
  fleet-IB rollup from `nico doctor`.
- **RoCE / ethernet-only operator** — must not see IB clutter or
  errors. The layer skips cleanly with a clear reason in the banner.

## Goals

- New `infiniband_present` capability flag in the deployment-type
  capability bundle (parallel to `forgedb_present`). Detected at
  boot via SQL on `machines.inventory->'infiniband_interfaces'`.
  Banner reflects the resolved value.
- New per-DPU `infiniband` layer (`nico doctor infiniband <dpu-id>`)
  surfacing per-port observation detail.
- Shared `ib_verdict()` helper (PRD-003 pattern) consumed by:
  - `infiniband` layer's headline.
  - `dpu_health` per-DPU holistic summary (one IB headline among
    the per-axis row).
  - `dpu` fleet rollup (one IB-fleet headline + top-N detail).
- Move IB-typed HealthReport alerts (`IbPortDown`,
  `IbCleanupPending`) from `dpu_health` to `infiniband`.

## Non-goals

- Config-sync against `InstanceInfinibandConfig` — deferred to
  issue #301 / potential PRD-005.
- RoCEv2 — future epic. The capability bundle will gain a parallel
  `rocev2_present` flag at that point; this PRD does not anticipate
  RoCE specifics.
- Per-port history / timeseries. Observation is point-in-time.
- A separate fleet-IB layer. Fleet rollup lives in `dpu` (PRD-003).
- Cross-repo coupling: do not depend on `infra-controller-core`'s
  `api-model::ib_config_synced`. Replicate any comparison locally
  (relevant only when config-sync lands; documented here for
  forward-reference).

## Producer-side reality

Already surveyed in PRD-002's Validation section. Specific to IB:

| JSON path on `machines` | Rust type | Defined at |
|---|---|---|
| `infiniband_status_observation` | `MachineInfinibandStatusObservation` | `api-model/src/machine/infiniband.rs:30-38` |

Schema (proto and Rust mirror):

```rust
pub struct MachineInfinibandStatusObservation {
    pub ib_interfaces: Vec<MachineIbInterfaceStatusObservation>,
    pub observed_at: DateTime<Utc>,
}

pub struct MachineIbInterfaceStatusObservation {
    pub guid: String,
    pub lid: u16,                                          // 0xffff = port not Active
    pub fabric_id: String,                                 // "" = never seen on any fabric
    pub associated_pkeys: Option<HashSet<PartitionKey>>,   // None = unobservable from UFM
    pub associated_partition_ids: Option<HashSet<IBPartitionId>>,
}
```

Migration: `20241128161015_machine_infiniband_status_observation.sql`
adds the JSONB column, nullable.

Producer: `IbFabricMonitor`
(`infra-controller-core/crates/ib-fabric/src/lib.rs:788-1078`)
queries UFM, populates the observation, and emits `IbPortDown` /
`IbCleanupPending` alerts into `dpu_agent_health_report`.

## High-level design

### IB-presence detection (capability bundle)

Add `infiniband_present` to the deployment-type capability bundle.
Shape mirrors `forgedb_present` exactly:

```rust
impl DeploymentType {
    pub fn forgedb_present(&self) -> bool;
    pub fn infiniband_present(&self) -> Option<bool>; // None = unknown
}
```

`Option<bool>` semantics:
- `Some(true)` — at least one machine has non-empty
  `inventory.infiniband_interfaces`. IB-dependent layers run.
- `Some(false)` — forgedb present, no IB hardware in inventory.
  IB-dependent layers `Skipped { reason: "no IB hardware in cluster" }`.
- `None` — unknown (forgedb absent, can't query). IB-dependent
  layers `Skipped { reason: "IB-presence unknown — forgedb not reachable" }`.

Detection runs as a new boot-probe step `detect_infiniband_present`
in the `validating` section, after `detect_deployment_type` and
gated on `forgedb_present == true`. SQL:

```sql
SELECT EXISTS (
    SELECT 1 FROM machines
    WHERE jsonb_array_length(inventory->'infiniband_interfaces') > 0
);
```

`Force` deployment-type returns `Some(true)` (assume IB present;
fail naturally if it isn't — same posture as `forgedb_present` in
force mode).

Banner gains a top-line indicator: `· ib: present|absent|unknown`.

### `infiniband` layer (per-DPU drill-down)

Command: `nico doctor infiniband <dpu-id>`.

Skip behaviour: when `infiniband_present()` is anything other than
`Some(true)`, return `LayerOutcome::Skipped { reason }`.

Verdict shape: **headline + per-port detail rows.**

**Headlines:**

- `infiniband` axis verdict — produced by the shared `ib_verdict`
  helper (see below). Same headline as the `dpu_health` per-axis
  row and the `dpu` fleet IB rollup.
- `ib-observation-fresh` — separate freshness check on
  `observation.observed_at`. Threshold shared with the DHCP
  staleness threshold (still an open item; see PRD-002 open items
  + open question below).

**Detail rows (one per port):**

| Condition | Status | Detail value |
|---|---|---|
| `fabric_id.is_empty()` | Fail | `port-{guid}: never observed on any fabric` |
| `lid == 0xffff` | Fail | `port-{guid}: not active (lid=0xffff)` |
| `associated_pkeys.is_none()` | Warn | `port-{guid}: unobservable from UFM` |
| otherwise | Ok | `port-{guid}: active · fabric={id} · pkeys={n}` |

Precedence: empty `fabric_id` is more severe than `lid==0xffff`
(never connected vs currently down). UFM unobservable is operationally
a UFM issue, not a DPU issue → Warn, not Fail.

Port identifier: full `guid` (matches `dpu_health`'s full-MAC
convention; operators grep the same GUID against UFM).

**HealthReport IB-typed alerts:**

`IbPortDown` / `IbCleanupPending` from `dpu_agent_health_report`
become detail rows in this layer when present. Two classes:

- **Per-port alert (`IbPortDown` with affected GUIDs)** — if the
  observation shows the same ports already down, no extra row
  (don't duplicate). If the alert exists but observation shows all
  ports active (rare divergence), emit a Warn detail row
  `agent/observation disagree on port {guid}`.
- **Fabric-level alert (`IbCleanupPending`)** — no observation
  parallel; surface as a detail row `cleanup pending: {message}`.

Future-proof: any HealthReport probe id starting with `Ib` flows
through this layer.

### `ib_verdict()` helper (PRD-003 pattern)

Pure function in `nico-doctor/src/verdicts/ib.rs`:

```rust
pub fn ib_verdict(machine: &MachineRow) -> AxisSummary {
    AxisSummary { axis: "infiniband", status, message }
}
```

Status precedence (same as per-port, rolled up):
- any port `fabric_id.is_empty()` → Fail (`"port {guid} never on fabric"`)
- any port `lid == 0xffff` → Fail (`"{n}/{total} ports down"`)
- any port `pkeys.is_none()` → Warn (`"{n} ports unobservable from UFM"`)
- observation stale → Warn (`"observation {age} old"`)
- IB-typed HealthReport alert present → Warn (`"agent alert: {id}"`)
- else → Ok (`"{total}/{total} ports active"`)

### `dpu_health` carve-out amendment (PRD-002)

PRD-002's `dpu_health` description currently reads "All alert
categories from the agent's report **except** BGP-typed and the
config-error category." This PRD amends the carve-out to:

> All alert categories from the agent's report **except** BGP-typed,
> the config-error category, **and IB-typed (probe ID starts with
> `Ib`)**.

The IB-typed alerts move to `infiniband`. The carve-out lands in
the slice that introduces the IB layer; it is recorded as an
amendment in PRD-002's Decisions table at that point.

### `dpu` fleet rollup IB headline (PRD-003)

Once PRD-003 lands, `dpu` iterates machines, calls `ib_verdict` per
machine (gated on `infiniband_present`), folds into the fleet
rollup, and emits an `infiniband` headline + top-N detail rows
linked to `nico doctor infiniband <dpu-id>`. No separate fleet-IB
layer.

## UX

### Boot banner addition

```
  ◐ booting nico  ·  reach: …  ·  type: full (auto)  ·  ib: present (auto)

    validating
      ✓  credentials
      ✓  detect deployment-type: full
      ✓  detect infiniband-present: yes              ← NEW step
      ✓  namespace 'forge-system' exists
      ✓  list-pods permission
```

When `infiniband_present == Some(false)`: `· ib: absent (auto)`.
When `None` (forgedb unreachable): `· ib: unknown`.

### Per-DPU drill-down example

```
infiniband DPU-abc
  ✓  infiniband              4/4 ports active
  ✓  ib-observation-fresh    23s ago

  ✓  port-0xfeed1234abcd···  active · fabric=fab-1 · pkeys=2
  ✓  port-0xfeed1234abce···  active · fabric=fab-1 · pkeys=2
  ✓  port-0xfeed1234abcf···  active · fabric=fab-2 · pkeys=1
  ✓  port-0xfeed1234abd0···  active · fabric=fab-2 · pkeys=1
```

Failure example:

```
infiniband DPU-xyz
  ✗  infiniband              1/4 ports down, 1 unobservable
  ✓  ib-observation-fresh    1m12s ago

  ✓  port-0xfeed1234aaa1···  active · fabric=fab-1 · pkeys=2
  ✗  port-0xfeed1234aaa2···  not active (lid=0xffff)
  ⚠  port-0xfeed1234aaa3···  unobservable from UFM
  ✓  port-0xfeed1234aaa4···  active · fabric=fab-2 · pkeys=1

  ⚠  agent alert              IbPortDown
```

### JSON output

`AxisSummary` for IB serialises like any other axis. Detail rows
carry `kind: "detail"` per the existing convention. No new
top-level fields.

## ADR work

Amend ADR-0013 (boot probe) to document the new
`detect_infiniband_present` step in the `validating` section,
parallel to the `detect_deployment_type` amendment from PRD-001.
Failure mode: SQL error → `infiniband_present = None` (treat as
unknown, layer skips).

## Domain language to add (CONTEXT.md)

- **Infiniband layer** — per-DPU drill-down on
  `machines.infiniband_status_observation`. Surfaces per-port
  active/down/unobservable state plus IB-typed HealthReport alerts.
  Skipped when `infiniband_present != Some(true)`.
- **`infiniband_present` capability** — second feature gate in the
  deployment-type capability bundle (after `forgedb_present`).
  `Option<bool>` to distinguish `absent` from `unknown`. Detected
  at boot via SQL on hardware inventory; gated on `forgedb_present`.
- **IB-typed alert** — HealthReport probe whose id starts with
  `Ib` (today: `IbPortDown`, `IbCleanupPending`). Surfaces in the
  `infiniband` layer; carved out from `dpu_health` parallel to
  BGP-typed alerts being carved out in favour of `hbn`.

CONTEXT.md updates fold into the slice that introduces them.

## Open questions

- **DHCP staleness threshold** — open item carried over from
  PRD-002. The IB freshness threshold should share whatever value
  lands there (both come from the same agent observation cadence).
- **Operator override flag** — should `--infiniband-present=true`
  / `--no-infiniband` exist, parallel to `--deployment-type=force`?
  Defer until first slice ships and a real need surfaces.
- **`pf_guid`** — the proto carries `pf_guid` (hardware device GUID)
  in addition to `guid` (per-VF GUID). The Rust observation struct
  drops `pf_guid`. Reconsider only if operators report needing
  hardware-vs-VF distinction in the per-port row.
- **`hbn` collapse** — PRD-003 collapses `hbn`'s N-headline shape
  to a single rolled-up headline. If that decision reverses for
  `hbn`, this PRD's IB layer should follow whatever shape `hbn`
  ends up with for consistency.

## Implementation tracking

The slice breakdown lives in the PRD-004 epic as a tasklist.
Sub-issues are created via `/to-issues` against the epic, all
carrying the `prd-004` label and `Parent: #<epic>` per the
conventions in `docs/agents/issue-tracker.md`.

## Related

- Issue #264 — IB verdict-shape design spike (PRD-002 child).
  PRD-002 amendment records the field study and points the
  verdict-shape decision at this PRD.
- Issue #265 — original IB layer implementation slice under PRD-002.
  Superseded by this PRD's epic; left for triage to handle.
- Issue #301 — IB config-sync detection design spike. Resolved
  2026-05-12 by `docs/design/ib-config-sync-detection.md`:
  decisions captured, implementation deferred to a future PRD-008
  gated on operator demand.
- PRD-001 — capability bundle pattern (`forgedb_present`). This PRD
  adds `infiniband_present` as the second flag.
- PRD-002 — DPU layer rewrite. Defers IB verdict shape here; this
  PRD amends the `dpu_health` carve-out for IB-typed alerts.
- PRD-003 — holistic summary refactor. This PRD's `ib_verdict`
  helper is the first new axis added under the PRD-003 pattern.
- ADR-0013 — boot probe; amended for the new
  `detect_infiniband_present` step.
