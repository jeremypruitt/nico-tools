# PRD-002 — DPU layer rewrite: schema realignment + new axes

- **Status:** Specced (2026-05-09; amended same day to fold in the fleet-wide `dpu` layer after PR #241 landed); awaiting `/to-issues` breakdown.
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

### `infiniband` (NEW)

Consumes `machines.infiniband_status_observation` (separate JSON
column, already populated by core via
`update_infiniband_status_observation`). Decision 2026-05-09: in scope
for nico-doctor — for GPU/IB clusters, IB fabric is often more
operationally critical than HBN.

Schema needs a brief field-by-field study at implementation time
(parallel to the network observation). Verdict shape (per-DPU
rollup, per-port detail, both?) deferred to implementation.

## Decisions captured

| Decision | Choice | Date |
|---|---|---|
| `network_config_error` placement | Top-line headline on `hbn` | 2026-05-09 |
| Non-BGP alerts | New `dpu_health` layer | 2026-05-09 |
| Extension services | New `dpu_services` layer | 2026-05-09 |
| Agent-version drift | Surface in `dpu_health` with verdict | 2026-05-09 |
| IB fabric | New `infiniband` layer | 2026-05-09 |
| DHCP staleness | New check inside `dpu_health` | 2026-05-09 |
| `dpu_health` output grouping | By category | 2026-05-09 |

## Open items (resolve during implementation)

- DHCP staleness threshold default (4h vs 24h vs operator-configurable)
- IB fabric verdict shape (per-port, per-DPU rollup, both)
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
- CONTEXT.md `dpu` layer entry — fleet-wide rollup; data-source
  description updated alongside this PRD.
