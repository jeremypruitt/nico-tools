# PRD-002 — DPU layer rewrite: schema realignment + new axes

- **Status:** Specced (2026-05-09; amended same day to fold in the fleet-wide `dpu` layer after PR #241 landed; amended again 2026-05-09 to defer the `infiniband` layer to PRD-004 after the verdict-shape grilling triggered a wider holistic-summary scope expansion captured in PRD-003); awaiting `/to-issues` breakdown.
- **Epic:** #253 (carries `prd-002` label; tracks slice progress).
- **Touches:** `CONTEXT.md` (`dpu` layer entry; new layer entries for `dpu_health`, `dpu_services`, `infiniband`).
- **Pre-existing bugs this fixes:** #213, #147.
- **Upstream dep:** #237.

PRD covering the rewrite of nico-doctor's per-DPU drill-down layers
against the actual core schema, plus three new axes (`dpu_health`,
`dpu_services`, `infiniband`).

## Problem

The per-DPU drill-down layers (`hbn`, `dpu_cert`, `dpu_isolation`),
the fleet-wide rollup layer (`dpu`, shipped 2026-05-08 in PR #241
closing #214), and the correlate-side drift trail
(`nico-correlate/src/hbn_drift.rs`) currently query SQL tables that
do not exist in either `infra-controller-core` or `infra-controller-rest`:

- `dpu_network_status` — referenced in `dpu.rs`, `hbn.rs`, `dpu_cert.rs`,
  `dpu_isolation.rs` (nico-doctor) and `hbn_drift.rs` (nico-correlate);
  defined nowhere upstream
- `dpu_desired_network_config` — referenced in `dpu.rs`, `hbn.rs`
  (nico-doctor) and `hbn_drift.rs` (nico-correlate); defined nowhere upstream
- `dpu_network_status_history` — 1 reference in `hbn_drift.rs`;
  defined nowhere upstream
- `health_report` as a relational table with `alert_name` /
  `in_alert_since` columns — defined nowhere upstream;
  `HealthReport` exists only as a JSON-serialized struct stored in
  `machines.dpu_agent_health_report`

Each layer wraps the query in a defensive
`SELECT EXISTS (... table_name = 'dpu_network_status')` probe and
returns empty when the probe fails — which it always does. The
graceful-degradation path is the only path that ever runs against any
real cluster. Operators get "no DPUs" output even when the cluster has
thousands.

## Validation (2026-05-08)

- Searched all 334 SQL files in `infra-controller-core` and all 41 in
  `infra-controller-rest`. Zero hits for `dpu_network_status` or
  `dpu_desired_network_config`.
- `mock-core` (`infra-controller-rest/site-agent/cmd/mock-core/main.go`)
  is a 31-line gRPC stub with no DB schema — different deployment
  modes don't change the answer.
- Producer write path: `record_dpu_network_status` gRPC handler
  (`infra-controller-core/crates/api/src/handlers/dpu.rs:803`)
  receives `DpuNetworkStatus` proto and writes to JSON columns on the
  `machines` row, not to dedicated tables.

## Goals

- Rewrite existing per-DPU drill-down layers against producer-side reality.
- Rewrite the fleet-wide `dpu` rollup layer (PR #241) against the same
  producer-side reality. Same missing-table bug; same JSON columns; was
  out-of-scope in the original draft because the PRD was written before
  PR #241 merged.
- Add three new per-DPU layers surfacing currently-invisible signal:
  `dpu_health`, `dpu_services`, `infiniband`.
- Preserve read-only / non-blocking discipline (no remediation, fast exit).

## Non-goals

- Tier 3 axes — captured in a follow-up PRD issue.
- Producing migrations on the core side. Adapt to what exists.

## Producer-side reality

DPUs are stored as machine rows in the `machines` table, with JSON
columns:

| Column | Rust type | Defined at |
|---|---|---|
| `network_status_observation` | `MachineNetworkStatusObservation` | `api-model/src/machine/network.rs:38` |
| `dpu_agent_health_report` | `health_report::HealthReport` | written via `update_dpu_agent_health_report` |
| `network_config` | `ManagedHostNetworkConfig` | `api-model/src/machine/network.rs:250` |
| `inventory` | `MachineInventory.components: Vec<{name, version, url}>` | `api-model/src/hardware_info.rs:955` |
| `infiniband_status_observation` | (separate observation feed) | written via `update_infiniband_status_observation` |

"Desired vs status" is two JSON columns on the same row, not two
tables joined on `dpu_id`.

## Field mapping (existing layer queries → producer reality)

| nico-doctor query expects | Where it actually lives |
|---|---|
| `s.dpu_id` | `machines.id` |
| `s.last_seen_at` | `network_status_observation->>'observed_at'` |
| `s.client_certificate_expiry_unix_epoch_secs` | `network_status_observation->>'client_certificate_expiry'` (i64) |
| `s.applied_managed_host_config_version` | `network_status_observation->>'network_config_version'` |
| `s.applied_instance_network_config_version` | `network_status_observation->'instance_network_observation'->>'config_version'` |
| `d.managed_host_config_version` (desired) | `network_config->>'managed_host_config_version'` (same row) |
| `d.instance_network_config_version` (desired) | `network_config` JSON — exact path TBD at implementation time |
| `s.quarantine_state` | `network_config->'quarantine_state'` — **desired-side** semantics |
| `s.hbn_version` | `inventory->'components'` array, filter `name='hbn'`, take `version` |
| `s.bgp_alerts` | `dpu_agent_health_report` JSON, filter alerts by category |
| `s.container_running` | **No producer.** Drop field. |
| `dpu_network_status_history` | **No producer.** Drop drift-since timestamp. |
| `health_report` table with `alert_name`/`in_alert_since` columns | **Wrong shape.** Read alerts from `dpu_agent_health_report` JSON instead. |

## Layer changes

### `dpu` (existing — fleet-wide rewrite)

Just-shipped fleet-wide rollup (PR #241, closing #214) hits the same
missing-table problem as the per-DPU layers. Rewrite against:

- `network_status_observation` JSON for applied state per DPU
- `network_config` JSON for desired state per DPU
- `dpu_agent_health_report` JSON for alert summary

`DpuSnapshot` shape stays compatible. Drop `container_running` field
(no producer). Update `quarantine_state` semantics to "quarantine
requested" (matches `dpu_isolation` adjust — desired-side, not
observed). BGP-typed alerts continue to surface in the rollup;
non-BGP categories are exposed via `dpu_health` for drill-down.

### `dpu_cert` (existing — clean rewrite)

Reads `network_status_observation->>'client_certificate_expiry'` (i64
unix epoch secs). No field drops. No semantic changes. Verdict logic
unchanged.

### `hbn` (existing — narrowed)

- Drop field `container_running` (no producer).
- **New top-line:** `network_config_error` from
  `network_status_observation`, when present, becomes the verdict
  headline. Drift / version info shown beneath. Decision 2026-05-09:
  an explicit error from the agent is more actionable than "versions
  disagree" — the error tells the operator *why*.
- BGP-typed alerts continue to surface here (other categories move to
  the new `dpu_health` layer).
- Drift comparison uses applied (`network_status_observation`) vs
  desired (`network_config`) on the same machine row.

### `dpu_isolation` (existing — semantic adjust)

`quarantine_state` lives on `network_config` (the **desired** side),
not the observation side. Verdict copy must say "quarantine
requested" rather than "quarantine active" — operator intent, not
observed effect. `last_seen_at` lookup stays for the lost-connection
verdict.

### `hbn_drift` in `nico-correlate` (existing — narrowed)

Drop capability: "first observed drift at" timestamp (queried the
non-existent `dpu_network_status_history`). Drift detection remains.
Drift output reads "drift exists" rather than "drift exists since T".

### `dpu_health` (NEW)

Consumes `machines.dpu_agent_health_report` (JSON-serialized
`HealthReport`).

Surfaces:
- All alert categories from the agent's report **except** BGP-typed
  (those stay in `hbn`) and the config-error category (covered by
  `hbn`'s top-line). Agent staleness, cert (cross-references
  `dpu_cert`), interface, fabric, etc.
- Agent-version drift: emit a check when `network_status_observation->>'agent_version'`
  is not the controller's current version, including
  `agent_version_superseded_at` in the verdict
  ("agent version X, superseded since Y"). Decision 2026-05-09:
  fleet-wide agent-version drift is a real ops pain point.
- DHCP staleness: per-interface check if
  `last_dhcp_requests[i].timestamp` is older than threshold.
  Storage column TBD at implementation (handler iterates the proto
  field but exact persistence column to be traced).

Rendered output groups alerts by category (decision 2026-05-09).
Reconsider if operator feedback prefers flat-with-category-column.

### `dpu_services` (NEW)

Consumes
`network_status_observation->'extension_service_observation'->'extension_service_statuses'`.

Per-service rows: `service_name`, `version`, `overall_state`,
`message`, `removed` flag. Decision 2026-05-09: this is structured
inventory, not an alert stream — own layer preserves shape.

Verdict shape:
- Any service in non-`Ready` state for longer than threshold → `warn`.
- Any service with `removed` flag → info-only line.
- Stale `extension_service_observation->>'observed_at'` → `warn`.

### `infiniband` (NEW — deferred to PRD-004)

Decision 2026-05-09 (verdict-shape grill, issue #264): the
`infiniband` layer scope grew beyond what fits as a PRD-002 slice.
The verdict-shape grilling reached three conclusions that warrant
their own PRD epic:

1. The layer should produce both per-DPU drill-down detail AND a
   fleet rollup. The fleet rollup belongs as a headline inside the
   existing `dpu` layer (not as a parallel fleet-IB layer), which
   pulls in a holistic-summary refactor across all per-DPU axes.
2. The detail-vs-rollup separation needs a shared verdict primitive
   (one source of truth per axis, summary referenced everywhere)
   to avoid two-source divergence as new axes (IB, future RoCE)
   land. That primitive is broader than IB.
3. IB-presence detection needs a new capability flag
   (`infiniband_present`) parallel to `forgedb_present` from PRD-001.
   Some clusters have no IB at all (RoCE / ethernet-only) and the
   layer must skip cleanly.

The shared-verdict refactor lands as **PRD-003** (per-DPU + fleet
holistic summary refactor). The IB layer drops into the PRD-003
pattern as **PRD-004** (`infiniband` layer). PRD-004 also amends
`dpu_health` below (carve out IB-typed alerts in addition to
BGP-typed and config-error).

Field-by-field study of `machines.infiniband_status_observation`
(per issue #264 acceptance criteria) is recorded inline here so
PRD-002 captures the producer-side reality consistent with the
other layers. The verdict-shape decision is recorded in PRD-004.

#### Field study — `MachineInfinibandStatusObservation`

Stored as JSONB on `machines.infiniband_status_observation`
(nullable; populated by the IB fabric monitor on the core side).
Defined at `infra-controller-core/crates/api-model/src/machine/infiniband.rs:30-61`.

```rust
pub struct MachineInfinibandStatusObservation {
    pub ib_interfaces: Vec<MachineIbInterfaceStatusObservation>,
    pub observed_at: DateTime<Utc>,
}

pub struct MachineIbInterfaceStatusObservation {
    pub guid: String,
    pub lid: u16,                                          // 0xffff = port not Active
    pub fabric_id: String,                                 // empty = never seen on any fabric
    pub associated_pkeys: Option<HashSet<PartitionKey>>,   // None = unobservable from UFM
    pub associated_partition_ids: Option<HashSet<IBPartitionId>>,
}
```

Per-port signal interpretation:

| Field | Healthy | Unhealthy reading |
|---|---|---|
| `lid` | non-`0xffff` | `0xffff` ⇒ port not Active |
| `fabric_id` | non-empty fabric id string | empty ⇒ GUID never observed on any fabric |
| `associated_pkeys` | `Some(set)` | `None` ⇒ UFM reports unobservable |
| `associated_partition_ids` | `Some(set)`; cardinality may differ from `pkeys` if a pkey doesn't map to a partition | `None` ⇒ same as above |
| `observed_at` (parent) | recent | stale (threshold shared with DHCP staleness — open item below) |

Producer: `IbFabricMonitor`
(`infra-controller-core/crates/ib-fabric/src/lib.rs:788-1078`)
queries UFM, populates the observation, and emits IB-typed alerts
(`IbPortDown`, `IbCleanupPending`) in `dpu_agent_health_report`.

Multiple ports per DPU is a first-class case (`ib_interfaces` is
`Vec<...>`). There is a parallel "expected" config in core
(`InstanceInfinibandConfig`, instance-level) and core's
`ib_config_synced()` helper compares them — surfacing that
comparison is **out of scope** for both PRD-004 and this PRD.
Issue #301 resolved 2026-05-12 via
`docs/design/ib-config-sync-detection.md`: lives inside
`ib_verdict()` (per-layer); compared locally (no cross-repo dep);
implementation deferred to a future PRD-008 gated on operator
demand.

## Decisions captured

| Decision | Choice | Date |
|---|---|---|
| `network_config_error` placement | Top-line headline on `hbn` | 2026-05-09 |
| Non-BGP alerts | New `dpu_health` layer | 2026-05-09 |
| Extension services | New `dpu_services` layer | 2026-05-09 |
| Agent-version drift | Surface in `dpu_health` with verdict | 2026-05-09 |
| IB fabric | New `infiniband` layer (deferred to PRD-004; verdict shape recorded there) | 2026-05-09 |
| DHCP staleness | New check inside `dpu_health` | 2026-05-09 |
| `dpu_health` output grouping | By category | 2026-05-09 |

## Open items (resolve during implementation)

- DHCP staleness threshold default (4h vs 24h vs operator-configurable).
  Shared with PRD-004's `ib-observation-fresh` threshold; resolve here
  and PRD-004 inherits.
- ~~IB fabric verdict shape (per-port, per-DPU rollup, both)~~ —
  resolved 2026-05-09 by deferral to PRD-004 (verdict shape
  recorded there).
- Extension-service "non-Ready for too long" threshold
- Exact storage column for `last_dhcp_requests` (handler iterates;
  trace the persistence path during implementation)
- Exact JSON path for desired `instance_network_config_version`
  inside `network_config`

## Out of scope (Tier 3 follow-up — separate PRD issue)

- Interface bring-up state from `interfaces[*].mac_address` /
  `addresses` empty-list signals
- DPU-instance assignment surface from
  `network_status_observation->>'instance_id'`
- Routing IP diagnostics from `network_config->>'loopback_ip'` /
  `secondary_overlay_vtep_ip`
- **OS-level config drift as a full new axis** using
  `instance_config_version` (distinct from
  `instance_network_config_version` and extension-service drift).
  Called out as deserving its own PRD.
- **`vpc_vni` mismatch detection and BGP-EVPN type-2 route presence**
  (issue #216). Verified 2026-05-09 against `infra-controller-core`
  (`crates/rpc/proto/forge.proto:4592` `DpuNetworkStatus`,
  `crates/agent/src/health/{bgp.rs,probe_ids.rs}`,
  `crates/api-model/src/machine/network.rs:38` /
  `crates/api-db/migrations/20230308160000_machine_network_status_observation.sql`):
  neither configured-vs-expected `vpc_vni` nor EVPN type-2 route presence
  is exposed by `RecordDpuNetworkStatus`, the `network_status_observation`
  JSONB column, or the `HealthReport` probe vocabulary. The agent runs
  `vtysh -c 'show bgp summary json'` for **session state only** —
  `pfx_rcd`/`pfx_snt` are read but explicitly skipped
  (`bgp.rs:362-369`), and `show bgp l2vpn evpn` route-table contents are
  not parsed anywhere. Probe IDs are session-typed
  (`BgpPeeringTor`, `BgpPeeringRouteServer`, `UnexpectedBgpPeer`,
  `BgpStats`, `BgpDaemonEnabled`). Surfacing these would require an
  upstream change to NICo before nico-doctor could query them; remains
  operator-side debug (`crictl exec … vtysh -c 'show bgp l2vpn evpn
  summary'`) until then. See `docs/playbooks/stuck_objects/waiting_for_network_config.md`
  in core for the operator path.

## Testing strategy

- Existing layers have unit tests against mock clients implementing
  the layer's data trait. Rewrite preserves the trait shape; new
  client implementations target JSON-column queries. Mock tests
  remain valid.
- Integration tests need a live core DB. Mode 1 (core-only kind
  quick-dev) reproduces the JSON columns. Mode 2 (rest + mock-core)
  does not — `mock-core` is a stub. Document this in the PR description.

## Related

- ADR-0010 — `nico ops` dashboard consumes the same layer outputs.
- Issue #205 — `nico doctor hbn <dpu-id>`. Implementation hits missing tables.
- Issue #206 — `nico doctor dpu-cert <dpu-id>`. Recently merged (PR #233).
  Implementation hits missing tables.
- Issue #207 — `nico doctor dpu-isolation <machine-id>`. Implementation
  hits missing tables.
- Issue #214 / PR #241 — `nico doctor` fleet-wide `dpu` layer. Shipped
  2026-05-08 with the same missing-table bug; in scope of this PRD per
  the 2026-05-09 amendment.
- Issue #264 — IB verdict-shape design spike. Resolved by this PRD's
  field study + deferral to PRD-004.
- PRD-003 — per-DPU + fleet holistic summary refactor. Refactors the
  layer surface PRD-002 establishes; sequenced after this PRD.
- PRD-004 — `infiniband` layer. Drops into PRD-003's pattern; amends
  this PRD's `dpu_health` carve-out to also exclude IB-typed alerts.
- CONTEXT.md `dpu` layer entry — fleet-wide rollup; data-source
  description updated alongside this PRD.
