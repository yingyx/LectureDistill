//! Source registry for the Web GUI.
//!
//! A Source is a user-facing record that represents one ingested artifact:
//! - A Canvas transcript day (one course, one date, merged subtitle segments)
//! - An uploaded Markdown note file
//!
//! Sources are persisted to `<project_dir>/artifacts/sources.json` and
//! reference artifacts stored under `<project_dir>/artifacts/sources/...`.
//! No secrets are ever stored in the source registry.
//!
//! ## Deterministic ask fallback
//!
//! When no LLM key is configured, `POST /api/sources/{id}/ask` falls back to
//! a deterministic token-scoring search over the source text. This is safe to
//! call in tests and does not require network access.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Source model
// ---------------------------------------------------------------------------

/// Kinds of sources supported by the system.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    TranscriptDay,
    TranscriptCourse,
    Note,
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TranscriptDay => write!(f, "transcript_day"),
            Self::TranscriptCourse => write!(f, "transcript_course"),
            Self::Note => write!(f, "note"),
        }
    }
}

/// Processing status of a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Ready,
    Processing,
    Failed,
}

impl std::fmt::Display for SourceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => write!(f, "ready"),
            Self::Processing => write!(f, "processing"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// A single source record in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRecord {
    /// Stable unique identifier (UUID v4).
    pub id: String,
    /// Kind of source.
    pub kind: SourceKind,
    /// Human-readable title.
    pub title: String,
    /// Processing status.
    pub status: SourceStatus,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-update timestamp.
    pub updated_at: String,
    /// Human-readable length description (e.g. "5 videos, 120 segments",
    /// "340 lines").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<String>,
    /// Internal artifact path under `<project_dir>/artifacts/sources/...`.
    pub path: String,
    /// Source-specific metadata (course_id, course_name, date, video_count,
    /// segment_count, original_filename, size_bytes, etc.).
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Last error message, if the source is in failed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Associated background job ID, if a job is running or has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

impl SourceRecord {
    pub fn now_iso() -> String {
        chrono::Utc::now().to_rfc3339()
    }
}

// ---------------------------------------------------------------------------
// Source store
// ---------------------------------------------------------------------------

/// Persistence layer for source records.
///
/// Reads/writes a JSON array at `<project_dir>/artifacts/sources.json`.
pub struct SourceStore {
    path: PathBuf,
    artifacts_dir: PathBuf,
}

impl SourceStore {
    const FILENAME: &'static str = "artifacts/sources.json";

    pub fn new(project_dir: &str) -> Self {
        let base = Path::new(project_dir);
        let path = base.join(Self::FILENAME);
        let artifacts_dir = base.join("artifacts/sources");
        Self {
            path,
            artifacts_dir,
        }
    }

    /// Ensure the artifacts directory exists.
    pub fn ensure_dirs(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&self.artifacts_dir)?;
        Ok(())
    }

    /// Load all source records from disk. Returns empty vec if file is missing
    /// or corrupted.
    pub fn load_all(&self) -> Vec<SourceRecord> {
        match fs::read_to_string(&self.path) {
            Ok(content) => serde_json::from_str::<Vec<SourceRecord>>(&content).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Save all source records to disk.
    pub fn save_all(&self, records: &[SourceRecord]) -> Result<()> {
        self.ensure_dirs()?;
        let json = serde_json::to_string_pretty(records)?;
        fs::write(&self.path, json)?;
        Ok(())
    }

    /// Insert a new record and persist.
    pub fn insert(&self, record: SourceRecord) -> Result<SourceRecord> {
        let mut records = self.load_all();
        records.push(record.clone());
        self.save_all(&records)?;
        Ok(record)
    }

    /// Update an existing record by ID and persist. Returns None if not found.
    pub fn update(
        &self,
        id: &str,
        updater: impl FnOnce(&mut SourceRecord),
    ) -> Option<SourceRecord> {
        let mut records = self.load_all();
        let idx = records.iter().position(|r| r.id == id)?;
        updater(&mut records[idx]);
        records[idx].updated_at = SourceRecord::now_iso();
        let updated = records[idx].clone();
        let _ = self.save_all(&records);
        Some(updated)
    }

    /// Delete a record by ID and persist. Returns the removed record or None.
    pub fn delete(&self, id: &str) -> Option<SourceRecord> {
        let mut records = self.load_all();
        if let Some(idx) = records.iter().position(|r| r.id == id) {
            let removed = records.remove(idx);
            let _ = self.save_all(&records);
            Some(removed)
        } else {
            None
        }
    }

    /// Get a single record by ID.
    pub fn get(&self, id: &str) -> Option<SourceRecord> {
        self.load_all().into_iter().find(|r| r.id == id)
    }

    /// Build the artifacts path for a given source kind and ID.
    pub fn artifact_path(&self, kind: &SourceKind, id: &str, ext: &str) -> PathBuf {
        match kind {
            SourceKind::TranscriptDay => self
                .artifacts_dir
                .join("transcripts")
                .join(format!("{}.{}", id, ext)),
            SourceKind::TranscriptCourse => self
                .artifacts_dir
                .join("courses")
                .join(format!("{}.{}", id, ext)),
            SourceKind::Note => self
                .artifacts_dir
                .join("notes")
                .join(format!("{}.{}", id, ext)),
        }
    }

    pub fn transcript_work_dir(&self, course_id: &str, date: &str) -> PathBuf {
        self.artifacts_dir
            .join("transcripts")
            .join(course_id)
            .join(date)
    }

    pub fn course_index_dir(&self, source_id: &str) -> PathBuf {
        self.artifacts_dir.join("course_indexes").join(source_id)
    }
}

// ---------------------------------------------------------------------------
// Deterministic ask fallback
// ---------------------------------------------------------------------------

/// Score lines against a query using simple token matching.
///
/// Splits the question into non-whitespace, non-punctuation tokens, then
/// counts how many unique tokens appear in each line. Returns the top N lines
/// sorted by score descending.
pub fn deterministic_answer(question: &str, text: &str) -> String {
    let raw_tokens: Vec<String> = question
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|t| t.len() >= 1)
        .map(|t| t.to_lowercase())
        .collect();
    let stopwords = [
        "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "is", "it",
        "of", "on", "or", "the", "to", "was", "what", "when", "where", "which", "who", "why",
        "with",
    ];
    let mut query_tokens: Vec<String> = raw_tokens
        .iter()
        .filter(|t| !stopwords.contains(&t.as_str()))
        .cloned()
        .collect();
    if query_tokens.is_empty() {
        query_tokens = raw_tokens;
    }

    if query_tokens.is_empty() {
        return "No meaningful query tokens found. Please rephrase your question.".to_string();
    }

    let lines: Vec<&str> = text.lines().collect();
    let mut scored: Vec<(usize, &str)> = lines
        .iter()
        .map(|line| {
            let lower = line.to_lowercase();
            let score = query_tokens
                .iter()
                .filter(|token| lower.contains(token.as_str()))
                .count();
            (score, *line)
        })
        .filter(|(score, _)| *score > 0)
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0));

    if scored.is_empty() {
        return "No local matches found. Set OPENAI_API_KEY to enable LLM-powered answers."
            .to_string();
    }

    let snippets: Vec<String> = scored
        .into_iter()
        .take(5)
        .map(|(score, line)| format!("[score={}] {}", score, line))
        .collect();

    format!(
        "LLM is not available. Showing top-matching lines instead:\n\n{}\n\n\
         Set an LLM API key in Settings to get AI-powered answers.",
        snippets.join("\n")
    )
}

