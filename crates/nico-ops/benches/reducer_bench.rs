//! PRD-005 Slice 0a.2: reducer-microbench.
//!
//! One bench per non-trivial `Action` variant. Each starts from a
//! warmed `App` and drives a single `handle()` call so the wall-clock
//! cost of each reducer arm shows up cleanly in the criterion output.
//!
//! Covers the six variants called out in the issue body:
//! `Snapshots`, `NamespaceEvents`, `LogLines`, `Tick`-while-refreshing,
//! `Focus`, `Refresh`.

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use nico_ops::action::{Action, Dir};
use nico_ops::app::App;

mod common;
use common::{fleet_log_lines, fleet_namespace_events, fleet_snapshots, warmed_app};

const WARM_N: usize = 18;

fn bench_snapshots(c: &mut Criterion) {
    let snapshots = fleet_snapshots(WARM_N);
    c.bench_function("reducer/snapshots", |b| {
        b.iter_batched(
            || (warmed_app(WARM_N), snapshots.clone()),
            |(mut app, snaps)| app.handle(Action::Snapshots(snaps)),
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_namespace_events(c: &mut Criterion) {
    let events = fleet_namespace_events(WARM_N);
    c.bench_function("reducer/namespace_events", |b| {
        b.iter_batched(
            || (warmed_app(WARM_N), events.clone()),
            |(mut app, evs)| app.handle(Action::NamespaceEvents(evs)),
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_log_lines(c: &mut Criterion) {
    let lines = fleet_log_lines(WARM_N);
    c.bench_function("reducer/log_lines", |b| {
        b.iter_batched(
            || (warmed_app(WARM_N), lines.clone()),
            |(mut app, ls)| app.handle(Action::LogLines(ls)),
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_tick_while_refreshing(c: &mut Criterion) {
    c.bench_function("reducer/tick_while_refreshing", |b| {
        b.iter_batched(
            || {
                // Drive into the refreshing branch by sending Refresh
                // first; subsequent Ticks should take the
                // `if self.refreshing` early-return path (Finding #1).
                let mut app = warmed_app(WARM_N);
                app.handle(Action::Refresh);
                (app, Instant::now())
            },
            |(mut app, now)| app.handle(Action::Tick(now)),
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_focus(c: &mut Criterion) {
    c.bench_function("reducer/focus", |b| {
        b.iter_batched(
            || warmed_app(WARM_N),
            |mut app| app.handle(Action::Focus(Dir::Right)),
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_refresh(c: &mut Criterion) {
    c.bench_function("reducer/refresh", |b| {
        b.iter_batched(
            || {
                // Start from a non-refreshing state so the Refresh
                // path actually does work (it's a no-op while
                // already refreshing).
                let mut app = App::new();
                app.handle(Action::Tick(Instant::now()));
                app.handle(Action::Snapshots(fleet_snapshots(WARM_N)));
                app
            },
            |mut app| app.handle(Action::Refresh),
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    bench_snapshots,
    bench_namespace_events,
    bench_log_lines,
    bench_tick_while_refreshing,
    bench_focus,
    bench_refresh,
);
criterion_main!(benches);
