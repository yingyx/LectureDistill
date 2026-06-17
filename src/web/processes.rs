//! Process registry for the Web GUI.
//!
//! A Process is a durable record of applying one or more output methods to one
//! or more Sources.  Output methods may have dependencies; for example,
//! `cheating_sheet` is rendered from a completed `note_patch`.
//!
//! Processes are persisted to `<project_dir>/artifacts/processes.json` and
//! artifact files are stored under `<project_dir>/artifacts/processes/<id>/...`.
//! No secrets are ever stored in the process registry.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Process model
// ---------------------------------------------------------------------------

/// Kinds of output methods supported by a Process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOutputKind {
    NotePatch,
    ReferenceDigest,
    CheatingSheet,
}

impl std::fmt::Display for ProcessOutputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotePatch => write!(f, "note_patch"),
            Self::ReferenceDigest => write!(f, "reference_digest"),
            Self::CheatingSheet => write!(f, "cheating_sheet"),
        }
    }
}

/// Processing status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStatus {
    Ready,
    Processing,
    Failed,
}

impl std::fmt::Display for ProcessStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => write!(f, "ready"),
            Self::Processing => write!(f, "processing"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

/// A single output within a Process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutput {
    /// Stable unique identifier (UUID v4).
    pub id: String,
    /// Kind of output method (e.g. "note_patch").
    pub kind: ProcessOutputKind,
    /// Status of this output.
    pub status: ProcessStatus,
    /// Human-readable title.
    pub title: String,
    /// Path to the output artifact (.md).
    pub path: String,
    /// Optional path to a unified diff file (.diff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_path: Option<String>,
    /// The source ID of the base Note, if one was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_source_id: Option<String>,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-update timestamp.
    pub updated_at: String,
    /// Last error message, if the output is in failed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Extensible status metadata, e.g. progress counters.
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// A single process record in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessRecord {
    /// Stable unique identifier (UUID v4).
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Overall processing status.
    pub status: ProcessStatus,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// ISO-8601 last-update timestamp.
    pub updated_at: String,
    /// Source IDs that are inputs to this process.
    pub source_ids: Vec<String>,
    /// Output method records.
    #[serde(default)]
    pub outputs: Vec<ProcessOutput>,
    /// Last error message, if the process is in failed state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Associated background job ID, if a job is running or has run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

impl ProcessRecord {
    pub fn now_iso() -> String {
        chrono::Utc::now().to_rfc3339()
    }
}

// ---------------------------------------------------------------------------
// Process store
// ---------------------------------------------------------------------------

/// Persistence layer for process records.
///
/// Reads/writes a JSON array at `<project_dir>/artifacts/processes.json`.
pub struct ProcessStore {
    path: PathBuf,
    artifacts_dir: PathBuf,
}

impl ProcessStore {
    const FILENAME: &'static str = "artifacts/processes.json";

    pub fn new(project_dir: &str) -> Self {
        let base = Path::new(project_dir);
        let path = base.join(Self::FILENAME);
        let artifacts_dir = base.join("artifacts/processes");
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

    /// Load all process records from disk. Returns empty vec if file is missing
    /// or corrupted.
    pub fn load_all(&self) -> Vec<ProcessRecord> {
        match fs::read_to_string(&self.path) {
            Ok(content) => serde_json::from_str::<Vec<ProcessRecord>>(&content).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Save all process records to disk.
    pub fn save_all(&self, records: &[ProcessRecord]) -> Result<()> {
        self.ensure_dirs()?;
        let json = serde_json::to_string_pretty(records)?;
        fs::write(&self.path, json)?;
        Ok(())
    }

    /// Insert a new record and persist.
    pub fn insert(&self, record: ProcessRecord) -> Result<ProcessRecord> {
        let mut records = self.load_all();
        records.push(record.clone());
        self.save_all(&records)?;
        Ok(record)
    }

    /// Update an existing record by ID and persist. Returns None if not found.
    pub fn update(
        &self,
        id: &str,
        updater: impl FnOnce(&mut ProcessRecord),
    ) -> Option<ProcessRecord> {
        let mut records = self.load_all();
        let idx = records.iter().position(|r| r.id == id)?;
        updater(&mut records[idx]);
        records[idx].updated_at = ProcessRecord::now_iso();
        let updated = records[idx].clone();
        let _ = self.save_all(&records);
        Some(updated)
    }

    /// Delete a record by ID and persist. Returns the removed record or None.
    pub fn delete(&self, id: &str) -> Option<ProcessRecord> {
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
    pub fn get(&self, id: &str) -> Option<ProcessRecord> {
        self.load_all().into_iter().find(|r| r.id == id)
    }

    /// Build the artifacts directory path for a given process ID.
    pub fn process_dir(&self, process_id: &str) -> PathBuf {
        self.artifacts_dir.join(process_id)
    }

    /// Build the output markdown artifact path.
    pub fn output_path(&self, process_id: &str, output_id: &str) -> PathBuf {
        self.process_dir(process_id)
            .join(format!("{}.md", output_id))
    }

    /// Build the output diff artifact path.
    pub fn diff_path(&self, process_id: &str, output_id: &str) -> PathBuf {
        self.process_dir(process_id)
            .join(format!("{}.diff", output_id))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(id: &str) -> ProcessRecord {
        ProcessRecord {
            id: id.to_string(),
            title: format!("Process {}", id),
            status: ProcessStatus::Ready,
            created_at: ProcessRecord::now_iso(),
            updated_at: ProcessRecord::now_iso(),
            source_ids: vec!["src1".to_string()],
            outputs: vec![],
            last_error: None,
            job_id: None,
        }
    }

    // -----------------------------------------------------------------------
    // Serialization
    // -----------------------------------------------------------------------

    #[test]
    fn test_process_record_serialization() {
        let output = ProcessOutput {
            id: "out1".to_string(),
            kind: ProcessOutputKind::NotePatch,
            status: ProcessStatus::Ready,
            title: "Note Patch".to_string(),
            path: "artifacts/processes/p1/out1.md".to_string(),
            diff_path: Some("artifacts/processes/p1/out1.diff".to_string()),
            base_source_id: Some("note_src".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_error: None,
            metadata: serde_json::json!({}),
        };

        let record = ProcessRecord {
            id: "p1".to_string(),
            title: "My Process".to_string(),
            status: ProcessStatus::Ready,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            source_ids: vec!["src1".to_string(), "src2".to_string()],
            outputs: vec![output],
            last_error: None,
            job_id: Some("job1".to_string()),
        };

        let json = serde_json::to_string_pretty(&record).unwrap();
        assert!(json.contains("note_patch"));
        assert!(json.contains("My Process"));
        assert!(json.contains("job1"));
        // last_error should not appear when None.
        assert!(!json.contains("last_error"));
    }

    #[test]
    fn test_process_record_deserialization() {
        let json = r#"{
            "id": "abc",
            "title": "Test",
            "status": "ready",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "source_ids": ["s1"],
            "outputs": [{
                "id": "o1",
                "kind": "note_patch",
                "status": "ready",
                "title": "Note Patch",
                "path": "p/o1.md",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }]
        }"#;
        let record: ProcessRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.id, "abc");
        assert_eq!(record.status, ProcessStatus::Ready);
        assert_eq!(record.outputs.len(), 1);
        assert_eq!(record.outputs[0].kind, ProcessOutputKind::NotePatch);
        assert!(record.last_error.is_none());
        assert!(record.job_id.is_none());
    }

    #[test]
    fn test_process_status_display() {
        assert_eq!(ProcessStatus::Ready.to_string(), "ready");
        assert_eq!(ProcessStatus::Processing.to_string(), "processing");
        assert_eq!(ProcessStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn test_process_output_kind_display() {
        assert_eq!(ProcessOutputKind::NotePatch.to_string(), "note_patch");
        assert_eq!(
            ProcessOutputKind::ReferenceDigest.to_string(),
            "reference_digest"
        );
        assert_eq!(
            ProcessOutputKind::CheatingSheet.to_string(),
            "cheating_sheet"
        );
    }

    // -----------------------------------------------------------------------
    // ProcessStore
    // -----------------------------------------------------------------------

    #[test]
    fn test_store_insert_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());

        let record = make_record("test-id");
        store.insert(record.clone()).unwrap();
        let all = store.load_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, record.id);
        assert_eq!(all[0].title, record.title);
    }

    #[test]
    fn test_store_update() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());

        let record = make_record("update-test");
        store.insert(record.clone()).unwrap();

        let updated = store
            .update(&record.id, |r| {
                r.title = "Updated".to_string();
                r.status = ProcessStatus::Failed;
                r.last_error = Some("test error".to_string());
            })
            .unwrap();

        assert_eq!(updated.title, "Updated");
        assert_eq!(updated.status, ProcessStatus::Failed);
        assert_eq!(updated.last_error.as_deref(), Some("test error"));

        let reloaded = store.get(&record.id).unwrap();
        assert_eq!(reloaded.title, "Updated");
    }

    #[test]
    fn test_store_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());

        let id = uuid::Uuid::new_v4().to_string();
        let record = make_record(&id);
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
        let store = ProcessStore::new(dir.path().to_str().unwrap());
        assert!(store.get("nonexistent").is_none());
    }

    #[test]
    fn test_store_empty_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());
        let all = store.load_all();
        assert!(all.is_empty());
    }

    #[test]
    fn test_store_corrupted_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());
        store.ensure_dirs().unwrap();
        fs::write(&store.path, "not valid json").unwrap();
        let all = store.load_all();
        assert!(all.is_empty());
    }

    // -----------------------------------------------------------------------
    // Artifact paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_artifact_paths() {
        let store = ProcessStore::new("/tmp/proj");
        let proc_dir = store.process_dir("proc1");
        assert!(proc_dir.to_string_lossy().contains("processes"));
        assert!(proc_dir.to_string_lossy().contains("proc1"));

        let out_path = store.output_path("proc1", "out1");
        assert!(out_path.to_string_lossy().ends_with("out1.md"));

        let diff_path = store.diff_path("proc1", "out1");
        assert!(diff_path.to_string_lossy().ends_with("out1.diff"));
    }

    // -----------------------------------------------------------------------
    // Outputs within record lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn test_add_output_to_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());

        let record = make_record("with-output");
        store.insert(record).unwrap();

        let output_id = uuid::Uuid::new_v4().to_string();
        let updated = store
            .update("with-output", |r| {
                r.outputs.push(ProcessOutput {
                    id: output_id.clone(),
                    kind: ProcessOutputKind::NotePatch,
                    status: ProcessStatus::Ready,
                    title: "Note Patch".to_string(),
                    path: "some/path.md".to_string(),
                    diff_path: None,
                    base_source_id: None,
                    created_at: ProcessRecord::now_iso(),
                    updated_at: ProcessRecord::now_iso(),
                    last_error: None,
                    metadata: serde_json::json!({}),
                });
            })
            .unwrap();

        assert_eq!(updated.outputs.len(), 1);
        assert_eq!(updated.outputs[0].id, output_id);
    }

    #[test]
    fn test_remove_output_from_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());

        let mut record = make_record("remove-output");
        record.outputs.push(ProcessOutput {
            id: "out-to-remove".to_string(),
            kind: ProcessOutputKind::NotePatch,
            status: ProcessStatus::Ready,
            title: "Note Patch".to_string(),
            path: "some/path.md".to_string(),
            diff_path: None,
            base_source_id: None,
            created_at: ProcessRecord::now_iso(),
            updated_at: ProcessRecord::now_iso(),
            last_error: None,
            metadata: serde_json::json!({}),
        });
        store.insert(record).unwrap();

        let updated = store
            .update("remove-output", |r| {
                r.outputs.retain(|o| o.id != "out-to-remove");
            })
            .unwrap();

        assert_eq!(updated.outputs.len(), 0);
    }

    #[test]
    fn test_multiple_records_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcessStore::new(dir.path().to_str().unwrap());

        let r1 = make_record("r1");
        let mut r2 = make_record("r2");
        r2.last_error = Some("something broke".to_string());
        r2.outputs.push(ProcessOutput {
            id: "out1".to_string(),
            kind: ProcessOutputKind::NotePatch,
            status: ProcessStatus::Failed,
            title: "Bad output".to_string(),
            path: "bad.md".to_string(),
            diff_path: None,
            base_source_id: Some("note1".to_string()),
            created_at: ProcessRecord::now_iso(),
            updated_at: ProcessRecord::now_iso(),
            last_error: Some("LLM unavailable".to_string()),
            metadata: serde_json::json!({}),
        });

        store.insert(r1).unwrap();
        store.insert(r2).unwrap();

        let all = store.load_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "r1");
        assert_eq!(all[1].id, "r2");
        assert_eq!(all[1].last_error.as_deref(), Some("something broke"));
        assert_eq!(
            all[1].outputs[0].last_error.as_deref(),
            Some("LLM unavailable")
        );
    }
}
