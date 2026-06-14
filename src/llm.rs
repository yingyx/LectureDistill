//! OpenAI-compatible chat completion client.
//!
//! Thin async wrapper around the [OpenAI chat completions
//! endpoint](https://platform.openai.com/docs/api-reference/chat).  Uses
//! environment variables for configuration so that the crate can be
//! self-contained without an external config file.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::env;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single chat message with an OpenAI-compatible role and content.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn api_key() -> Result<String> {
    env::var("OPENAI_API_KEY").context("OPENAI_API_KEY environment variable is not set")
}

fn base_url() -> String {
    env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into())
}

fn model() -> String {
    env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into())
}

fn max_concurrency() -> usize {
    env::var("LECTURE_DISTILL_LLM_MAX_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(2)
        .clamp(1, 32)
}

fn max_retries() -> usize {
    env::var("LECTURE_DISTILL_LLM_MAX_RETRIES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(2)
        .min(5)
}

async fn acquire_llm_permit() -> Result<OwnedSemaphorePermit> {
    static GATE: OnceLock<Mutex<(usize, Arc<Semaphore>)>> = OnceLock::new();
    let desired = max_concurrency();
    let sem = {
        let gate = GATE.get_or_init(|| Mutex::new((desired, Arc::new(Semaphore::new(desired)))));
        let mut guard = gate.lock().unwrap();
        if guard.0 != desired {
            *guard = (desired, Arc::new(Semaphore::new(desired)));
        }
        guard.1.clone()
    };
    sem.acquire_owned()
        .await
        .context("failed to acquire LLM concurrency permit")
}

/// Build a single-shot `reqwest::Client` with a reasonable timeout.
fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("failed to create HTTP client")
}

async fn send_with_retries(
    client: &Client,
    url: &str,
    key: &str,
    body: &Value,
    label: &str,
) -> Result<reqwest::Response> {
    let retries = max_retries();
    let mut last_error: Option<anyhow::Error> = None;

    for attempt in 0..=retries {
        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {}", key))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await;

        match resp {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                let retryable = status.as_u16() == 429 || status.is_server_error();
                let body_text = resp.text().await.unwrap_or_default();
                let err =
                    anyhow::anyhow!("{} returned HTTP {}: {}", label, status.as_u16(), body_text);
                if retryable && attempt < retries {
                    last_error = Some(err);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * (attempt as u64 + 1),
                    ))
                    .await;
                    continue;
                }
                return Err(err);
            }
            Err(e) => {
                let err = anyhow::anyhow!("failed to send {} request: {}", label, e);
                if attempt < retries {
                    last_error = Some(err);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * (attempt as u64 + 1),
                    ))
                    .await;
                    continue;
                }
                return Err(err);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("{} request failed", label)))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns `true` when the `OPENAI_API_KEY` environment variable is set.
pub fn is_available() -> bool {
    env::var("OPENAI_API_KEY").is_ok()
}

/// Send a chat completion request and return the text content of the first
/// choice.
///
/// # Environment variables
///
/// | Variable           | Default                          |
/// |--------------------|----------------------------------|
/// | `OPENAI_API_KEY`   | **required**                     |
/// | `OPENAI_BASE_URL`  | `https://api.openai.com/v1`      |
/// | `OPENAI_MODEL`     | `gpt-4o-mini`                    |
///
/// # Arguments
///
/// * `messages` - the conversation messages.
/// * `temperature` - sampling temperature (0.0 - 2.0).
/// * `max_tokens` - maximum tokens in the response.
/// * `response_format` - pass `Some("json_object")` to force JSON output,
///   or `None` for plain text.
pub async fn chat_completion(
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    response_format: Option<&str>,
) -> Result<String> {
    let _permit = acquire_llm_permit().await?;
    let key = api_key()?;
    let client = http_client()?;
    let url = format!("{}/chat/completions", base_url());

    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect();

    let mut body = json!({
        "model": model(),
        "messages": msgs,
        "temperature": temperature,
        "max_tokens": max_tokens,
    });

    if let Some(fmt) = response_format {
        body["response_format"] = json!({ "type": fmt });
    }

    let resp = send_with_retries(&client, &url, &key, &body, "chat completion").await?;

    let json: Value = resp
        .json()
        .await
        .context("failed to parse chat completion response JSON")?;

    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .context("unexpected response shape: missing choices[0].message.content")
}

// ---------------------------------------------------------------------------
// Convenience helpers
// ---------------------------------------------------------------------------

