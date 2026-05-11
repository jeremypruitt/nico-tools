//! PRD-005 Slice 0a.3: regression-guard integration tests for the
//! `nico ops` event loop.
//!
//! These three tests pin the user-visible behaviors that PRD-005's
//! initial findings flagged:
//!
//! 1. `cold_start_to_first_paint` — wall-clock from refresh-trigger to
//!    first `Action::Snapshots` reduce stays under a generous bound.
//! 2. `idle_tick_does_not_re_render` — once a refresh has settled,
//!    `Action::Tick` does not flip `app.dirty()` (Finding #1 guard).
//! 3. `memory_bounded_after_n_refreshes` — 1000 reduce cycles do not
//!    grow live heap past a bound (dhat-gated).
//!
//! ## Scope notes
//!
//! Acceptance criterion #2 of issue #348 calls for "drive
//! `run_event_loop` against fully stubbed clients". The current
//! `run_event_loop` takes a concrete `Terminal<CrosstermBackend<Stdout>>`
//! and pulls events off `EventStream::new()`, neither of which is
//! testable without a backend-genericisation refactor (out of scope
//! for this slice). These tests instead drive the same behaviors
//! at the `data::collect` + `App::handle` layer — the same composable
//! seam Slice 0a.2's wall-clock benches use (`benches/common/mod.rs`
//! in PR #357). Future slices that genericise the event loop can
//! re-target these tests to the wider end-to-end path without changing
//! the assertions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nico_doctor::layer::{Layer, RunOpts, SkippedLayer};
use nico_ops::action::Action;
use nico_ops::app::App;
use nico_ops::data;

// dhat's global allocator must live in the test binary's root file;
// the `dhat-heap` feature is the same opt-in flag the criterion
// benches use (see `benches/README.md`).
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Build N synthetic no-I/O layers. `SkippedLayer` resolves immediately
/// to `LayerOutcome::Skipped` — exactly the right shape for measuring
/// the fan-out + reduce path in isolation from the live cluster.
fn synthetic_layers(n: usize) -> Arc<Vec<Box<dyn Layer>>> {
    // `&'static str` constraint on `Layer::name` forces us to leak the
    // names; tests are short-lived so the leak is harmless.
    let layers: Vec<Box<dyn Layer>> = (0..n)
        .map(|i| {
            let raw = format!("synthetic_layer_{i}");
            let name: &'static str = Box::leak(raw.into_boxed_str());
            SkippedLayer::new(name)
        })
        .collect();
    Arc::new(layers)
}

/// Cold-start regression guard. Bound is intentionally generous (~1s
/// wall-clock) — this is a tripwire for catastrophic regressions, not
/// a tight latency budget. Local baseline on the maintainer's box is
/// O(1ms) for synthetic layers; the bound leaves three orders of
/// magnitude of slack so CI variance does not flake the test.
#[tokio::test]
async fn cold_start_to_first_paint() {
    let layers = synthetic_layers(6);
    let opts = RunOpts::default();
    let mut app = App::with_interval(Duration::from_secs(30));

    let start = Instant::now();
    let snapshots = data::collect(layers, opts, None).await;
    app.handle(Action::Snapshots(snapshots));
    let elapsed = start.elapsed();

    assert!(app.dirty(), "first paint should leave app dirty");
    assert_eq!(
        app.snapshots().len(),
        6,
        "six synthetic layers should produce six snapshots"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "cold-start to first paint took {elapsed:?}; bound is 1s"
    );
}

/// Finding #1 regression guard. The `Tick` reducer at `app.rs:484-487`
/// previously flipped `dirty = true` on every tick while `refreshing`
/// was set, forcing a full re-render at 10Hz even when nothing on
/// screen had changed. After a refresh settles (Snapshots reduce →
/// `refreshing = false`), idle ticks before the next deadline should
/// leave `dirty` alone. This test pins the post-settle steady state.
#[tokio::test]
async fn idle_tick_does_not_re_render() {
    let mut app = App::with_interval(Duration::from_secs(60));
    let t0 = Instant::now();

    // Warm: first Tick seeds boot+now, Snapshots schedules next_refresh
    // for `t0 + 60s` and clears `refreshing`. Drain dirty.
    app.handle(Action::Tick(t0));
    app.handle(Action::Snapshots(vec![]));
    app.clear_dirty();

    // Drive 100 idle ticks at 10Hz, all comfortably before the next
    // refresh deadline. None of them should flip `dirty`.
    let mut dirty_ticks = 0usize;
    for i in 1..=100u64 {
        let now = t0 + Duration::from_millis(i * 100);
        app.handle(Action::Tick(now));
        if app.dirty() {
            dirty_ticks += 1;
            app.clear_dirty();
        }
    }

    assert_eq!(
        dirty_ticks, 0,
        "{dirty_ticks} of 100 idle ticks flipped dirty — Finding #1 regression"
    );
}

/// Memory regression guard. Drives 1000 reduce cycles of fleet-scale
/// snapshots through the `App` and asserts dhat-tracked live heap
/// stays under a ceiling. Catches unbounded growth in the ringbuffer,
/// any future state-sharing stage, or per-cycle leaks. Gated behind
/// `--features dhat-heap` (Slice 0a.1) because the dhat allocator is
/// significantly slower than the system allocator; the default
/// `cargo test` run skips this test entirely.
#[cfg(feature = "dhat-heap")]
#[tokio::test]
async fn memory_bounded_after_n_refreshes() {
    use nico_common::output::Status;
    use nico_ops::model::{Finding, LayerSnapshot};

    let _profiler = dhat::Profiler::builder().testing().build();

    let mut app = App::with_interval(Duration::from_secs(30));
    let t0 = Instant::now();
    app.handle(Action::Tick(t0));

    // Pre-built 50-snapshot fleet, cloned per cycle so each reduce
    // sees a fresh allocation. 1000 cycles is the spec from the
    // acceptance criteria; 50-wide fleet is roughly twice the
    // operator-visible card grid.
    let template = (0..50)
        .map(|i| LayerSnapshot {
            name: format!("layer_{i:03}"),
            status: if i % 5 == 0 { Status::Warn } else { Status::Ok },
            evidence: format!("synthetic evidence row {i}"),
            findings: if i % 5 == 0 {
                vec![Finding {
                    status: Status::Warn,
                    message: format!("synthetic finding {i}"),
                    next_command: None,
                    link: None,
                }]
            } else {
                vec![]
            },
            duration_ms: (i % 100) as u64,
        })
        .collect::<Vec<_>>();

    for _ in 0..1000 {
        app.handle(Action::Snapshots(template.clone()));
    }

    // 50 MiB is generous: the ringbuffer is bounded, snapshots replace
    // (not accumulate), and per-cycle allocations should release on
    // the next reduce. Anything past this ceiling means a regression.
    let stats = dhat::HeapStats::get();
    let bound: u64 = 50 * 1024 * 1024;
    assert!(
        stats.curr_bytes < bound as usize,
        "live heap {} bytes after 1000 reduce cycles exceeds {bound} bound",
        stats.curr_bytes
    );
}
