# `nico-doctor` per-layer benches — PRD-005 follow-up (#353)

Seven criterion benches — one per layer registered in
`nico_doctor::layers::*` — that exercise each layer's `Layer::run`
against in-process mocks. No I/O. Each fleet-scoped layer parametrizes
over `N ∈ {1, 18, 250, 1000, 10000}`; the two layers whose work is
constant in fleet size (`grpc`, `postgres`) keep a single flat bench.

These benches are deliberately low-value until a specific layer shows
up in a Slice 1 audit. They live here so the next Slice that goes
deep on one layer can measure against a stable baseline.

| Bench | Shape | Targets |
| --- | --- | --- |
| `cluster_layer_bench` | N-sweep over pods + Warning events | `list_pods` + `list_events` + `checks_from` (issue #190 pod-log-tail) |
| `logs_layer_bench` | N-sweep over per-pod error lines | `checks_from` pod-grouping + value truncation (issue #201) |
| `workflows_layer_bench` | N-sweep over Temporal `WorkflowExecutionInfo` | proto → `RunningWorkflow` / `FailedWorkflow` conversion (Findings #7 + #8) |
| `health_layer_bench` | N-sweep over HTTP probe endpoints | per-endpoint `healthz` / `readyz` serial probe loop |
| `grpc_layer_bench` | flat — one reflection inspection | `checks_from` reachable path (no N-sweep — single addr) |
| `postgres_layer_bench` | flat — pool stats + 5 lock waits | `pool_check` + `lock_checks` assembly |
| `dpu_layer_bench` | N-sweep over `DpuSnapshot` rows | `dpu::assemble_checks` fleet rollup (PRD-003 slice 6) |

## How to run

```bash
# All seven (one run, criterion's regression detector compares to prior):
cargo bench -p nico-doctor

# One bench:
cargo bench -p nico-doctor --bench cluster_layer_bench

# Quick smoke pass (fewer samples — useful for CI / repro on slower hosts):
cargo bench -p nico-doctor --bench dpu_layer_bench -- --quick

# Heap profiling (writes dhat-heap.json next to the bench binary):
cargo bench -p nico-doctor --features dhat-heap --bench dpu_layer_bench
```

Output lives under `target/criterion/`; the HTML report at
`target/criterion/report/index.html` lets you compare runs.

## Fixtures

Each bench inlines a small synthetic generator in `benches/common/mod.rs`
that builds the layer's domain types directly (`RawPod`,
`WorkflowExecutionInfo`, `DpuSnapshot`, …). This is intentionally one
layer above `nico_doctor::perf_fixtures` (which serializes seed JSON):
the per-layer benches care about the post-deserialize assembly cost,
so they skip the JSON round-trip and feed typed values straight in.

When a later slice extends a layer's audit to include raw-wire
deserialize cost, it should switch that layer's bench to drive
`nico_doctor::perf_fixtures::synthesize_*` through the matching
counting decorator from PRD-005 Slice 0b.

## Baseline numbers — 2026-05-11

Captured locally on an Apple Silicon Mac (M-series, optimized `bench`
profile, `--quick` sample count). Treat these as a regression-guard
floor, **not** budgets. Re-capture on the same host after landing any
change that should move the needle.

### cluster_layer (fleet-sweep)

| N | Median |
| --- | --- |
| 1 | 703 ns |
| 18 | 11.64 µs |
| 250 | 180 µs |
| 1000 | 1.33 ms |
| 10000 | 83.6 ms |

Super-linear at N=10000 — `top_k_restarting`'s `sort_by_key` over the
full pod list scales O(N log N); `group_by_pod` in the cluster
checks path is the same shape. Catching this early is exactly why
this bench exists.

### logs_layer (fleet-sweep)

| N | Median |
| --- | --- |
| 1 | 635 ns |
| 18 | 6.98 µs |
| 250 | 146 µs |
| 1000 | 1.34 ms |
| 10000 | 105 ms |

Roughly linear-with-a-bend: `group_by_pod`'s O(N²) `iter_mut().find`
shows up at 10000 (105 ms vs the 13.4 ms a linear extrapolation
would predict). Same hot spot as the cluster layer at scale.

### workflows_layer (fleet-sweep)

| N | Median |
| --- | --- |
| 1 | 1.57 µs |
| 18 | 16.81 µs |
| 250 | 208 µs |
| 1000 | 864 µs |
| 10000 | 9.88 ms |

Linear in N. Two `list_workflow_executions` calls + per-execution
proto → domain conversion (Findings #7 + #8).

### health_layer (fleet-sweep)

| N | Median |
| --- | --- |
| 1 | 519 ns |
| 18 | 5.80 µs |
| 250 | 72 µs |
| 1000 | 284 µs |
| 10000 | 2.93 ms |

Linear; the serial `healthz` → `readyz` probe loop is exercised
without any actual I/O via the mock.

### grpc_layer (flat)

| Bench | Median |
| --- | --- |
| `reachable_two_services` | 310 ns |

One inspection per `collect`. The bench is a control surface: any
regression here likely means the `checks_from` assembly grew an
unexpected allocation.

### postgres_layer (flat)

| Bench | Median |
| --- | --- |
| `pool_and_locks` | 2.45 µs |

5 long-running lock waits + 90% pool utilization (representative
worst-case). Linear-time `lock_checks` over a bounded input.

### dpu_layer (fleet-sweep)

| N | Median |
| --- | --- |
| 1 | 2.43 µs |
| 18 | 21.81 µs |
| 250 | 285 µs |
| 1000 | 1.15 ms |
| 10000 | 11.80 ms |

Linear in N. `assemble_checks` iterates the fleet once per axis (cert,
isolation, hbn, services, IB) — five passes is what gives the ~1.18 µs
per-DPU rate at 10000.

## Per-DPU subcommand benches — deferred

`dpu_cert`, `dpu_isolation`, `dpu_health`, `dpu_services`, `hbn`,
`infiniband` each run one DPU at a time, so an N-sweep doesn't apply.
Adding a single flat bench per subcommand is straightforward (each
layer has a public `Layer::new(client, dpu_id)`) but deferred until a
layer surfaces in an audit — pure tooling expansion that buys nothing
until then.
