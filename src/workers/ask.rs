//! Background "ask your notes" worker.
//!
//! Sends a question to the RAG pipeline off the render thread and returns the
//! answer (or an error) as an [`AskEvent`]. The event types are plain and
//! always compiled (like the embedding worker), so the TUI loop wires the same
//! way with or without the `ai` feature; without it the worker is a no-op.

use tokio::sync::mpsc;
use uuid::Uuid;

use crate::config::ChatConfig;
use crate::storage::SqliteStorage;

/// How many notes to ground an answer on. Only read by the `ai` worker.
#[cfg_attr(not(feature = "ai"), allow(dead_code))]
const ASK_K: usize = 6;

/// A citation backing an answer, resolvable to a note by the UI.
#[derive(Debug, Clone)]
pub struct AskCitation {
    pub index: usize,
    pub note_id: Uuid,
    pub title: String,
}

/// Events emitted by the Ask worker back to the TUI loop. Only constructed in
/// `ai` builds (the worker is inert otherwise).
#[derive(Debug, Clone)]
#[cfg_attr(not(feature = "ai"), allow(dead_code))]
pub enum AskEvent {
    Answer {
        text: String,
        citations: Vec<AskCitation>,
    },
    Error(String),
}

/// Spawn the Ask worker. Send a question string on the returned sender; receive
/// an [`AskEvent`] on the receiver. Questions are processed one at a time.
/// Without the `ai` feature the channel is inert (the worker never runs).
pub fn spawn_ask_worker(
    storage: SqliteStorage,
    chat_cfg: ChatConfig,
) -> (mpsc::Sender<String>, mpsc::Receiver<AskEvent>) {
    let (req_tx, req_rx) = mpsc::channel::<String>(8);
    let (evt_tx, evt_rx) = mpsc::channel::<AskEvent>(8);

    #[cfg(feature = "ai")]
    tokio::spawn(run(storage, chat_cfg, req_rx, evt_tx));

    #[cfg(not(feature = "ai"))]
    {
        let _ = (storage, chat_cfg, req_rx, evt_tx);
    }

    (req_tx, evt_rx)
}

#[cfg(feature = "ai")]
async fn run(
    storage: SqliteStorage,
    chat_cfg: ChatConfig,
    mut req_rx: mpsc::Receiver<String>,
    evt_tx: mpsc::Sender<AskEvent>,
) {
    use crate::ai::ask::answer_question;

    while let Some(question) = req_rx.recv().await {
        let event = match answer_question(&storage, &chat_cfg, &question, ASK_K).await {
            Ok(answer) => AskEvent::Answer {
                text: answer.answer,
                citations: answer
                    .citations
                    .into_iter()
                    .map(|c| AskCitation {
                        index: c.index,
                        note_id: c.note_id,
                        title: c.title,
                    })
                    .collect(),
            },
            Err(e) => AskEvent::Error(format!("{e:#}")),
        };
        if evt_tx.send(event).await.is_err() {
            break; // UI gone
        }
    }
}
