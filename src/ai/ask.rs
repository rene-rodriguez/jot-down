//! "Ask your notes" — retrieval-augmented question answering.
//!
//! Retrieves the most relevant notes for a question (reusing the hybrid
//! retriever), builds a grounded prompt with citation-tagged snippets, and asks
//! the configured chat model. The prompt builder is a pure function so it's
//! unit-testable without a model; [`answer_question`] / [`spawn_ask_worker`]
//! perform the retrieval and the network call.

use uuid::Uuid;

use crate::ai::chat::ChatMessage;

/// How much of each note to include in the prompt.
const SNIPPET_CHARS: usize = 600;

/// A source note backing an answer, as shown to the model.
#[derive(Debug, Clone)]
pub struct Source {
    pub index: usize,
    pub title: String,
    pub snippet: String,
}

/// A citation the UI can render and resolve back to a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub index: usize,
    pub note_id: Uuid,
    pub title: String,
}

/// A completed answer plus the notes it was grounded on.
#[derive(Debug, Clone)]
pub struct AskAnswer {
    pub answer: String,
    pub citations: Vec<Citation>,
}

/// Build the grounded chat prompt: a system instruction plus a user message
/// carrying the numbered note snippets and the question. Pure and deterministic.
pub fn build_grounded_prompt(question: &str, sources: &[Source]) -> Vec<ChatMessage> {
    const SYSTEM: &str = "You are answering questions using ONLY the user's notes, provided below as numbered sources. \
Cite the sources you use inline as [n], matching the numbers. \
If the notes do not contain the answer, say so plainly rather than guessing.";

    let mut context = String::new();
    if sources.is_empty() {
        context.push_str("(no relevant notes found)\n");
    }
    for source in sources {
        context.push_str(&format!(
            "[{}] {}\n{}\n\n",
            source.index, source.title, source.snippet
        ));
    }

    let user = format!("Notes:\n\n{context}\nQuestion: {question}");
    vec![ChatMessage::system(SYSTEM), ChatMessage::user(user)]
}

/// Truncate text to at most `max` characters on a char boundary.
fn snippet(body: &str, max: usize) -> String {
    if body.chars().count() <= max {
        return body.to_string();
    }
    let mut out: String = body.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(feature = "ai")]
mod runtime {
    use super::*;
    use anyhow::Result;

    use crate::ai::{chat, retriever};
    use crate::config::ChatConfig;
    use crate::storage::SqliteStorage;

    /// Answer a question over the user's notes: retrieve → ground → ask.
    pub async fn answer_question(
        storage: &SqliteStorage,
        chat_cfg: &ChatConfig,
        question: &str,
        k: usize,
    ) -> Result<AskAnswer> {
        // Privacy gate: never send note content to a non-local endpoint unless
        // the user explicitly opted in.
        if !chat::is_local(&chat_cfg.base_url) && !chat_cfg.allow_remote {
            anyhow::bail!(
                "Remote chat endpoint blocked. Set [ai.chat].allow_remote = true to use {}",
                chat_cfg.base_url
            );
        }

        let notes = retriever::hybrid_search(storage, question, k).await?;

        let mut sources = Vec::with_capacity(notes.len());
        let mut citations = Vec::with_capacity(notes.len());
        for (i, note) in notes.iter().enumerate() {
            let index = i + 1;
            let body = storage
                .get_note(note.id)
                .await?
                .map(|n| n.body)
                .unwrap_or_default();
            sources.push(Source {
                index,
                title: note.title.clone(),
                snippet: snippet(&body, SNIPPET_CHARS),
            });
            citations.push(Citation {
                index,
                note_id: note.id,
                title: note.title.clone(),
            });
        }

        let messages = build_grounded_prompt(question, &sources);
        let api_key = chat_cfg
            .api_key_env
            .as_ref()
            .and_then(|var| std::env::var(var).ok());
        let answer = chat::complete(
            &chat_cfg.base_url,
            &chat_cfg.model,
            api_key.as_deref(),
            &messages,
        )
        .await?;

        Ok(AskAnswer { answer, citations })
    }
}

#[cfg(feature = "ai")]
pub use runtime::answer_question;

#[cfg(test)]
mod tests {
    use super::*;

    fn source(index: usize, title: &str, snippet: &str) -> Source {
        Source {
            index,
            title: title.to_string(),
            snippet: snippet.to_string(),
        }
    }

    #[test]
    fn prompt_includes_question_and_numbered_sources() {
        let sources = vec![
            source(1, "Rust notes", "tokio is an async runtime"),
            source(2, "Cooking", "roast at 200C"),
        ];
        let messages = build_grounded_prompt("how do I do async?", &sources);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert!(messages[0].content.contains("[n]"));

        let user = &messages[1].content;
        assert!(user.contains("how do I do async?"));
        assert!(user.contains("[1] Rust notes"));
        assert!(user.contains("tokio is an async runtime"));
        assert!(user.contains("[2] Cooking"));
    }

    #[test]
    fn prompt_handles_no_sources() {
        let messages = build_grounded_prompt("anything?", &[]);
        assert!(messages[1].content.contains("no relevant notes found"));
        assert!(messages[1].content.contains("anything?"));
    }

    #[test]
    fn snippet_truncates_on_char_boundary() {
        let long: String = "é".repeat(1000);
        let s = snippet(&long, 10);
        assert_eq!(s.chars().count(), 11); // 10 chars + ellipsis
        assert!(s.ends_with('…'));

        let short = "short";
        assert_eq!(snippet(short, 10), "short");
    }
}
