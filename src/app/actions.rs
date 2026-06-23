use crate::models::note::CreateNoteInput;
use crate::workers::PersistenceCommand;
use anyhow::Result;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

/// Actions that can be dispatched to modify state or perform I/O.
///
/// Routes mutations through the persistence worker channel.
pub enum AppAction {
    /// Create a new note with the given title.
    CreateNote { title: String },
    /// Delete the note with the given ID.
    DeleteNote { note_id: Uuid },
}

/// Handle an action by sending it through the persistence worker.
///
/// Returns an optional status message.
pub async fn handle_action(
    persist_tx: &mpsc::Sender<PersistenceCommand>,
    action: AppAction,
) -> Result<Option<String>> {
    match action {
        AppAction::CreateNote { title } => {
            let input = CreateNoteInput {
                title,
                body: String::new(),
            };
            let (reply, rx) = oneshot::channel();
            persist_tx
                .send(PersistenceCommand::CreateNote { input, reply })
                .await?;
            rx.await??;
            Ok(Some("Note created".to_string()))
        }
        AppAction::DeleteNote { note_id } => {
            let (reply, rx) = oneshot::channel();
            persist_tx
                .send(PersistenceCommand::DeleteNote { note_id, reply })
                .await?;
            rx.await??;
            Ok(Some("Note deleted".to_string()))
        }
    }
}
