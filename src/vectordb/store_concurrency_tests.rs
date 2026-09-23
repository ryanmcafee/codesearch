use super::*;
use crate::chunker::{Chunk, ChunkKind};
use crate::embed::EmbeddedChunk;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use tempfile::tempdir;

fn chunk(path: &str, content: &str, embedding: [f32; 4]) -> EmbeddedChunk {
    EmbeddedChunk::new(
        Chunk::new(
            content.to_string(),
            0,
            1,
            ChunkKind::Function,
            path.to_string(),
        ),
        embedding.to_vec(),
    )
}

fn indexed_store(db_path: &Path) -> VectorStore {
    let store = VectorStore::new(db_path, 4).unwrap();
    store
        .insert_chunks_with_ids(vec![chunk(
            "auth.rs",
            "fn authenticate() {}",
            [1.0, 0.0, 0.0, 0.0],
        )])
        .unwrap();
    store.build_index().unwrap();
    store
}

#[test]
fn vector_store_is_shareable_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VectorStore>();
}

#[test]
fn inserts_into_an_indexed_store_publish_a_searchable_snapshot() {
    let dir = tempdir().unwrap();
    let store = indexed_store(&dir.path().join("v.db"));

    store
        .insert_chunks_with_ids(vec![chunk(
            "fresh.rs",
            "fn fresh() {}",
            [0.0, 0.0, 1.0, 0.0],
        )])
        .unwrap();

    assert!(
        store.is_indexed(),
        "an insert into a serving store must publish a built index, not NeedBuild"
    );
    let results = store.search(&[0.0, 0.0, 1.0, 0.0], 1).unwrap();
    assert!(results[0].content.contains("fresh"));
}

#[test]
fn deletes_from_an_indexed_store_publish_a_searchable_snapshot() {
    let dir = tempdir().unwrap();
    let store = indexed_store(&dir.path().join("v.db"));
    let ids = store
        .insert_chunks_with_ids(vec![chunk("gone.rs", "fn gone() {}", [0.0, 1.0, 0.0, 0.0])])
        .unwrap();

    store.delete_chunks(&ids).unwrap();

    assert!(store.is_indexed());
    let results = store.search(&[0.0, 1.0, 0.0, 0.0], 5).unwrap();
    assert!(results.iter().all(|r| !r.content.contains("gone")));
    assert!(results.iter().any(|r| r.content.contains("authenticate")));
}

#[test]
fn inserts_into_an_unbuilt_store_defer_the_build() {
    let dir = tempdir().unwrap();
    let store = VectorStore::new(&dir.path().join("v.db"), 4).unwrap();

    store
        .insert_chunks_with_ids(vec![chunk("a.rs", "fn a() {}", [1.0, 0.0, 0.0, 0.0])])
        .unwrap();

    assert!(
        !store.is_indexed(),
        "a fresh store builds once at the end, not per batch"
    );
    store.build_index().unwrap();
    assert!(store.is_indexed());
}

#[test]
fn search_reads_the_committed_snapshot_while_a_writer_is_mid_transaction() {
    let dir = tempdir().unwrap();
    let store = Arc::new(indexed_store(&dir.path().join("v.db")));

    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let writer = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            store.with_open_write_txn_for_test(|| {
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        })
    };
    held_rx.recv().unwrap();

    let (result_tx, result_rx) = mpsc::channel();
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            result_tx
                .send(
                    store
                        .search(&[1.0, 0.0, 0.0, 0.0], 1)
                        .map(|r| r[0].content.clone()),
                )
                .unwrap();
        });
    }
    let found = result_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("search must not wait for an in-flight writer")
        .unwrap();
    assert!(found.contains("authenticate"));

    release_tx.send(()).unwrap();
    writer.join().unwrap().unwrap();
}

#[test]
fn replace_chunks_swaps_a_files_chunks_in_one_snapshot() {
    let dir = tempdir().unwrap();
    let store = indexed_store(&dir.path().join("v.db"));
    let old = store
        .insert_chunks_with_ids(vec![chunk(
            "lib.rs",
            "fn old_version() {}",
            [0.0, 1.0, 0.0, 0.0],
        )])
        .unwrap();

    let new = store
        .replace_chunks(
            &old,
            vec![chunk("lib.rs", "fn new_version() {}", [0.0, 1.0, 0.0, 0.0])],
        )
        .unwrap();

    assert_eq!(new.len(), 1);
    assert!(store.is_indexed());
    let results = store.search(&[0.0, 1.0, 0.0, 0.0], 5).unwrap();
    assert!(results.iter().any(|r| r.content.contains("new_version")));
    assert!(results.iter().all(|r| !r.content.contains("old_version")));
    assert!(store.get_chunk(old[0]).unwrap().is_none());
}
