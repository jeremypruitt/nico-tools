//! PRD-005 follow-up (#353): `postgres` layer criterion bench.
//!
//! Pool stats + lock wait list are bounded regardless of fleet size,
//! so no N-sweep. Flat bench with a realistic "5 long-running lock
//! waits" payload — the worst case operators see today.

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use nico_doctor::layer::{Layer, RunOpts};
use nico_doctor::layers::postgres::PostgresLayer;
use nico_doctor::postgres::{LockWait, PoolStats};
use tokio::runtime::Runtime;

mod common;
use common::BenchPostgres;

fn bench_postgres_layer(c: &mut Criterion) {
    let rt = Runtime::new().expect("tokio runtime");

    c.bench_function("postgres_layer/pool_and_locks", |b| {
        b.to_async(&rt).iter_batched(
            || {
                let waits = (0i32..5)
                    .map(|i| LockWait {
                        waiting_pid: 1000 + i,
                        blocking_pid: Some(2000 + i),
                        relation: Some(format!("table_{i}")),
                        wait_secs: 6.0 + f64::from(i),
                    })
                    .collect();
                let pg = Arc::new(BenchPostgres {
                    stats: PoolStats {
                        active: 18,
                        max_conn: 20,
                    },
                    waits,
                });
                let layer = PostgresLayer::new(pg);
                (layer, RunOpts::default())
            },
            |(layer, opts)| async move { layer.run(&opts).await },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_postgres_layer);
criterion_main!(benches);
