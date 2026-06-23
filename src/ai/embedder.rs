//! Text embedding for semantic search and ask-your-notes.
//!
//! [`Embedder`] is the stable interface consumed by the embedding worker and
//! the retriever. [`LocalEmbedder`] is the default backend: a deterministic,
//! dependency-free **lexical** embedder (feature-hashed unigrams + bigrams,
//! L2-normalized). It is not a neural model — it captures lexical overlap, not
//! deep semantics — but it is fully offline, fast, and deterministic (the same
//! text always yields the same vector, which matters because vectors are
//! persisted and only recomputed when a note's `content_hash` changes).
//!
//! It exists so the rest of the pipeline (vector storage, the embedding worker,
//! hybrid retrieval) can be built and tested end-to-end today. A transformer
//! backed embedder (fastembed/candle) is intended to drop in behind this same
//! trait; because it will report a different [`Embedder::id`], the model
//! management flow can trigger a one-time reindex when it lands.

use anyhow::Result;

/// Default embedding width. Matches `vec_notes.embedding FLOAT[384]` and the
/// seeded `embedding_dimensions` metadata in the AI migrations.
pub const DEFAULT_DIMENSIONS: usize = 384;

/// Produces fixed-width embedding vectors for text.
pub trait Embedder: Send + Sync {
    /// Stable identifier for the model/algorithm. Persisted alongside each
    /// vector so a backend change can be detected and trigger a reindex.
    fn id(&self) -> &str;

    /// Width of every vector this embedder produces.
    fn dimensions(&self) -> usize;

    /// Embed a batch of texts, returning one vector per input in order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Convenience wrapper for embedding a single text.
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed(std::slice::from_ref(&text.to_string()))?;
        Ok(out.pop().unwrap_or_default())
    }
}

/// Deterministic, offline lexical embedder (the default backend).
#[derive(Debug, Clone)]
pub struct LocalEmbedder {
    dimensions: usize,
}

impl LocalEmbedder {
    /// Create an embedder producing [`DEFAULT_DIMENSIONS`]-wide vectors.
    pub fn new() -> Self {
        Self {
            dimensions: DEFAULT_DIMENSIONS,
        }
    }

    /// Create an embedder with a custom width (clamped to at least 1).
    pub fn with_dimensions(dimensions: usize) -> Self {
        Self {
            dimensions: dimensions.max(1),
        }
    }
}

impl Default for LocalEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl Embedder for LocalEmbedder {
    fn id(&self) -> &str {
        "hashed-bow-v1"
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| embed_text(text, self.dimensions))
            .collect())
    }
}

/// Embed one text into a normalized feature-hashed vector.
fn embed_text(text: &str, dimensions: usize) -> Vec<f32> {
    let mut vector = vec![0.0f32; dimensions];
    let tokens = tokenize(text);

    for token in &tokens {
        accumulate(&mut vector, token);
    }
    // Bigrams add a little word-order signal beyond a pure bag of words.
    for pair in tokens.windows(2) {
        accumulate(&mut vector, &format!("{} {}", pair[0], pair[1]));
    }

    normalize(&mut vector);
    vector
}

/// Hash one feature into the vector with a deterministic sign.
fn accumulate(vector: &mut [f32], feature: &str) {
    let hash = fnv1a(feature.as_bytes());
    let index = (hash % vector.len() as u64) as usize;
    // Use the top bit for the sign so it is decorrelated from the index bits.
    let sign = if (hash >> 63) & 1 == 1 { -1.0 } else { 1.0 };
    vector[index] += sign;
}

/// L2-normalize in place. A zero vector (e.g. empty text) is left untouched.
fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vector.iter_mut() {
            *x /= norm;
        }
    }
}

/// Split into lowercase alphanumeric tokens.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// FNV-1a, 64-bit. A fixed hash (not `DefaultHasher`) so persisted vectors stay
/// stable across builds and Rust versions.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn id_and_dimensions_match_schema() {
        let embedder = LocalEmbedder::new();
        assert_eq!(embedder.dimensions(), DEFAULT_DIMENSIONS);
        assert_eq!(embedder.dimensions(), 384);
        assert_eq!(embedder.id(), "hashed-bow-v1");
    }

    #[test]
    fn produces_fixed_width_normalized_vectors() {
        let embedder = LocalEmbedder::new();
        let out = embedder.embed(&["hello world".to_string()]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 384);
        let norm = out[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
    }

    #[test]
    fn is_deterministic() {
        let embedder = LocalEmbedder::new();
        let a = embedder.embed(&["rust note taking".to_string()]).unwrap();
        let b = embedder.embed(&["rust note taking".to_string()]).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn empty_text_is_zero_vector() {
        let embedder = LocalEmbedder::new();
        let out = embedder.embed(&[String::new()]).unwrap();
        assert!(out[0].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn batch_matches_individual() {
        let embedder = LocalEmbedder::new();
        let batch = embedder
            .embed(&["one".to_string(), "two".to_string()])
            .unwrap();
        let single = embedder.embed(&["one".to_string()]).unwrap();
        assert_eq!(batch[0], single[0]);
    }

    #[test]
    fn related_text_scores_above_unrelated() {
        let embedder = LocalEmbedder::new();
        let query = embedder.embed_one("postgres database sync").unwrap();
        let related = embedder.embed_one("syncing the postgres database").unwrap();
        let unrelated = embedder.embed_one("a poem about autumn leaves").unwrap();
        assert!(
            cosine(&query, &related) > cosine(&query, &unrelated),
            "related text should score higher than unrelated"
        );
    }

    #[test]
    fn embed_one_matches_batch() {
        let embedder = LocalEmbedder::new();
        let one = embedder.embed_one("hello").unwrap();
        let batch = embedder.embed(&["hello".to_string()]).unwrap();
        assert_eq!(one, batch[0]);
    }
}
