//! Minimal OpenAI-compatible chat client for "ask your notes".
//!
//! Defaults to a local LLM runtime (Ollama / llama.cpp); any OpenAI-compatible
//! `/v1/chat/completions` endpoint works. Request building, response parsing,
//! and the loopback check are pure functions, so they're unit-testable without
//! a network or a running model — only [`complete`] touches the wire.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

/// One chat message in the OpenAI format.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }
}

/// Build the JSON body for `POST /chat/completions`.
pub fn build_request_body(model: &str, messages: &[ChatMessage], stream: bool) -> Value {
    json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    })
}

/// Extract the assistant message text from a chat-completions response.
pub fn parse_response(body: &Value) -> Result<String> {
    body.get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .map(str::to_string)
        .context("chat response missing choices[0].message.content")
}

/// Whether `base_url` points at the local machine (loopback host). Drives the
/// LOCAL/REMOTE marker and the `allow_remote` gate.
pub fn is_local(base_url: &str) -> bool {
    // Strip scheme, path, and any user-info, then the port, to get the host.
    let after_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or("");
    let host = host_port
        .strip_prefix('[')
        .and_then(|rest| rest.split(']').next()) // [::1]:port
        .unwrap_or_else(|| host_port.split(':').next().unwrap_or(""));
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
}

/// Call the chat endpoint and return the assistant's reply (non-streaming).
#[cfg(feature = "ai")]
pub async fn complete(
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    messages: &[ChatMessage],
) -> Result<String> {
    let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut request = reqwest::Client::new()
        .post(&endpoint)
        .json(&build_request_body(model, messages, false));
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("chat request to {endpoint} failed"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .context("chat response was not valid JSON")?;

    if !status.is_success() {
        anyhow::bail!("chat endpoint returned {status}: {body}");
    }
    parse_response(&body)
}

/// Best-effort reachability probe for the chat endpoint (for `jot-down doctor`).
/// Any HTTP response — even an error status — counts as reachable; only a
/// connection/timeout failure is "unreachable".
#[cfg(feature = "ai")]
pub async fn reachable(base_url: &str) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    client
        .get(base_url.trim_end_matches('/'))
        .send()
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_has_model_messages_and_stream() {
        let messages = vec![ChatMessage::system("be terse"), ChatMessage::user("hello")];
        let body = build_request_body("llama3.1:8b", &messages, false);
        assert_eq!(body["model"], "llama3.1:8b");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hello");
    }

    #[test]
    fn parses_assistant_content() {
        let body = json!({
            "choices": [{ "message": { "role": "assistant", "content": "the answer" } }]
        });
        assert_eq!(parse_response(&body).unwrap(), "the answer");
    }

    #[test]
    fn parse_errors_on_missing_content() {
        let body = json!({ "choices": [] });
        assert!(parse_response(&body).is_err());
    }

    #[test]
    fn loopback_hosts_are_local() {
        assert!(is_local("http://localhost:11434/v1"));
        assert!(is_local("http://127.0.0.1:8080/v1"));
        assert!(is_local("http://[::1]:11434/v1"));
        assert!(!is_local("https://api.openai.com/v1"));
        assert!(!is_local("https://example.com:443/v1"));
    }
}
