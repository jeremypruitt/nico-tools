//! PRD-005 follow-up (#353): `logs` layer criterion bench.
//!
//! Drives `LogsLayer::run` against a `BenchLogSource` returning `N`
//! synthetic per-pod error lines. No Loki/K8s I/O — measures
//! `checks_from`'s pod-grouping + truncation cost at fleet scale.

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nico_doctor::layer::{Layer, RunOpts};
use nico_doctor::layers::logs::LogsLayer;
use tokio::runtime::Runtime;

mod common;
use common::{BenchLogSource, FLEET_SIZES, fleet_log_entries};

fn bench_logs_layer(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("logs_layer");
    group.sample_size(10);

    for &n in FLEET_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || {
                    let source = Arc::new(BenchLogSource::new(
                        "loki",
                        true,
                        fleet_log_entries(n),
                    ));
                    let layer = LogsLayer::new(source);
                    (layer, RunOpts::default())
                },
                |(layer, opts)| async move { layer.run(&opts).await },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_logs_layer);
criterion_main!(benches);
