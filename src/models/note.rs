use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A note summary, used for list display. The body is loaded separately for the
/// preview pane (see `SqliteStorage::get_note`) so the list stays lightweight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteSummary {
    pub id: Uuid,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tags: Vec<String>,
}

/// A full note with body content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Note {
    pub id: Uuid,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub content_hash: String,
    /// The remote version for optimistic concurrency. 0 = never synced.
    pub remote_version: i32,
}

impl Note {
    /// Compute the content hash from title + body for change detection.
    pub fn compute_content_hash(&self) -> String {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.title.hash(&mut hasher);
        self.body.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }
}

/// Input for creating a new note.
#[derive(Debug, Clone)]
pub struct CreateNoteInput {
    pub title: String,
    pub body: String,
}

/// Input for updating an existing note.
#[derive(Debug, Clone)]
pub struct UpdateNoteInput {
    pub id: Uuid,
    pub title: String,
    pub body: String,
}
