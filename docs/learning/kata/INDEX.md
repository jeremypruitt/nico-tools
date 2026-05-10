# Kata Index

Short hands-on exercises against a NICo deployment. One file per kata. New kata get added here as I encounter things worth practicing. Kata that turn out to be wrong or pointless get deleted with a one-line note in the "Retired" section.

## Conventions

- **Deployment-type matters.** Every kata file opens with the deployment-type it's been verified against (`core-only`, `rest-only-mock`, `full`). Layers/Sources skip differently per type — see CONTEXT.md "Deployment-type" and PRD-001. A kata that needs Temporal won't run on `core-only` or `rest-only-mock`; a kata that needs forgedb won't run on `rest-only-mock`.
- **Tie kata to shipped behavior where possible.** If a kata exercises a path that the doctor or correlate already covers, name the layer/sub-check it touches in the kata's "Hooks into nico" section. Kata then double as regression checks on this repo.
- **30–60 min target.** Longer = split it.

## Bringup matrix (which env unblocks which kata)

| Env produced by                                                         | Deployment-type   | Forgedb | Temporal | REST | Unblocks            |
|-------------------------------------------------------------------------|-------------------|---------|----------|------|---------------------|
| `infra-controller-core/devspace.yaml` (k01)                             | `core-only`       | ✓       | ✗        | ✗    | k02, k03, k05, k06  |
| `infra-controller-rest/Makefile` → `make kind-reset` (k01b, not specced)| `rest-only-mock`  | ✗       | ✗        | ✓    | (REST-API kata, TBD)|
| Both side-by-side, custom plumbing (k01c, not specced)                  | `full`            | ✓       | ✓        | ✓    | k04, k07            |

`core-only` is the cheapest bringup and unblocks the most seed kata, so it's the gate. `rest-only-mock` and `full` get specced when a downstream kata actually needs them.

## Status values

`seed` (sketch only) → `running` (in progress) → `done` (completed at least once) → `retired` (kept here for the lesson, body archived).

## Kata

| ID  | Title                                       | Status | Deployment-type | Hooks into nico                                              |
|-----|---------------------------------------------|--------|-----------------|--------------------------------------------------------------|
| k01 | [devspace bringup (core-only)](k01-devspace-bringup.md) | seed | `core-only` (this kata produces it) | boot probe, deployment-type detection |
| k02 | grpc tour                                   | seed   | `core-only`     | `grpc` layer                                                 |
| k03 | kill the dhcp pod                           | seed   | `core-only`     | `cluster`, `logs` layers (`carbide-dhcp` is in core devspace) |
| k04 | trace a tenant allocation                   | seed   | `full`          | `nico correlate <id>`, `dpu_isolation` headline; **blocked on k01c** |
| k05 | postgres forgedb tour                       | seed   | `core-only`     | per-DPU layers (`hbn`, `dpu_cert`, `dpu_isolation`, `dpu_health`, `dpu_services`) all read `machines` JSON |
| k06 | cert expiry sim                             | seed   | `core-only`     | `nico doctor dpu-cert <id>`, `cert-fleet` sub-check of `dpu` layer |
| k07 | temporal workflow inspect                   | seed   | `full`          | `workflows` layer (skips on `core-only` via `temporal_present` gate); **blocked on k01c** |

## Retired

_(none yet)_
