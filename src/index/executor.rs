//! Dedicated low-priority pool for CPU-heavy indexing work (chunking, ONNX
//! embedding, HNSW builds), isolated from the tokio threads that serve reads.
//!
//! Every pool thread sets its own QoS at start (macOS does not propagate QoS
//! to child threads), ONNX runs single-threaded on the calling pool thread,
//! and rayon work started here (arroy's build) stays on this pool.

use crate::embed::{EmbeddedChunk, EmbeddingService, ModelType};
use crate::qos::{self, ThreadQos};
use anyhow::Result;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::OnceLock;

/// An indexing task panicked or was dropped before finishing.
#[derive(Debug)]
pub struct IndexTaskError(String);

impl std::fmt::Display for IndexTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IndexTaskError {}

pub struct IndexExecutor {
    pool: rayon::ThreadPool,
    threads: usize,
    qos: ThreadQos,
}

impl IndexExecutor {
    pub fn new(threads: usize, qos: ThreadQos) -> Result<Self> {
        let threads = threads.max(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("codesearch-index-{i}"))
            .start_handler(move |i| {
                if let Err(e) = qos::set_current_thread(qos) {
                    tracing::warn!("index thread {i}: could not lower priority: {e:#}");
                }
            })
            .build()?;
        Ok(Self { pool, threads, qos })
    }

    /// Number of pool threads (also the embedding fan-out).
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// QoS class the pool threads run at.
    pub fn qos(&self) -> ThreadQos {
        self.qos
    }

    /// Run `f` on the pool; drop-in for `tokio::task::spawn_blocking` on index paths.
    pub async fn run<T, F>(&self, f: F) -> std::result::Result<T, IndexTaskError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pool.spawn(move || {
            let _ = tx.send(panic::catch_unwind(AssertUnwindSafe(f)));
        });
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(payload)) => Err(IndexTaskError(format!(
                "indexing task panicked: {}",
                panic_message(&payload)
            ))),
            Err(_) => Err(IndexTaskError(
                "indexing task was dropped before it finished".to_string(),
            )),
        }
    }

    /// Embed `chunks` across the pool, preserving order. Call from inside [`Self::run`].
    pub fn embed_chunks(
        &self,
        model: ModelType,
        cache_dir: &Path,
        chunks: Vec<crate::chunker::Chunk>,
    ) -> Result<Vec<EmbeddedChunk>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }
        let per_thread = chunks.len().div_ceil(self.threads);
        let parts: Vec<Vec<crate::chunker::Chunk>> = chunks
            .chunks(per_thread)
            .map(<[crate::chunker::Chunk]>::to_vec)
            .collect();
        let cache_dir = cache_dir.to_path_buf();
        let embedded: Vec<Vec<EmbeddedChunk>> = self.pool.install(|| {
            parts
                .into_par_iter()
                .map(|part| with_thread_embedder(model, &cache_dir, |svc| svc.embed_chunks(part)))
                .collect::<Result<_>>()
        })?;
        Ok(embedded.into_iter().flatten().collect())
    }
}

thread_local! {
    static EMBEDDERS: RefCell<HashMap<ModelType, EmbeddingService>> = RefCell::new(HashMap::new());
}

/// Run `f` with this thread's long-lived indexing embedder for `model`, loading it once.
fn with_thread_embedder<R>(
    model: ModelType,
    cache_dir: &Path,
    f: impl FnOnce(&mut EmbeddingService) -> Result<R>,
) -> Result<R> {
    EMBEDDERS.with(|cell| {
        let mut embedders = cell.borrow_mut();
        let service = match embedders.entry(model) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(EmbeddingService::for_indexing(model, Some(cache_dir))?)
            }
        };
        f(service)
    })
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string())
}

/// Pool size from `CODESEARCH_INDEX_THREADS`, else half the cores (1..=4).
pub fn configured_threads() -> usize {
    std::env::var(crate::constants::INDEX_THREADS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            (cores / 2).clamp(1, 4)
        })
}

/// Pool QoS from `CODESEARCH_INDEX_QOS` (`utility` | `background`), default utility.
pub fn configured_qos() -> ThreadQos {
    match std::env::var(crate::constants::INDEX_QOS_ENV) {
        Ok(value) => ThreadQos::parse(&value).unwrap_or_else(|| {
            tracing::warn!(
                "{}={value:?} is not utility|background|user-initiated; using utility",
                crate::constants::INDEX_QOS_ENV
            );
            ThreadQos::Utility
        }),
        Err(_) => ThreadQos::Utility,
    }
}

/// The process-wide indexing pool, sized and prioritized from the environment.
pub fn global() -> &'static IndexExecutor {
    static EXECUTOR: OnceLock<IndexExecutor> = OnceLock::new();
    EXECUTOR.get_or_init(|| {
        let threads = configured_threads();
        let qos = configured_qos();
        tracing::info!("indexing pool: {threads} thread(s) at {qos:?} QoS");
        IndexExecutor::new(threads, qos)
            .unwrap_or_else(|e| panic!("cannot start the {threads}-thread indexing pool: {e:#}"))
    })
}

/// Run `f` on the global indexing pool.
pub async fn spawn_index_blocking<T, F>(f: F) -> std::result::Result<T, IndexTaskError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    global().run(f).await
}

#[cfg(test)]
#[path = "executor_tests.rs"]
mod tests;
