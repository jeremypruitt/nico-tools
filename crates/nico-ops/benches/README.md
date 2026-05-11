# nico-ops benches — PRD-005 baseline (Slice 0a.2)

Four criterion benches that quantify the wall-clock + CPU cost of the
`nico ops` hot path. Each one targets a specific PRD-005 finding so
later slices have a numeric baseline to rank improvements against.

| Bench | What it measures | Target finding |
| --- | --- | --- |
| `idle_tick_bench` | 1000 ticks through `App::handle(Action::Tick(now))` with no in-flight refresh | Finding #1 (always-dirty tick re-render while refreshing) |
| `fan_out_bench` | `data::collect` against synthetic layers, sweep `N ∈ {1, 18, 250, 1000, 10000}` | Fan-out scaling baseline for `prepare_layers` + `runner::run` |
| `reducer_bench` | One bench per `Action` variant: Snapshots, NamespaceEvents, LogLines, Tick-while-refreshing, Focus, Refresh | Reducer microbench surface |
| `render_bench` | `view::render` through `ratatui::backend::TestBackend`, sweep `N ∈ {1, 18, 250}` | Findings #2 + #3 (sparkline recompute + per-card `evidence.clone()`) |

## How to run

```bash
# All four benches:
cargo bench -p nico-ops

# One bench:
cargo bench -p nico-ops --bench fan_out_bench

# Quick smoke pass (smaller sample counts):
cargo bench -p nico-ops -- --quick

# With dhat heap profiling (writes dhat-heap.json next to the binary):
cargo bench -p nico-ops --features dhat-heap --bench render_bench
```

Output lives under `target/criterion/`; the HTML report at
`target/criterion/report/index.html` lets you compare runs.

## Fixtures

Slice 0a.1 (#346) shipped `nico_doctor::perf_fixtures::synthesize_*(n)`
which multiplies KB-scale seed rows under
`crates/nico-doctor/tests/fixtures/perf/` up to fleet-scale `N` (1, 18,
250, 1000, 10000) at bench startup. These benches operate one layer
above that — at the `LayerSnapshot` / `App` reducer / `view::render`
level — so they keep their own synthetic generators in
`benches/common/mod.rs` (`fleet_snapshots`, `fleet_layers`,
`warmed_app`, …). When a later slice grows benches that consume raw
DPU/pod/Temporal/Loki JSON, those should switch to
`nico_doctor::perf_fixtures` directly.

To refresh the seed rows against a live cluster:

```bash
./scripts/capture-fixtures.sh
```

## Baseline numbers — 2026-05-10

Captured locally on an Apple Silicon Mac (M-series, optimized
`bench` profile). These are the regression-guard baselines that
later PRD-005 slices will rank against; they are **not** budgets and
are not portable across machines. Re-capture on the same host after
landing any change that should move the needle.

### idle_tick

| Bench | Median |
| --- | --- |
| `1000_ticks_no_refresh` | **4.34 µs** (~4.3 ns/tick) |

The 1000-tick batch sits well under the 100 ms tick cadence
(`nico_ops::TICK`), so idle-tick wall-clock is not the bottleneck —
which makes it a useful flat baseline against which Slice 0a.3's
`idle_tick_does_not_re_render` integration test will catch the
"always-dirty while refreshing" path (Finding #1).

### fan_out

| N (layers) | Median |
| --- | --- |
| 1 | 8.1 µs |
| 18 | 25.5 µs |
| 250 | 252 µs |
| 1000 | 989 µs |
| 10000 | 10.99 ms |

Roughly linear in N (≈ 1 µs/layer once N ≥ 18). The synthetic
`BenchLayer` does no I/O — this baseline isolates the cost of
`run_streaming`'s `FuturesUnordered` + per-result mpsc roundtrip from
the cost of the layer bodies themselves.

### reducer

| Action variant | Median |
| --- | --- |
| `Snapshots(18)` | 8.6 µs |
| `NamespaceEvents(18)` | 4.1 µs |
| `LogLines(18)` | 3.9 µs |
| `Tick` (while refreshing) | 3.4 µs |
| `Focus(Right)` | 3.4 µs |
| `Refresh` | 2.3 µs |

All six are O(N=18) one-shot calls, so the relative ordering reflects
how much work each arm does. `Snapshots` is the heaviest (recomputes
deltas, pulses, prev-status, history push); `Refresh` is the cheapest
(it flips two flags and emits `Effect::StartRefresh`).

### render

| N (fleet snapshots) | Terminal | Median |
| --- | --- | --- |
| 1 | 160×48 | 196 µs |
| 18 | 160×48 | 261 µs |
| 250 | 160×48 | 351 µs |

Render scales sub-linearly with N because off-screen cards are not
painted — but the per-card work (sparkline recompute + the
`evidence.clone()` from Finding #3) still runs for every snapshot.
The N=250 row is the leading indicator for Findings #2 + #3.
