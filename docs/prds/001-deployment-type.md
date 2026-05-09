# PRD-001 — `nico --deployment-type`: capability-based detection

- **Status:** Specced (2026-05-09); awaiting `/to-issues` breakdown.
- **Epic:** #245 (carries `prd-001` label; tracks slice progress).
- **Touches:** ADR-0013 (boot probe, to be amended).
- **Deferred follow-up:** #242 (capabilities object in JSON).

## Problem

`nico` (ops, doctor) hard-fails (exit 3) when the configured controller namespace doesn't exist on the active cluster. NICo has at least three documented dev shapes — full (core+rest), core-only (carbide-kind), and the rest repo's documented quick-setup (`kind-nico-rest-local`, with mock-core stand-in for the real gRPC core). The shapes use *different* controller namespaces (`forge-system` vs `nico-rest`), different gRPC services (`carbide-api:1079` vs `nico-rest-mock-core:11079`), and different postgres schemas. The tool has zero awareness of which shape is in front of it, so the rest contributor's documented quick-setup path errors out at boot.

## Personas

- **Rest-repo contributor** following the documented quick-setup. Primary unblock target.
- **Core / full-stack operator** running against a co-located full or core-only kind cluster. Behavior must remain identical to today (no regressions).

## Goals

- `nico ops` and `nico doctor` correctly classify the active cluster as one of three deployment-types and behave appropriately for each.
- Auto-detect by default; explicit override available; safe escape hatch when detection is wrong or absent.
- Read-only; no remediation; no new external dependencies.

## Non-goals

