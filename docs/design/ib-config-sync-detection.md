# IB config-sync detection — design note

- **Status:** Decisions resolved (2026-05-12). Implementation deferred — gated on operator demand for config-drift visibility in the live `infiniband` layer.
- **Resolves:** Issue #301 (design spike).
- **Touches:** PRD-004 (referenced from the non-goals + related sections); PRD-002 (verdict-shape lineage).

## Problem

NICo records observed IB state per port in `machines.infiniband_status_observation` (`associated_pkeys`, `associated_partition_ids`, `fabric_id`, `lid`, `port_state`). Core also carries the **desired** IB config per instance in `InstanceInfinibandConfig` and a helper `ib_config_synced(observation, InstanceInfinibandConfig) -> Result<(), IbConfigNotSyncedReason>` returning one of `{MissingObservation, PortStateUnobservable, ConfigurationMismatch}` (`infra-controller-core/crates/api-model/src/machine/infiniband.rs:131-248`).

PRD-004's `infiniband` layer is observation-only — it surfaces port up/down + UFM observability + observation freshness, but never reaches for the desired-side instance config. The spike question is whether/how `nico doctor` should surface drift between observed pkeys/partition_ids and the instance's expected `InstanceInfinibandConfig`.

#301's original framing called instance-level lookup "a new pattern." That framing is **stale**: the `hbn` layer already joins `instances.network_config_version` on `instances.machine_id` to surface applied-vs-desired drift, and a `Fail` rung exists for `instance-network` drift in `hbn_verdict` (see `crates/nico-doctor/src/verdicts/hbn.rs:81-99`). The pattern is established. This spike narrows to: how does IB join the same pattern, and how broadly does the pattern extend?

## Survey — per-DPU layers and the instance-config comparison pattern

| Axis             | Data today (machine-side)                                                                          | Desired-side (instance) available?                                                                                            | Drift naturally lives in this layer? |
|------------------|----------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------|---------------------------------------|
| `dpu_cert`       | `machines.network_status_observation.client_certificate_expiry`                                    | No — cert expiry is a property of the issued cert, not an instance-level config target.                                       | N/A — not a drift axis.               |
| `dpu_isolation`  | `machines.network_config.quarantine_state` + observation freshness                                 | The desired-side IS the machine's `network_config`; there is no second instance-level signal to compare against.              | N/A — already desired-vs-observed on the same row. |
| `hbn`            | `machines.network_status_observation` (applied) + `machines.network_config` (desired managed-host) | **Yes — already implemented.** Joins `instances.network_config_version` for the desired instance-network axis (`Fail` rung).  | ✓ already lives in `hbn`.             |
| `dpu_services`   | `machines.network_status_observation.extension_service_observation.extension_service_statuses`     | Plausibly yes if `Instance` carries desired extension-service config. Not yet surveyed; out of scope here.                    | If/when surfaced — extends `dpu_services`, mirror of IB. |
| `infiniband`     | `machines.infiniband_status_observation` (per-port `pkeys`, `partition_ids`, `fabric_id`, `lid`)   | Yes — `InstanceInfinibandConfig` is the desired-side. Core's `ib_config_synced` is the canonical comparison.                  | ✓ — this spike's recommendation.      |

**Summary of the pattern:** `hbn` is the lone existing consumer; `infiniband` is the obvious next one; `dpu_services` is a potential third when/if instance-bound service config surfaces a real divergence operators care about. `dpu_cert` and `dpu_isolation` don't fit the pattern (no instance-side comparand). The pattern is not generic enough to warrant a cross-cutting "instance-config drift" axis layer; it's an opt-in concern per axis.

## Decisions

### Decision 1 — config-drift surface: per-layer axis, not a new cross-cutting layer

Config-sync drift surfaces **inside the per-DPU layer that owns the axis**, mirroring how `hbn` handles instance-network drift today. For IB specifically: a new rung in `ib_verdict()` returning `Fail` (or `Warn` — see open question below) when `InstanceInfinibandConfig` is present and the per-port observed pkeys/partition_ids disagree with the configured pkeys/partition_ids for that port.

