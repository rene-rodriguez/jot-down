//! Local, in-process AI building blocks.
//!
//! Everything here runs on the user's machine with no network and no API key,
//! in keeping with Jot's local-models-first stance. The [`Embedder`] trait is
//! the stable seam: the embedding worker and retriever depend on it, so the
//! backend (today a deterministic lexical embedder, later a transformer model)
//! can be swapped without touching call sites.

pub mod ask;
pub mod chat;
pub mod embedder;
pub mod retriever;

pub use embedder::{Embedder, LocalEmbedder};

/// The embedding backend the rest of the app uses (worker, retriever, and the
/// startup reindex). Centralizing construction here gives a single place to
/// swap in a different model — and a single identity (`Embedder::id` /
/// `dimensions`) the persisted index is keyed on. The startup path compares
/// that identity against what the index was last built with and reindexes on a
/// change, so dropping a transformer embedder in here is all it takes.
pub fn active_embedder() -> LocalEmbedder {
    LocalEmbedder::new()
}