/// Send a single-turn chat (system + user) and parse the response as JSON.
///
/// Automatically strips markdown code fences (```` ```json ```` ... ```` ``` ````)
/// from the LLM output before parsing.  Retries **once** on JSON parse
/// failure with a corrective system prompt appended.
pub async fn chat_json(
    system_prompt: &str,
    user_prompt: &str,
    temperature: f32,
    max_tokens: u32,
) -> Result<Value> {
    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user_prompt.to_string(),
        },
    ];

    // First attempt.
    let text = chat_completion(&messages, temperature, max_tokens, Some("json_object")).await?;
    match strip_code_fences_and_parse(&text) {
        Ok(value) => return Ok(value),
        Err(_) => { /* fall through to retry */ }
    }

    // Retry with a corrective nudge.
    let mut retry_messages = messages;
    retry_messages.push(ChatMessage {
        role: "assistant".into(),
        content: text,
    });
    retry_messages.push(ChatMessage {
        role: "user".into(),
        content: "The previous response was not valid JSON. Please output ONLY valid JSON, with no markdown fences or extra text.".into(),
    });

    let retry_text = chat_completion(
        &retry_messages,
        temperature,
        max_tokens,
        Some("json_object"),
    )
    .await?;
    strip_code_fences_and_parse(&retry_text).context("JSON parse failed on retry as well")
}

/// Send a single-turn chat (system + user) and return the raw text response.
pub async fn chat_text(
    system_prompt: &str,
    user_prompt: &str,
    temperature: f32,
    max_tokens: u32,
) -> Result<String> {
    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user_prompt.to_string(),
        },
    ];

    chat_completion(&messages, temperature, max_tokens, None).await
}

// ---------------------------------------------------------------------------
// Streaming chat completion
// ---------------------------------------------------------------------------

