use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::models::note::{CreateNoteInput, Note, UpdateNoteInput};
use crate::storage::SqliteStorage;

/// Commands sent from the TUI to the persistence worker.
#[derive(Debug)]
pub enum PersistenceCommand {
    /// Create a new note.
    CreateNote {
        input: CreateNoteInput,
        reply: oneshot::Sender<Result<Note>>,
    },
    /// Update an existing note (title + body).
    UpdateNote {
        input: UpdateNoteInput,
        reply: oneshot::Sender<Result<Note>>,
    },
    /// Soft-delete a note.
    DeleteNote {
        note_id: Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Add a tag to a note.
    AddTag {
        note_id: Uuid,
        tag_name: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Remove a tag from a note.
    RemoveTag {
        note_id: Uuid,
        tag_name: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Events emitted by the persistence worker back to the TUI loop.
///
/// `NoteDeleted.note_id` and `Error` are reserved for richer status reporting
/// the TUI doesn't consume yet (the worker currently surfaces errors through
/// each command's `reply` channel instead), hence the `allow`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum PersistenceEvent {
    /// A note was deleted.
    NoteDeleted { note_id: Uuid },
    /// A recoverable error the TUI can surface (reserved; not emitted yet).
    Error { message: String },
}

/// Spawn the persistence worker task.
///
/// Returns a sender for commands and a receiver for events.
pub fn spawn_persistence_worker(
    storage: SqliteStorage,
) -> (
    mpsc::Sender<PersistenceCommand>,
    mpsc::Receiver<PersistenceEvent>,
) {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<PersistenceCommand>(64);
    let (evt_tx, evt_rx) = mpsc::channel::<PersistenceEvent>(64);

    tokio::spawn(async move {
        persistence_worker_task(&storage, &mut cmd_rx, &evt_tx).await;
    });

    (cmd_tx, evt_rx)
}

async fn persistence_worker_task(
    storage: &SqliteStorage,
    cmd_rx: &mut mpsc::Receiver<PersistenceCommand>,
    evt_tx: &mpsc::Sender<PersistenceEvent>,
) {
    while let Some(cmd) = cmd_rx.recv().await {
        handle_command(storage, cmd, evt_tx).await;
    }
}

/// Process a single command.
async fn handle_command(
    storage: &SqliteStorage,
    cmd: PersistenceCommand,
    evt_tx: &mpsc::Sender<PersistenceEvent>,
) {
    use PersistenceCommand::*;

    match cmd {
        CreateNote { input, reply } => {
            let result = storage.create_note(input).await;
            let _ = reply.send(result);
        }
        UpdateNote { input, reply } => {
            let result = storage.update_note(input).await;
            let _ = reply.send(result);
        }
        DeleteNote { note_id, reply } => {
            let result = storage.soft_delete_note(note_id).await;
            if result.is_ok() {
                let _ = evt_tx.send(PersistenceEvent::NoteDeleted { note_id }).await;
            }
            let _ = reply.send(result);
        }
        AddTag {
            note_id,
            tag_name,
            reply,
        } => {
            let result = storage.add_tag_to_note(note_id, &tag_name).await;
            let _ = reply.send(result.map(|_| ()));
        }
        RemoveTag {
            note_id,
            tag_name,
            reply,
        } => {
            let result = storage.remove_tag_by_name(note_id, &tag_name).await;
            let _ = reply.send(result);
        }
    }
}
