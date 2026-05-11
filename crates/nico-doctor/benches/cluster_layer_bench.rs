//! PRD-005 follow-up (#353): `cluster` layer criterion bench.
//!
//! Drives `ClusterLayer::run` against an in-process `BenchK8s` fake
//! populated with `N ∈ {1, 18, 250, 1000, 10000}` synthetic pods +
//! their derived Warning events. No I/O — measures the assembly path
//! (`checks_from` plus the `list_pods` / `list_events` filter chain)
//! that runs on every refresh.

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nico_doctor::layer::{Layer, RunOpts};
use nico_doctor::layers::cluster::ClusterLayer;
use tokio::runtime::Runtime;

mod common;
use common::{BenchK8s, FLEET_SIZES, fleet_events, fleet_pods};

fn bench_cluster_layer(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("cluster_layer");
    group.sample_size(10);

    for &n in FLEET_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || {
                    let client = Arc::new(BenchK8s::new(fleet_pods(n), fleet_events(n)));
                    let layer = ClusterLayer::new(client);
                    (layer, RunOpts::default())
                },
                |(layer, opts)| async move { layer.run(&opts).await },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_cluster_layer);
criterion_main!(benches);
