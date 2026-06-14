//! LLM request/response logging.
//!
//! Writes JSON log files to the directory specified by
//! `LECTURE_DISTILL_LLM_LOG_DIR` (falling back to `artifacts/llm-logs`
//! relative to the current working directory).  Logging is best-effort:
//! disk errors are silently ignored so they never fail an LLM call.
//!
//! ## Log file format
//!
//! Each file is named `<epoch_ms>-<uuid>.json` so that directory listings
//! sort chronologically.  The JSON payload is a [`LogEntry`].

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Directory resolution
// ---------------------------------------------------------------------------

/// Directory where LLM log files are written.
pub fn log_dir() -> PathBuf {
    std::env::var("LECTURE_DISTILL_LLM_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("artifacts/llm-logs"))
}

fn ensure_log_dir() -> Result<PathBuf> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir).context("failed to create LLM log directory")?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Full log entry written to disk for every LLM call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub created_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    /// `"succeeded"` or `"failed"`
    pub status: String,
    /// `"chat_completion"` or `"chat_completion_stream"`
    pub kind: String,
    pub model: String,
    pub base_url: String,
    pub temperature: f32,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    /// Full OpenAI-compatible request body (without Authorization header).
    pub request: Value,
    /// Response payload.  For non-streaming: `{"raw": <full response JSON>, "content": "..."}`.
    /// For streaming: `{"content": "..."}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// Error message when the call failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Lightweight metadata returned by the list endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogMeta {
    pub id: String,
    pub created_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub status: String,
    pub kind: String,
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// First ~200 chars of response content or error for preview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Write a log entry to disk.  Silently ignores I/O errors.
///
/// Synchronous so that callers (including spawned tasks) can log without
/// worrying about async runtime availability.
pub fn write_log(entry: LogEntry) {
    let write = || -> Result<()> {
        let dir = ensure_log_dir()?;
        let ts_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let path = dir.join(format!("{}-{}.json", ts_ms, entry.id));
        let json = serde_json::to_string_pretty(&entry)?;
        std::fs::write(&path, json)?;
        Ok(())
    };
    let _ = write();
}

// ---------------------------------------------------------------------------
// Read / list
// ---------------------------------------------------------------------------

/// List log metadata, newest first.
///
/// Reads every `.json` file in the log directory, parses the [`LogEntry`]
/// inside, and returns a [`LogMeta`] summary for each one.  Files that
/// cannot be parsed are silently skipped.
pub fn list_logs(limit: usize) -> Result<Vec<LogMeta>> {
    let dir = log_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<(PathBuf, u64)> = Vec::new();
    for entry in std::fs::read_dir(&dir).context("failed to read LLM log directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let ts = name
            .split('-')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        entries.push((path, ts));
    }

    // Newest first (highest epoch ms).
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.truncate(limit.min(1000));

    let mut metas = Vec::new();
    for (path, _) in &entries {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(entry) = serde_json::from_str::<LogEntry>(&content) {
                let preview = entry
                    .response
                    .as_ref()
                    .and_then(|r| r.get("content").and_then(|c| c.as_str()))
                    .or(entry.error.as_deref())
                    .map(|s| s.chars().take(200).collect::<String>());
                metas.push(LogMeta {
                    id: entry.id,
                    created_at: entry.created_at,
                    finished_at: entry.finished_at,
                    duration_ms: entry.duration_ms,
                    status: entry.status,
                    kind: entry.kind,
                    model: entry.model,
                    temperature: entry.temperature,
                    max_tokens: entry.max_tokens,
                    response_format: entry.response_format,
                    error: entry.error,
                    preview,
                });
            }
        }
    }

    Ok(metas)
}

