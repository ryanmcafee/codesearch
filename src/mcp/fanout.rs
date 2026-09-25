//! Bounded parallel per-store reads for multi-repo fan-out.
//!
//! Group queries touch every repo's stores; running them one after another
//! makes latency the sum of all stores instead of the slowest one. Reads run
//! on a dedicated pool at read-path QoS so they never queue behind indexing.

use crate::index::SharedStores;
use crate::qos::{self, ThreadQos};
use crate::serve::ServeState;
use rayon::prelude::*;
use std::sync::{Arc, OnceLock};

/// Upper bound on concurrent per-store reads for one fan-out.
pub(crate) fn fanout_threads() -> usize {
    num_cpus::get().clamp(4, 16)
}

fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(fanout_threads())
            .thread_name(|i| format!("codesearch-fanout-{i}"))
            .start_handler(|i| {
                if let Err(e) = qos::set_current_thread(ThreadQos::UserInitiated) {
                    tracing::warn!("fan-out thread {i}: could not set read QoS: {e:#}");
                }
            })
            .build()
            .map_err(|e| {
                tracing::warn!("fan-out pool unavailable, reading stores sequentially: {e:#}");
            })
            .ok()
    })
    .as_ref()
}

/// Run `read(idx, item)` for every item concurrently; results keep input order.
pub(crate) fn map_ordered<T, R, F>(items: &[T], read: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(usize, &T) -> R + Sync,
{
    match pool() {
        Some(pool) if items.len() > 1 => pool.install(|| {
            items
                .par_iter()
                .enumerate()
                .map(|(idx, item)| read(idx, item))
                .collect()
        }),
        _ => items
            .iter()
            .enumerate()
            .map(|(idx, item)| read(idx, item))
            .collect(),
    }
}

/// Concurrent cold opens per fan-out; each open blocks a runtime worker, so keep it below the worker count.
const STORE_OPEN_CONCURRENCY: usize = 8;

/// Fetch or cold-open every alias's stores concurrently; order matches `aliases`.
///
/// Fails with the first error in alias order, like the sequential loop it replaces.
pub(crate) async fn open_fanout_stores(
    serve_state: &Arc<ServeState>,
    aliases: &[String],
) -> Result<Vec<Arc<SharedStores>>, String> {
    let permits = Arc::new(tokio::sync::Semaphore::new(STORE_OPEN_CONCURRENCY));
    let mut opens = tokio::task::JoinSet::new();
    for (idx, alias) in aliases.iter().enumerate() {
        let state = Arc::clone(serve_state);
        let alias = alias.clone();
        let permits = Arc::clone(&permits);
        opens.spawn(async move {
            let _permit = permits.acquire_owned().await;
            (idx, state.get_or_open_stores(&alias, false).await)
        });
    }
    let mut opened: Vec<Option<Result<Arc<SharedStores>, String>>> = vec![None; aliases.len()];
    while let Some(joined) = opens.join_next().await {
        match joined {
            Ok((idx, result)) => opened[idx] = Some(result),
            Err(e) => return Err(format!("store open task failed: {e}")),
        }
    }
    opened
        .into_iter()
        .zip(aliases)
        .map(|(result, alias)| {
            result.unwrap_or_else(|| Err(format!("store open for '{alias}' did not complete")))
        })
        .collect()
}
