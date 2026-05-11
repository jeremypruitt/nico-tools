//! PRD-005 follow-up (#353): `grpc` layer criterion bench.
//!
//! One reflection inspection per `collect` — not fleet-scoped, so no
//! N-sweep. Single flat bench against a `BenchGrpc` fake returning a
//! representative two-service reachable result.

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use nico_doctor::layer::{Layer, RunOpts};
use nico_doctor::layers::grpc::GrpcLayer;
use tokio::runtime::Runtime;

mod common;
use common::BenchGrpc;

fn bench_grpc_layer(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");

    c.bench_function("grpc_layer/reachable_two_services", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let layer = GrpcLayer::new(Arc::new(BenchGrpc), "bench:50051".into());
                (layer, RunOpts::default())
            },
            |(layer, opts)| async move { layer.run(&opts).await },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_grpc_layer);
criterion_main!(benches);