/// Read a single log entry by id.
///
/// Log files are named `<epoch_ms>-<uuid>.json`.  We parse every `.json` file
/// in the log directory and return the first entry whose `id` field matches
/// exactly.  This avoids accidental substring matches.
pub fn read_log(id: &str) -> Result<Value> {
    let dir = log_dir();
    if !dir.exists() {
        bail!("LLM log directory does not exist");
    }

    for entry in std::fs::read_dir(&dir).context("failed to read LLM log directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read log file {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&content).context("failed to parse log JSON")?;
        // Exact match on the `id` field.
        if value.get("id").and_then(|v| v.as_str()) == Some(id) {
            return Ok(value);
        }
    }

    bail!("LLM log not found: {}", id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Serialise env-var mutation tests.
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn temp_log_dir() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("llm-logs");
        std::fs::create_dir_all(&dir).unwrap();
        (tmp, dir)
    }

    #[tokio::test]
    async fn test_write_and_read_log() {
        let (_tmp, dir) = temp_log_dir();
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LECTURE_DISTILL_LLM_LOG_DIR", dir.to_str().unwrap());

        let entry = LogEntry {
            id: "test-id-123".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            finished_at: "2025-01-01T00:00:01Z".to_string(),
            duration_ms: 1000,
            status: "succeeded".to_string(),
            kind: "chat_completion".to_string(),
            model: "test-model".to_string(),
            base_url: "https://test.example.com/v1".to_string(),
            temperature: 0.5,
            max_tokens: 100,
            response_format: Some("json_object".to_string()),
            request: json!({"model": "test-model", "messages": []}),
            response: Some(json!({"content": "Hello, world!"})),
            error: None,
        };

        write_log(entry.clone());

        // Now list.
        let metas = list_logs(10).expect("list_logs");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, "test-id-123");
        assert_eq!(metas[0].status, "succeeded");
        assert_eq!(metas[0].preview.as_deref(), Some("Hello, world!"));

        // Read full.
        let full = read_log("test-id-123").expect("read_log");
        assert_eq!(full["id"].as_str().unwrap(), "test-id-123");
        assert_eq!(full["response"]["content"].as_str().unwrap(), "Hello, world!");

        std::env::remove_var("LECTURE_DISTILL_LLM_LOG_DIR");
    }

    #[tokio::test]
    async fn test_write_failed_log() {
        let (_tmp, dir) = temp_log_dir();
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LECTURE_DISTILL_LLM_LOG_DIR", dir.to_str().unwrap());

        let entry = LogEntry {
            id: "fail-id".to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            finished_at: "2025-01-01T00:00:02Z".to_string(),
            duration_ms: 2000,
            status: "failed".to_string(),
            kind: "chat_completion".to_string(),
            model: "test-model".to_string(),
            base_url: "https://test.example.com/v1".to_string(),
            temperature: 0.0,
            max_tokens: 50,
            response_format: None,
            request: json!({"model": "test-model"}),
            response: None,
            error: Some("connection refused".to_string()),
        };

        write_log(entry);

        let metas = list_logs(10).expect("list_logs");
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].status, "failed");
        assert_eq!(metas[0].error.as_deref(), Some("connection refused"));
        assert_eq!(metas[0].preview.as_deref(), Some("connection refused"));

        std::env::remove_var("LECTURE_DISTILL_LLM_LOG_DIR");
    }

    #[tokio::test]
    async fn test_list_respects_limit() {
        let (_tmp, dir) = temp_log_dir();
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LECTURE_DISTILL_LLM_LOG_DIR", dir.to_str().unwrap());

        for i in 0..5 {
            let entry = LogEntry {
                id: format!("id-{}", i),
                created_at: "2025-01-01T00:00:00Z".to_string(),
                finished_at: "2025-01-01T00:00:01Z".to_string(),
                duration_ms: 100,
                status: "succeeded".to_string(),
                kind: "chat_completion".to_string(),
                model: "m".to_string(),
                base_url: "https://x.com".to_string(),
                temperature: 0.0,
                max_tokens: 10,
                response_format: None,
                request: json!({}),
                response: Some(json!({"content": format!("msg-{}", i)})),
                error: None,
            };
            write_log(entry);
            // Tiny sleep so files get different epoch-ms names.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let metas = list_logs(3).expect("list_logs");
        assert_eq!(metas.len(), 3);

        std::env::remove_var("LECTURE_DISTILL_LLM_LOG_DIR");
    }

    #[test]
    fn test_read_missing_log() {
        let (_tmp, dir) = temp_log_dir();
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LECTURE_DISTILL_LLM_LOG_DIR", dir.to_str().unwrap());

        let result = read_log("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));

        std::env::remove_var("LECTURE_DISTILL_LLM_LOG_DIR");
    }

    #[test]
    fn test_list_empty_dir() {
        let (_tmp, dir) = temp_log_dir();
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("LECTURE_DISTILL_LLM_LOG_DIR", dir.to_str().unwrap());

        let metas = list_logs(10).expect("list_logs");
        assert!(metas.is_empty());

        std::env::remove_var("LECTURE_DISTILL_LLM_LOG_DIR");
    }
}
