//! Notes handlers: POST /api/notes/complete, GET /api/notes/diff.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use std::fs;

use crate::diff;
use crate::pipeline::PipelineRunner;
use crate::web::app::AppState;
use crate::web::jobs::JobStatus;

// ---------------------------------------------------------------------------
// NotesDiffQuery
// ---------------------------------------------------------------------------

/// `GET /api/notes/diff?base=...&patched=...`
///
/// Returns JSON hunks using the deterministic line diff.
#[derive(Debug, Deserialize)]
pub(crate) struct NotesDiffQuery {
    pub(crate) base: String,
    pub(crate) patched: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/notes/complete` -- patch notes with transcripts.
///
/// Body: `{ notes_path, transcripts_dir, output_notes, output_patches }`
pub(crate) async fn api_notes_complete(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let notes_path = body
        .get("notes_path")
        .and_then(|v| v.as_str())
        .unwrap_or("notes.md")
        .to_string();
    let transcripts_dir = body
        .get("transcripts_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("artifacts/transcripts")
        .to_string();
    let output_notes = body
        .get("output_notes")
        .and_then(|v| v.as_str())
        .unwrap_or("artifacts/notes/notes.patched.md")
        .to_string();
    let output_patches = body
        .get("output_patches")
        .and_then(|v| v.as_str())
        .unwrap_or("artifacts/notes/patches.json")
        .to_string();

    let fields = serde_json::json!({
        "notes_path": notes_path,
        "transcripts_dir": transcripts_dir,
        "output_notes": output_notes,
        "output_patches": output_patches,
    });
    let _ = state.store.update_and_save(&fields);

    let registry = state.registry.clone();
    let registry_clone = registry.clone();
    let saved_secrets = state.secrets.load();

    let job = registry.run_in_background("patch-notes", move |job| {
        saved_secrets.apply_to_env();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let runner = match PipelineRunner::new(".") {
                Ok(r) => r,
                Err(e) => {
                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Failed),
                        None,
                        Some(&format!("Failed to create pipeline runner: {}", e)),
                        None,
                        None,
                    );
                    return;
                }
            };
            let result = runner
                .patch_notes(
                    &notes_path,
                    &transcripts_dir,
                    &output_notes,
                    &output_patches,
                )
                .await;
            let result_json = serde_json::to_value(&result).unwrap_or_default();
            registry_clone.update(
                &job.job_id,
                Some(if result.ok() {
                    JobStatus::Succeeded
                } else {
                    JobStatus::Failed
                }),
                None,
                None,
                None,
                Some(result_json),
            );
        });
    });

    Json(serde_json::json!({
        "job_id": job.job_id,
        "status": "running"
    }))
}

/// `GET /api/notes/diff?base=...&patched=...`
///
/// Returns JSON hunks using the deterministic line diff.
pub(crate) async fn api_notes_diff(Query(query): Query<NotesDiffQuery>) -> Json<serde_json::Value> {
    let base_content = match fs::read_to_string(&query.base) {
        Ok(c) => c,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to read base file {}: {}", query.base, e),
                "hunks": [],
            }));
        }
    };

    let patched_content = match fs::read_to_string(&query.patched) {
        Ok(c) => c,
        Err(e) => {
            return Json(serde_json::json!({
                "error": format!("Failed to read patched file {}: {}", query.patched, e),
                "hunks": [],
            }));
        }
    };

    let hunks = diff::line_diff(&base_content, &patched_content, 3);
    let unified = diff::unified_diff(&base_content, &patched_content, 3);

    Json(serde_json::json!({
        "base": query.base,
        "patched": query.patched,
        "hunks": hunks,
        "unified": unified,
    }))
}
