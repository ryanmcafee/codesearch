//! Multi-repo search fan-out benchmarks on synthetic temp-dir stores.
//!
//! Sizes: `CODESEARCH_BENCH_STORES` (default 64) and `CODESEARCH_BENCH_CHUNKS`
//! (chunks per store, default 3000).

use codesearch::bench_support::{
    all_group, group_name, repo_alias, synthetic_vector, FanoutFixture, Scope, SHARED_TERM,
};
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::time::{Duration, Instant};

const DIMS: usize = 384;
const GROUP_SIZES: [usize; 3] = [8, 32, 64];

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Harness {
    _tmp: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    fx: FanoutFixture,
    group_sizes: Vec<usize>,
    query: Vec<f32>,
}

fn harness() -> Harness {
    let stores = env_usize("CODESEARCH_BENCH_STORES", 64);
    let chunks = env_usize("CODESEARCH_BENCH_CHUNKS", 3000);
    let group_sizes: Vec<usize> = GROUP_SIZES
        .iter()
        .copied()
        .filter(|&n| n <= stores)
        .collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let tmp = tempfile::tempdir().expect("tempdir");
    let started = Instant::now();
    let fx = rt
        .block_on(FanoutFixture::build(
            tmp.path(),
            stores,
            chunks,
            DIMS,
            &group_sizes,
        ))
        .expect("fixture");
    eprintln!(
        "fixture: {stores} stores x {chunks} chunks built in {:.1?}",
        started.elapsed()
    );
    Harness {
        _tmp: tmp,
        rt,
        fx,
        group_sizes,
        query: synthetic_vector(u64::MAX, DIMS),
    }
}

fn bench_fanout(c: &mut Criterion) {
    let mut h = harness();
    let project = Scope::Project(repo_alias(0));

    let mut single = c.benchmark_group("single_project");
    single.sample_size(20);
    single.bench_function("semantic", |b| {
        b.iter(|| black_box(h.rt.block_on(h.fx.semantic(&project, &h.query, "semantic"))))
    });
    single.bench_function("literal", |b| {
        b.iter(|| black_box(h.rt.block_on(h.fx.literal(&project, SHARED_TERM))))
    });
    single.finish();

    let mut fanout = c.benchmark_group("group_fanout");
    fanout.sample_size(10);
    fanout.measurement_time(Duration::from_secs(10));
    for &n in &h.group_sizes {
        let scope = Scope::Group(group_name(n));
        for mode in ["semantic", "hybrid"] {
            fanout.bench_with_input(BenchmarkId::new(mode, n), &scope, |b, scope| {
                b.iter(|| black_box(h.rt.block_on(h.fx.semantic(scope, &h.query, mode))))
            });
        }
        fanout.bench_with_input(BenchmarkId::new("literal", n), &scope, |b, scope| {
            b.iter(|| black_box(h.rt.block_on(h.fx.literal(scope, SHARED_TERM))))
        });
    }
    fanout.finish();

    let mut cold = c.benchmark_group("cold_first_query");
    cold.sample_size(10);
    cold.measurement_time(Duration::from_secs(20));
    cold.bench_function(
        BenchmarkId::new("semantic_all", h.fx.aliases().len()),
        |b| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    h.fx.reset_to_cold().expect("reset hub");
                    let started = Instant::now();
                    black_box(h.rt.block_on(h.fx.semantic(&all_group(), &h.query, "semantic")));
                    total += started.elapsed();
                }
                total
            })
        },
    );
    cold.finish();
}

criterion_group!(benches, bench_fanout);
criterion_main!(benches);
