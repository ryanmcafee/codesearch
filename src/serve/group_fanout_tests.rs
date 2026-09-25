//! Group fan-out must resolve every hit against the store that produced it.
//!
//! Chunk ids are allocated per store starting at 0, so the same id exists in
//! every repo of a group. These fixtures give two repos a colliding chunk 0
//! with different content and assert each tool returns the matching repo's
//! chunk, never the first store that happens to hold that id.

use super::ServeState;
use crate::chunker::{Chunk, ChunkKind};
use crate::constants::DB_DIR_NAME;
use crate::db_discovery::repos::ReposConfig;
use crate::embed::EmbeddedChunk;
use crate::mcp::CodesearchService;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;
use std::sync::Arc;

const GROUP: &str = "pair";

struct GroupFixture {
    _tmp: tempfile::TempDir,
    service: CodesearchService,
}

/// Registers `repo-a` and `repo-b` in one group, each holding one chunk with id 0.
async fn group_fixture(repo_a: (&str, ChunkKind), repo_b: (&str, ChunkKind)) -> GroupFixture {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = ReposConfig::default();
    let repos = [("repo-a", repo_a), ("repo-b", repo_b)];
    for (alias, _) in &repos {
        let repo_path = tmp.path().join(alias);
        let db_path = repo_path.join(DB_DIR_NAME);
        std::fs::create_dir_all(&db_path).unwrap();
        std::fs::write(
            db_path.join("metadata.json"),
            r#"{"schema_version":1,"dimensions":2,"partial":false}"#,
        )
        .unwrap();
        config
            .register_with_alias(repo_path, Some(alias.to_string()))
            .unwrap();
    }
    config.groups.insert(
        GROUP.to_string(),
        repos.iter().map(|(a, _)| a.to_string()).collect(),
    );
    let config_file = tmp.path().join("repos.json");
    config.save_to(&config_file).unwrap();
    let state = Arc::new(ServeState::new(config, Some(config_file)));

    for (alias, (content, kind)) in repos {
        let stores = state.get_or_open_stores(alias, false).await.unwrap();
        let path = crate::cache::normalize_path_str(
            tmp.path()
                .join(alias)
                .join("src/lib.rs")
                .to_string_lossy()
                .as_ref(),
        );
        let kind_name = format!("{kind:?}");
        let chunk = Chunk::new(content.to_string(), 1, 1, kind, path.clone());
        stores
            .vector_store
            .insert_chunks(vec![EmbeddedChunk::new(chunk, vec![0.0, 1.0])])
            .unwrap();
        stores.vector_store.build_index().unwrap();
        let chunk_id = stores
            .vector_store
            .chunks_for_file(&path)
            .unwrap()
            .first()
            .map(|c| c.id)
            .unwrap();
        assert_eq!(chunk_id, 0, "precondition: both repos must hold chunk 0");
        stores
            .fts_store
            .add_chunk(chunk_id, content, &path, None, &kind_name)
            .unwrap();
        stores.fts_store.commit().unwrap();
    }

    let service = CodesearchService::new_for_serve(state).unwrap();
    GroupFixture { _tmp: tmp, service }
}

fn text_of(result: CallToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn assert_only_repo_b(text: &str, needle: &str) {
    assert!(
        text.contains("repo-b"),
        "hit must resolve to repo-b: {text}"
    );
    assert!(
        text.contains(needle),
        "hit must carry repo-b's content: {text}"
    );
    assert!(
        !text.contains("repo-a") && !text.contains("alpha"),
        "repo-a's colliding chunk 0 must not be returned: {text}"
    );
}

#[tokio::test]
async fn group_literal_search_resolves_hit_in_its_own_store() {
    let fx = group_fixture(
        ("fn alpha() {}", ChunkKind::Function),
        ("fn beta() {}", ChunkKind::Function),
    )
    .await;
    let request = serde_json::from_value(json!({"query": "beta", "group": GROUP})).unwrap();
    let text = text_of(
        fx.service
            .literal_search(Parameters(request))
            .await
            .unwrap(),
    );
    assert_only_repo_b(&text, "beta");
}

#[tokio::test]
async fn group_lexical_search_resolves_hit_in_its_own_store() {
    let fx = group_fixture(
        ("fn alpha() {}", ChunkKind::Function),
        ("fn beta() {}", ChunkKind::Function),
    )
    .await;
    let request = serde_json::from_value(json!({
        "query": "beta",
        "mode": "lexical",
        "compact": false,
        "group": GROUP
    }))
    .unwrap();
    let text = text_of(
        fx.service
            .semantic_search(Parameters(request))
            .await
            .unwrap(),
    );
    assert_only_repo_b(&text, "beta");
}

#[tokio::test]
async fn group_find_definition_resolves_hit_in_its_own_store() {
    let fx = group_fixture(
        ("fn alpha() {}", ChunkKind::Function),
        ("fn beta() {}", ChunkKind::Function),
    )
    .await;
    let request = serde_json::from_value(json!({
        "symbol": "beta",
        "kind": "definition",
        "group": GROUP
    }))
    .unwrap();
    let text = text_of(fx.service.find(Parameters(request)).await.unwrap());
    assert_only_repo_b(&text, "src/lib.rs");
}

#[tokio::test]
async fn group_find_usages_resolves_hit_in_its_own_store() {
    let fx = group_fixture(
        ("alpha_value = 1;", ChunkKind::Block),
        ("beta();", ChunkKind::Block),
    )
    .await;
    let request = serde_json::from_value(json!({
        "symbol": "beta",
        "kind": "usages",
        "group": GROUP
    }))
    .unwrap();
    let text = text_of(fx.service.find(Parameters(request)).await.unwrap());
    assert_only_repo_b(&text, "src/lib.rs");
}

#[tokio::test]
async fn group_find_dependents_resolves_hit_in_its_own_store() {
    let fx = group_fixture(
        ("use alpha;", ChunkKind::Imports),
        ("use beta;", ChunkKind::Imports),
    )
    .await;
    let request = serde_json::from_value(json!({
        "symbol_or_path": "beta",
        "group": GROUP
    }))
    .unwrap();
    let text = text_of(
        fx.service
            .find_dependents(Parameters(request))
            .await
            .unwrap(),
    );
    assert_only_repo_b(&text, "use beta;");
}

#[tokio::test]
async fn group_similar_keeps_neighbours_sharing_the_source_chunk_id() {
    let fx = group_fixture(
        ("fn alpha() {}", ChunkKind::Function),
        ("fn beta() {}", ChunkKind::Function),
    )
    .await;
    let request = serde_json::from_value(json!({
        "kind": "similar",
        "target": "0",
        "group": GROUP
    }))
    .unwrap();
    let text = text_of(fx.service.explore(Parameters(request)).await.unwrap());
    assert!(
        text.contains("repo-b"),
        "repo-b's chunk 0 is a neighbour, not the source chunk: {text}"
    );
    assert!(
        !text.contains("repo-a"),
        "the source chunk itself must be excluded: {text}"
    );
}