**Why:**

- Matches the established convention (hbn drift already lives there, not as a new layer).
- Each axis has different drift semantics — pkey set comparison (IB) is unrelated to NVUE version drift (hbn) is unrelated to extension-service state drift (services). A unifying layer would either re-duplicate each layer's data fetch or take a generic shape too coarse to be actionable.
- Operator drill-down is naturally per-axis. The verdict line "ib: pkey drift on dpu X (configured {1,2}, observed {1})" routes to `nico doctor infiniband <id>`, where the per-port detail already lives.
- Holistic rollups (`dpu_health`, `dpu`) already consume per-axis `AxisSummary` values and need no new wiring.

**Why not a new "instance-config drift" axis layer:**

- Only two real consumers today (hbn + ib). Three at most if services adds it. A new top-level axis introduces a fourth axis people have to map; the cost-to-payoff ratio is poor with N=2.
- Would invert the established flow: `hbn` already owns its drift signal. Pulling it out to be re-emitted from a cross-cutting layer is churn without operator benefit.

### Decision 2 — comparison logic: replicate locally, do not depend on `infra-controller-core`'s `ib_config_synced`

The comparison is implemented inside `crates/nico-doctor/src/verdicts/infiniband.rs` (or a small sibling helper). `nico-doctor` does not pull in `infra-controller-core`'s `api-model` crate for this.

**Why:**

- Consistent with PRD-004's existing non-goal: _"Cross-repo coupling: do not depend on `infra-controller-core`'s `api-model::ib_config_synced`. Replicate any comparison locally."_ This spike ratifies that prior call; it does not re-litigate it.
- Consistent with how `hbn` handles applied-vs-desired drift today — string/version comparison directly in the verdict, no core helper.
- The comparison is mechanically small (a pkey set + partition_id set per port). The risk of replication drift is low; the risk of cross-repo coupling drift across NICo upgrades is higher.
- Keeps `nico-doctor` decoupled from internal NICo API churn. The doctor reads producer-side state via SQL — adding a Rust API dependency would crack that boundary.

**Drift-risk mitigation when implementation lands:**

- Snapshot the relevant `InstanceInfinibandConfig` shape in a local `IbConfigSnapshot` struct mapped from the column read in SQL. Don't try to mirror `IbConfigNotSyncedReason`'s enum verbatim — the doctor's verdict vocabulary is what operators see, not the producer's.
- A field-shape test reads the same JSON column an integration test produces, asserting the doctor's local read covers the same fields the producer writes.

### Decision 3 — PRD placement: defer to a future PRD-008, do not amend PRD-004

PRD-004 is shippable today as observation-only. Folding config-sync into PRD-004 would:

1. Re-open the "observation-only" boundary the PRD made an explicit shipping decision around (PRD-004 §"PRD-005 (potential, issue #301)" para).
2. Conflate observation work (already in flight per PRD-004 slices) with comparison work that gates on operator demand.

A new PRD (next free `prd-NNN` label, currently **`prd-008`**) is the cleaner home **when operator demand surfaces**. Until then:

- This design note records the resolved decisions so the PRD doesn't re-derive them from scratch.
- PRD-004's non-goals are amended to point here rather than to "potential PRD-005."
- No PRD doc file or epic issue is created yet. Spawning an empty epic before operator demand exists is wasted board scaffolding.

## Implementation sketch (for the eventual PRD-008)

When operator demand justifies the work, the implementation is a small, single-slice PRD:

1. **Schema probe.** Extend the `infiniband` layer's SQL to LEFT JOIN `instances` on `instances.machine_id = m.id` and read whatever column stores `InstanceInfinibandConfig` (TBD at implementation time; survey first). Degrade gracefully when the instance row is absent (DPU not assigned) — no drift surfaces in that case.

2. **`IbConfigSnapshot` local mapping.** A small struct mirroring the producer-side fields the doctor needs (per-port configured pkeys + partition_ids, indexed by GUID or port identifier).

