//! PRD-005 Slice 0a.2: fan-out wall-clock bench.
//!
//! Runs `nico_ops::data::collect` (which spawns `nico_doctor::run_streaming`)
//! against synthetic layers and measures wall-clock for the full
//! fan-out under fleet sizes `N ∈ {1, 18, 250, 1000, 10000}`.
//!
//! Why this matters: `prepare_layers` + `runner::run` are the shared
//! machinery behind both `nico ops` and `nico doctor` — the same code
//! path baseline is reused by future PRD-005 slices.

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nico_doctor::layer::RunOpts;
use tokio::runtime::Runtime;

mod common;
use common::fleet_layers;

const FAN_OUT_SIZES: &[usize] = &[1, 18, 250, 1000, 10000];

fn bench_fan_out(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");

    let mut group = c.benchmark_group("fan_out");
    group.sample_size(10);

    for &n in FAN_OUT_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || fleet_layers(n),
                |layers| async move {
                    let snapshots = nico_ops::data::collect(
                        layers,
                        RunOpts::default(),
                        None,
                    )
                    .await;
                    assert_eq!(snapshots.len(), n, "fan-out lost snapshots");
                    snapshots
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_fan_out);
criterion_main!(benches);