/// Send a streaming chat completion request.
///
/// Returns a channel receiver that yields content delta chunks as `String`
/// values, followed by a final `Ok("")` or an `Err(...)`.
///
/// Uses the same environment variables as [`chat_completion`].
pub async fn chat_completion_stream(
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
) -> Result<mpsc::Receiver<Result<String>>> {
    let permit = acquire_llm_permit().await?;
    let key = api_key()?;
    let client = http_client()?;
    let url = format!("{}/chat/completions", base_url());

    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| json!({ "role": m.role, "content": m.content }))
        .collect();

    let body = json!({
        "model": model(),
        "messages": msgs,
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": true,
    });

    let resp = send_with_retries(&client, &url, &key, &body, "streaming chat completion").await?;

    let (tx, rx) = mpsc::channel::<Result<String>>(64);
    let mut stream = resp.bytes_stream();

    tokio::spawn(async move {
        let _permit = permit;
        let mut buf = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(bytes) => {
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    // Process complete SSE lines from the buffer.
                    while let Some(line_end) = buf.find('\n') {
                        let line = buf[..line_end].trim().to_string();
                        buf = buf[line_end + 1..].to_string();
                        if line.is_empty() || line.starts_with(':') {
                            continue;
                        }
                        if let Some(data) = line.strip_prefix("data: ") {
                            if data == "[DONE]" {
                                let _ = tx.send(Ok(String::new())).await;
                                return;
                            }
                            match serde_json::from_str::<Value>(data) {
                                Ok(parsed) => {
                                    if let Some(delta) =
                                        parsed["choices"][0]["delta"]["content"].as_str()
                                    {
                                        let content = delta.to_string();
                                        if !content.is_empty() {
                                            if tx.send(Ok(content)).await.is_err() {
                                                return; // receiver dropped
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(Err(anyhow::anyhow!(
                                            "Failed to parse SSE data: {}",
                                            e
                                        )))
                                        .await;
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("Stream error: {}", e))).await;
                    return;
                }
            }
        }
        // Stream ended without [DONE] marker; signal completion.
        let _ = tx.send(Ok(String::new())).await;
    });

    Ok(rx)
}

/// Send a streaming single-turn chat (system + user).
pub async fn chat_text_stream(
    system_prompt: &str,
    user_prompt: &str,
    temperature: f32,
    max_tokens: u32,
) -> Result<mpsc::Receiver<Result<String>>> {
    let messages = vec![
        ChatMessage {
            role: "system".into(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user_prompt.to_string(),
        },
    ];

    chat_completion_stream(&messages, temperature, max_tokens).await
}

// ---------------------------------------------------------------------------
// Compact source text for LLM context
// ---------------------------------------------------------------------------

/// Compact transcript-day Markdown into a more LLM-friendly format.
///
/// - Joins consecutive timestamped segment lines into readable sentences/paragraphs
///   within each video section, preserving `[mm:ss]` anchors at sentence starts.
/// - Collapses repeated blank lines.
/// - Preserves video headings and video boundaries.
/// - Returns the compacted text, suitable for LLM context.
pub fn compact_transcript_for_llm(text: &str, max_chars: usize) -> String {
    let mut result = String::new();
    let mut prev_empty = false;
    let mut in_paragraph = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Preserve headings as-is.
        if trimmed.starts_with('#') {
            if in_paragraph {
                result.push('\n');
                in_paragraph = false;
            }
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(trimmed);
            result.push('\n');
            prev_empty = false;
            continue;
        }

        // Collapse blank lines.
        if trimmed.is_empty() {
            if in_paragraph {
                result.push('\n');
                in_paragraph = false;
            }
            if !prev_empty && !result.is_empty() {
                result.push('\n');
                prev_empty = true;
            }
            continue;
        }

        // Timestamped segment lines: `[mm:ss] text...`
        if trimmed.starts_with('[') && trimmed.len() > 6 && &trimmed[6..7] == "]" {
            // Join consecutive timestamp lines into a paragraph.
            if in_paragraph {
                result.push(' ');
            } else if !result.is_empty() && !prev_empty {
                result.push('\n');
            }
            result.push_str(trimmed);
            in_paragraph = true;
            prev_empty = false;
            continue;
        }

        // Other content lines.
        if in_paragraph {
            result.push('\n');
            in_paragraph = false;
        }
        if !result.is_empty() && !prev_empty {
            result.push('\n');
        }
        result.push_str(trimmed);
        result.push('\n');
        prev_empty = false;
    }

    if in_paragraph {
        result.push('\n');
    }

    // Truncate if needed, but use a higher cap than the default.
    if result.chars().count() <= max_chars {
        result
    } else {
        let truncated: String = result.chars().take(max_chars).collect();
        format!(
            "{}\n\n[... content truncated to {} chars for context; ask about specific sections if needed ...]",
            truncated, max_chars
        )
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Strip markdown code fences (```` ```json ```` / ```` ``` ````) from LLM
/// output, then attempt JSON parsing.
fn strip_code_fences_and_parse(text: &str) -> Result<Value, serde_json::Error> {
    let text = text.trim();

    // Try to strip ```json ... ``` or ``` ... ```
    let inner = if let Some(rest) = text.strip_prefix("```json") {
        rest.strip_suffix("```").unwrap_or(rest).trim()
    } else if let Some(rest) = text.strip_prefix("```") {
        rest.strip_suffix("```").unwrap_or(rest).trim()
    } else {
        text
    };

    serde_json::from_str(inner)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Global mutex to serialise env-var mutation tests.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn test_is_available_when_key_not_set() {
        // Ensure the key is absent (best-effort - other test may have set it).
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("OPENAI_API_KEY");
        assert!(!is_available());
    }

    #[test]
    fn test_is_available_when_key_is_set() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("OPENAI_API_KEY", "test-fake-key");
        assert!(is_available());
        std::env::remove_var("OPENAI_API_KEY");
    }

    // -----------------------------------------------------------------------
    // compact_transcript_for_llm
    // -----------------------------------------------------------------------

    #[test]
    fn test_compact_preserves_headings() {
        let input = "# Course - Date\n\n## 08:00 Video Title - vid1\n\n[00:00] Hello world\n[00:05] This is a test\n";
        let result = compact_transcript_for_llm(input, 10000);
        assert!(result.contains("# Course - Date"));
        assert!(result.contains("## 08:00 Video Title - vid1"));
        assert!(result.contains("[00:00] Hello world [00:05] This is a test"));
    }

    #[test]
    fn test_compact_joins_timestamp_lines() {
        let input = "[00:00] First sentence\n[00:05] Second sentence\n[00:10] Third sentence\n";
        let result = compact_transcript_for_llm(input, 10000);
        // All three should be on one line joined by spaces.
        let lines: Vec<&str> = result.lines().collect();
        let content_lines: Vec<&str> = lines
            .iter()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .copied()
            .collect();
        assert_eq!(content_lines.len(), 1);
        assert!(content_lines[0].contains("[00:00] First sentence"));
        assert!(content_lines[0].contains("[00:05] Second sentence"));
        assert!(content_lines[0].contains("[00:10] Third sentence"));
    }

    #[test]
    fn test_compact_collapses_blank_lines() {
        let input = "[00:00] Line one\n\n\n\n[00:05] Line two\n";
        let result = compact_transcript_for_llm(input, 10000);
        // Should have at most one blank line between content.
        let blank_count = result.lines().filter(|l| l.trim().is_empty()).count();
        assert!(blank_count <= 2); // 2 is ok: one after heading area, one between paragraphs
    }

    #[test]
    fn test_compact_truncates_when_needed() {
        let long: String = std::iter::repeat("a").take(500).collect();
        let result = compact_transcript_for_llm(&long, 300);
        assert!(result.contains("truncated"));
        assert!(result.len() < long.len() + 100); // allow for truncation message
    }

    #[test]
    fn test_compact_preserves_non_timestamp_lines() {
        let input = "Some introductory text\n\n[00:00] Timestamped line\n\nMore text after\n";
        let result = compact_transcript_for_llm(input, 10000);
        assert!(result.contains("Some introductory text"));
        assert!(result.contains("Timestamped line"));
        assert!(result.contains("More text after"));
    }
}