/// Truncate text to at most `max_chars` characters, adding a note.
pub fn truncate_for_llm(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!(
            "{}\n\n[... content truncated, {}+ total chars ...]",
            truncated, max_chars
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // SourceRecord
    // -----------------------------------------------------------------------

    #[test]
    fn test_source_record_serialization() {
        let record = SourceRecord {
            id: "test-id".to_string(),
            kind: SourceKind::TranscriptDay,
            title: "CS101 - 2026-03-01".to_string(),
            status: SourceStatus::Ready,
            created_at: "2026-03-01T00:00:00Z".to_string(),
            updated_at: "2026-03-01T00:00:00Z".to_string(),
            length: Some("5 videos, 120 segments".to_string()),
            path: "artifacts/sources/transcripts/test-id.md".to_string(),
            metadata: serde_json::json!({
                "course_id": "12345",
                "course_name": "CS101",
                "date": "2026-03-01",
                "video_count": 5,
                "segment_count": 120,
            }),
            last_error: None,
            job_id: None,
        };

        let json = serde_json::to_string_pretty(&record).unwrap();
        assert!(json.contains("transcript_day"));
        assert!(json.contains("CS101"));
        assert!(json.contains("5 videos"));
        // last_error should not appear when None.
        assert!(!json.contains("last_error"));
        // job_id should not appear when None.
        assert!(!json.contains("job_id"));
    }

    #[test]
    fn test_source_record_deserialization() {
        let json = r#"{
            "id": "abc",
            "kind": "note",
            "title": "My Notes",
            "status": "ready",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "length": "340 lines",
            "path": "artifacts/sources/notes/abc.md",
            "metadata": {"original_filename": "lecture.md", "size_bytes": 12345}
        }"#;
        let record: SourceRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.id, "abc");
        assert_eq!(record.kind, SourceKind::Note);
        assert_eq!(record.title, "My Notes");
        assert_eq!(record.status, SourceStatus::Ready);
        assert!(record.last_error.is_none());
        assert!(record.job_id.is_none());
    }

    #[test]
    fn test_source_status_display() {
        assert_eq!(SourceStatus::Ready.to_string(), "ready");
        assert_eq!(SourceStatus::Processing.to_string(), "processing");
        assert_eq!(SourceStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn test_source_kind_display() {
        assert_eq!(SourceKind::TranscriptDay.to_string(), "transcript_day");
        assert_eq!(SourceKind::Note.to_string(), "note");
    }

    // -----------------------------------------------------------------------
    // SourceStore
    // -----------------------------------------------------------------------

    #[test]
    fn test_store_insert_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = SourceStore::new(dir.path().to_str().unwrap());

        let record = SourceRecord {
            id: uuid::Uuid::new_v4().to_string(),
            kind: SourceKind::Note,
            title: "Test Note".to_string(),
            status: SourceStatus::Ready,
            created_at: SourceRecord::now_iso(),
            updated_at: SourceRecord::now_iso(),
            length: Some("10 lines".to_string()),
            path: "artifacts/sources/notes/test.md".to_string(),
            metadata: serde_json::json!({}),
            last_error: None,
            job_id: None,
        };

        store.insert(record.clone()).unwrap();
        let all = store.load_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, record.id);
        assert_eq!(all[0].title, "Test Note");
    }

    #[test]
    fn test_store_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = SourceStore::new(dir.path().to_str().unwrap());

        let record = SourceRecord {
            id: uuid::Uuid::new_v4().to_string(),
            kind: SourceKind::Note,
            title: "Original".to_string(),
            status: SourceStatus::Ready,
            created_at: SourceRecord::now_iso(),
            updated_at: SourceRecord::now_iso(),
            length: None,
            path: "path".to_string(),
            metadata: serde_json::json!({}),
            last_error: None,
            job_id: None,
        };

        store.insert(record.clone()).unwrap();
        let updated = store
            .update(&record.id, |r| {
                r.title = "Updated".to_string();
                r.status = SourceStatus::Failed;
                r.last_error = Some("test error".to_string());
            })
            .unwrap();

        assert_eq!(updated.title, "Updated");
        assert_eq!(updated.status, SourceStatus::Failed);
        assert_eq!(updated.last_error.as_deref(), Some("test error"));

        let reloaded = store.get(&record.id).unwrap();
        assert_eq!(reloaded.title, "Updated");
    }

    #[test]
    fn test_store_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = SourceStore::new(dir.path().to_str().unwrap());

        let id = uuid::Uuid::new_v4().to_string();
        let record = SourceRecord {
            id: id.clone(),
            kind: SourceKind::Note,
            title: "To Delete".to_string(),
            status: SourceStatus::Ready,
            created_at: SourceRecord::now_iso(),
            updated_at: SourceRecord::now_iso(),
            length: None,
            path: "path".to_string(),
            metadata: serde_json::json!({}),
            last_error: None,
            job_id: None,
        };

        store.insert(record).unwrap();
        assert_eq!(store.load_all().len(), 1);

        let removed = store.delete(&id).unwrap();
        assert_eq!(removed.id, id);
        assert_eq!(store.load_all().len(), 0);
        assert!(store.delete("nonexistent").is_none());
    }

    #[test]
    fn test_store_get_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SourceStore::new(dir.path().to_str().unwrap());
        assert!(store.get("nonexistent").is_none());
    }

    #[test]
    fn test_store_empty_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SourceStore::new(dir.path().to_str().unwrap());
        let all = store.load_all();
        assert!(all.is_empty());
    }

    #[test]
    fn test_store_corrupted_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SourceStore::new(dir.path().to_str().unwrap());
        store.ensure_dirs().unwrap();
        fs::write(&store.path, "not valid json").unwrap();
        let all = store.load_all();
        assert!(all.is_empty());
    }

    // -----------------------------------------------------------------------
    // Deterministic answer
    // -----------------------------------------------------------------------

    #[test]
    fn test_deterministic_answer_finds_matches() {
        let question = "What is machine learning?";
        let text = "Machine learning is a subset of artificial intelligence.\n\
                    Deep learning uses neural networks.\n\
                    Statistics is the foundation of data science.\n\
                    Machine learning algorithms learn from data.";

        let answer = deterministic_answer(question, text);
        assert!(answer.contains("Machine learning"));
        assert!(!answer.contains("Statistics"));
        assert!(answer.contains("LLM is not available"));
    }

    #[test]
    fn test_deterministic_answer_no_matches() {
        let question = "quantum physics";
        let text = "Machine learning basics.\nDeep learning advanced.";
        let answer = deterministic_answer(question, text);
        assert!(answer.contains("No local matches found"));
    }

    #[test]
    fn test_deterministic_answer_empty_query() {
        let answer = deterministic_answer("", "some text");
        assert!(answer.contains("No meaningful query tokens"));
    }

    #[test]
    fn test_truncate_for_llm() {
        let text = "Hello world!";
        assert_eq!(truncate_for_llm(text, 100), text);

        let long: String = std::iter::repeat("a").take(500).collect();
        let truncated = truncate_for_llm(&long, 300);
        assert!(truncated.contains("truncated"));
        assert!(truncated.len() < long.len());
    }

    // -----------------------------------------------------------------------
    // SourceRecord serialization roundtrip with multiple records
    // -----------------------------------------------------------------------

    #[test]
    fn test_multiple_records_roundtrip() {
        let records = vec![
            SourceRecord {
                id: "id1".to_string(),
                kind: SourceKind::TranscriptDay,
                title: "Course A".to_string(),
                status: SourceStatus::Ready,
                created_at: "t1".to_string(),
                updated_at: "t1".to_string(),
                length: Some("5 videos".to_string()),
                path: "p1".to_string(),
                metadata: serde_json::json!({"course_id": "1"}),
                last_error: None,
                job_id: None,
            },
            SourceRecord {
                id: "id2".to_string(),
                kind: SourceKind::Note,
                title: "Notes B".to_string(),
                status: SourceStatus::Processing,
                created_at: "t2".to_string(),
                updated_at: "t2".to_string(),
                length: Some("100 lines".to_string()),
                path: "p2".to_string(),
                metadata: serde_json::json!({}),
                last_error: Some("timeout".to_string()),
                job_id: Some("job123".to_string()),
            },
        ];

        let json = serde_json::to_string(&records).unwrap();
        let parsed: Vec<SourceRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, "id1");
        assert_eq!(parsed[1].id, "id2");
        assert_eq!(parsed[1].last_error.as_deref(), Some("timeout"));
        assert_eq!(parsed[1].job_id.as_deref(), Some("job123"));
    }
}
