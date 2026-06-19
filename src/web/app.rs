//! Axum app factory and routes for the lecture-distill Web GUI.
//!
//! Provides:
//! - JSON APIs under `/api/...`
//! - React SPA serving (embedded via rust-embed, with filesystem fallback)

use std::collections::HashMap;
use std::fs;
use std::path::Path as FsPath;
use std::sync::Arc;

use axum::{
    extract::State,
    response::Json,
    routing::{get, post, put},
    Router,
};

use crate::utils::output::walk_output_dir;
use crate::web::handlers::{
    canvas::{api_canvas_fetch_subtitles, api_canvas_list_videos},
    courses::{api_canvas_course_dates, api_canvas_courses},
    jobs::{api_job_status, api_list_jobs, job_status},
    llm_logs::{api_get_llm_log, api_list_llm_logs},
    notes::{api_notes_complete, api_notes_diff},
    processes::{
        api_add_process_output, api_create_process, api_delete_process_output, api_get_process,
        api_get_process_output, api_get_process_output_file, api_get_processes, api_retry_process,
        api_retry_process_output, api_revise_process_output, api_stream_process_output,
    },
    secrets::{api_get_secrets, api_patch_secrets},
    sources::{
        api_ask_source, api_create_note_source, api_create_transcript_course_source,
        api_create_transcript_day_source, api_delete_source, api_get_source, api_get_source_index,
        api_get_sources, api_reindex_source, api_source_ask_stream, api_sync_source,
        api_update_note_source,
    },
    spa::{serve_spa_asset, serve_spa_fallback, serve_spa_index},
    state_config::{api_get_state, api_patch_state},
    transcripts::api_transcripts_status,
};
use crate::web::jobs::JobRegistry;
use crate::web::processes::ProcessStore;
use crate::web::secrets::SecretStore;
use crate::web::sources::SourceStore;
use crate::web::state::ProjectStateStore;

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<ProjectStateStore>,
    pub secrets: Arc<SecretStore>,
    pub registry: Arc<JobRegistry>,
    pub source_store: Arc<SourceStore>,
    pub process_store: Arc<ProcessStore>,
    pub version: String,
    pub project_dir: String,
}

// ---------------------------------------------------------------------------
// App factory
// ---------------------------------------------------------------------------

