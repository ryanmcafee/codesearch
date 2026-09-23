use super::*;
use tempfile::tempdir;

#[test]
fn shared_cache_is_one_instance_per_directory_while_held() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("minilm-test");

    let first = PersistentEmbeddingCache::shared_at("minilm-test", path.clone()).unwrap();
    let second = PersistentEmbeddingCache::shared_at("minilm-test", path.clone())
        .expect("a second service must reuse the open env, not trip the double-open guard");
    assert!(Arc::ptr_eq(&first, &second));

    first.lock().unwrap().put("hash-a", &[1.0, 2.0]).unwrap();
    assert_eq!(
        second.lock().unwrap().get("hash-a").unwrap(),
        Some(vec![1.0, 2.0])
    );
}

#[test]
fn shared_cache_closes_when_the_last_holder_drops() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("minilm-test");

    drop(PersistentEmbeddingCache::shared_at("minilm-test", path.clone()).unwrap());

    PersistentEmbeddingCache::open_with_cache_dir("minilm-test", path)
        .expect("the env must be closed once no service holds the shared cache");
}
