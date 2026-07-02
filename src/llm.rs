//! OpenAI-compatible chat completion client.
//!
//! Thin async wrapper around the [OpenAI chat completions
//! endpoint](https://platform.openai.com/docs/api-reference/chat).  Uses
//! environment variables for configuration so that the crate can be
//! self-contained without an external config file.

use anyhow::{Context, Result};
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single chat message with an OpenAI-compatible role and content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

// ---------------------------------------------------------------------------
// Multi-turn conversation with prefix-caching support
// ---------------------------------------------------------------------------

/// A multi-turn conversation that accumulates message history across turns.
///
/// DeepSeek's disk-based prefix caching automatically reuses KV states for
/// matching message prefixes.  Because every [`turn`] call sends the full
/// accumulated history, long-lived content (transcripts, Reference Digest)
/// only needs to be encoded once — subsequent turns hit the cache.
///
/// Conversations can be persisted to disk via [`save_to_file`] and restored
/// via [`load_from_file`] so that retries of downstream steps (Cheat Sheet,
/// Expansion) can continue the conversation instead of starting fresh.
///
/// [`turn`]: ChatConversation::turn
/// [`save_to_file`]: ChatConversation::save_to_file
/// [`load_from_file`]: ChatConversation::load_from_file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatConversation {
    messages: Vec<ChatMessage>,
    /// Model name used when this conversation was created (for compatibility).
    #[serde(default)]
    model: String,
    /// Base URL used when this conversation was created (for compatibility).
    #[serde(default)]
    base_url: String,
}

impl ChatConversation {
    /// Create a new conversation with the given system prompt.
    ///
    /// Records the current model and base URL for compatibility checks when
    /// the conversation is later restored from disk.
    pub fn new(system_prompt: &str) -> Self {
        Self {
            messages: vec![ChatMessage {
                role: "system".into(),
                content: system_prompt.to_string(),
            }],
            model: model(),
            base_url: base_url(),
        }
    }

    /// Return a reference to all messages in the conversation.
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Create an independent fork of this conversation.
    ///
    /// The fork shares the same message history up to the fork point but can
    /// diverge independently.  Use this when you need per-output branches
    /// (e.g. one cheat-sheet output per `max_pages` value) while keeping the
    /// shared prefix intact for caching.
    pub fn fork(&self) -> Self {
        Self {
            messages: self.messages.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
        }
    }

    /// Send a user message and return the assistant's text response together with
    /// the `finish_reason` from the API.
    ///
    /// Like [`turn`] but additionally returns `Option<finish_reason>` so callers
    /// can distinguish natural completion from token-limit truncation.
    ///
    /// # Errors
    ///
    /// Returns an error if the LLM call fails.  The conversation state is
    /// **not** modified on failure (the user message is not persisted).
    pub async fn turn_with_metadata(
        &mut self,
        user_message: &str,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<(String, Option<String>)> {
        // Append user message tentatively — we'll remove it on failure.
        self.messages.push(ChatMessage {
            role: "user".into(),
            content: user_message.to_string(),
        });

        match chat_completion_with_metadata(&self.messages, temperature, max_tokens, None).await {
            Ok((response, finish_reason)) => {
                self.messages.push(ChatMessage {
                    role: "assistant".into(),
                    content: response.clone(),
                });
                Ok((response, finish_reason))
            }
            Err(e) => {
                // Roll back the user message on failure.
                self.messages.pop();
                Err(e)
            }
        }
    }

    /// Persist this conversation to a JSON file on disk.
    ///
    /// The file is overwritten if it already exists (idempotent for the same
    /// process — one conversation per process directory).
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let json =
            serde_json::to_string_pretty(self).context("failed to serialize conversation")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir: {}", parent.display()))?;
        }
        fs::write(path, &json)
            .with_context(|| format!("failed to write conversation to {}", path.display()))?;
        Ok(())
    }

    /// Load a conversation from a JSON file on disk.
    ///
    /// Returns `Ok(None)` if the file does not exist (not an error — the
    /// caller should fall back to a fresh conversation).  Returns an error
    /// if the file exists but cannot be parsed, or if the model / base URL
    /// have changed since the conversation was saved (to avoid sending
    /// messages to a different provider).
    pub fn load_from_file(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let json = fs::read_to_string(path)
            .with_context(|| format!("failed to read conversation from {}", path.display()))?;
        let conv: Self = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse conversation from {}", path.display()))?;

        // Compatibility check: reject if model or base URL changed.
        let current_model = model();
        let current_base = base_url();
        if conv.model != current_model || conv.base_url != current_base {
            log::warn!(
                "conversation file {} was created with model={} base_url={}; \
                 current model={} base_url={} — starting fresh conversation",
                path.display(),
                conv.model,
                conv.base_url,
                current_model,
                current_base,
            );
            return Ok(None);
        }

        Ok(Some(conv))
    }

    /// Manually append an assistant message to the conversation history.
    ///
    /// Use this when content is produced externally (e.g. a merge step or a
    /// multi-section path) and needs to be available as context for subsequent
    /// turns.
    pub fn add_assistant(&mut self, content: &str) {
        self.messages.push(ChatMessage {
            role: "assistant".into(),
            content: content.to_string(),
        });
    }

    /// Send a user message and return the assistant's text response.
    ///
    /// Appends the user message, sends the complete conversation history via
    /// [`chat_completion`], appends the assistant response, and returns it.
    ///
    /// # Errors
    ///
    /// Returns an error if the LLM call fails.  The conversation state is
    /// **not** modified on failure (the user message is not persisted).
    pub async fn turn(
        &mut self,
        user_message: &str,
        temperature: f32,
        max_tokens: u32,
    ) -> Result<String> {
        // Append user message tentatively — we'll remove it on failure.
        self.messages.push(ChatMessage {
            role: "user".into(),
            content: user_message.to_string(),
        });

        match chat_completion(&self.messages, temperature, max_tokens, None).await {
            Ok(response) => {
                self.messages.push(ChatMessage {
                    role: "assistant".into(),
                    content: response.clone(),
                });
                Ok(response)
            }
            Err(e) => {
                // Roll back the user message on failure.
                self.messages.pop();
                Err(e)
            }
        }
    }
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