/// Create and configure the Axum application.
pub fn create_app(project_dir: &str) -> Router {
    let project_state_store = ProjectStateStore::new(project_dir);
    let project_state = project_state_store.load();
    std::env::set_var(
        "LECTURE_DISTILL_LLM_MAX_CONCURRENCY",
        project_state.llm_max_concurrency.clamp(1, 32).to_string(),
    );
    if project_state.typst_path.trim().is_empty() {
        std::env::remove_var("LECTURE_DISTILL_TYPST_PATH");
    } else {
        std::env::set_var(
            "LECTURE_DISTILL_TYPST_PATH",
            project_state.typst_path.trim(),
        );
    }

    // Point LLM logging at the project's artifacts directory.
    std::env::set_var(
        "LECTURE_DISTILL_LLM_LOG_DIR",
        std::path::Path::new(project_dir).join("artifacts/llm-logs"),
    );

    let state = AppState {
        version: env!("CARGO_PKG_VERSION").to_string(),
        store: Arc::new(project_state_store),
        secrets: Arc::new(SecretStore::new(project_dir)),
        registry: Arc::new(JobRegistry::new(100)),
        source_store: Arc::new(SourceStore::new(project_dir)),
        process_store: Arc::new(ProcessStore::new(project_dir)),
        project_dir: project_dir.to_string(),
    };

    Router::new()
        // ------------------------------------------------------------------
        // JSON API routes
        // ------------------------------------------------------------------
        .route("/api/state", get(api_get_state).patch(api_patch_state))
        .route(
            "/api/secrets",
            get(api_get_secrets).patch(api_patch_secrets),
        )
        .route("/api/outputs", get(api_get_outputs))
        .route("/api/jobs", get(api_list_jobs))
        .route("/api/jobs/{job_id}", get(api_job_status))
        .route("/api/llm-logs", get(api_list_llm_logs))
        .route("/api/llm-logs/{log_id}", get(api_get_llm_log))
        .route("/api/canvas/list-videos", post(api_canvas_list_videos))
        .route(
            "/api/canvas/fetch-subtitles",
            post(api_canvas_fetch_subtitles),
        )
        .route("/api/transcripts/status", get(api_transcripts_status))
        .route("/api/notes/complete", post(api_notes_complete))
        .route("/api/notes/diff", get(api_notes_diff))
        // ------------------------------------------------------------------
        // Source management APIs
        // ------------------------------------------------------------------
        .route("/api/sources", get(api_get_sources))
        .route("/api/sources/note", post(api_create_note_source))
        .route(
            "/api/sources/transcript-day",
            post(api_create_transcript_day_source),
        )
        .route(
            "/api/sources/transcript-course",
            post(api_create_transcript_course_source),
        )
        .route(
            "/api/sources/{id}",
            get(api_get_source).delete(api_delete_source),
        )
        .route("/api/sources/{id}/index", get(api_get_source_index))
        .route("/api/sources/{id}/reindex", post(api_reindex_source))
        .route("/api/sources/{id}/note", put(api_update_note_source))
        .route("/api/sources/{id}/sync", post(api_sync_source))
        .route("/api/sources/{id}/ask", post(api_ask_source))
        .route("/api/sources/{id}/ask-stream", get(api_source_ask_stream))
        // ------------------------------------------------------------------
        // Process management APIs
        // ------------------------------------------------------------------
        .route(
            "/api/processes",
            get(api_get_processes).post(api_create_process),
        )
        .route("/api/processes/{id}", get(api_get_process))
        .route("/api/processes/{id}/retry", post(api_retry_process))
        .route("/api/processes/{id}/outputs", post(api_add_process_output))
        .route(
            "/api/processes/{id}/outputs/{output_id}",
            get(api_get_process_output).delete(api_delete_process_output),
        )
        .route(
            "/api/processes/{id}/outputs/{output_id}/revise",
            post(api_revise_process_output),
        )
        .route(
            "/api/processes/{id}/outputs/{output_id}/retry",
            post(api_retry_process_output),
        )
        .route(
            "/api/processes/{id}/outputs/{output_id}/stream",
            get(api_stream_process_output),
        )
        .route(
            "/api/processes/{id}/outputs/{output_id}/file",
            get(api_get_process_output_file),
        )
        // ------------------------------------------------------------------
        // Canvas LMS courses API
        // ------------------------------------------------------------------
        .route("/api/canvas/courses", get(api_canvas_courses))
        .route("/api/canvas/course-dates", get(api_canvas_course_dates))
        // ------------------------------------------------------------------
        // Legacy job API
        // ------------------------------------------------------------------
        .route("/jobs/{job_id}", get(job_status))
        // ------------------------------------------------------------------
        // SPA: serve embedded / filesystem assets, fallback to index.html
        // ------------------------------------------------------------------
        .route("/assets/{*path}", get(serve_spa_asset))
        .route("/{*path}", get(serve_spa_fallback))
        .route("/", get(serve_spa_index))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// JSON API -- outputs (stays in app.rs as it's tightly coupled to AppState)
// ---------------------------------------------------------------------------

/// `GET /api/outputs` — list artifacts grouped by category.
async fn api_get_outputs(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut groups: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    // Only scan user-output directories, not internal work files.
    let artifacts_dir = FsPath::new(&state.project_dir).join("artifacts");
    let output_dirs: &[&str] = &["artifacts/outputs", "artifacts/notes"];

    for sub in output_dirs {
        let walk_dir = FsPath::new(&state.project_dir).join(sub);
        if !walk_dir.exists() {
            continue;
        }
        walk_output_dir(&artifacts_dir, &walk_dir, &mut groups);
    }

    // Also scan top-level non-hidden, non-internal files.
    if let Ok(entries) = fs::read_dir(&state.project_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name.starts_with('.')
                || file_name == "secrets.local.json"
                || file_name == "sources.json"
                || file_name == "processes.json"
                || file_name == "config.json"
            {
                continue;
            }
            if path.is_file() {
                let rel = path
                    .strip_prefix(&state.project_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_string();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let category = match ext.as_str() {
                    "json" => "transcripts",
                    "srt" => "transcripts",
                    "md" => "notes",
                    "pdf" => "outputs",
                    "tex" => "outputs",
                    _ => "other",
                };
                groups
                    .entry(category.to_string())
                    .or_default()
                    .push(serde_json::json!({
                        "name": file_name,
                        "path": rel,
                        "size": size,
                        "ext": ext,
                    }));
            }
        }
    }

    // Sort each group by name.
    for items in groups.values_mut() {
        items.sort_by(|a, b| {
            let na = a["name"].as_str().unwrap_or("");
            let nb = b["name"].as_str().unwrap_or("");
            nb.cmp(na)
        });
    }

    Json(serde_json::json!({
        "outputs": groups,
    }))
}
