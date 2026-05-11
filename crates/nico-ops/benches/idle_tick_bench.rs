//! PRD-005 Slice 0a.2: idle-tick wall-clock bench.
//!
//! Drives 1000 ticks through `App::handle(Action::Tick(now))` from a
//! warmed (post-first-paint) `App`. With no in-flight refresh and no
//! deadline crossed, the reducer should be effectively a no-op aside
//! from the throbber-frame computation and toast TTL check.
//!
//! Paired with the integration test in Slice 0a.3
//! (`idle_tick_does_not_re_render`), this bench bounds the wall-clock
//! cost per tick — the regression guard for Finding #1 (always-dirty
//! tick re-render while refreshing).

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use nico_ops::action::Action;

mod common;
use common::warmed_app;

fn bench_idle_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("idle_tick");
    group.sample_size(20);

    // N = 18 fleet rows is the canonical "medium fleet" size for nico
    // ops; idle-tick cost should be flat under all fleet sizes since
    // it doesn't touch snapshots, but we pre-warm to keep the bench
    // representative of the steady-state hot path.
    let warm_n = 18;

    group.bench_function("1000_ticks_no_refresh", |b| {
        b.iter_batched(
            || warmed_app(warm_n),
            |mut app| {
                // Start from a known `now` so the reducer can observe
                // monotonic progress without the wall-clock floor of
                // `Instant::now()` dominating the timing.
                let start = Instant::now();
                for i in 0..1000 {
                    app.handle(Action::Tick(start + Duration::from_millis(i)));
                }
                app
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_idle_tick);
criterion_main!(benches);
