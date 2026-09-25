//! Synthetic multi-repo fixtures for the search fan-out benches and tests.
//!
//! Not a stable API: it exists so `benches/` can drive the real serve-mode
//! fan-out without an embedding model or any store outside a temp dir.

use crate::chunker::{Chunk, ChunkKind};
use crate::constants::{ALL_GROUP_NAME, DB_DIR_NAME};
use crate::db_discovery::repos::ReposConfig;
use crate::embed::EmbeddedChunk;
use crate::mcp::types::{LiteralSearchRequest, SemanticSearchRequest};
use crate::mcp::CodesearchService;
use crate::serve::ServeState;
use anyhow::{anyhow, Result};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Word shared by every chunk so literal and lexical queries hit every store.
pub const SHARED_TERM: &str = "request";

/// Where a fixture query is routed.
#[derive(Debug, Clone)]
pub enum Scope {
    /// `project=<alias>`: one store.
    Project(String),
    /// `group=<name>`: fan-out over the group's stores.
    Group(String),
}

/// N synthetic repos registered with a serve hub, each with its own stores.
pub struct FanoutFixture {
    config_file: PathBuf,
    dims: usize,
    aliases: Vec<String>,
    service: CodesearchService,
}

/// Alias of the `i`th fixture repo; zero-padded so sorted order equals index order.
pub fn repo_alias(i: usize) -> String {
    format!("repo-{i:03}")
}

/// Name of the fixture group holding the first `n` repos.
pub fn group_name(n: usize) -> String {
    format!("first-{n}")
}

/// Deterministic unit vector, so every run searches the same corpus.
pub fn synthetic_vector(seed: u64, dims: usize) -> Vec<f32> {
    let mut state = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut v: Vec<f32> = (0..dims)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) as f32 / u32::MAX as f32) - 0.25
        })
        .collect();
    let norm = v
        .iter()
        .map(|x| x * x)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

fn synthetic_content(store: usize, chunk: usize) -> String {
    format!(
        "pub fn handle_{chunk}_{store}({SHARED_TERM}: &Request) -> Response {{\n    \
         let value = compute_{bucket}({SHARED_TERM}.body());\n    \
         validate_{bucket}(&value)?;\n    Response::ok(value)\n}}",
        bucket = chunk % 97
    )
}

fn text_of(result: CallToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        _ => String::new(),
    }
}

