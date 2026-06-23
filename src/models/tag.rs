use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A tag that can be attached to notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// NoteTag struct reserved for future use in bulk tag operations.
