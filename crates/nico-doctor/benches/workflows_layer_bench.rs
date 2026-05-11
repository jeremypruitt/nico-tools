//! PRD-005 follow-up (#353): `workflows` layer criterion bench.
//!
//! Drives `WorkflowsLayer::run` against a `BenchTemporal` that returns
//! `N` running workflows on the "stuck" query and `N` failed
//! workflows on the "failed" query. Each invocation is one full
//! `collect`: two visibility queries plus the per-execution proto →
//! `RunningWorkflow` / `FailedWorkflow` conversion (Findings #7 + #8
//! live here at fleet scale).

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nico_doctor::layer::{Layer, RunOpts};
use nico_doctor::layers::workflows::WorkflowsLayer;
use tokio::runtime::Runtime;

mod common;
use common::{BenchTemporal, FLEET_SIZES, fleet_executions};

fn bench_workflows_layer(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("workflows_layer");
    group.sample_size(10);

    for &n in FLEET_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || {
                    let temporal = Arc::new(BenchTemporal {
                        stuck: fleet_executions(n, false),
                        failed: fleet_executions(n, true),
                    });
                    let layer = WorkflowsLayer::new(
                        temporal,
                        "nico".into(),
                        Duration::from_secs(15 * 60),
                    );
                    (layer, RunOpts::default())
                },
                |(layer, opts)| async move { layer.run(&opts).await },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_workflows_layer);
criterion_main!(benches);
