//! Background embedding worker.
//!
//! Drains `embedding_queue` (populated on every note save by the storage
//! layer), embeds the note text with the local [`Embedder`], and writes the
//! vectors — all off the render thread. It only does real work when the
//! sqlite-vec index is available; otherwise it reports once and exits, so the
//! TUI degrades gracefully.
//!
//! [`Embedder`]: crate::ai::Embedder

use tokio::sync::mpsc;

use crate::storage::SqliteStorage;

#[cfg(feature = "ai")]
use chrono::Utc;
#[cfg(feature = "ai")]
use tokio::time::{self, Duration};

/// How long to wait before retrying a failed embedding job, given how many
/// times it has already failed. Exponential — 5s, 10s, 20s, … — capped at five
/// minutes so a persistently-failing note is retried occasionally (e.g. once
/// storage recovers) without churning every batch. `attempts` is the count
/// before the current failure.
#[cfg(feature = "ai")]
fn backoff_delay(attempts: i32) -> chrono::Duration {
    const MAX_SECS: i64 = 300;
    let exp = attempts.clamp(0, 6) as u32; // cap the shift so it can't overflow
    let secs = (5_i64.saturating_mul(1_i64 << exp)).min(MAX_SECS);
    chrono::Duration::seconds(secs)
}

/// Events emitted by the embedding worker back to the TUI loop. Only constructed
/// in `ai` builds (the worker is inert otherwise).
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "ai"), allow(dead_code))]
pub enum EmbeddingEvent {
    /// A drain pass finished: `embedded` notes indexed this pass, `pending`
    /// jobs still queued.
    Progress { embedded: usize, pending: i64 },
    /// Indexing is unavailable (vector index missing / AI off). Sent once.
    Unavailable { reason: String },
}

/// Spawn the background embedding worker.
///
/// Always returns a receiver. With the `ai` feature off — or the vector index
/// unavailable at runtime — the worker emits at most one [`EmbeddingEvent::
/// Unavailable`] and exits, leaving the channel closed (an inert `select`
/// arm), mirroring the dummy-receiver pattern used for sync.
pub fn spawn_embedding_worker(storage: SqliteStorage) -> mpsc::Receiver<EmbeddingEvent> {
    let (tx, rx) = mpsc::channel(16);

    #[cfg(feature = "ai")]
    tokio::spawn(async move {
        if let Err(e) = run(storage, tx).await {
            tracing::error!("Embedding worker exited with error: {:?}", e);
        }
    });

    #[cfg(not(feature = "ai"))]
    {
        // No worker without the ai feature; dropping `tx` closes the channel.
        let _ = (storage, tx);
    }

    rx
}

#[cfg(feature = "ai")]
async fn run(storage: SqliteStorage, tx: mpsc::Sender<EmbeddingEvent>) -> anyhow::Result<()> {
    use crate::ai::{active_embedder, Embedder};

    // Without a usable vector index there is nowhere to store vectors; report
    // once and stop rather than spin.
    if !storage.ai_available() {
        let _ = tx
            .send(EmbeddingEvent::Unavailable {
                reason: storage.ai_status_reason(),
            })
            .await;
        return Ok(());
    }

    let embedder = active_embedder();
    const BATCH: i64 = 16;
    const IDLE: Duration = Duration::from_secs(2);

    loop {
        let jobs = storage.fetch_embedding_batch(BATCH).await?;
        if jobs.is_empty() {
            time::sleep(IDLE).await;
            continue;
        }

        let mut embedded = 0usize;
        for job in jobs {
            match embedder.embed_one(&job.text) {
                Ok(vector) => {
                    match storage
                        .store_note_embedding(
                            job.note_id,
                            embedder.id(),
                            embedder.dimensions(),
                            &job.content_hash,
                            &vector,
                        )
                        .await
                    {
                        Ok(()) => embedded += 1,
                        Err(e) => {
                            tracing::warn!("store embedding for {} failed: {:?}", job.note_id, e);
                            let retry_after = Utc::now() + backoff_delay(job.attempts);
                            let _ = storage
                                .mark_embedding_failed(job.note_id, &e.to_string(), retry_after)
                                .await;
                        }
                    }
                }
                Err(e) => {
                    let retry_after = Utc::now() + backoff_delay(job.attempts);
                    let _ = storage
                        .mark_embedding_failed(job.note_id, &e.to_string(), retry_after)
                        .await;
                }
            }
        }

        let pending = storage.count_pending_embeddings().await.unwrap_or(0);
        if tx
            .send(EmbeddingEvent::Progress { embedded, pending })
            .await
            .is_err()
        {
            break; // UI is gone
        }
    }

    Ok(())
}
