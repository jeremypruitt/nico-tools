//! PRD-005 Slice 0a.2: render-bench.
//!
//! Drives `view::render` against an `App` warmed with N fleet
//! snapshots, using `ratatui::backend::TestBackend` as the render
//! target. Reports per-frame wall-clock under criterion; when the
//! `dhat-heap` feature is enabled, also captures per-frame allocation
//! count via dhat (writes `dhat-heap.json` for offline inspection).
//!
//! Quantifies Findings #2 + #3 (sparkline recomputation + per-card
//! `evidence.clone()`) by producing a per-frame alloc-count baseline
//! the next slice can rank against.

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use nico_common::theme::DEFAULT;
use nico_ops::view::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

mod common;
use common::warmed_app;

const RENDER_SIZES: &[usize] = &[1, 18, 250];
const TERMINAL_WIDTH: u16 = 160;
const TERMINAL_HEIGHT: u16 = 48;

fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");
    group.sample_size(50);

    for &n in RENDER_SIZES {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let app = warmed_app(n);
                    let backend = TestBackend::new(TERMINAL_WIDTH, TERMINAL_HEIGHT);
                    let terminal = Terminal::new(backend).expect("terminal");
                    (app, terminal)
                },
                |(mut app, mut terminal)| {
                    terminal
                        .draw(|frame| render(&mut app, &DEFAULT, frame))
                        .expect("draw");
                    (app, terminal)
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_render);
criterion_main!(benches);
