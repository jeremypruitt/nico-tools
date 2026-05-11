//! PRD-005 follow-up (#353): `health` layer criterion bench.
//!
//! Drives `HealthLayer::run` against a `BenchHttp` fake serving
//! 200/503 status codes synchronously. Sweep over `N` simulates
//! deployments that proxy `nico doctor` past dozens of services
//! (`carbide-api`, `forgedb`, internal sidecars).

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nico_doctor::layer::{Layer, RunOpts};
use nico_doctor::layers::health::HealthLayer;
use tokio::runtime::Runtime;

mod common;
use common::{BenchHttp, FLEET_SIZES, fleet_endpoints};

fn bench_health_layer(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("health_layer");
    group.sample_size(10);

    for &n in FLEET_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || {
                    let client = Arc::new(BenchHttp);
                    let layer = HealthLayer::new(client, fleet_endpoints(n));
                    (layer, RunOpts::default())
                },
                |(layer, opts)| async move { layer.run(&opts).await },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_health_layer);
criterion_main!(benches);
