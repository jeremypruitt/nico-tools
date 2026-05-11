//! PRD-005 follow-up (#353): `dpu` layer fleet-rollup criterion bench.
//!
//! Drives `DpuLayer::run` against a `BenchDpuClient` returning `N`
//! synthetic `DpuSnapshot`s. The bench exercises `dpu::assemble_checks`
//! — the per-axis verdict roll-up that scales linearly with fleet
//! size and produces the bulk of the headline-and-finding work in the
//! fleet-wide DPU view (PRD-003 slice 6).

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nico_doctor::layer::{Layer, RunOpts};
use nico_doctor::layers::dpu::DpuLayer;
use tokio::runtime::Runtime;

mod common;
use common::{BenchDpuClient, FLEET_SIZES, dpu_config, fleet_dpu_snapshots};

fn bench_dpu_layer(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");
    let mut group = c.benchmark_group("dpu_layer");
    group.sample_size(10);

    for &n in FLEET_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.to_async(&rt).iter_batched(
                || {
                    let client = Arc::new(BenchDpuClient::new(fleet_dpu_snapshots(n)));
                    let layer = DpuLayer::new(client, dpu_config())
                        .with_infiniband_present(Some(true));
                    (layer, RunOpts::default())
                },
                |(layer, opts)| async move { layer.run(&opts).await },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_dpu_layer);
criterion_main!(benches);