3. **New rung in `ib_verdict()`.** Precedence (above `Ok`, below or around the observation-stale rung — final ordering decided at implementation):

    | Configured? | Observed?          | Match?   | Verdict | Message                                                                       |
    |-------------|--------------------|----------|---------|-------------------------------------------------------------------------------|
    | No          | —                  | —        | (skip)  | DPU not assigned to an instance, or instance has no IB config — no drift.     |
    | Yes         | `pkeys.is_none()`  | —        | Warn    | "ib: pkeys unobservable from UFM on dpu X — config-drift unverifiable"        |
    | Yes         | `Some(observed)`   | mismatch | Fail    | "ib: config drift on dpu X (configured {1,2,3}, observed {1,2})"              |
    | Yes         | `Some(observed)`   | match    | (skip)  | Falls through to existing `Ok` rung.                                          |

4. **Per-port detail rows** in the `infiniband` layer renderer surface the per-port drift detail beneath the headline (same pattern as PRD-004's per-port active/down/unobservable rows).

5. **Holistic rollups.** Zero changes — `dpu_health` and `dpu` already consume `AxisSummary` and surface whatever rung `ib_verdict` returned.

6. **CONTEXT.md.** Update the `infiniband` layer entry to mention the config-sync rung.

7. **Tests.** Unit tests against synthetic `MachineRow` fixtures covering each rung; one integration-style test against a real-mode 1 core-only-kind cluster confirming the column path is correct.

The eventual PRD is one slice. Estimated `(b*d)/c = (3 * 3) / 1 = 9` (med band) — moderate breadth (only IB-cluster operators), moderate depth (catches a specific class of misconfig invisible today), low cost (one rung + one SQL extension).

## Open questions (deferred to the eventual PRD-008)

- **Verdict severity — Fail vs Warn.** Configured-vs-observed mismatch is unambiguous evidence of misconfig (Fail), but cleanup pending or in-flight reconfiguration can produce transient drift (Warn). Resolve from operator field data when demand arrives — the rung's status is the first decision, not the last.
- **Per-port granularity vs DPU-level rollup.** The verdict can summarise drift count ("3/4 ports drift") or call out a single port ("port-{guid} pkey drift"). Mirror the PRD-004 per-port detail-row pattern as the default; revisit if operators report noise.
- **Interaction with PRD-004's `Unknown` capability state.** When `infiniband_present == None`, the IB layer skips entirely — same rule applies to the new rung. No new gating logic.
- **`dpu_services` extension.** Out of scope here. If operators report instance-level service-config drift, a separate sibling spike + design note covers the equivalent for services; the same two decisions (per-layer + replicate locally) are the strong prior.

## Acceptance-criteria mapping (issue #301)

- [x] Survey of other per-DPU-layer signals that could share an instance-config comparison pattern — §"Survey".
- [x] Decision recorded: separate "instance-config drift" axis vs. per-layer comparison — §"Decision 1": per-layer.
- [x] Decision recorded: depend on core's `ib_config_synced` helper vs. replicate locally — §"Decision 2": replicate locally.
- [x] PRD-004 amended OR new PRD created with its own epic — §"Decision 3": PRD-004 amended to point here; no new PRD/epic spawned yet (deferred until operator demand).

## Related

- Issue #301 — this spike.
- Issue #264 — original IB verdict-shape design spike under PRD-002. Resolved by deferral to PRD-004.
- PRD-002 — DPU layer rewrite; established the verdict pattern; mentions #301 in its "Related" section.
- PRD-003 — holistic summary refactor; established the `AxisSummary` primitive consumed by both rungs.
- PRD-004 — observation-only IB layer. Non-goals reference this design note as the home for config-sync decisions.
- ADR-0015 — axis verdict primitive (the `AxisSummary` shape).
- `infra-controller-core/crates/api-model/src/machine/infiniband.rs:131-248` — `ib_config_synced` + `IbConfigNotSyncedReason`. Replicated locally per Decision 2; not depended on.
