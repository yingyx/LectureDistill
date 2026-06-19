//! Transcript handlers: GET /api/transcripts/status.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use std::fs;
use std::path::Path as FsPath;

use crate::artifacts::TranscriptArtifact;
use crate::web::app::AppState;

// ---------------------------------------------------------------------------
// TranscriptsStatusQuery
// ---------------------------------------------------------------------------

/// `GET /api/transcripts/status?course_id=...&transcripts_dir=...`
#[derive(Debug, Deserialize)]
pub(crate) struct TranscriptsStatusQuery {
    #[serde(default, rename = "course_id")]
    _course_id: String,
    #[serde(default = "default_transcripts_dir")]
    transcripts_dir: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn default_transcripts_dir() -> String {
    "artifacts/transcripts".to_string()
}

fn resolve_project_path(project_dir: &str, path: &str) -> std::path::PathBuf {
    let path = FsPath::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        FsPath::new(project_dir).join(path)
    }
}

fn transcript_date(artifact: &TranscriptArtifact) -> String {
    artifact
        .recorded_at
        .as_deref()
        .or(Some(artifact.fetched_at.as_str()))
        .and_then(|value| value.get(0..10))
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/transcripts/status?course_id=...&transcripts_dir=...`
pub(crate) async fn api_transcripts_status(
    State(state): State<AppState>,
    Query(query): Query<TranscriptsStatusQuery>,
) -> Json<serde_json::Value> {
    let dir = resolve_project_path(&state.project_dir, &query.transcripts_dir);

    if !dir.exists() {
        return Json(serde_json::json!({
            "exists": false,
            "count": 0,
            "files": [],
        }));
    }

    let mut files: Vec<serde_json::Value> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);

            // Try to read the transcript for extra metadata.
            let mut video_title = String::new();
            let mut segment_count: usize = 0;
            let mut date_str = String::new();

            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(artifact) = serde_json::from_str::<TranscriptArtifact>(&content) {
                    date_str = transcript_date(&artifact);
                    video_title = artifact.video_title;
                    segment_count = artifact.segments.len();
                }
            }

            files.push(serde_json::json!({
                "name": file_name,
                "size": size,
                "video_title": video_title,
                "segment_count": segment_count,
                "date": date_str,
            }));
        }
    }

    files.sort_by(|a, b| {
        let da = a["date"].as_str().unwrap_or("");
        let db = b["date"].as_str().unwrap_or("");
        db.cmp(da)
    });

    Json(serde_json::json!({
        "exists": true,
        "count": files.len(),
        "files": files,
    }))
}