impl FanoutFixture {
    /// Build `stores` repos under `root` with `chunks_per_store` chunks of `dims`-dim vectors.
    ///
    /// Registers a `first-<n>` group for every `n` in `group_sizes`.
    pub async fn build(
        root: &Path,
        stores: usize,
        chunks_per_store: usize,
        dims: usize,
        group_sizes: &[usize],
    ) -> Result<Self> {
        let mut config = ReposConfig::default();
        let aliases: Vec<String> = (0..stores).map(repo_alias).collect();
        for alias in &aliases {
            let db_path = root.join(alias).join(DB_DIR_NAME);
            std::fs::create_dir_all(&db_path)?;
            std::fs::write(
                db_path.join("metadata.json"),
                format!(r#"{{"schema_version":1,"dimensions":{dims},"partial":false}}"#),
            )?;
            config.register_with_alias(root.join(alias), Some(alias.clone()))?;
        }
        for &n in group_sizes {
            config
                .groups
                .insert(group_name(n), aliases.iter().take(n).cloned().collect());
        }
        let config_file = root.join("repos.json");
        config.save_to(&config_file)?;
        let state = Arc::new(ServeState::new(config, Some(config_file.clone())));

        for (store_idx, alias) in aliases.iter().enumerate() {
            let stores = state
                .get_or_open_stores(alias, false)
                .await
                .map_err(|e| anyhow!(e))?;
            let chunks: Vec<EmbeddedChunk> = (0..chunks_per_store)
                .map(|i| {
                    let path = crate::cache::normalize_path_str(
                        root.join(alias)
                            .join(format!("src/file_{}.rs", i / 20))
                            .to_string_lossy()
                            .as_ref(),
                    );
                    let chunk = Chunk::new(
                        synthetic_content(store_idx, i),
                        i * 6 + 1,
                        i * 6 + 5,
                        ChunkKind::Function,
                        path,
                    );
                    let seed = (store_idx * chunks_per_store + i) as u64;
                    EmbeddedChunk::new(chunk, synthetic_vector(seed, dims))
                })
                .collect();
            let meta: Vec<(String, String)> = chunks
                .iter()
                .map(|c| (c.chunk.content.clone(), c.chunk.path.clone()))
                .collect();
            let ids = stores.vector_store.insert_chunks_with_ids(chunks)?;
            stores.vector_store.build_index()?;
            for (id, (content, path)) in ids.into_iter().zip(meta) {
                stores
                    .fts_store
                    .add_chunk(id, &content, &path, None, "Function")?;
            }
            stores.fts_store.commit()?;
        }

        let service = CodesearchService::new_for_serve(state)?;
        Ok(Self {
            config_file,
            dims,
            aliases,
            service,
        })
    }

    /// Embedding dimensions of every fixture store.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Aliases of every fixture repo, in index order.
    pub fn aliases(&self) -> &[String] {
        &self.aliases
    }

    /// Replace the hub with a fresh one, so the next query cold-opens every store.
    pub fn reset_to_cold(&mut self) -> Result<()> {
        let config = ReposConfig::load_from(&self.config_file)?;
        let state = Arc::new(ServeState::new(config, Some(self.config_file.clone())));
        self.service = CodesearchService::new_for_serve(state)?;
        Ok(())
    }

    /// Semantic or hybrid search with a precomputed query embedding; returns the response text.
    pub async fn semantic(&self, scope: &Scope, query_vector: &[f32], mode: &str) -> String {
        let (project, group) = match scope {
            Scope::Project(p) => (Some(p.clone()), None),
            Scope::Group(g) => (None, Some(g.clone())),
        };
        let ctx = match self
            .service
            .resolve_routing(&project, &group, false, "search")
            .await
        {
            Ok(ctx) => ctx,
            Err(e) => return e,
        };
        let (stores, aliases) = match (ctx.stores_vec, ctx.store_aliases, ctx.stores) {
            (Some(sv), Some(sa), _) => (sv, sa),
            (_, _, Some(store)) => (vec![store], ctx.project_alias.into_iter().collect()),
            _ => return "no stores resolved".to_string(),
        };
        let embeddings: HashMap<String, Vec<f32>> = aliases
            .iter()
            .map(|a| (a.clone(), query_vector.to_vec()))
            .collect();
        let request = SemanticSearchRequest {
            query: format!("{SHARED_TERM} handler"),
            limit: Some(10),
            compact: Some(true),
            filter_path: None,
            mode: Some(mode.to_string()),
            project,
            group,
        };
        match self
            .service
            .semantic_search_multi_embedded(
                &request,
                &[],
                10,
                true,
                stores,
                &aliases,
                &ctx.alias_roots,
                &embeddings,
                Vec::new(),
            )
            .await
        {
            Ok(result) => text_of(result),
            Err(e) => format!("{e:?}"),
        }
    }

    /// Literal (BM25) search through the real tool entry point; returns the response text.
    pub async fn literal(&self, scope: &Scope, query: &str) -> String {
        let (project, group) = match scope {
            Scope::Project(p) => (Some(p.clone()), None),
            Scope::Group(g) => (None, Some(g.clone())),
        };
        let request = LiteralSearchRequest {
            query: query.to_string(),
            regex: None,
            phrase: None,
            limit: Some(10),
            file_glob: None,
            language: None,
            format: None,
            project,
            group,
        };
        match self.service.literal_search(Parameters(request)).await {
            Ok(result) => text_of(result),
            Err(e) => format!("{e:?}"),
        }
    }

    /// The service under test, for callers that need a tool this fixture does not wrap.
    pub fn service(&self) -> &CodesearchService {
        &self.service
    }
}

/// The virtual group covering every registered repo.
pub fn all_group() -> Scope {
    Scope::Group(ALL_GROUP_NAME.to_string())
}
