//! Hybrid retrieval over notes.
//!
//! Fuses a semantic ranking (vector k-NN over the embedding index) with the
//! existing keyword ranking (substring match) using Reciprocal Rank Fusion.
//! Degrades gracefully to keyword-only when the vector index is unavailable or
//! the query embeds to nothing, so search always returns something useful.

use std::collections::HashMap;

use anyhow::Result;
use uuid::Uuid;

use crate::ai::{Embedder, LocalEmbedder};
use crate::models::note::NoteSummary;
use crate::storage::SqliteStorage;

/// Reciprocal-rank-fusion damping constant (the standard default).
const RRF_K: f32 = 60.0;

/// Search notes by meaning, fusing vector similarity with keyword matches.
/// Returns up to `k` summaries ordered by fused relevance.
pub async fn hybrid_search(
    storage: &SqliteStorage,
    query: &str,
    k: usize,
) -> Result<Vec<NoteSummary>> {
    // Keyword ranking is always available.
    let keyword = storage.search_notes(query).await?;

    // Semantic ranking, only when the vector index is ready.
    let semantic_ids = if storage.ai_available() {
        let embedding = crate::ai::active_embedder().embed_one(query)?;
        storage
            .semantic_note_ids(&embedding, k as i64)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Nothing semantic to fuse — keep keyword order as-is.
    if semantic_ids.is_empty() {
        return Ok(keyword.into_iter().take(k).collect());
    }

    // Reciprocal Rank Fusion over the two ranked id lists.
    let mut scores: HashMap<Uuid, f32> = HashMap::new();
    for (rank, note) in keyword.iter().enumerate() {
        *scores.entry(note.id).or_default() += rrf(rank);
    }
    for (rank, id) in semantic_ids.iter().enumerate() {
        *scores.entry(*id).or_default() += rrf(rank);
    }

    // Hydrate summaries for the union of ids, then order by fused score.
    let summaries = summary_map(storage, keyword, &semantic_ids).await?;
    let mut ranked: Vec<(Uuid, f32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

    Ok(ranked
        .into_iter()
        .take(k)
        .filter_map(|(id, _)| summaries.get(&id).cloned())
        .collect())
}

fn rrf(rank: usize) -> f32 {
    1.0 / (RRF_K + rank as f32 + 1.0)
}

/// Build an id → summary map from the keyword hits plus any semantic-only ids.
async fn summary_map(
    storage: &SqliteStorage,
    keyword: Vec<NoteSummary>,
    semantic_ids: &[Uuid],
) -> Result<HashMap<Uuid, NoteSummary>> {
    let mut map: HashMap<Uuid, NoteSummary> =
        keyword.into_iter().map(|note| (note.id, note)).collect();

    let missing: Vec<Uuid> = semantic_ids
        .iter()
        .copied()
        .filter(|id| !map.contains_key(id))
        .collect();

    if !missing.is_empty() {
        for summary in storage.summaries_for_ids(&missing).await? {
            map.insert(summary.id, summary);
        }
    }

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::models::note::CreateNoteInput;

    async fn ready_storage() -> SqliteStorage {
        SqliteStorage::connect_with_ai(Path::new(":memory:"), true)
            .await
            .expect("connect with ai")
    }

    async fn index(storage: &SqliteStorage, title: &str, body: &str) -> Uuid {
        let note = storage
            .create_note(CreateNoteInput {
                title: title.to_string(),
                body: body.to_string(),
            })
            .await
            .expect("create");
        let embedder = LocalEmbedder::new();
        let vector = embedder
            .embed_one(&format!("{title}\n\n{body}"))
            .expect("embed");
        storage
            .store_note_embedding(
                note.id,
                embedder.id(),
                embedder.dimensions(),
                &note.content_hash,
                &vector,
            )
            .await
            .expect("store");
        note.id
    }

    #[tokio::test]
    async fn ranks_semantically_related_note_first() {
        let storage = ready_storage().await;
        assert!(storage.ai_available());

        let rust_id = index(&storage, "Rust async", "tokio futures and await in rust").await;
        index(
            &storage,
            "Dinner",
            "a recipe for roasted vegetables and pasta",
        )
        .await;

        let results = hybrid_search(&storage, "async rust programming", 10)
            .await
            .expect("search");
        assert!(!results.is_empty());
        assert_eq!(results[0].id, rust_id, "the rust note should rank first");
    }

    #[tokio::test]
    async fn finds_note_that_keyword_search_misses() {
        let storage = ready_storage().await;
        let id = index(
            &storage,
            "Postgres tips",
            "vacuum and analyze keep the database healthy",
        )
        .await;

        // The exact phrase isn't a substring of the note, so keyword search
        // returns nothing — but the semantic side shares "database".
        assert!(storage
            .search_notes("database maintenance")
            .await
            .expect("keyword")
            .is_empty());

        let results = hybrid_search(&storage, "database maintenance", 10)
            .await
            .expect("search");
        assert!(results.iter().any(|n| n.id == id));
    }
}
