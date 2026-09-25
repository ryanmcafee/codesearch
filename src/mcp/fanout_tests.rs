//! Group fan-out must read stores concurrently and merge them deterministically.

use crate::bench_support::{group_name, FanoutFixture, SHARED_TERM};
use crate::fts::FtsResult;
use crate::mcp::fanout::{fanout_threads, map_ordered};
use crate::mcp::merge_store_reads;
use std::time::{Duration, Instant};

const STORES: usize = 8;
const PER_STORE_DELAY: Duration = Duration::from_millis(100);

async fn fixture(tmp: &tempfile::TempDir) -> FanoutFixture {
    FanoutFixture::build(tmp.path(), STORES, 4, 8, &[STORES])
        .await
        .unwrap()
}

/// Sequential fan-out takes STORES x delay; half of that leaves room for a loaded CI box.
fn assert_concurrent(elapsed: Duration) {
    let sequential = PER_STORE_DELAY * STORES as u32;
    assert!(
        elapsed < sequential / 2,
        "fan-out over {STORES} stores took {elapsed:?}; sequential would be {sequential:?}"
    );
}

#[tokio::test]
async fn fts_fanout_reads_stores_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(&tmp).await;
    let ctx = fx
        .service()
        .resolve_routing(&None, &Some(group_name(STORES)), false, "search")
        .await
        .unwrap();

    let started = Instant::now();
    let outcome = fx
        .service()
        .with_fts_store_read_multi(
            |fts| {
                std::thread::sleep(PER_STORE_DELAY);
                fts.search(SHARED_TERM, 5, None)
            },
            ctx.stores_vec.clone().unwrap(),
            ctx.aliases(),
        )
        .await
        .unwrap();
    assert_concurrent(started.elapsed());
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    let mut stores_hit: Vec<usize> = outcome.results.iter().map(|r| r.store_idx).collect();
    stores_hit.sort_unstable();
    stores_hit.dedup();
    assert_eq!(stores_hit, (0..STORES).collect::<Vec<_>>());
}

#[tokio::test]
async fn vector_fanout_reads_stores_concurrently() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = fixture(&tmp).await;
    let ctx = fx
        .service()
        .resolve_routing(&None, &Some(group_name(STORES)), false, "search")
        .await
        .unwrap();
    let query = crate::bench_support::synthetic_vector(7, fx.dims());

    let started = Instant::now();
    let outcome = fx
        .service()
        .with_vector_store_read_multi(
            |_, store| {
                std::thread::sleep(PER_STORE_DELAY);
                store.search(&query, 5)
            },
            ctx.stores_vec.clone().unwrap(),
            ctx.aliases(),
        )
        .await
        .unwrap();
    assert_concurrent(started.elapsed());
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
    assert_eq!(outcome.results.len(), STORES * 4);
}

#[test]
fn map_ordered_keeps_input_order_when_later_items_finish_first() {
    let items: Vec<u64> = (0..fanout_threads() as u64 * 2).collect();
    let out = map_ordered(&items, |idx, &item| {
        std::thread::sleep(Duration::from_millis(40 - item.min(39)));
        (idx, item)
    });
    assert_eq!(
        out,
        items.iter().copied().enumerate().collect::<Vec<_>>(),
        "results must follow input order, not completion order"
    );
}

fn hit(chunk_id: u32, score: f32) -> FtsResult {
    FtsResult { chunk_id, score }
}

#[test]
fn merge_orders_equal_scores_by_store_and_keeps_failures() {
    let aliases: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
    let per_store = vec![
        Ok(vec![hit(0, 1.0), hit(0, 3.0)]),
        Err(anyhow::anyhow!("store b is down")),
        Ok(vec![hit(0, 1.0), hit(1, 2.0)]),
    ];
    let outcome = merge_store_reads(per_store, &aliases, "FTS");
    let order: Vec<(usize, u32, f32)> = outcome
        .results
        .iter()
        .map(|r| (r.store_idx, r.hit.chunk_id, r.hit.score))
        .collect();
    assert_eq!(order, vec![(0, 0, 3.0), (2, 1, 2.0), (2, 0, 1.0)]);
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(outcome.failures[0].0, "b");
}

#[tokio::test]
async fn group_open_resolves_stores_in_alias_order() {
    let tmp = tempfile::tempdir().unwrap();
    let mut fx = fixture(&tmp).await;
    fx.reset_to_cold().unwrap();
    let ctx = fx
        .service()
        .resolve_routing(&None, &Some(group_name(STORES)), false, "search")
        .await
        .unwrap();
    assert_eq!(ctx.aliases(), fx.aliases());
    let query = crate::bench_support::synthetic_vector(7, fx.dims());
    let outcome = fx
        .service()
        .with_vector_store_read_multi(
            |_, store| store.search(&query, 50),
            ctx.stores_vec.clone().unwrap(),
            ctx.aliases(),
        )
        .await
        .unwrap();
    let paths_match_alias = outcome.results.iter().all(|r| {
        r.hit
            .path
            .contains(&format!("/{}/", fx.aliases()[r.store_idx]))
    });
    assert!(
        paths_match_alias,
        "every store index must map to its own alias after a concurrent cold open"
    );
}