/// Build a `reqwest::Client` with a generous timeout for large streaming
/// responses (max_tokens ≥ 32 k).  Even at 20 tokens/s an 80 k response
/// needs ~68 minutes, but in practice providers deliver much faster.
/// 600 s is a safe upper bound for most deployments.
fn http_client_streaming() -> Result<Client> {
    Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .context("failed to create streaming HTTP client")
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

/// Build rich parse-failure diagnostics from a raw response body.
///
/// Returns `(error_message, response_value_for_log)`.
fn json_parse_error_diagnostics(
    status: reqwest::StatusCode,
    body_text: &str,
    parse_err: &serde_json::Error,
) -> (String, Value) {
    let char_len = body_text.chars().count();
    let byte_len = body_text.len();
    let head: String = body_text.chars().take(200).collect();
    let tail: String = if char_len > 200 {
        body_text
            .chars()
            .skip(char_len.saturating_sub(200))
            .collect()
    } else {
        String::new()
    };
    let error_msg = format!(
        "failed to parse chat completion response JSON: \
         HTTP status={}, body char_len={}, byte_len={}, \
         head={:?}, tail={:?}, parse error: {}",
        status.as_u16(),
        char_len,
        byte_len,
        head,
        tail,
        parse_err
    );
    let response_value = json!({
        "raw_text_head": head,
        "raw_text_tail": tail,
        "status": status.as_u16(),
    });
    (error_msg, response_value)
}

/// Threshold above which non-streaming chat completions are routed through
/// the SSE streaming endpoint internally.  This avoids HTTP transport failures
/// (`error decoding response body`) that some API providers exhibit when
/// delivering very large response bodies in a single chunk.
const LARGE_OUTPUT_THRESHOLD: u32 = 32768;

/// Internal helper: collect a streaming chat completion into a single result.
///
/// When `response_format` is `Some("json_object")`, the JSON constraint is
/// enforced via the system prompt rather than the API parameter (which is
/// incompatible with `stream: true`).  Code fences are stripped from the
/// collected text before returning.
///
/// Returns `(accumulated_text, Option<finish_reason>)`.
async fn chat_completion_via_stream(
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    response_format: Option<&str>,
    log_kind: &str,
) -> Result<(String, Option<String>)> {
    use futures_util::StreamExt;
    let key = api_key()?;
    let client = http_client_streaming()?;
    let url = format!("{}/chat/completions", base_url());

    let json_mode = response_format == Some("json_object");

    // Build messages.  For JSON mode, inject a strong output constraint
    // because the `response_format` API parameter is incompatible with
    // `stream: true`.
    let msgs: Vec<Value> = if json_mode {
        let mut augmented: Vec<ChatMessage> = messages.to_vec();
        // Append a system-level instruction if the first message is "system";
        // otherwise prepend one.
        if augmented.first().map(|m| m.role.as_str()) == Some("system") {
            augmented[0].content = format!(
                "{}\n\nYou MUST output ONLY valid JSON. No markdown fences, no explanatory text, no commentary. Just the JSON object.",
                augmented[0].content
            );
        } else {
            augmented.insert(
                0,
                ChatMessage {
                    role: "system".into(),
                    content:
                        "You MUST output ONLY valid JSON. No markdown fences, no explanatory text, no commentary. Just the JSON object."
                            .into(),
                },
            );
        }
        augmented
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect()
    } else {
        messages
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect()
    };

    let body = json!({
        "model": model(),
        "messages": msgs,
        "temperature": temperature,
        "max_tokens": max_tokens,
        "stream": true,
    });

    // --- LLM logging: capture context before the call ---
    let log_id = Uuid::new_v4().to_string();
    let log_created_at = Utc::now().to_rfc3339();
    let log_started = std::time::Instant::now();
    let log_model = model();
    let log_base_url = base_url();
    let log_request = body.clone();
    let log_temp = temperature;
    let log_max_tokens = max_tokens;
    let log_rf = response_format.map(|s| s.to_string());
    // ---------------------------------------------------

    let resp = send_with_retries(
        &client,
        &url,
        &key,
        &body,
        "chat completion (internal stream)",
    )
    .await;

    let log_duration_ms = log_started.elapsed().as_millis() as u64;
    let log_finished_at = Utc::now().to_rfc3339();

    match resp {
        Err(e) => {
            crate::llm_log::write_log(crate::llm_log::LogEntry {
                id: log_id,
                created_at: log_created_at,
                finished_at: log_finished_at,
                duration_ms: log_duration_ms,
                status: "failed".into(),
                kind: log_kind.to_string(),
                model: log_model,
                base_url: log_base_url,
                temperature: log_temp,
                max_tokens: log_max_tokens,
                response_format: log_rf.clone(),
                request: log_request,
                response: None,
                error: Some(e.to_string()),
            });
            Err(e)
        }
        Ok(resp) => {
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut accumulated = String::new();
            let mut finish_reason: Option<String> = None;

            loop {
                match stream.next().await {
                    Some(Ok(bytes)) => {
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(line_end) = buf.find('\n') {
                            let line = buf[..line_end].trim().to_string();
                            buf = buf[line_end + 1..].to_string();
                            if line.is_empty() || line.starts_with(':') {
                                continue;
                            }
                            if let Some(data) = line.strip_prefix("data: ") {
                                if data == "[DONE]" {
                                    // Strip JSON fences if needed before returning.
                                    let final_text = if json_mode {
                                        strip_fences_for_json_mode(&accumulated)
                                    } else {
                                        accumulated
                                    };
                                    // Success — log and return.
                                    crate::llm_log::write_log(crate::llm_log::LogEntry {
                                        id: log_id,
                                        created_at: log_created_at,
                                        finished_at: log_finished_at.clone(),
                                        duration_ms: log_duration_ms,
                                        status: "succeeded".into(),
                                        kind: log_kind.to_string(),
                                        model: log_model,
                                        base_url: log_base_url,
                                        temperature: log_temp,
                                        max_tokens: log_max_tokens,
                                        response_format: log_rf.clone(),
                                        request: log_request,
                                        response: Some(
                                            json!({ "content": final_text, "finish_reason": finish_reason }),
                                        ),
                                        error: None,
                                    });
                                    return Ok((final_text, finish_reason));
                                }
                                match serde_json::from_str::<Value>(data) {
                                    Ok(parsed) => {
                                        // Capture finish_reason from the last choice delta.
                                        if let Some(fr) =
                                            parsed["choices"][0]["finish_reason"].as_str()
                                        {
                                            if !fr.is_empty() {
                                                finish_reason = Some(fr.to_string());
                                            }
                                        }
                                        if let Some(delta) =
                                            parsed["choices"][0]["delta"]["content"].as_str()
                                        {
                                            accumulated.push_str(delta);
                                        }
                                    }
                                    Err(e) => {
                                        let err_msg = format!("Failed to parse SSE data: {}", e);
                                        crate::llm_log::write_log(crate::llm_log::LogEntry {
                                            id: log_id,
                                            created_at: log_created_at,
                                            finished_at: log_finished_at,
                                            duration_ms: log_duration_ms,
                                            status: "failed".into(),
                                            kind: log_kind.to_string(),
                                            model: log_model,
                                            base_url: log_base_url,
                                            temperature: log_temp,
                                            max_tokens: log_max_tokens,
                                            response_format: log_rf.clone(),
                                            request: log_request,
                                            response: Some(
                                                json!({ "partial_content": accumulated }),
                                            ),
                                            error: Some(err_msg.clone()),
                                        });
                                        return Err(anyhow::anyhow!("{}", err_msg));
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        // Stream error — return what we have if any, else error.
                        if accumulated.is_empty() {
                            let err_msg = format!("Stream error: {}", e);
                            crate::llm_log::write_log(crate::llm_log::LogEntry {
                                id: log_id,
                                created_at: log_created_at,
                                finished_at: log_finished_at,
                                duration_ms: log_duration_ms,
                                status: "failed".into(),
                                kind: log_kind.to_string(),
                                model: log_model,
                                base_url: log_base_url,
                                temperature: log_temp,
                                max_tokens: log_max_tokens,
                                response_format: log_rf.clone(),
                                request: log_request,
                                response: None,
                                error: Some(err_msg.clone()),
                            });
                            return Err(anyhow::anyhow!("{}", err_msg));
                        }
                        // Partial success — log warning but return what we have.
                        log::warn!(
                            "chat completion stream interrupted after {} chars: {}",
                            accumulated.chars().count(),
                            e
                        );
                        crate::llm_log::write_log(crate::llm_log::LogEntry {
                            id: log_id,
                            created_at: log_created_at,
                            finished_at: log_finished_at,
                            duration_ms: log_duration_ms,
                            status: "partial".into(),
                            kind: log_kind.to_string(),
                            model: log_model,
                            base_url: log_base_url,
                            temperature: log_temp,
                            max_tokens: log_max_tokens,
                            response_format: log_rf.clone(),
                            request: log_request,
                            response: Some(json!({ "partial_content": accumulated })),
                            error: Some(format!("Stream interrupted: {}", e)),
                        });
                        return Ok((accumulated, finish_reason));
                    }
                    None => {
                        // Stream ended without [DONE] — return what we collected.
                        let final_text = if json_mode {
                            strip_fences_for_json_mode(&accumulated)
                        } else {
                            accumulated
                        };
                        crate::llm_log::write_log(crate::llm_log::LogEntry {
                            id: log_id,
                            created_at: log_created_at,
                            finished_at: log_finished_at,
                            duration_ms: log_duration_ms,
                            status: "succeeded".into(),
                            kind: log_kind.to_string(),
                            model: log_model,
                            base_url: log_base_url,
                            temperature: log_temp,
                            max_tokens: log_max_tokens,
                            response_format: log_rf.clone(),
                            request: log_request,
                            response: Some(
                                json!({ "content": final_text, "finish_reason": finish_reason }),
                            ),
                            error: None,
                        });
                        return Ok((final_text, finish_reason));
                    }
                }
            }
        }
    }
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
///
/// When `max_tokens` equals or exceeds [`LARGE_OUTPUT_THRESHOLD`] and
/// `response_format` is `None`, this function internally uses SSE streaming
/// to avoid HTTP transport failures on very large response bodies.
pub async fn chat_completion(
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    response_format: Option<&str>,
) -> Result<String> {
    // Route large outputs through internal streaming to avoid HTTP transport
    // failures on very large response bodies.  On failure, retry with
    // progressively smaller max_tokens as a last-resort fallback.
    if max_tokens >= LARGE_OUTPUT_THRESHOLD {
        let _permit = acquire_llm_permit().await?;
        let mut mt = max_tokens;
        loop {
            match chat_completion_via_stream(
                messages,
                temperature,
                mt,
                response_format,
                "chat_completion",
            )
            .await
            {
                Ok((text, _)) => return Ok(text),
                Err(e) if mt > 16384 => {
                    mt /= 2;
                    log::warn!(
                        "chat_completion streaming failed at max_tokens={}, retrying with {}: {}",
                        max_tokens,
                        mt,
                        e
                    );
                }
                Err(e) => return Err(e),
            }
        }
    }
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

    // --- LLM logging: capture context before the call ---
    let log_id = uuid::Uuid::new_v4().to_string();
    let log_created_at = chrono::Utc::now().to_rfc3339();
    let log_started = std::time::Instant::now();
    let log_model = model();
    let log_base_url = base_url();
    let log_request = body.clone();
    let log_rf = response_format.map(|s| s.to_string());
    let log_temp = temperature;
    let log_max_tokens = max_tokens;
    // ---------------------------------------------------

    let resp = send_with_retries(&client, &url, &key, &body, "chat completion").await;

    let log_duration_ms = log_started.elapsed().as_millis() as u64;
    let log_finished_at = chrono::Utc::now().to_rfc3339();

    match resp {
        Ok(resp) => {
            let http_status = resp.status();
            match resp.text().await {
                Ok(body_text) => match serde_json::from_str::<Value>(&body_text) {
                    Ok(json) => {
                        match json["choices"][0]["message"]["content"].as_str() {
                            Some(content) => {
                                // Log success with full response.
                                crate::llm_log::write_log(crate::llm_log::LogEntry {
                                    id: log_id,
                                    created_at: log_created_at,
                                    finished_at: log_finished_at,
                                    duration_ms: log_duration_ms,
                                    status: "succeeded".into(),
                                    kind: "chat_completion".into(),
                                    model: log_model,
                                    base_url: log_base_url,
                                    temperature: log_temp,
                                    max_tokens: log_max_tokens,
                                    response_format: log_rf,
                                    request: log_request,
                                    response: Some(json!({ "raw": json, "content": content })),
                                    error: None,
                                });
                                Ok(content.to_string())
                            }
                            None => {
                                let err_msg =
                                    "unexpected response shape: missing choices[0].message.content"
                                        .to_string();
                                crate::llm_log::write_log(crate::llm_log::LogEntry {
                                    id: log_id,
                                    created_at: log_created_at,
                                    finished_at: log_finished_at,
                                    duration_ms: log_duration_ms,
                                    status: "failed".into(),
                                    kind: "chat_completion".into(),
                                    model: log_model,
                                    base_url: log_base_url,
                                    temperature: log_temp,
                                    max_tokens: log_max_tokens,
                                    response_format: log_rf,
                                    request: log_request,
                                    response: Some(json!({ "raw": json })),
                                    error: Some(err_msg.clone()),
                                });
                                Err(anyhow::anyhow!("{}", err_msg))
                            }
                        }
                    }
                    Err(parse_err) => {
                        let (err_msg, response_value) =
                            json_parse_error_diagnostics(http_status, &body_text, &parse_err);
                        crate::llm_log::write_log(crate::llm_log::LogEntry {
                            id: log_id,
                            created_at: log_created_at,
                            finished_at: log_finished_at,
                            duration_ms: log_duration_ms,
                            status: "failed".into(),
                            kind: "chat_completion".into(),
                            model: log_model,
                            base_url: log_base_url,
                            temperature: log_temp,
                            max_tokens: log_max_tokens,
                            response_format: log_rf,
                            request: log_request,
                            response: Some(response_value),
                            error: Some(err_msg.clone()),
                        });
                        Err(anyhow::anyhow!("{}", err_msg))
                    }
                },
                Err(e) => {
                    let err_msg = format!(
                        "failed to read chat completion response body as text: HTTP status={}, error={}",
                        http_status.as_u16(), e
                    );
                    crate::llm_log::write_log(crate::llm_log::LogEntry {
                        id: log_id,
                        created_at: log_created_at,
                        finished_at: log_finished_at,
                        duration_ms: log_duration_ms,
                        status: "failed".into(),
                        kind: "chat_completion".into(),
                        model: log_model,
                        base_url: log_base_url,
                        temperature: log_temp,
                        max_tokens: log_max_tokens,
                        response_format: log_rf,
                        request: log_request,
                        response: Some(json!({ "status": http_status.as_u16() })),
                        error: Some(err_msg.clone()),
                    });
                    Err(anyhow::anyhow!("{}", err_msg))
                }
            }
        }
        Err(e) => {
            crate::llm_log::write_log(crate::llm_log::LogEntry {
                id: log_id,
                created_at: log_created_at,
                finished_at: log_finished_at,
                duration_ms: log_duration_ms,
                status: "failed".into(),
                kind: "chat_completion".into(),
                model: log_model,
                base_url: log_base_url,
                temperature: log_temp,
                max_tokens: log_max_tokens,
                response_format: log_rf,
                request: log_request,
                response: None,
                error: Some(e.to_string()),
            });
            Err(e)
        }
    }
}

/// Send a chat completion request and return the text content together with the
/// `finish_reason` from the first choice.
///
/// Uses the same environment variables and retry logic as
/// [`chat_completion`].  The returned tuple is `(content,
/// Option<finish_reason>)`; `finish_reason` is `None` when the field is
/// absent from the response (which should be rare).
pub async fn chat_completion_with_metadata(
    messages: &[ChatMessage],
    temperature: f32,
    max_tokens: u32,
    response_format: Option<&str>,
) -> Result<(String, Option<String>)> {
    // Route large outputs through internal streaming to avoid HTTP transport
    // failures on very large response bodies.  On failure, retry with
    // progressively smaller max_tokens.
    if max_tokens >= LARGE_OUTPUT_THRESHOLD {
        let _permit = acquire_llm_permit().await?;
        let mut mt = max_tokens;
        loop {
            match chat_completion_via_stream(
                messages,
                temperature,
                mt,
                response_format,
                "chat_completion_with_metadata",
            )
            .await
            {
                Ok(result) => return Ok(result),
                Err(e) if mt > 16384 => {
                    mt /= 2;
                    log::warn!(
                        "chat_completion_with_metadata streaming failed at max_tokens={}, retrying with {}: {}",
                        max_tokens,
                        mt,
                        e
                    );
                }
                Err(e) => return Err(e),
            }
        }
    }

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

    // --- LLM logging: capture context before the call ---
    let log_id = uuid::Uuid::new_v4().to_string();
    let log_created_at = chrono::Utc::now().to_rfc3339();
    let log_started = std::time::Instant::now();
    let log_model = model();
    let log_base_url = base_url();
    let log_request = body.clone();
    let log_rf = response_format.map(|s| s.to_string());
    let log_temp = temperature;
    let log_max_tokens = max_tokens;
    // ---------------------------------------------------

    let resp = send_with_retries(&client, &url, &key, &body, "chat completion").await;

    let log_duration_ms = log_started.elapsed().as_millis() as u64;
    let log_finished_at = chrono::Utc::now().to_rfc3339();

    match resp {
        Ok(resp) => {
            let http_status = resp.status();
            match resp.text().await {
                Ok(body_text) => match serde_json::from_str::<Value>(&body_text) {
                    Ok(json) => {
                        let finish_reason = json["choices"][0]["finish_reason"]
                            .as_str()
                            .map(|s| s.to_string());
                        match json["choices"][0]["message"]["content"].as_str() {
                            Some(content) => {
                                crate::llm_log::write_log(crate::llm_log::LogEntry {
                                    id: log_id,
                                    created_at: log_created_at,
                                    finished_at: log_finished_at,
                                    duration_ms: log_duration_ms,
                                    status: "succeeded".into(),
                                    kind: "chat_completion_with_metadata".into(),
                                    model: log_model,
                                    base_url: log_base_url,
                                    temperature: log_temp,
                                    max_tokens: log_max_tokens,
                                    response_format: log_rf,
                                    request: log_request,
                                    response: Some(
                                        json!({ "raw": json, "content": content, "finish_reason": finish_reason }),
                                    ),
                                    error: None,
                                });
                                Ok((content.to_string(), finish_reason))
                            }
                            None => {
                                let err_msg =
                                    "unexpected response shape: missing choices[0].message.content"
                                        .to_string();
                                crate::llm_log::write_log(crate::llm_log::LogEntry {
                                    id: log_id,
                                    created_at: log_created_at,
                                    finished_at: log_finished_at,
                                    duration_ms: log_duration_ms,
                                    status: "failed".into(),
                                    kind: "chat_completion_with_metadata".into(),
                                    model: log_model,
                                    base_url: log_base_url,
                                    temperature: log_temp,
                                    max_tokens: log_max_tokens,
                                    response_format: log_rf,
                                    request: log_request,
                                    response: Some(json!({ "raw": json })),
                                    error: Some(err_msg.clone()),
                                });
                                Err(anyhow::anyhow!("{}", err_msg))
                            }
                        }
                    }
                    Err(parse_err) => {
                        let (err_msg, response_value) =
                            json_parse_error_diagnostics(http_status, &body_text, &parse_err);
                        crate::llm_log::write_log(crate::llm_log::LogEntry {
                            id: log_id,
                            created_at: log_created_at,
                            finished_at: log_finished_at,
                            duration_ms: log_duration_ms,
                            status: "failed".into(),
                            kind: "chat_completion_with_metadata".into(),
                            model: log_model,
                            base_url: log_base_url,
                            temperature: log_temp,
                            max_tokens: log_max_tokens,
                            response_format: log_rf,
                            request: log_request,
                            response: Some(response_value),
                            error: Some(err_msg.clone()),
                        });
                        Err(anyhow::anyhow!("{}", err_msg))
                    }
                },
                Err(e) => {
                    let err_msg = format!(
                        "failed to read chat completion response body as text: HTTP status={}, error={}",
                        http_status.as_u16(), e
                    );
                    crate::llm_log::write_log(crate::llm_log::LogEntry {
                        id: log_id,
                        created_at: log_created_at,
                        finished_at: log_finished_at,
                        duration_ms: log_duration_ms,
                        status: "failed".into(),
                        kind: "chat_completion_with_metadata".into(),
                        model: log_model,
                        base_url: log_base_url,
                        temperature: log_temp,
                        max_tokens: log_max_tokens,
                        response_format: log_rf,
                        request: log_request,
                        response: Some(json!({ "status": http_status.as_u16() })),
                        error: Some(err_msg.clone()),
                    });
                    Err(anyhow::anyhow!("{}", err_msg))
                }
            }
        }
        Err(e) => {
            crate::llm_log::write_log(crate::llm_log::LogEntry {
                id: log_id,
                created_at: log_created_at,
                finished_at: log_finished_at,
                duration_ms: log_duration_ms,
                status: "failed".into(),
                kind: "chat_completion_with_metadata".into(),
                model: log_model,
                base_url: log_base_url,
                temperature: log_temp,
                max_tokens: log_max_tokens,
                response_format: log_rf,
                request: log_request,
                response: None,
                error: Some(e.to_string()),
            });
            Err(e)
        }
    }
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

    // --- LLM logging: capture context before the call ---
    let log_id = Uuid::new_v4().to_string();
    let log_created_at = Utc::now().to_rfc3339();
    let log_started = std::time::Instant::now();
    let log_model = model();
    let log_base_url = base_url();
    let log_request = body.clone();
    let log_temp = temperature;
    let log_max_tokens = max_tokens;
    // ---------------------------------------------------

    let resp = send_with_retries(&client, &url, &key, &body, "streaming chat completion").await;

    match resp {
        Err(e) => {
            // Log the failure before returning.
            let log_duration_ms = log_started.elapsed().as_millis() as u64;
            let log_finished_at = Utc::now().to_rfc3339();
            crate::llm_log::write_log(crate::llm_log::LogEntry {
                id: log_id,
                created_at: log_created_at,
                finished_at: log_finished_at,
                duration_ms: log_duration_ms,
                status: "failed".into(),
                kind: "chat_completion_stream".into(),
                model: log_model,
                base_url: log_base_url,
                temperature: log_temp,
                max_tokens: log_max_tokens,
                response_format: None,
                request: log_request,
                response: None,
                error: Some(e.to_string()),
            });
            return Err(e);
        }
        Ok(resp) => {
            let (tx, rx) = mpsc::channel::<Result<String>>(64);
            let mut stream = resp.bytes_stream();

            tokio::spawn(async move {
                let _permit = permit;
                let mut buf = String::new();
                let mut accumulated = String::new(); // accumulate for logging

                // Helper to write the log from inside the spawned task.
                let write_stream_log = |status: &str, content: &str, error: Option<String>| {
                    let log_duration_ms = log_started.elapsed().as_millis() as u64;
                    let log_finished_at = Utc::now().to_rfc3339();
                    crate::llm_log::write_log(crate::llm_log::LogEntry {
                        id: log_id.clone(),
                        created_at: log_created_at.clone(),
                        finished_at: log_finished_at,
                        duration_ms: log_duration_ms,
                        status: status.to_string(),
                        kind: "chat_completion_stream".into(),
                        model: log_model.clone(),
                        base_url: log_base_url.clone(),
                        temperature: log_temp,
                        max_tokens: log_max_tokens,
                        response_format: None,
                        request: log_request.clone(),
                        response: if content.is_empty() {
                            None
                        } else {
                            Some(json!({ "content": content }))
                        },
                        error,
                    });
                };

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
                                        write_stream_log("succeeded", &accumulated, None);
                                        return;
                                    }
                                    match serde_json::from_str::<Value>(data) {
                                        Ok(parsed) => {
                                            if let Some(delta) =
                                                parsed["choices"][0]["delta"]["content"].as_str()
                                            {
                                                let content = delta.to_string();
                                                if !content.is_empty() {
                                                    accumulated.push_str(&content);
                                                    if tx.send(Ok(content)).await.is_err() {
                                                        // receiver dropped
                                                        write_stream_log(
                                                            "succeeded",
                                                            &accumulated,
                                                            None,
                                                        );
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let err_msg =
                                                format!("Failed to parse SSE data: {}", e);
                                            let _ =
                                                tx.send(Err(anyhow::anyhow!("{}", &err_msg))).await;
                                            write_stream_log("failed", &accumulated, Some(err_msg));
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let err_msg = format!("Stream error: {}", e);
                            let _ = tx.send(Err(anyhow::anyhow!("{}", &err_msg))).await;
                            write_stream_log("failed", &accumulated, Some(err_msg));
                            return;
                        }
                    }
                }
                // Stream ended without [DONE] marker; signal completion.
                let _ = tx.send(Ok(String::new())).await;
                write_stream_log("succeeded", &accumulated, None);
            });

            Ok(rx)
        }
    }
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
/// Strip markdown code fences from JSON output.
///
/// In streaming mode we cannot use `response_format: "json_object"`, so the LLM
/// may wrap its output in ```json ... ``` fences.  This helper strips them
/// before the text is handed to JSON-aware callers (e.g. outline parsers in
/// `app.rs`).
fn strip_fences_for_json_mode(text: &str) -> String {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("```json") {
        rest.strip_suffix("```").unwrap_or(rest).trim().to_string()
    } else if let Some(rest) = text.strip_prefix("```") {
        rest.strip_suffix("```").unwrap_or(rest).trim().to_string()
    } else {
        text.to_string()
    }
}

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

    // -----------------------------------------------------------------------
    // ChatConversation
    // -----------------------------------------------------------------------

    #[test]
    fn test_conversation_new_starts_with_system() {
        let conv = ChatConversation::new("You are a helpful assistant.");
        let msgs = conv.messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.contains("helpful assistant"));
    }

    #[test]
    fn test_conversation_fork_is_independent() {
        let mut conv = ChatConversation::new("System.");

        // Seed the conversation with a fake assistant message.
        conv.add_assistant("Fake response 1");

        // Fork — should have the same history.
        let mut fork = conv.fork();
        assert_eq!(fork.messages().len(), 2);
        assert_eq!(fork.messages()[1].content, "Fake response 1");

        // Add to fork — original should be unaffected.
        fork.add_assistant("Fork-only response");
        assert_eq!(fork.messages().len(), 3);
        assert_eq!(conv.messages().len(), 2);
    }

    #[test]
    fn test_conversation_add_assistant() {
        let mut conv = ChatConversation::new("System.");
        assert_eq!(conv.messages().len(), 1);

        conv.add_assistant("Response one.");
        assert_eq!(conv.messages().len(), 2);
        assert_eq!(conv.messages()[1].role, "assistant");
        assert_eq!(conv.messages()[1].content, "Response one.");

        conv.add_assistant("Response two.");
        assert_eq!(conv.messages().len(), 3);
    }

    #[test]
    fn test_conversation_fork_preserves_prefix() {
        let mut conv = ChatConversation::new("System prompt.");
        conv.add_assistant("Digest content here.");

        let fork = conv.fork();
        // Fork must have the exact same prefix for caching.
        assert_eq!(fork.messages().len(), 2);
        assert_eq!(fork.messages()[0].content, "System prompt.");
        assert_eq!(fork.messages()[1].content, "Digest content here.");
    }

    #[test]
    fn test_conversation_save_and_load_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("conversation.json");

        let mut conv = ChatConversation::new("Test system.");
        conv.add_assistant("First response.");
        conv.add_assistant("Second response.");
        conv.save_to_file(&path).unwrap();

        let loaded = ChatConversation::load_from_file(&path).unwrap().unwrap();
        assert_eq!(loaded.messages().len(), 3);
        assert_eq!(loaded.messages()[0].role, "system");
        assert_eq!(loaded.messages()[0].content, "Test system.");
        assert_eq!(loaded.messages()[1].role, "assistant");
        assert_eq!(loaded.messages()[1].content, "First response.");
        assert_eq!(loaded.messages()[2].role, "assistant");
        assert_eq!(loaded.messages()[2].content, "Second response.");
    }

    #[test]
    fn test_conversation_save_overwrites() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("conversation.json");

        let conv = ChatConversation::new("A.");
        conv.save_to_file(&path).unwrap();

        let conv2 = ChatConversation::new("B.");
        conv2.save_to_file(&path).unwrap();

        let loaded = ChatConversation::load_from_file(&path).unwrap().unwrap();
        assert_eq!(loaded.messages()[0].content, "B.");
    }

    #[test]
    fn test_conversation_load_missing_file_returns_none() {
        let result =
            ChatConversation::load_from_file(Path::new("/nonexistent/conversation.json")).unwrap();
        assert!(result.is_none());
    }

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

    // -------------------------------------------------------------------
    // json_parse_error_diagnostics
    // -------------------------------------------------------------------

    #[test]
    fn test_parse_error_diagnostics_short_body() {
        use reqwest::StatusCode;
        let status = StatusCode::OK;
        let body = "{not valid json";
        let parse_err = serde_json::from_str::<Value>(body).unwrap_err();

        let (error_msg, response_value) = json_parse_error_diagnostics(status, body, &parse_err);

        // Error message contains key fields.
        assert!(error_msg.contains("failed to parse chat completion response JSON"));
        assert!(error_msg.contains("HTTP status=200"));
        assert!(error_msg.contains(&format!("body char_len={}", body.chars().count())));
        assert!(error_msg.contains(&format!("byte_len={}", body.len())));
        assert!(error_msg.contains("parse error:"));

        // Response value has expected structure.
        assert_eq!(response_value["status"].as_u64(), Some(200));
        assert_eq!(
            response_value["raw_text_head"].as_str(),
            Some("{not valid json")
        );
        // For a body shorter than 200 chars, tail should be empty.
        assert_eq!(response_value["raw_text_tail"].as_str(), Some(""));
    }

    #[test]
    fn test_parse_error_diagnostics_long_body() {
        use reqwest::StatusCode;
        let status = StatusCode::BAD_GATEWAY;
        // Build a body > 200 chars with distinct head and tail.
        let mut body = String::from("HEAD_MARKER_");
        body.extend(std::iter::repeat('x').take(500));
        body.push_str("TAIL_MARKER");
        let parse_err = serde_json::from_str::<Value>(&body).unwrap_err();

        let (error_msg, response_value) = json_parse_error_diagnostics(status, &body, &parse_err);

        // Status in error message.
        assert!(error_msg.contains("HTTP status=502"));
        assert!(error_msg.contains(&format!("body char_len={}", body.chars().count())));
        assert!(error_msg.contains(&format!("byte_len={}", body.len())));

        // Head should start with HEAD_MARKER_.
        let head = response_value["raw_text_head"].as_str().unwrap();
        assert!(head.starts_with("HEAD_MARKER_"));
        assert_eq!(head.chars().count(), 200); // exactly 200

        // Tail should end with TAIL_MARKER.
        let tail = response_value["raw_text_tail"].as_str().unwrap();
        assert!(tail.ends_with("TAIL_MARKER"));
        assert_eq!(tail.chars().count(), 200); // exactly 200

        assert_eq!(response_value["status"].as_u64(), Some(502));
    }

    #[test]
    fn test_parse_error_diagnostics_empty_body() {
        use reqwest::StatusCode;
        let status = StatusCode::INTERNAL_SERVER_ERROR;
        let body = "";
        let parse_err = serde_json::from_str::<Value>(body).unwrap_err();

        let (error_msg, response_value) = json_parse_error_diagnostics(status, body, &parse_err);

        assert!(error_msg.contains("body char_len=0"));
        assert!(error_msg.contains("byte_len=0"));
        assert_eq!(response_value["raw_text_head"].as_str(), Some(""));
        assert_eq!(response_value["raw_text_tail"].as_str(), Some(""));
        assert_eq!(response_value["status"].as_u64(), Some(500));
    }

    #[test]
    fn test_parse_error_diagnostics_non_200_status() {
        use reqwest::StatusCode;
        let status = StatusCode::TOO_MANY_REQUESTS;
        let body = "<html>Rate limit exceeded</html>";
        let parse_err = serde_json::from_str::<Value>(body).unwrap_err();

        let (error_msg, response_value) = json_parse_error_diagnostics(status, body, &parse_err);

        assert!(error_msg.contains("HTTP status=429"));
        assert!(error_msg.contains("parse error:"));
        assert_eq!(response_value["status"].as_u64(), Some(429));
        assert!(response_value["raw_text_head"]
            .as_str()
            .unwrap()
            .contains("<html>"));
    }
}