- Multi-cluster / cross-cluster correlation.
- Bringing up clusters (kind setup, helm install, etc.).
- A signature-catalog DSL or external sig file. Detection rules stay hardcoded.
- A `capabilities` object in JSON output (deferred — see #242).

## High-level design

### Three deployment-types (hardcoded labels)

| Type             | Controller ns | gRPC address                          | forgedb |
|------------------|---------------|---------------------------------------|---------|
| `full`           | `forge-system`| `carbide-api.forge-system:1079`       | yes     |
| `core-only`      | `forge-system`| `carbide-api.forge-system:1079`       | yes     |
| `rest-only-mock` | `nico-rest`   | `nico-rest-mock-core.nico-rest:11079` | no      |

### Detection (capability-based; signals 2 + 3 + 4)

Architecture: detection resolves the active cluster to one of the named `DeploymentType` variants. The type carries deployment-type-derived defaults for existing config keys (no new identifier namespace) plus one feature gate (`forgedb_present`) that forgedb-dependent layers consult to skip cleanly. Layers gate on the predicate (`forgedb_present()`), not on the type-name label. Vocabulary is finalized below in the **Capability vocabulary** section.

Signal ladder (first match wins):

1. **Signature workload probe** — `Service nico-rest-mock-core@nico-rest` definitively → `rest-only-mock`. `Service carbide-api@forge-system` + `nico-rest-api@nico-rest` → `full`. `Service carbide-api@forge-system`, no `nico-rest` ns → `core-only`.
2. **Namespace inventory** — fallback when (1) is inconclusive. Combination of `forge-system` / `nico-rest` presence/absence.
3. **CRD inventory** — fallback when (1) and (2) are inconclusive. `sites.nico.nvidia.io` present → rest deployed; core CRDs present → core deployed.

If all three signals fail to match a known type → exit 3 with diagnostic data (observed namespaces, observed services). Recovery: pass `--deployment-type` explicitly or use `--deployment-type=force`.

### Hybrid trust model

- `--deployment-type=<full|core-only|rest-only-mock>` → trust it, skip detection.
- `--deployment-type=force` → trust nothing, skip detection, run with raw config; banner shows `deployment-type: force (no enforcement)`.
- `[cluster] deployment_type = "..."` in `config.toml` or `NICO_DEPLOYMENT_TYPE` env → trust it, skip detection.
- Otherwise → run the detection ladder above.

### Per-layer behavior

| Layer       | full / core-only                      | rest-only-mock                                  |
|-------------|---------------------------------------|-------------------------------------------------|
| `cluster`   | runs                                  | runs                                            |
| `logs`      | runs                                  | runs                                            |
| `workflows` | runs                                  | runs (Temporal real)                            |
| `health`    | runs (per-layer endpoint detail TBD)  | runs (per-layer endpoint detail TBD)            |
| `grpc`      | dials `carbide-api:1079`              | dials `nico-rest-mock-core:11079`               |
| `postgres`  | runs                                  | runs                                            |
| `dpu`       | runs                                  | **n/a — no forgedb**                            |

`dpu`-in-`rest-only-mock` is the only layer that "skips" by deployment-type. All other type-dependent variation is address re-pointing via the capability bundle.

### Status semantics for "n/a in this deployment-type"

Extend `LayerOutcome::Skipped { reason: Option<String> }`. Status priority is unchanged (`Fail > Warn > Unknown > Ok`; `Skipped` sits independently). Formatter renders the reason when present (`. dpu (skipped — n/a in rest-only-mock: no forgedb)`). JSON gains `skipped_reason` field on layer entries.

`Status::Unknown` (the existing `UnconfiguredLayer` path) is *not* reused — that's a soft-fail meaning "your config is broken"; n/a-by-design must not look like a fail.

## UX

### Boot banner

```
  ◐ booting nico  ·  reach: port-forward (auto-detected)  ·  type: rest-only-mock (auto)

    connecting
      ✓  load kubeconfig
      ✓  reach API server

    validating
      ✓  credentials
      ✓  detect deployment-type: rest-only-mock              ← NEW step
      ✓  namespace 'nico-rest' exists                         ← capability-resolved
      ✓  list-pods permission

    serving
      ✓  port-forward: workflows
      ✓  port-forward: grpc → nico-rest-mock-core:11079       ← resolved addr shown
      ✓  port-forward: postgres
      ✓  reach postgres
```

Source tag values for the top-line indicator: `auto | flag | config | force`.

### Config precedence

Capability bundle slots in as a new defaults layer:

```
hardcoded defaults < deployment-type capability bundle < file < env < CLI
```

When a per-key file/env/CLI override contradicts the active deployment-type's bundle (e.g., `cluster.namespace=forge-system` with `deployment-type=rest-only-mock`), emit a one-line warning at boot. `--deployment-type=force` silences this warning.

### JSON output additions

- New top-level `deployment_type: { name: "...", source: "auto|flag|config|force" }`.
- New `skipped_reason: "..."` field on layer entries when `Skipped` carries a reason.
- *Not* shipping `capabilities` object — deferred to #242.

## ADR work

Amend ADR-0013 (boot probe) to document the new `detect_deployment_type` step in the `validating` section, its placement (after `credentials`, before `namespace_exists`, because the latter needs the resolved namespace), and the failure semantics (timeout vs no-match-with-diagnostic-data).

## Domain language to add (CONTEXT.md)

`Deployment-type` and `Force mode` are already part of the ubiquitous-language section (added when this PRD was specced).

## Capability vocabulary

Resolved 2026-05-09. The "capability bundle" is a defaults overlay on existing config keys plus a single feature gate. Implemented as methods on the `DeploymentType` enum (no parallel identifier namespace, no separate bundle struct).

```rust
pub enum DeploymentType { Full, CoreOnly, RestOnlyMock, Force }

impl DeploymentType {
    pub fn default_cluster_namespace(&self) -> Option<&'static str>;
    pub fn default_grpc_address(&self) -> Option<&'static str>;
    pub fn default_postgres_namespace(&self) -> Option<&'static str>;
    pub fn default_temporal_address(&self) -> Option<&'static str>;
    pub fn default_temporal_namespace(&self) -> Option<&'static str>;
    pub fn forgedb_present(&self) -> bool;
}
```

- `Force` returns `None` for every default (falls through to existing hardcoded fallbacks) and `true` for `forgedb_present` (assume present; forgedb-dependent layers fail naturally if it isn't — that's the price of force).
- **Override-conflict warning rule.** When a per-key file/env/CLI value differs from the active deployment-type's default for one of the five default-keys above, emit a one-line stderr warning after the boot banner header (one line per contradicting key). `--deployment-type=force` silences. Keys without deployment-type-derived defaults (`cluster.context`, `cluster.reach_mode`, `postgres.url`, `dpu.*`) are not checked.
- **Single feature gate.** `forgedb_present` is substrate-shaped (names the database the layers depend on) and covers every forgedb-dependent layer (`dpu`, `hbn`, `dpu_cert`, `dpu_isolation`, `hbn_drift`, plus PRD-002's `dpu_health` / `dpu_services` / `infiniband` once they land). If a future deployment-type introduces a different DPU data source, replace `bool` with an enum at that point — not before.
- **Slice 1 escape valve.** The α-flat shape (`Force` as an enum variant; methods on the enum) is the spec. If a concrete need emerges during slice 1 to switch to a separate `CapabilityBundle` struct or to nest `Force` as `DeploymentTypeResolved::Force`, the slice 1 PR may do so without amending this PRD.

## Implementation tracking

The slice breakdown lives in epic #245 as a tasklist. Sub-issues are created via `/to-issues` against the epic, all carrying `prd-001` label and `Parent: #245` per the conventions in `docs/agents/issue-tracker.md`.
