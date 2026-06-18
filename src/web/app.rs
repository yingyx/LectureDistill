//! Axum app factory and routes for the lecture-distill Web GUI.
//!
//! Provides:
//! - JSON APIs under `/api/...`
//! - React SPA serving (embedded via rust-embed, with filesystem fallback)
//!
use crate::artifacts::{KeepLevel, PatchEntry, TranscriptArtifact};
use crate::canvas_sjtu::CanvasPptSlice;
use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode, Uri},
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Json, Response},
    routing::{get, post, put},
    Router,
};
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::fs;
use std::path::Path as FsPath;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::diff;
use crate::latex;
use crate::llm::{self, compact_transcript_for_llm};
use crate::llm_log;
use crate::pipeline::{PipelineResult, PipelineRunner};
use crate::web::course::{
    bm25_search, estimate_token_count, extract_timestamp_ranges, read_indexes, read_manifest,
    split_note_sections, truncate_chars, write_index, write_manifest, CourseDateIndex,
    CourseManifest, CourseManifestDate, RetrievalMatch, RetrievalTrace, TimestampRange,
};
use crate::web::jobs::{JobRegistry, JobStatus};
use crate::web::processes::{
    ProcessOutput, ProcessOutputKind, ProcessRecord, ProcessStatus as ProcessRecordStatus,
    ProcessStore,
};
use crate::web::secrets::SecretStore;
use crate::web::sources::{
    deterministic_answer as source_deterministic_answer, truncate_for_llm, SourceKind,
    SourceRecord, SourceStatus, SourceStore,
};
use crate::web::state::ProjectStateStore;

// ---------------------------------------------------------------------------
// PlannedSection — staged course-note outline
// ---------------------------------------------------------------------------

/// A planned section in a course note outline.
///
/// Built by the LLM during the outline phase; used to drive per-section
/// retrieval and generation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PlannedSection {
    /// Section heading (used as `## <title>` in the output).
    pub title: String,
    /// Why this section exists in the outline.
    pub purpose: String,
    /// Date sub-strings that hint which indexes to retrieve first.
    #[serde(default)]
    pub date_hints: Vec<String>,
    /// Video-ID sub-strings that hint which indexes to retrieve first.
    #[serde(default)]
    pub video_hints: Vec<String>,
    /// Extra search terms for BM25 fallback retrieval.
    #[serde(default)]
    pub query_terms: Vec<String>,
    /// Key concepts / formulas / definitions that must appear.
    #[serde(default)]
    pub must_include: Vec<String>,
}

// ---------------------------------------------------------------------------
// Embedded frontend assets (production)
// ---------------------------------------------------------------------------

/// Embedded React/Vite build output.
///
/// When `web/dist/` exists at compile time, `rust-embed` captures it.
/// When it does not, the struct is still valid but `get()` returns `None`
/// for every path, and the server falls back to filesystem serving.
#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

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

fn web_log(message: impl AsRef<str>) {
    eprintln!("[lecture-distill:web] {}", message.as_ref());
}

fn format_clock(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn is_sentence_terminal(text: &str) -> bool {
    text.trim_end()
        .chars()
        .last()
        .map(|ch| matches!(ch, '.' | '!' | '?'))
        .unwrap_or(false)
}

fn push_joined_text(target: &mut String, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if !target.is_empty()
        && !target.ends_with(char::is_whitespace)
        && !text.starts_with(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '!' | '?'))
    {
        target.push(' ');
    }
    target.push_str(text);
}

fn split_sentence_chunks(text: &str, max_sentences: usize) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        sentences.push(current);
    }
    if sentences.is_empty() {
        return vec![text.trim().to_string()];
    }
    sentences
        .chunks(max_sentences.max(1))
        .map(|chunk| chunk.concat())
        .collect()
}
fn transcript_markdown_for_video(
    artifact: &TranscriptArtifact,
    ppt_slices: &[CanvasPptSlice],
) -> String {
    let mut slides = ppt_slices.to_vec();
    slides.sort_by(|a, b| a.create_sec.total_cmp(&b.create_sec));

    if slides.is_empty() {
        slides.push(CanvasPptSlice {
            create_sec: 0.0,
            ppt_img_url: None,
            ocr_words: Vec::new(),
        });
    }

    let max_end = artifact
        .segments
        .iter()
        .map(|segment| segment.end_time)
        .fold(0.0_f64, f64::max);

    #[derive(Debug)]
    struct SentenceLine {
        start_time: f64,
        end_time: f64,
        text: String,
    }

    let mut lines = Vec::new();
    let mut current_text = String::new();
    let mut current_start = 0.0_f64;
    let mut current_end = 0.0_f64;
    let mut has_current = false;

    for segment in &artifact.segments {
        if !has_current {
            current_start = segment.start_time;
            current_text.clear();
            has_current = true;
        }
        current_end = segment.end_time;
        push_joined_text(&mut current_text, &segment.text);
        if is_sentence_terminal(&segment.text) {
            lines.push(SentenceLine {
                start_time: current_start,
                end_time: current_end,
                text: std::mem::take(&mut current_text),
            });
            has_current = false;
        }
    }
    if has_current && !current_text.trim().is_empty() {
        lines.push(SentenceLine {
            start_time: current_start,
            end_time: current_end,
            text: current_text,
        });
    }

    let mut markdown = String::new();
    if !artifact.video_title.trim().is_empty() {
        markdown.push_str(&format!("## {}\n\n", artifact.video_title.trim()));
    }

    for (idx, slide) in slides.iter().enumerate() {
        let slide_start = slide.create_sec.max(0.0);
        let next_start = slides.get(idx + 1).map(|next| next.create_sec);
        let raw_slide_end = next_start.unwrap_or(max_end);
        let slide_end = next_start
            .map(|next| next.min(max_end.max(slide_start)))
            .unwrap_or_else(|| max_end.max(slide_start));

        markdown.push_str(&format!(
            "### Slide {} [{}-{}]\n\n",
            idx + 1,
            format_clock(slide_start),
            format_clock(slide_end)
        ));

        if !slide.ocr_words.is_empty() {
            markdown.push_str(&format!("_Slide OCR:_ {}\n\n", slide.ocr_words.join(" ")));
        }

        let range_end = next_start.unwrap_or(f64::MAX);
        for line in lines
            .iter()
            .filter(|line| line.start_time >= slide_start && line.start_time < range_end)
        {
            let chunks =
                if line.end_time - line.start_time > 180.0 || raw_slide_end - slide_start > 180.0 {
                    split_sentence_chunks(&line.text, 4)
                } else {
                    vec![line.text.trim().to_string()]
                };
            for chunk in chunks {
                if !chunk.is_empty() {
                    markdown.push_str(&format!(
                        "[{}] {}\n\n",
                        format_clock(line.start_time),
                        chunk
                    ));
                }
            }
        }
    }

    markdown
}

// ---------------------------------------------------------------------------
// API -- Job status
// ---------------------------------------------------------------------------

async fn job_status(State(state): State<AppState>, Path(job_id): Path<String>) -> Response {
    match state.registry.get(&job_id) {
        Some(job) => Json(serde_json::to_value(&job).unwrap_or_default()).into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Job not found"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// JSON API -- state
// ---------------------------------------------------------------------------

/// `GET /api/state` 闁?return current project state (never exposes secrets).
async fn api_get_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    apply_project_runtime_config(&state);
    let project_state = state.store.load();
    let secrets = state.secrets.load();
    let config = project_state.to_config_value();

    Json(serde_json::json!({
        "version": state.version,
        "project_dir": state.project_dir,
        "config": config,
        "llm_available": llm::is_available() || !secrets.llm_api_key.is_empty(),
        "typst_compiler": latex::find_typst_compiler(),
        "latex_compiler": latex::find_latex_compiler(),
        "pdf_renderer": latex::find_pdf_renderer(),
    }))
}

fn apply_project_runtime_config(state: &AppState) {
    let project_state = state.store.load();
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
}

/// `PATCH /api/state` 闁?update project state fields.
#[derive(Debug, Deserialize)]
struct PatchStateBody {
    #[serde(default)]
    fields: HashMap<String, serde_json::Value>,
}

async fn api_patch_state(
    State(state): State<AppState>,
    Json(body): Json<PatchStateBody>,
) -> Json<serde_json::Value> {
    let fields_value = serde_json::to_value(&body.fields).unwrap_or_default();
    match state.store.update_and_save(&fields_value) {
        Ok(updated) => {
            std::env::set_var(
                "LECTURE_DISTILL_LLM_MAX_CONCURRENCY",
                updated.llm_max_concurrency.clamp(1, 32).to_string(),
            );
            if updated.typst_path.trim().is_empty() {
                std::env::remove_var("LECTURE_DISTILL_TYPST_PATH");
            } else {
                std::env::set_var("LECTURE_DISTILL_TYPST_PATH", updated.typst_path.trim());
            }
            let config = updated.to_config_value();
            Json(serde_json::json!({
                "status": "ok",
                "config": config,
            }))
        }
        Err(e) => Json(serde_json::json!({
            "status": "error",
            "error": e.to_string(),
        })),
    }
}

// ---------------------------------------------------------------------------
// JSON API -- secrets
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PatchSecretsBody {
    #[serde(default)]
    fields: HashMap<String, serde_json::Value>,
    #[serde(default)]
    clear: Vec<String>,
}

/// `GET /api/secrets` returns only redacted credential status.
async fn api_get_secrets(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "secrets": state.secrets.load().status_value(),
    }))
}

/// `PATCH /api/secrets` stores local credentials in `secrets.local.json`.
async fn api_patch_secrets(
    State(state): State<AppState>,
    Json(body): Json<PatchSecretsBody>,
) -> Json<serde_json::Value> {
    match state.secrets.update(&body.fields, &body.clear) {
        Ok(secrets) => Json(serde_json::json!({
            "status": "ok",
            "secrets": secrets.status_value(),
        })),
        Err(e) => Json(serde_json::json!({
            "status": "error",
            "error": e.to_string(),
        })),
    }
}

// ---------------------------------------------------------------------------
// JSON API -- outputs
// ---------------------------------------------------------------------------

/// `GET /api/outputs` 闁?list artifacts grouped by category.
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

fn walk_output_dir(
    base: &FsPath,
    dir: &FsPath,
    groups: &mut HashMap<String, Vec<serde_json::Value>>,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                // Skip internal source artifact directories and job dirs.
                if file_name == "sources"
                    || file_name == "transcripts"
                    || file_name == "jobs"
                    || file_name == "processes"
                {
                    continue;
                }
                walk_output_dir(base, &path, groups);
            } else if path.is_file() {
                // Skip sources.json at the artifacts level.
                if file_name == "sources.json" {
                    continue;
                }
                let rel = path
                    .strip_prefix(base)
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
}

// ---------------------------------------------------------------------------
// JSON API -- jobs
// ---------------------------------------------------------------------------

/// `GET /api/jobs` 闁?list recent jobs.
#[derive(Debug, Deserialize)]
struct ListJobsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    30
}

async fn api_list_jobs(
    State(state): State<AppState>,
    Query(query): Query<ListJobsQuery>,
) -> Json<serde_json::Value> {
    let jobs = state.registry.list_jobs(query.limit);
    Json(serde_json::json!({
        "jobs": jobs,
    }))
}

/// `GET /api/jobs/{job_id}` 闁?get individual job status.
async fn api_job_status(State(state): State<AppState>, Path(job_id): Path<String>) -> Response {
    match state.registry.get(&job_id) {
        Some(job) => Json(serde_json::to_value(&job).unwrap_or_default()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Job not found"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// JSON API -- LLM logs
// ---------------------------------------------------------------------------

/// `GET /api/llm-logs`  — list LLM call logs, newest first.
#[derive(Debug, Deserialize)]
struct ListLlmLogsQuery {
    #[serde(default = "default_llm_log_limit")]
    limit: usize,
}

fn default_llm_log_limit() -> usize {
    100
}

async fn api_list_llm_logs(Query(query): Query<ListLlmLogsQuery>) -> Json<serde_json::Value> {
    let limit = query.limit.min(1000);
    match llm_log::list_logs(limit) {
        Ok(logs) => Json(serde_json::json!({ "logs": logs })),
        Err(e) => Json(serde_json::json!({
            "logs": [],
            "error": e.to_string(),
        })),
    }
}

/// `GET /api/llm-logs/{log_id}`  — return the full JSON content of a single log.
async fn api_get_llm_log(Path(log_id): Path<String>) -> Response {
    match llm_log::read_log(&log_id) {
        Ok(value) => Json(value).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// JSON API -- canvas
// ---------------------------------------------------------------------------

/// `POST /api/canvas/list-videos`
async fn api_canvas_list_videos(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let course_id = body
        .get("course_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cookie_input = body
        .get("cookie")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let saved_secrets = state.secrets.load();
    let cookie = if cookie_input.trim().is_empty() {
        saved_secrets.canvas_auth_cookie().unwrap_or_default()
    } else {
        cookie_input
    };

    if course_id.is_empty() || cookie.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "errors": ["Course ID and Cookie are required. Save a Canvas/jAccount cookie in Settings or paste one here."]
        }));
    }

    // Save non-secret state.
    let fields = serde_json::json!({"course_id": course_id.as_str()});
    let _ = state.store.update_and_save(&fields);

    let registry = state.registry.clone();
    let registry_clone = registry.clone();
    let cid = course_id;
    let ck = cookie;

    let job = registry.run_in_background("list-videos", move |job| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut client = crate::canvas_sjtu::CanvasSJTUVideoClient::new(cid.clone(), ck);
            match client.list_videos().await {
                Ok(videos) => {
                    let video_list: Vec<serde_json::Value> = videos
                        .iter()
                        .map(|v| {
                            serde_json::json!({
                                "video_id": v.video_id,
                                "title": v.title,
                                "duration": v.duration,
                                "course_begin_time": v.course_begin_time,
                                "course_end_time": v.course_end_time,
                                "teacher": v.teacher,
                                "classroom": v.classroom,
                            })
                        })
                        .collect();
                    let result = serde_json::json!({
                        "videos": video_list,
                        "count": video_list.len(),
                    });
                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Succeeded),
                        Some(&format!("Found {} video(s)", video_list.len())),
                        None,
                        None,
                        Some(result),
                    );
                }
                Err(e) => {
                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Failed),
                        None,
                        Some(&e.to_string()),
                        None,
                        None,
                    );
                }
            }
        });
    });

    Json(serde_json::json!({
        "job_id": job.job_id,
        "status": "running"
    }))
}

/// `POST /api/canvas/fetch-subtitles`
///
/// Accepts `scope` (course|day|selected), optional `date`, optional
/// `video_ids` array.
async fn api_canvas_fetch_subtitles(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let course_id = body
        .get("course_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cookie_input = body
        .get("cookie")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let saved_secrets = state.secrets.load();
    let cookie = if cookie_input.trim().is_empty() {
        saved_secrets.canvas_auth_cookie().unwrap_or_default()
    } else {
        cookie_input
    };
    let scope = body
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("course")
        .to_string();
    let date = body
        .get("date")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let video_ids: Vec<String> = body
        .get("video_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let transcripts_dir = body
        .get("transcripts_dir")
        .and_then(|v| v.as_str())
        .unwrap_or("artifacts/transcripts")
        .to_string();

    if course_id.is_empty() || cookie.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "errors": ["Course ID and Cookie are required. Save a Canvas/jAccount cookie in Settings or paste one here."]
        }));
    }

    let fields = serde_json::json!({
        "course_id": course_id.as_str(),
        "transcripts_dir": transcripts_dir.as_str(),
    });
    let _ = state.store.update_and_save(&fields);

    let registry = state.registry.clone();
    let registry_clone = registry.clone();

    let job = registry.run_in_background("fetch-subtitles", move |job| {
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

            let result = match scope.as_str() {
                "selected" if !video_ids.is_empty() => {
                    // Fetch selected videos one by one.
                    let mut all_ok = true;
                    let mut errors: Vec<String> = Vec::new();
                    let mut logs: Vec<String> = Vec::new();
                    let mut paths: Vec<String> = Vec::new();

                    for vid in &video_ids {
                        match runner
                            .fetch_transcripts(&course_id, &cookie, Some(vid), &transcripts_dir)
                            .await
                        {
                            res if res.ok() => {
                                paths.extend(res.artifact_paths);
                                logs.extend(res.logs);
                            }
                            res => {
                                all_ok = false;
                                errors.extend(res.errors);
                                logs.extend(res.logs);
                            }
                        }
                    }

                    PipelineResult {
                        status: if all_ok {
                            "succeeded".to_string()
                        } else {
                            "failed".to_string()
                        },
                        artifact_paths: paths,
                        errors,
                        logs,
                    }
                }
                "day" if date.is_some() => {
                    // Fetch all, then filter by date.
                    let result = runner
                        .fetch_transcripts(&course_id, &cookie, None, &transcripts_dir)
                        .await;
                    result
                }
                _ => {
                    // "course" 闁?fetch all.
                    runner
                        .fetch_transcripts(&course_id, &cookie, None, &transcripts_dir)
                        .await
                }
            };

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

// ---------------------------------------------------------------------------
// JSON API -- transcripts status
// ---------------------------------------------------------------------------

/// `GET /api/transcripts/status?course_id=...&transcripts_dir=...`
#[derive(Debug, Deserialize)]
struct TranscriptsStatusQuery {
    #[serde(default, rename = "course_id")]
    _course_id: String,
    #[serde(default = "default_transcripts_dir")]
    transcripts_dir: String,
}

async fn api_transcripts_status(
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

// ---------------------------------------------------------------------------
// JSON API -- notes complete
// ---------------------------------------------------------------------------

/// `POST /api/notes/complete` 闁?patch notes with transcripts.
///
/// Body: `{ notes_path, transcripts_dir, output_notes, output_patches }`
async fn api_notes_complete(
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

// ---------------------------------------------------------------------------
// JSON API -- notes diff
// ---------------------------------------------------------------------------

/// `GET /api/notes/diff?base=...&patched=...`
///
/// Returns JSON hunks using the deterministic line diff.
#[derive(Debug, Deserialize)]
struct NotesDiffQuery {
    base: String,
    patched: String,
}

async fn api_notes_diff(Query(query): Query<NotesDiffQuery>) -> Json<serde_json::Value> {
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

// ---------------------------------------------------------------------------
// Source management APIs
// ---------------------------------------------------------------------------

/// `GET /api/sources` 闁?list all sources, newest first.
async fn api_get_sources(State(state): State<AppState>) -> Json<serde_json::Value> {
    reconcile_stale_source_jobs(&state);
    let mut sources = state.source_store.load_all();
    sources.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(serde_json::json!({
        "sources": sources,
    }))
}

fn reconcile_stale_source_jobs(state: &AppState) {
    let sources = state.source_store.load_all();
    for source in sources {
        if source.status != SourceStatus::Processing {
            continue;
        }

        let Some(job_id) = source.job_id.clone() else {
            let _ = state.source_store.update(&source.id, |r| {
                r.status = SourceStatus::Failed;
                r.last_error = Some(
                    "Source is marked processing but has no active job. Sync or reindex again."
                        .to_string(),
                );
            });
            continue;
        };

        match state.registry.get(&job_id) {
            Some(job) if matches!(job.status, JobStatus::Running | JobStatus::Pending) => {}
            Some(job) if job.status == JobStatus::Failed => {
                let error = job
                    .errors
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "Background job failed.".to_string());
                let _ = state.source_store.update(&source.id, |r| {
                    r.status = SourceStatus::Failed;
                    r.last_error = Some(error);
                });
            }
            Some(job) if job.status == JobStatus::Succeeded => {
                if source.kind == SourceKind::TranscriptCourse
                    && !FsPath::new(&source.path).exists()
                {
                    let _ = state.source_store.update(&source.id, |r| {
                        r.status = SourceStatus::Failed;
                        r.last_error = Some(
                            "Background job succeeded but course manifest is missing. Sync again."
                                .to_string(),
                        );
                    });
                }
            }
            _ => {
                let _ = state.source_store.update(&source.id, |r| {
                    r.status = SourceStatus::Failed;
                    r.last_error = Some(
                        "Background job is no longer active. Sync or reindex again.".to_string(),
                    );
                });
            }
        }
    }
}

/// `GET /api/sources/{id}` 闁?get a single source.
async fn api_get_source(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.source_store.get(&id) {
        Some(source) => Json(serde_json::to_value(&source).unwrap_or_default()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Source not found"})),
        )
            .into_response(),
    }
}

async fn api_get_source_index(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let source = match state.source_store.get(&id) {
        Some(source) => source,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Source not found"})),
            )
                .into_response();
        }
    };

    match source.kind {
        SourceKind::TranscriptCourse => match read_manifest(&source.path) {
            Ok(manifest) => {
                let indexes = read_indexes(&manifest);
                let missing_indexes = manifest.dates.len().saturating_sub(indexes.len());
                Json(serde_json::json!({
                    "status": source.status.to_string(),
                    "source_id": id,
                    "manifest": manifest,
                    "indexes": indexes,
                    "missing_index_count": missing_indexes,
                    "error": source.last_error,
                }))
                .into_response()
            }
            Err(e) => {
                let message = if source.status == SourceStatus::Processing {
                    "Course index is still being built. Refresh after the sync job finishes."
                        .to_string()
                } else if let Some(err) = source.last_error.clone() {
                    err
                } else {
                    format!("Course manifest is not available: {}", e)
                };
                Json(serde_json::json!({
                    "status": source.status.to_string(),
                    "source_id": id,
                    "manifest": serde_json::Value::Null,
                    "indexes": [],
                    "error": message,
                }))
                .into_response()
            }
        },
        SourceKind::TranscriptDay => {
            let content = fs::read_to_string(&source.path).unwrap_or_default();
            let index = CourseDateIndex {
                date: source
                    .metadata
                    .get("date")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                title: source.title.clone(),
                summary: truncate_chars(&content, 320),
                keywords: Vec::new(),
                concepts: Vec::new(),
                timestamp_ranges: extract_timestamp_ranges(&content),
                char_count: content.chars().count(),
                token_count: estimate_token_count(&content),
                source_path: source.path.clone(),
                status: source.status.to_string(),
            };
            Json(serde_json::json!({
                "status": "succeeded",
                "source_id": id,
                "indexes": [index],
            }))
            .into_response()
        }
        SourceKind::Note => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "status": "failed",
                "error": "Note sources do not have transcript indexes.",
            })),
        )
            .into_response(),
    }
}

async fn api_reindex_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let source = match state.source_store.get(&id) {
        Some(source) => source,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Source not found",
            }));
        }
    };

    let saved_secrets = state.secrets.load();
    saved_secrets.apply_to_env();
    if !crate::llm::is_available() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "LLM is required to rebuild transcript indexes.",
        }));
    }

    match source.kind {
        SourceKind::TranscriptCourse => {
            let manifest = match read_manifest(&source.path) {
                Ok(m) => m,
                Err(e) => {
                    return Json(serde_json::json!({
                        "status": "failed",
                        "error": format!("Failed to read manifest: {}", e),
                    }));
                }
            };

            let registry = state.registry.clone();
            let source_store = state.source_store.clone();
            let secrets = state.secrets.clone();
            let source_id = id.clone();
            let processing_language = normalize_processing_language(
                source
                    .metadata
                    .get("processing_language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("zh"),
            );
            let job = registry.run_in_background("course-reindex", move |job| {
                let saved_secrets = secrets.load();
                saved_secrets.apply_to_env();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let mut indexed = 0usize;
                    let mut updated_manifest = manifest.clone();
                    for date in &mut updated_manifest.dates {
                        match fs::read_to_string(&date.source_path) {
                            Ok(content) => {
                                let char_count = content.chars().count();
                                let token_count = estimate_token_count(&content);
                                match build_course_date_index_with_llm(
                                    &date.date,
                                    &date.title,
                                    &processing_language,
                                    &date.source_path,
                                    &content,
                                    char_count,
                                    token_count,
                                )
                                .await
                                {
                                    Ok(index) => {
                                        if let Err(e) = write_index(
                                            std::path::Path::new(&date.index_path),
                                            &index,
                                        ) {
                                            date.status = format!("failed: {}", e);
                                        } else {
                                            date.char_count = char_count;
                                            date.token_count = token_count;
                                            date.status = "ready".to_string();
                                            indexed += 1;
                                        }
                                    }
                                    Err(e) => {
                                        date.status = format!("failed: {}", e);
                                    }
                                }
                            }
                            Err(e) => {
                                date.status = format!("failed: {}", e);
                            }
                        }
                    }
                    updated_manifest.updated_at = SourceRecord::now_iso();
                    let _ = write_manifest(std::path::Path::new(&source.path), &updated_manifest);
                    let total_chars: usize =
                        updated_manifest.dates.iter().map(|d| d.char_count).sum();
                    let total_tokens: usize =
                        updated_manifest.dates.iter().map(|d| d.token_count).sum();
                    let _ = source_store.update(&source_id, |r| {
                        if let Some(meta) = r.metadata.as_object_mut() {
                            meta.insert(
                                "indexed_date_count".to_string(),
                                serde_json::json!(indexed),
                            );
                            meta.insert("char_count".to_string(), serde_json::json!(total_chars));
                            meta.insert("token_count".to_string(), serde_json::json!(total_tokens));
                        }
                        r.status = if indexed == updated_manifest.dates.len() {
                            SourceStatus::Ready
                        } else {
                            SourceStatus::Failed
                        };
                        r.last_error = if indexed == updated_manifest.dates.len() {
                            None
                        } else {
                            Some(format!(
                                "Indexed {}/{} date(s)",
                                indexed,
                                updated_manifest.dates.len()
                            ))
                        };
                        r.job_id = Some(job.job_id.clone());
                    });
                });
            });
            let _ = state.source_store.update(&id, |r| {
                r.status = SourceStatus::Processing;
                r.job_id = Some(job.job_id.clone());
                r.last_error = None;
            });
            Json(serde_json::json!({
                "status": "processing",
                "source_id": id,
                "job_id": job.job_id,
            }))
        }
        SourceKind::TranscriptDay => {
            let content = match fs::read_to_string(&source.path) {
                Ok(content) => content,
                Err(e) => {
                    return Json(serde_json::json!({
                        "status": "failed",
                        "error": format!("Failed to read source artifact: {}", e),
                    }));
                }
            };
            let date = source
                .metadata
                .get("date")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown-date");
            let processing_language = normalize_processing_language(
                source
                    .metadata
                    .get("processing_language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("zh"),
            );
            match build_course_date_index_with_llm(
                date,
                &source.title,
                &processing_language,
                &source.path,
                &content,
                content.chars().count(),
                estimate_token_count(&content),
            )
            .await
            {
                Ok(index) => {
                    let index_path = state
                        .source_store
                        .course_index_dir(&id)
                        .join(format!("{}.json", date));
                    match write_index(&index_path, &index) {
                        Ok(_) => Json(serde_json::json!({
                            "status": "succeeded",
                            "source_id": id,
                            "index_path": index_path.to_string_lossy(),
                        })),
                        Err(e) => Json(serde_json::json!({
                            "status": "failed",
                            "error": format!("Failed to write index: {}", e),
                        })),
                    }
                }
                Err(e) => Json(serde_json::json!({
                    "status": "failed",
                    "error": format!("Failed to build index: {}", e),
                })),
            }
        }
        SourceKind::Note => Json(serde_json::json!({
            "status": "failed",
            "error": "Note sources cannot be reindexed as transcript indexes.",
        })),
    }
}

/// `DELETE /api/sources/{id}` 闁?delete a source record (does not delete
/// artifact files).
async fn api_delete_source(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.source_store.delete(&id) {
        Some(removed) => Json(serde_json::json!({
            "status": "deleted",
            "source": removed,
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Source not found"})),
        )
            .into_response(),
    }
}

/// `POST /api/sources/note` 闁?create a note source from Markdown content.
///
/// Body: `{ name?: string, content: string }`
#[derive(Debug, Deserialize)]
struct CreateNoteBody {
    #[serde(default)]
    name: String,
    content: String,
}

async fn api_create_note_source(
    State(state): State<AppState>,
    Json(body): Json<CreateNoteBody>,
) -> Json<serde_json::Value> {
    if body.content.trim().is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Content is required."
        }));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let title = if body.name.trim().is_empty() {
        format!("Note {}", &id[..8])
    } else {
        body.name.trim().to_string()
    };

    // Save the Markdown artifact.
    let md_path = state
        .source_store
        .artifact_path(&SourceKind::Note, &id, "md");
    if let Err(e) = state.source_store.ensure_dirs() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Failed to create directories: {}", e),
        }));
    }
    // Ensure notes subdir exists.
    if let Some(parent) = md_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&md_path, &body.content) {
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Failed to write note file: {}", e),
        }));
    }

    let line_count = body.content.lines().count();
    let char_count = body.content.chars().count();
    let now = SourceRecord::now_iso();

    let record = SourceRecord {
        id: id.clone(),
        kind: SourceKind::Note,
        title,
        status: SourceStatus::Ready,
        created_at: now.clone(),
        updated_at: now,
        length: Some(format!("{} lines, {} chars", line_count, char_count)),
        path: md_path.to_string_lossy().to_string(),
        metadata: serde_json::json!({
            "line_count": line_count,
            "char_count": char_count,
        }),
        last_error: None,
        job_id: None,
    };

    match state.source_store.insert(record) {
        Ok(inserted) => Json(serde_json::json!({
            "status": "created",
            "source": inserted,
        })),
        Err(e) => Json(serde_json::json!({
            "status": "failed",
            "error": format!("Failed to save source: {}", e),
        })),
    }
}

/// `PUT /api/sources/{id}/note` 闁?update an existing note source.
///
/// Body: `{ name?: string, content: string }`
#[derive(Debug, Deserialize)]
struct UpdateNoteBody {
    #[serde(default)]
    name: String,
    content: String,
}

async fn api_update_note_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateNoteBody>,
) -> Response {
    // Verify the source exists and is a note.
    let existing = match state.source_store.get(&id) {
        Some(s) if s.kind == SourceKind::Note => s,
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Source is not a note"})),
            )
                .into_response();
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Source not found"})),
            )
                .into_response();
        }
    };

    if body.content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Content is required."})),
        )
            .into_response();
    }

    // Overwrite the artifact file.
    let md_path = state
        .source_store
        .artifact_path(&SourceKind::Note, &id, "md");
    if let Some(parent) = md_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&md_path, &body.content) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to write note file: {}", e)})),
        )
            .into_response();
    }

    let line_count = body.content.lines().count();
    let char_count = body.content.chars().count();
    let title = if body.name.trim().is_empty() {
        existing.title
    } else {
        body.name.trim().to_string()
    };

    match state.source_store.update(&id, |r| {
        r.title = title;
        r.length = Some(format!("{} lines, {} chars", line_count, char_count));
        r.status = SourceStatus::Ready;
        r.last_error = None;
        if let Some(meta) = r.metadata.as_object_mut() {
            meta.insert(
                "line_count".to_string(),
                serde_json::Value::Number(serde_json::Number::from(line_count)),
            );
            meta.insert(
                "char_count".to_string(),
                serde_json::Value::Number(serde_json::Number::from(char_count)),
            );
        }
    }) {
        Some(updated) => Json(serde_json::json!({
            "status": "updated",
            "source": updated,
        }))
        .into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to update source"})),
        )
            .into_response(),
    }
}

/// `POST /api/sources/transcript-day` 闁?create a transcript-day source and
/// start background processing.
///
/// Body: `{ course_id, course_name?, date, cookie? }`
#[derive(Debug, Deserialize)]
struct CreateTranscriptDayBody {
    course_id: String,
    #[serde(default)]
    course_name: String,
    date: String,
    #[serde(default = "default_processing_language")]
    processing_language: String,
    /// Deprecated: cookie is no longer accepted from the request body.
    /// Credentials must be saved in Settings.
    #[serde(default)]
    #[allow(dead_code)]
    cookie: String,
}

fn default_processing_language() -> String {
    "zh".to_string()
}

fn normalize_processing_language(value: &str) -> String {
    match value {
        "en" => "en".to_string(),
        "bilingual" => "bilingual".to_string(),
        _ => "zh".to_string(),
    }
}

async fn api_create_transcript_day_source(
    State(state): State<AppState>,
    Json(body): Json<CreateTranscriptDayBody>,
) -> Json<serde_json::Value> {
    if body.course_id.is_empty() || body.date.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "course_id and date are required."
        }));
    }

    let saved_secrets = state.secrets.load();
    let cookie = match saved_secrets.canvas_auth_cookie() {
        Some(c) => c,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "No Canvas video credential saved. Go to Settings and save Canvas credentials first."
            }));
        }
    };

    let id = uuid::Uuid::new_v4().to_string();
    let course_name = if body.course_name.trim().is_empty() {
        body.course_id.clone()
    } else {
        body.course_name.trim().to_string()
    };
    let title = format!("{} - {}", course_name, body.date);
    let processing_language = normalize_processing_language(&body.processing_language);
    let now = SourceRecord::now_iso();

    // Create the source in processing state.
    let record = SourceRecord {
        id: id.clone(),
        kind: SourceKind::TranscriptDay,
        title,
        status: SourceStatus::Processing,
        created_at: now.clone(),
        updated_at: now,
        length: None,
        path: String::new(), // will be updated by job
        metadata: serde_json::json!({
            "course_id": body.course_id,
            "course_name": course_name,
            "date": body.date,
            "processing_language": processing_language,
        }),
        last_error: None,
        job_id: None,
    };

    // Save initial record.
    if let Err(e) = state.source_store.insert(record) {
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Failed to save source record: {}", e),
        }));
    }

    let registry = state.registry.clone();
    let registry_clone = registry.clone();
    let source_store = state.source_store.clone();
    let source_id = id.clone();
    let course_id = body.course_id;
    let date = body.date;
    let cname = course_name;

    let job = registry.run_in_background("transcript-day", move |job| {
        web_log(format!(
            "job {} transcript-day started course_id={} date={}",
            job.job_id, course_id, date
        ));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            // Ensure dirs.
            if let Err(e) = source_store.ensure_dirs() {
                let _ = source_store.update(&source_id, |r| {
                    r.status = SourceStatus::Failed;
                    r.last_error = Some(format!("Failed to create directories: {}", e));
                });
                registry_clone.update(
                    &job.job_id,
                    Some(JobStatus::Failed),
                    None,
                    Some(&format!("Failed to create directories: {}", e)),
                    None,
                    None,
                );
                return;
            }

            let transcripts_subdir = source_store.transcript_work_dir(&course_id, &date);

            // Create client and list videos.
            let mut client =
                crate::canvas_sjtu::CanvasSJTUVideoClient::new(course_id.clone(), cookie.clone());

            let videos = match client.list_videos().await {
                Ok(v) => v,
                Err(e) => {
                    let msg = format!("Failed to list videos: {}", e);
                    let _ = source_store.update(&source_id, |r| {
                        r.status = SourceStatus::Failed;
                        r.last_error = Some(msg.clone());
                    });
                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Failed),
                        None,
                        Some(&msg),
                        None,
                        None,
                    );
                    return;
                }
            };

            // Filter videos by date.
            let date_videos: Vec<_> = videos
                .iter()
                .filter(|v| v.course_begin_time.starts_with(&date))
                .collect();

            if date_videos.is_empty() {
                let msg = format!(
                    "No videos found for course {} on date {}. Total videos in course: {}.",
                    course_id,
                    date,
                    videos.len()
                );
                let _ = source_store.update(&source_id, |r| {
                    r.status = SourceStatus::Failed;
                    r.last_error = Some(msg.clone());
                });
                registry_clone.update(
                    &job.job_id,
                    Some(JobStatus::Failed),
                    None,
                    Some(&msg),
                    None,
                    None,
                );
                return;
            }

            // Sort videos: course_begin_time, then title, then video_id.
            let mut sorted: Vec<_> = date_videos.iter().collect();
            sorted.sort_by(|a, b| {
                a.course_begin_time
                    .cmp(&b.course_begin_time)
                    .then_with(|| a.title.cmp(&b.title))
                    .then_with(|| a.video_id.cmp(&b.video_id))
            });

            // Fetch subtitles for each video and accumulate segments.
            let mut video_count: usize = 0;
            let mut segment_count: usize = 0;
            let mut md_content = String::new();

            md_content.push_str(&format!("# {} - {}\n\n", cname, date));

            for v in &sorted {
                match client.fetch_subtitles(&v.video_id).await {
                    Ok(artifact) => {
                        let ppt_slices = match client.fetch_ppt_slices(&v.video_id).await {
                            Ok(slices) => slices,
                            Err(e) => {
                                web_log(format!(
                                    "job {} transcript-day video {} has no PPT slices: {}",
                                    job.job_id, v.video_id, e
                                ));
                                Vec::new()
                            }
                        };
                        // Build per-video heading.
                        let time_part = if v.course_begin_time.len() >= 16 {
                            &v.course_begin_time[11..16]
                        } else {
                            ""
                        };
                        md_content.push_str(&format!(
                            "## {} {} - {}\n\n",
                            time_part, v.title, v.video_id
                        ));

                        md_content.push_str(&transcript_markdown_for_video(&artifact, &ppt_slices));

                        video_count += 1;
                        segment_count += artifact.segments.len();
                    }
                    Err(e) => {
                        web_log(format!(
                            "job {} transcript-day skipped video {}: {}",
                            job.job_id, v.video_id, e
                        ));
                    }
                }
            }

            if video_count == 0 {
                let msg = "All subtitle fetches failed. No segments collected.".to_string();
                let _ = source_store.update(&source_id, |r| {
                    r.status = SourceStatus::Failed;
                    r.last_error = Some(msg.clone());
                });
                registry_clone.update(
                    &job.job_id,
                    Some(JobStatus::Failed),
                    None,
                    Some(&msg),
                    None,
                    None,
                );
                return;
            }

            // Write the merged markdown artifact.
            let md_path = source_store.artifact_path(&SourceKind::TranscriptDay, &source_id, "md");
            // Ensure parent directory structure.
            if let Some(parent) = md_path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            // Write a copy to the course/date subdir as well.
            let _ = fs::create_dir_all(&transcripts_subdir);
            let day_md_path = transcripts_subdir.join(format!("{}.md", source_id));

            if let Err(e) = fs::write(&md_path, &md_content) {
                let msg = format!("Failed to write markdown artifact: {}", e);
                let _ = source_store.update(&source_id, |r| {
                    r.status = SourceStatus::Failed;
                    r.last_error = Some(msg.clone());
                });
                registry_clone.update(
                    &job.job_id,
                    Some(JobStatus::Failed),
                    None,
                    Some(&msg),
                    None,
                    None,
                );
                return;
            }
            // Also save to the per-course-date directory for convenience.
            let _ = fs::write(&day_md_path, &md_content);

            let char_count = md_content.chars().count();
            let length_str = format!(
                "{} videos, {} segments, {} chars",
                video_count, segment_count, char_count
            );

            let _ = source_store.update(&source_id, |r| {
                r.status = SourceStatus::Ready;
                r.path = md_path.to_string_lossy().to_string();
                r.length = Some(length_str);
                r.metadata = serde_json::json!({
                    "course_id": course_id,
                    "course_name": cname,
                    "date": date,
                    "video_count": video_count,
                    "segment_count": segment_count,
                    "char_count": char_count,
                });
                r.last_error = None;
                r.job_id = Some(job.job_id.clone());
            });

            registry_clone.update(
                &job.job_id,
                Some(JobStatus::Succeeded),
                Some(&format!(
                    "Merged {} video(s), {} segment(s) for {}",
                    video_count, segment_count, date
                )),
                None,
                Some(&md_path.to_string_lossy().to_string()),
                Some(serde_json::json!({
                    "video_count": video_count,
                    "segment_count": segment_count,
                    "source_id": source_id,
                })),
            );
        });
    });

    // Update source with the job ID.
    let _ = state.source_store.update(&id, |r| {
        r.job_id = Some(job.job_id.clone());
    });

    Json(serde_json::json!({
        "status": "processing",
        "source_id": id,
        "job_id": job.job_id,
    }))
}

/// Body: `{ course_id, course_name? }`
#[derive(Debug, Deserialize)]
struct CreateTranscriptCourseBody {
    course_id: String,
    #[serde(default)]
    course_name: String,
    #[serde(default = "default_processing_language")]
    processing_language: String,
}

async fn api_create_transcript_course_source(
    State(state): State<AppState>,
    Json(body): Json<CreateTranscriptCourseBody>,
) -> Json<serde_json::Value> {
    if body.course_id.trim().is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "course_id is required."
        }));
    }

    let saved_secrets = state.secrets.load();
    saved_secrets.apply_to_env();
    let cookie = match saved_secrets.canvas_auth_cookie() {
        Some(c) => c,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "No Canvas video credential saved. Go to Settings and save Canvas credentials first."
            }));
        }
    };
    if !crate::llm::is_available() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "LLM is required to create Course Transcript indexes. Set OPENAI_API_KEY in Settings first."
        }));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let course_id = body.course_id.trim().to_string();
    let course_name = if body.course_name.trim().is_empty() {
        course_id.clone()
    } else {
        body.course_name.trim().to_string()
    };
    let processing_language = normalize_processing_language(&body.processing_language);
    let now = SourceRecord::now_iso();
    let manifest_path =
        state
            .source_store
            .artifact_path(&SourceKind::TranscriptCourse, &id, "json");

    let record = SourceRecord {
        id: id.clone(),
        kind: SourceKind::TranscriptCourse,
        title: format!("{} - Course Transcript", course_name),
        status: SourceStatus::Processing,
        created_at: now.clone(),
        updated_at: now,
        length: None,
        path: manifest_path.to_string_lossy().to_string(),
        metadata: serde_json::json!({
            "course_id": course_id,
            "course_name": course_name,
            "processing_language": processing_language,
            "date_count": 0,
            "video_count": 0,
            "segment_count": 0,
            "char_count": 0,
            "token_count": 0,
            "indexed_date_count": 0,
        }),
        last_error: None,
        job_id: None,
    };

    if let Err(e) = state.source_store.insert(record) {
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Failed to save source record: {}", e),
        }));
    }

    let registry = state.registry.clone();
    let registry_clone = registry.clone();
    let source_store = state.source_store.clone();
    let secrets = state.secrets.clone();
    let source_id = id.clone();
    let cname = course_name.clone();
    let lang = processing_language.clone();

    let job = registry.run_in_background("transcript-course", move |job| {
        web_log(format!(
            "job {} transcript-course started course_id={}",
            job.job_id, course_id
        ));
        let saved_secrets = secrets.load();
        saved_secrets.apply_to_env();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            if let Err(e) = sync_course_source(
                &source_id,
                &course_id,
                &cname,
                &lang,
                &cookie,
                &source_store,
                &registry_clone,
                &job.job_id,
            )
            .await
            {
                let msg = e.to_string();
                let _ = source_store.update(&source_id, |r| {
                    r.status = SourceStatus::Failed;
                    r.last_error = Some(msg.clone());
                });
                registry_clone.update(
                    &job.job_id,
                    Some(JobStatus::Failed),
                    None,
                    Some(&msg),
                    None,
                    None,
                );
            }
        });
    });

    let _ = state.source_store.update(&id, |r| {
        r.job_id = Some(job.job_id.clone());
    });

    Json(serde_json::json!({
        "status": "processing",
        "source_id": id,
        "job_id": job.job_id,
        "message": "Course transcript sync started as background job."
    }))
}

async fn sync_course_source(
    source_id: &str,
    course_id: &str,
    course_name: &str,
    processing_language: &str,
    cookie: &str,
    source_store: &SourceStore,
    registry: &JobRegistry,
    job_id: &str,
) -> Result<()> {
    source_store.ensure_dirs()?;
    let mut client =
        crate::canvas_sjtu::CanvasSJTUVideoClient::new(course_id.to_string(), cookie.to_string());
    let videos = client
        .list_videos()
        .await
        .context("Failed to list videos")?;
    if videos.is_empty() {
        anyhow::bail!("No videos found for course {}", course_id);
    }

    let mut by_date: HashMap<String, Vec<crate::canvas_sjtu::CanvasVideoInfo>> = HashMap::new();
    for video in videos {
        let date = extract_date_from_video(&video);
        if date != "unknown-date" {
            by_date.entry(date).or_default().push(video);
        }
    }
    if by_date.is_empty() {
        anyhow::bail!("No dated videos found for course {}", course_id);
    }

    let mut dates: Vec<String> = by_date.keys().cloned().collect();
    dates.sort();
    let total_planned_videos: usize = by_date.values().map(Vec::len).sum();
    let _ = source_store.update(source_id, |r| {
        if let Some(meta) = r.metadata.as_object_mut() {
            meta.insert("date_count".to_string(), serde_json::json!(dates.len()));
            meta.insert(
                "video_count".to_string(),
                serde_json::json!(total_planned_videos),
            );
            meta.insert("segment_count".to_string(), serde_json::json!(0));
            meta.insert("char_count".to_string(), serde_json::json!(0));
            meta.insert("token_count".to_string(), serde_json::json!(0));
            meta.insert("indexed_date_count".to_string(), serde_json::json!(0));
            meta.insert(
                "processing_language".to_string(),
                serde_json::json!(processing_language),
            );
        }
        r.length = Some(format!(
            "{} dates, {} videos, 0 segments, 0 chars",
            dates.len(),
            total_planned_videos
        ));
    });

    let manifest_path =
        source_store.artifact_path(&SourceKind::TranscriptCourse, source_id, "json");
    let index_dir = source_store.course_index_dir(source_id);
    let mut manifest_dates = Vec::new();
    let mut total_videos = 0usize;
    let mut total_segments = 0usize;
    let mut total_chars = 0usize;
    let mut total_tokens = 0usize;
    let mut indexed_dates = 0usize;

    for date in dates.clone() {
        let mut sorted = by_date.remove(&date).unwrap_or_default();
        sorted.sort_by(|a, b| {
            a.course_begin_time
                .cmp(&b.course_begin_time)
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.video_id.cmp(&b.video_id))
        });

        registry.update(
            job_id,
            Some(JobStatus::Running),
            Some(&format!("Syncing {} ({} video(s))", date, sorted.len())),
            None,
            None,
            None,
        );

        let mut md_content = String::new();
        md_content.push_str(&format!("# {} - {}\n\n", course_name, date));
        let mut video_count = 0usize;
        let mut segment_count = 0usize;

        for video in &sorted {
            match client.fetch_subtitles(&video.video_id).await {
                Ok(artifact) => {
                    let ppt_slices = match client.fetch_ppt_slices(&video.video_id).await {
                        Ok(slices) => slices,
                        Err(e) => {
                            web_log(format!(
                                "job {} transcript-course video {} has no PPT slices: {}",
                                job_id, video.video_id, e
                            ));
                            Vec::new()
                        }
                    };
                    let time_part = if video.course_begin_time.len() >= 16 {
                        &video.course_begin_time[11..16]
                    } else {
                        ""
                    };
                    md_content.push_str(&format!(
                        "## {} {} - {}\n\n",
                        time_part, video.title, video.video_id
                    ));
                    md_content.push_str(&transcript_markdown_for_video(&artifact, &ppt_slices));
                    video_count += 1;
                    segment_count += artifact.segments.len();
                }
                Err(e) => {
                    web_log(format!(
                        "job {} transcript-course skipped video {}: {}",
                        job_id, video.video_id, e
                    ));
                }
            }
        }

        if video_count == 0 {
            continue;
        }

        let day_dir = source_store.transcript_work_dir(course_id, &date);
        fs::create_dir_all(&day_dir)?;
        let day_path = day_dir.join(format!("{}.md", source_id));
        fs::write(&day_path, &md_content)
            .with_context(|| format!("Failed to write {}", day_path.display()))?;

        let char_count = md_content.chars().count();
        let token_count = estimate_token_count(&md_content);
        let index_path = index_dir.join(format!("{}.json", date));
        let mut date_status = "ready".to_string();
        match build_course_date_index_with_llm(
            &date,
            &format!("{} - {}", course_name, date),
            processing_language,
            day_path.to_string_lossy().as_ref(),
            &md_content,
            char_count,
            token_count,
        )
        .await
        {
            Ok(index) => {
                if let Err(e) = write_index(&index_path, &index) {
                    date_status = format!("failed: {}", e);
                    web_log(format!(
                        "job {} transcript-course failed to write index for {}: {}",
                        job_id, date, e
                    ));
                } else {
                    indexed_dates += 1;
                }
            }
            Err(e) => {
                date_status = format!("failed: {}", e);
                web_log(format!(
                    "job {} transcript-course failed to index {}: {}",
                    job_id, date, e
                ));
            }
        }

        total_videos += video_count;
        total_segments += segment_count;
        total_chars += char_count;
        total_tokens += token_count;

        let _ = source_store.update(source_id, |r| {
            r.length = Some(format!(
                "{} dates, {} videos, {} segments, {} chars",
                dates.len(),
                total_planned_videos,
                total_segments,
                total_chars
            ));
            if let Some(meta) = r.metadata.as_object_mut() {
                meta.insert("date_count".to_string(), serde_json::json!(dates.len()));
                meta.insert(
                    "video_count".to_string(),
                    serde_json::json!(total_planned_videos),
                );
                meta.insert(
                    "segment_count".to_string(),
                    serde_json::json!(total_segments),
                );
                meta.insert("char_count".to_string(), serde_json::json!(total_chars));
                meta.insert("token_count".to_string(), serde_json::json!(total_tokens));
                meta.insert(
                    "indexed_date_count".to_string(),
                    serde_json::json!(indexed_dates),
                );
            }
        });

        manifest_dates.push(CourseManifestDate {
            date: date.clone(),
            title: format!("{} - {}", course_name, date),
            source_path: day_path.to_string_lossy().to_string(),
            index_path: index_path.to_string_lossy().to_string(),
            video_count,
            segment_count,
            char_count,
            token_count,
            status: date_status,
        });
    }

    if manifest_dates.is_empty() {
        anyhow::bail!("No course transcripts could be fetched for {}", course_id);
    }
    if indexed_dates == 0 {
        anyhow::bail!("Course transcripts were fetched, but all date index generation failed.");
    }

    let now = SourceRecord::now_iso();
    let manifest = CourseManifest {
        source_id: source_id.to_string(),
        course_id: course_id.to_string(),
        course_name: course_name.to_string(),
        dates: manifest_dates,
        created_at: now.clone(),
        updated_at: now,
    };
    write_manifest(&manifest_path, &manifest)?;

    let length = format!(
        "{} dates, {} videos, {} segments, {} chars",
        manifest.dates.len(),
        total_videos,
        total_segments,
        total_chars
    );
    let all_indexed = indexed_dates == manifest.dates.len();
    let final_error = if all_indexed {
        None
    } else {
        Some(format!(
            "Indexed {}/{} date(s). Open Index Overview for failed dates, then Reindex.",
            indexed_dates,
            manifest.dates.len()
        ))
    };
    let _ = source_store.update(source_id, |r| {
        r.status = if all_indexed {
            SourceStatus::Ready
        } else {
            SourceStatus::Failed
        };
        r.path = manifest_path.to_string_lossy().to_string();
        r.length = Some(length);
        r.metadata = serde_json::json!({
            "course_id": course_id,
            "course_name": course_name,
            "processing_language": processing_language,
            "date_count": manifest.dates.len(),
            "video_count": total_videos,
            "segment_count": total_segments,
            "char_count": total_chars,
            "token_count": total_tokens,
            "indexed_date_count": indexed_dates,
        });
        r.last_error = final_error.clone();
        r.job_id = Some(job_id.to_string());
    });

    registry.update(
        job_id,
        Some(if all_indexed {
            JobStatus::Succeeded
        } else {
            JobStatus::Failed
        }),
        Some(&format!(
            "Synced {} date(s), {} video(s), {} segment(s), indexed {}/{} date(s)",
            manifest.dates.len(),
            total_videos,
            total_segments,
            indexed_dates,
            manifest.dates.len()
        )),
        final_error.as_deref(),
        Some(&manifest_path.to_string_lossy().to_string()),
        Some(serde_json::json!({
            "source_id": source_id,
            "date_count": manifest.dates.len(),
            "video_count": total_videos,
            "segment_count": total_segments,
        })),
    );

    Ok(())
}

async fn build_course_date_index_with_llm(
    date: &str,
    title: &str,
    processing_language: &str,
    source_path: &str,
    markdown: &str,
    char_count: usize,
    token_count: usize,
) -> Result<CourseDateIndex> {
    let timestamp_ranges = extract_timestamp_ranges(markdown);
    let previews = timestamp_ranges
        .iter()
        .take(24)
        .map(|r| {
            format!(
                "{} {} [{:.0}-{:.0}] {}",
                r.video_id, r.label, r.start, r.end, r.text_preview
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let compact = compact_transcript_for_llm(markdown, 18000);
    let language_instruction = match processing_language {
        "en" => "Write summary, keywords, and concepts in English. Keep source technical terms when useful.",
        "bilingual" => "Write summary in Chinese followed by concise English key terms when useful. Include both Chinese and English keywords/concepts when they appear or are helpful.",
        _ => "Write summary, keywords, and concepts in Chinese. Do not drift into English except for standard technical terms, proper nouns, formulas, or code identifiers.",
    };
    let system = format!(
        "You create compact searchable indexes for lecture transcripts. \
                  Output JSON only with fields: summary (string), keywords (array of strings), concepts (array of strings). \
                  The summary should be factual and concise. Keywords and concepts should help retrieve this lecture date later. {}",
        language_instruction
    );
    let user = format!(
        "Date: {}\nTitle: {}\nTimestamp range previews:\n{}\n\nTranscript:\n{}",
        date, title, previews, compact
    );
    let json = crate::llm::chat_json(&system, &user, 0.2, 2048).await?;
    let summary = json
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let keywords = json_array_strings(json.get("keywords"));
    let concepts = json_array_strings(json.get("concepts"));

    if summary.is_empty() && keywords.is_empty() && concepts.is_empty() {
        anyhow::bail!("LLM returned an empty date index for {}", date);
    }

    Ok(CourseDateIndex {
        date: date.to_string(),
        title: title.to_string(),
        summary,
        keywords,
        concepts,
        timestamp_ranges,
        char_count,
        token_count,
        source_path: source_path.to_string(),
        status: "ready".to_string(),
    })
}

fn json_array_strings(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `POST /api/sources/{id}/sync` 闁?re-sync a source.
///
/// For transcript_day sources, re-runs the download/merge using saved
/// metadata and secrets. For note sources, returns a message explaining
/// notes are updated via the upload endpoint.
async fn api_sync_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let source = match state.source_store.get(&id) {
        Some(s) => s,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Source not found"
            }));
        }
    };

    match source.kind {
        SourceKind::TranscriptDay => {
            let course_id = source
                .metadata
                .get("course_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let date = source
                .metadata
                .get("date")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let course_name = source
                .metadata
                .get("course_name")
                .and_then(|v| v.as_str())
                .unwrap_or(&course_id)
                .to_string();
            let processing_language = normalize_processing_language(
                source
                    .metadata
                    .get("processing_language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("zh"),
            );

            if course_id.is_empty() || date.is_empty() {
                return Json(serde_json::json!({
                    "status": "failed",
                    "error": "Source metadata missing course_id or date"
                }));
            }

            let saved_secrets = state.secrets.load();
            let cookie = match saved_secrets.canvas_auth_cookie() {
                Some(c) => c,
                None => {
                    return Json(serde_json::json!({
                        "status": "failed",
                        "error": "No Canvas cookie available. Save one in Settings."
                    }));
                }
            };

            // Re-create using the same ID (the background job will update the
            // existing record).
            let _ = state.source_store.update(&id, |r| {
                r.status = SourceStatus::Processing;
                r.last_error = None;
            });

            let registry = state.registry.clone();
            let registry_clone = registry.clone();
            let source_store = state.source_store.clone();
            let source_id = id.clone();

            let job = registry.run_in_background("transcript-day", move |job| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let mut client = crate::canvas_sjtu::CanvasSJTUVideoClient::new(
                        course_id.clone(),
                        cookie.clone(),
                    );

                    let videos = match client.list_videos().await {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = source_store.update(&source_id, |r| {
                                r.status = SourceStatus::Failed;
                                r.last_error = Some(format!("Failed to list videos: {}", e));
                            });
                            registry_clone.update(
                                &job.job_id,
                                Some(JobStatus::Failed),
                                None,
                                Some(&format!("Failed to list videos: {}", e)),
                                None,
                                None,
                            );
                            return;
                        }
                    };

                    let date_videos: Vec<_> = videos
                        .iter()
                        .filter(|v| v.course_begin_time.starts_with(&date))
                        .collect();

                    if date_videos.is_empty() {
                        let _ = source_store.update(&source_id, |r| {
                            r.status = SourceStatus::Failed;
                            r.last_error = Some(format!(
                                "No videos found for course {} on date {}",
                                course_id, date
                            ));
                        });
                        registry_clone.update(
                            &job.job_id,
                            Some(JobStatus::Failed),
                            None,
                            Some(&format!(
                                "No videos found for course {} on date {}",
                                course_id, date
                            )),
                            None,
                            None,
                        );
                        return;
                    }

                    let mut sorted: Vec<_> = date_videos.iter().collect();
                    sorted.sort_by(|a, b| {
                        a.course_begin_time
                            .cmp(&b.course_begin_time)
                            .then_with(|| a.title.cmp(&b.title))
                            .then_with(|| a.video_id.cmp(&b.video_id))
                    });

                    let mut video_count: usize = 0;
                    let mut segment_count: usize = 0;
                    let mut md_content = String::new();
                    md_content.push_str(&format!("# {} - {}\n\n", course_name, date));

                    for v in &sorted {
                        match client.fetch_subtitles(&v.video_id).await {
                            Ok(artifact) => {
                                let ppt_slices = match client.fetch_ppt_slices(&v.video_id).await {
                                    Ok(slices) => slices,
                                    Err(e) => {
                                        web_log(format!(
                                            "job {} sync video {} has no PPT slices: {}",
                                            job.job_id, v.video_id, e
                                        ));
                                        Vec::new()
                                    }
                                };
                                let time_part = if v.course_begin_time.len() >= 16 {
                                    &v.course_begin_time[11..16]
                                } else {
                                    ""
                                };
                                md_content.push_str(&format!(
                                    "## {} {} - {}\n\n",
                                    time_part, v.title, v.video_id
                                ));
                                md_content.push_str(&transcript_markdown_for_video(
                                    &artifact,
                                    &ppt_slices,
                                ));
                                video_count += 1;
                                segment_count += artifact.segments.len();
                            }
                            Err(e) => {
                                web_log(format!(
                                    "job {} sync skipped video {}: {}",
                                    job.job_id, v.video_id, e
                                ));
                            }
                        }
                    }

                    let md_path =
                        source_store.artifact_path(&SourceKind::TranscriptDay, &source_id, "md");
                    if let Some(parent) = md_path.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    if let Err(e) = fs::write(&md_path, &md_content) {
                        let _ = source_store.update(&source_id, |r| {
                            r.status = SourceStatus::Failed;
                            r.last_error = Some(format!("Failed to write artifact: {}", e));
                        });
                        registry_clone.update(
                            &job.job_id,
                            Some(JobStatus::Failed),
                            None,
                            Some(&format!("Failed to write artifact: {}", e)),
                            None,
                            None,
                        );
                        return;
                    }

                    let char_count = md_content.chars().count();
                    let _ = source_store.update(&source_id, |r| {
                        r.status = SourceStatus::Ready;
                        r.path = md_path.to_string_lossy().to_string();
                        r.length = Some(format!(
                            "{} videos, {} segments, {} chars",
                            video_count, segment_count, char_count
                        ));
                        r.job_id = Some(job.job_id.clone());
                        if let Some(meta) = r.metadata.as_object_mut() {
                            meta.insert("video_count".to_string(), serde_json::json!(video_count));
                            meta.insert(
                                "segment_count".to_string(),
                                serde_json::json!(segment_count),
                            );
                            meta.insert("char_count".to_string(), serde_json::json!(char_count));
                            meta.insert(
                                "processing_language".to_string(),
                                serde_json::json!(processing_language),
                            );
                        }
                    });

                    registry_clone.update(
                        &job.job_id,
                        Some(JobStatus::Succeeded),
                        Some(&format!(
                            "Synced {} video(s), {} segment(s)",
                            video_count, segment_count
                        )),
                        None,
                        None,
                        None,
                    );
                });
            });

            let _ = state.source_store.update(&id, |r| {
                r.job_id = Some(job.job_id.clone());
            });

            Json(serde_json::json!({
                "status": "processing",
                "source_id": id,
                "job_id": job.job_id,
                "message": "Sync started as background job."
            }))
        }
        SourceKind::TranscriptCourse => {
            let course_id = source
                .metadata
                .get("course_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let course_name = source
                .metadata
                .get("course_name")
                .and_then(|v| v.as_str())
                .unwrap_or(&course_id)
                .to_string();

            if course_id.is_empty() {
                return Json(serde_json::json!({
                    "status": "failed",
                    "error": "Source metadata missing course_id"
                }));
            }

            let saved_secrets = state.secrets.load();
            saved_secrets.apply_to_env();
            let cookie = match saved_secrets.canvas_auth_cookie() {
                Some(c) => c,
                None => {
                    return Json(serde_json::json!({
                        "status": "failed",
                        "error": "No Canvas cookie available. Save one in Settings."
                    }));
                }
            };
            if !crate::llm::is_available() {
                return Json(serde_json::json!({
                    "status": "failed",
                    "error": "LLM is required to sync Course Transcript indexes."
                }));
            }

            let _ = state.source_store.update(&id, |r| {
                r.status = SourceStatus::Processing;
                r.last_error = None;
            });

            let registry = state.registry.clone();
            let registry_clone = registry.clone();
            let source_store = state.source_store.clone();
            let secrets = state.secrets.clone();
            let source_id = id.clone();
            let processing_language = normalize_processing_language(
                source
                    .metadata
                    .get("processing_language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("zh"),
            );

            let job = registry.run_in_background("transcript-course", move |job| {
                let saved_secrets = secrets.load();
                saved_secrets.apply_to_env();
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    if let Err(e) = sync_course_source(
                        &source_id,
                        &course_id,
                        &course_name,
                        &processing_language,
                        &cookie,
                        &source_store,
                        &registry_clone,
                        &job.job_id,
                    )
                    .await
                    {
                        let msg = e.to_string();
                        let _ = source_store.update(&source_id, |r| {
                            r.status = SourceStatus::Failed;
                            r.last_error = Some(msg.clone());
                        });
                        registry_clone.update(
                            &job.job_id,
                            Some(JobStatus::Failed),
                            None,
                            Some(&msg),
                            None,
                            None,
                        );
                    }
                });
            });

            let _ = state.source_store.update(&id, |r| {
                r.job_id = Some(job.job_id.clone());
            });

            Json(serde_json::json!({
                "status": "processing",
                "source_id": id,
                "job_id": job.job_id,
                "message": "Course sync started as background job."
            }))
        }
        SourceKind::Note => Json(serde_json::json!({
            "status": "failed",
            "error": "Note sources are updated via upload (PUT /api/sources/{id}/note), not sync."
        })),
    }
}

/// `POST /api/sources/{id}/ask` - ask a natural language question.
///
/// Body: `{ question: string }`
#[derive(Debug, Deserialize)]
struct AskSourceBody {
    question: String,
}

async fn api_ask_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AskSourceBody>,
) -> Json<serde_json::Value> {
    let source = match state.source_store.get(&id) {
        Some(s) => s,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Source not found"
            }));
        }
    };

    if body.question.trim().is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Question is required."
        }));
    }

    if source.kind == SourceKind::TranscriptCourse {
        let saved_secrets = state.secrets.load();
        saved_secrets.apply_to_env();
        let manifest = match read_manifest(&source.path) {
            Ok(m) => m,
            Err(e) => {
                return Json(serde_json::json!({
                    "status": "failed",
                    "error": format!("Failed to read course manifest: {}", e),
                }));
            }
        };
        let indexes = read_indexes(&manifest);
        let hits = bm25_search(&indexes, &body.question, 3);
        if hits.is_empty() {
            return Json(serde_json::json!({
                "status": "succeeded",
                "answer": "No indexed course dates matched the question.",
                "llm_used": false,
                "source_id": id,
                "retrieval": [],
            }));
        }
        let mut context = String::new();
        let mut retrieval = Vec::new();
        for hit in &hits {
            retrieval.push(serde_json::json!({
                "date": hit.index.date,
                "score": hit.score,
                "timestamp_ranges": hit.index.timestamp_ranges.iter().take(5).collect::<Vec<_>>(),
            }));
            if let Ok(text) = fs::read_to_string(&hit.index.source_path) {
                context.push_str(&format!(
                    "\n\n--- Date: {} score {:.3} ---\n{}",
                    hit.index.date,
                    hit.score,
                    compact_transcript_for_llm(&text, 18000)
                ));
            }
        }
        if crate::llm::is_available() {
            let system = "You answer questions about a course transcript. Use only the retrieved lecture dates. \
                          Cite dates, video ids, and timestamps when relevant. Do not invent facts.";
            let user = format!(
                "Course: {}\nQuestion: {}\nRetrieved context:\n{}",
                source.title,
                body.question,
                truncate_for_llm(&context, 50000)
            );
            match crate::llm::chat_text(system, &user, 0.3, 8192).await {
                Ok(answer) => Json(serde_json::json!({
                    "status": "succeeded",
                    "answer": answer,
                    "llm_used": true,
                    "source_id": id,
                    "retrieval": retrieval,
                })),
                Err(e) => Json(serde_json::json!({
                    "status": "succeeded",
                    "answer": format!("LLM call failed after retrieval: {}", e),
                    "llm_used": false,
                    "source_id": id,
                    "retrieval": retrieval,
                })),
            }
        } else {
            Json(serde_json::json!({
                "status": "succeeded",
                "answer": "LLM is not available. Course Source questions require LLM after BM25 retrieval.",
                "llm_used": false,
                "source_id": id,
                "retrieval": retrieval,
            }))
        }
    } else {
        // Read the source artifact.
        let source_text = match fs::read_to_string(&source.path) {
            Ok(t) => t,
            Err(e) => {
                return Json(serde_json::json!({
                    "status": "failed",
                    "error": format!("Failed to read source artifact: {}", e),
                }));
            }
        };

        let saved_secrets = state.secrets.load();
        saved_secrets.apply_to_env();

        let llm_available = crate::llm::is_available();

        if llm_available {
            // Use LLM with truncated source text.
            let context = truncate_for_llm(&source_text, 40000);
            let system = "You answer questions about lecture transcript or note content. \
                          Be concise, cite specific sections and timestamps when relevant, \
                          and do not invent facts. Answer in the same language as the question.";
            let user = format!(
                "Source: {}\nQuestion: {}\nContent:\n{}",
                source.title, body.question, context
            );

            match crate::llm::chat_text(system, &user, 0.3, 6144).await {
                Ok(answer) => Json(serde_json::json!({
                    "status": "succeeded",
                    "answer": answer,
                    "llm_used": true,
                    "source_id": id,
                })),
                Err(e) => {
                    // Fall back to deterministic on LLM error.
                    let fallback = source_deterministic_answer(&body.question, &source_text);
                    Json(serde_json::json!({
                        "status": "succeeded",
                        "answer": fallback,
                        "llm_used": false,
                        "fallback_reason": format!("LLM call failed: {}", e),
                        "source_id": id,
                    }))
                }
            }
        } else {
            let answer = source_deterministic_answer(&body.question, &source_text);
            Json(serde_json::json!({
                "status": "succeeded",
                "answer": answer,
                "llm_used": false,
                "source_id": id,
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming source ask API (SSE)
// ---------------------------------------------------------------------------

/// `GET /api/sources/{id}/ask-stream?question=...`
///
/// Streams the answer as Server-Sent Events. Uses LLM streaming when
/// available, or deterministic fallback chunks otherwise.
#[derive(Debug, Deserialize)]
struct AskStreamQuery {
    question: String,
}

async fn api_source_ask_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AskStreamQuery>,
) -> Response {
    if query.question.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "question query parameter is required."})),
        )
            .into_response();
    }

    let source = match state.source_store.get(&id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Source not found"})),
            )
                .into_response();
        }
    };

    if source.kind == SourceKind::TranscriptCourse {
        let saved_secrets = state.secrets.load();
        saved_secrets.apply_to_env();
        let manifest = match read_manifest(&source.path) {
            Ok(m) => m,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Failed to read course manifest: {}", e)})),
                )
                    .into_response();
            }
        };
        let indexes = read_indexes(&manifest);
        let hits = bm25_search(&indexes, &query.question, 3);
        let hit_rows: Vec<(String, f64, String)> = hits
            .iter()
            .map(|h| (h.index.date.clone(), h.score, h.index.source_path.clone()))
            .collect();
        let retrieval = hit_rows
            .iter()
            .map(|(date, score, _)| {
                serde_json::json!({
                    "date": date,
                    "score": score,
                })
            })
            .collect::<Vec<_>>();
        let source_id = id.clone();
        let source_title = source.title.clone();
        let question = query.question.clone();
        let stream = async_stream::stream! {
            yield Ok::<Event, Infallible>(Event::default()
                .event("meta")
                .data(serde_json::json!({
                    "source_id": source_id,
                    "llm_used": crate::llm::is_available(),
                    "retrieval": retrieval,
                }).to_string()));

            if hit_rows.is_empty() {
                yield Ok::<Event, Infallible>(Event::default()
                    .event("chunk")
                    .data("No indexed course dates matched the question."));
                yield Ok::<Event, Infallible>(Event::default().event("done").data("complete"));
                return;
            }

            let mut context = String::new();
            for (date, score, source_path) in &hit_rows {
                if let Ok(text) = fs::read_to_string(source_path) {
                    context.push_str(&format!(
                        "\n\n--- Date: {} score {:.3} ---\n{}",
                        date,
                        score,
                        compact_transcript_for_llm(&text, 18000)
                    ));
                }
            }

            if crate::llm::is_available() {
                let system = "You answer questions about a course transcript. Use only the retrieved lecture dates. Cite dates, video ids, and timestamps when relevant. Do not invent facts.";
                let user = format!(
                    "Source title: {}\nQuestion: {}\n\nRetrieved content:\n{}",
                    source_title,
                    question,
                    truncate_for_llm(&context, 50000)
                );
                match crate::llm::chat_text(system, &user, 0.3, 8192).await {
                    Ok(answer) => {
                        yield Ok::<Event, Infallible>(Event::default().event("chunk").data(answer));
                    }
                    Err(e) => {
                        yield Ok::<Event, Infallible>(Event::default().event("error").data(e.to_string()));
                    }
                }
            } else {
                yield Ok::<Event, Infallible>(Event::default()
                    .event("chunk")
                    .data("LLM is not available. Course Source questions require LLM after BM25 retrieval."));
            }
            yield Ok::<Event, Infallible>(Event::default().event("done").data("complete"));
        };

        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }

    let source_text = match fs::read_to_string(&source.path) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Failed to read source: {}", e)})),
            )
                .into_response();
        }
    };

    let saved_secrets = state.secrets.load();
    saved_secrets.apply_to_env();

    let llm_available = crate::llm::is_available();

    if llm_available {
        // Use compact transcript format for LLM context.
        let context = compact_transcript_for_llm(&source_text, 80000);
        let system = "You answer questions about lecture transcript or note content. \
                      Be concise, cite specific sections and timestamps when relevant, \
                      and do not invent facts. Answer in the same language as the question. \
                      Format your answer in Markdown when helpful.";
        let user = format!(
            "Source title: {}\nQuestion: {}\n\nContent:\n{}",
            source.title, query.question, context
        );

        match llm::chat_text_stream(system, &user, 0.3, 8192).await {
            Ok(mut rx) => {
                let stream = async_stream::stream! {
                    // Send initial metadata event.
                    yield Ok::<Event, Infallible>(Event::default()
                        .event("meta")
                        .data(serde_json::json!({
                            "source_id": id,
                            "llm_used": true,
                        }).to_string()));

                    while let Some(chunk) = rx.recv().await {
                        match chunk {
                            Ok(text) => {
                                if text.is_empty() {
                                    // Empty string signals completion.
                                    yield Ok::<Event, Infallible>(Event::default()
                                        .event("done")
                                        .data("complete"));
                                    return;
                                }
                                yield Ok::<Event, Infallible>(Event::default()
                                    .event("chunk")
                                    .data(text));
                            }
                            Err(e) => {
                                yield Ok::<Event, Infallible>(Event::default()
                                    .event("error")
                                    .data(e.to_string()));
                                return;
                            }
                        }
                    }
                    // Channel closed without done marker.
                    yield Ok::<Event, Infallible>(Event::default()
                        .event("done")
                        .data("complete"));
                };

                Sse::new(stream)
                    .keep_alive(KeepAlive::default())
                    .into_response()
            }
            Err(e) => {
                // Streaming LLM failed; stream deterministic fallback.
                let fallback = source_deterministic_answer(&query.question, &source_text);
                stream_deterministic_fallback(id, fallback, &e.to_string())
            }
        }
    } else {
        // No LLM; stream deterministic fallback.
        let fallback = source_deterministic_answer(&query.question, &source_text);
        stream_deterministic_fallback(id, fallback, "LLM not available")
    }
}

/// Stream a deterministic fallback answer as SSE chunks.
fn stream_deterministic_fallback(source_id: String, answer: String, reason: &str) -> Response {
    let reason = reason.to_string();
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(Event::default()
            .event("meta")
            .data(serde_json::json!({
                "source_id": source_id,
                "llm_used": false,
                "fallback_reason": reason,
            }).to_string()));

        // Send the answer in chunks (paragraph-based).
        for paragraph in answer.split("\n\n") {
            if !paragraph.is_empty() {
                yield Ok::<Event, Infallible>(Event::default()
                    .event("chunk")
                    .data(format!("{}\n\n", paragraph)));
            }
        }

        yield Ok::<Event, Infallible>(Event::default()
            .event("done")
            .data("complete"));
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

// ---------------------------------------------------------------------------
// Process management APIs
// ---------------------------------------------------------------------------

/// `GET /api/processes` -- list all processes, newest first.
async fn api_get_processes(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut processes = state.process_store.load_all();
    processes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(serde_json::json!({
        "processes": processes,
    }))
}

/// `GET /api/processes/{id}` -- get a single process record.
async fn api_get_process(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.process_store.get(&id) {
        Some(process) => Json(serde_json::to_value(&process).unwrap_or_default()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Process not found"})),
        )
            .into_response(),
    }
}

/// `POST /api/processes` -- create a new process and start background jobs.
///
/// Body: `{ title?: string, source_ids: string[], outputs: [{ kind: "note_patch" | "reference_digest" | "cheating_sheet", max_pages?: 2 }] }`
#[derive(Debug, Deserialize)]
struct CreateProcessBody {
    #[serde(default)]
    title: String,
    source_ids: Vec<String>,
    outputs: Vec<CreateProcessOutputBody>,
}

#[derive(Debug, Deserialize)]
struct CreateProcessOutputBody {
    kind: String,
    #[serde(default)]
    max_pages: Option<usize>,
}

fn parse_process_output_kind(kind: &str) -> Option<ProcessOutputKind> {
    match kind {
        "note_patch" => Some(ProcessOutputKind::NotePatch),
        "reference_digest" => Some(ProcessOutputKind::ReferenceDigest),
        "cheating_sheet" => Some(ProcessOutputKind::CheatingSheet),
        _ => None,
    }
}

fn process_output_title(kind: &ProcessOutputKind) -> &'static str {
    match kind {
        ProcessOutputKind::NotePatch => "Note Patch",
        ProcessOutputKind::ReferenceDigest => "Reference Digest",
        ProcessOutputKind::CheatingSheet => "Cheating Sheet",
    }
}

fn process_output_path_for(
    process_store: &ProcessStore,
    process_id: &str,
    output_id: &str,
    kind: &ProcessOutputKind,
) -> std::path::PathBuf {
    match kind {
        ProcessOutputKind::NotePatch => process_store.output_path(process_id, output_id),
        ProcessOutputKind::ReferenceDigest => process_store.output_path(process_id, output_id),
        ProcessOutputKind::CheatingSheet => process_store
            .process_dir(process_id)
            .join(format!("{}.pdf", output_id)),
    }
}

fn cheating_sheet_markdown_path(
    process_store: &ProcessStore,
    process_id: &str,
    output_id: &str,
) -> std::path::PathBuf {
    process_store
        .process_dir(process_id)
        .join(format!("{}.cheatsheet.md", output_id))
}

fn expand_output_kinds(
    requested: &[CreateProcessOutputBody],
) -> std::result::Result<Vec<(ProcessOutputKind, usize)>, String> {
    let mut has_note_patch = false;
    let mut has_reference_digest = false;
    let mut has_cheating_sheet = false;
    let mut cheating_sheet_pages = 2usize;

    for out in requested {
        match parse_process_output_kind(&out.kind) {
            Some(ProcessOutputKind::NotePatch) => has_note_patch = true,
            Some(ProcessOutputKind::ReferenceDigest) => has_reference_digest = true,
            Some(ProcessOutputKind::CheatingSheet) => {
                has_cheating_sheet = true;
                cheating_sheet_pages = out.max_pages.unwrap_or(2).clamp(1, 20);
            }
            None => {
                return Err(format!(
                    "Unsupported output kind: {}. Supported kinds: note_patch, reference_digest, cheating_sheet.",
                    out.kind
                ));
            }
        }
    }

    let mut kinds = Vec::new();
    if has_note_patch || has_reference_digest || has_cheating_sheet {
        // Note Patch is independent; only add if explicitly requested.
        if has_note_patch {
            kinds.push((ProcessOutputKind::NotePatch, 2));
        }
    }
    if has_cheating_sheet {
        // Cheating Sheet depends on Reference Digest, not Note Patch.
        if !has_reference_digest {
            kinds.push((ProcessOutputKind::ReferenceDigest, 0));
        }
        kinds.push((ProcessOutputKind::CheatingSheet, cheating_sheet_pages));
    } else if has_reference_digest {
        kinds.push((ProcessOutputKind::ReferenceDigest, 0));
    }
    Ok(kinds)
}

fn update_process_terminal_status(process_store: &ProcessStore, process_id: &str, job_id: &str) {
    if let Some(proc) = process_store.get(process_id) {
        let all_ready = proc
            .outputs
            .iter()
            .all(|o| o.status == ProcessRecordStatus::Ready);
        let any_failed = proc
            .outputs
            .iter()
            .any(|o| o.status == ProcessRecordStatus::Failed);
        let all_done = proc.outputs.iter().all(|o| {
            o.status == ProcessRecordStatus::Ready || o.status == ProcessRecordStatus::Failed
        });
        if all_done {
            let _ = process_store.update(process_id, |r| {
                if any_failed && !all_ready {
                    r.status = ProcessRecordStatus::Failed;
                    let errs: Vec<String> = r
                        .outputs
                        .iter()
                        .filter_map(|o| o.last_error.clone())
                        .collect();
                    r.last_error = Some(errs.join("; "));
                } else {
                    r.status = ProcessRecordStatus::Ready;
                    r.last_error = None;
                }
                r.job_id = Some(job_id.to_string());
            });
        }
    }
}

async fn api_create_process(
    State(state): State<AppState>,
    Json(body): Json<CreateProcessBody>,
) -> Json<serde_json::Value> {
    // -- Validation ----------------------------------------------------------
    if body.source_ids.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "At least one source is required."
        }));
    }

    if body.outputs.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "At least one output method is required."
        }));
    }

    let expanded_outputs = match expand_output_kinds(&body.outputs) {
        Ok(outputs) => outputs,
        Err(error) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": error,
            }));
        }
    };

    // Validate source IDs exist and at most one Note source.
    let sources = state.source_store.load_all();
    let mut note_count = 0u32;
    let mut valid_source_ids: Vec<String> = Vec::new();
    for sid in &body.source_ids {
        match sources.iter().find(|s| s.id == *sid) {
            Some(s) => {
                if s.kind == SourceKind::Note {
                    note_count += 1;
                }
                valid_source_ids.push(sid.clone());
            }
            None => {
                return Json(serde_json::json!({
                    "status": "failed",
                    "error": format!("Source not found: {}", sid),
                }));
            }
        }
    }

    if note_count > 1 {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "At most one Note source is allowed. Note Patch supports many Transcript sources and at most one Note source."
        }));
    }

    // -- Create records -------------------------------------------------------
    let process_id = uuid::Uuid::new_v4().to_string();
    let now = ProcessRecord::now_iso();
    let title = if body.title.trim().is_empty() {
        format!("Process {}", &process_id[..8])
    } else {
        body.title.trim().to_string()
    };

    // Create output records.
    let mut outputs: Vec<ProcessOutput> = Vec::new();
    for (kind, max_pages) in &expanded_outputs {
        let output_id = uuid::Uuid::new_v4().to_string();
        let output_path =
            process_output_path_for(&state.process_store, &process_id, &output_id, kind);
        let diff_path = if *kind == ProcessOutputKind::NotePatch {
            Some(state.process_store.diff_path(&process_id, &output_id))
        } else {
            None
        };

        // Ensure artifact dirs exist.
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let markdown_path =
            cheating_sheet_markdown_path(&state.process_store, &process_id, &output_id);
        let mut metadata = serde_json::json!({
            "progress_current": 0,
            "progress_total": 1,
            "progress_label": "queued",
        });
        if *kind == ProcessOutputKind::ReferenceDigest {
            metadata = serde_json::json!({
                "progress_current": 0,
                "progress_total": 2,
                "progress_label": "queued",
            });
        }
        if *kind == ProcessOutputKind::CheatingSheet {
            metadata = serde_json::json!({
                "progress_current": 0,
                "progress_total": 4,
                "progress_label": "queued",
                "max_pages": max_pages,
                "markdown_path": markdown_path.to_string_lossy().to_string(),
            });
        }

        outputs.push(ProcessOutput {
            id: output_id,
            kind: kind.clone(),
            status: ProcessRecordStatus::Processing,
            title: process_output_title(kind).to_string(),
            path: output_path.to_string_lossy().to_string(),
            diff_path: diff_path.map(|p| p.to_string_lossy().to_string()),
            base_source_id: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_error: None,
            metadata,
        });
    }

    let record = ProcessRecord {
        id: process_id.clone(),
        title,
        status: ProcessRecordStatus::Processing,
        created_at: now.clone(),
        updated_at: now.clone(),
        source_ids: body.source_ids.clone(),
        outputs: outputs.clone(),
        last_error: None,
        job_id: None,
    };

    if let Err(e) = state.process_store.insert(record) {
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Failed to save process: {}", e),
        }));
    }

    // -- Launch background job for requested outputs --------------------------
    let registry = state.registry.clone();
    let process_store = state.process_store.clone();
    let source_store = state.source_store.clone();
    let secrets = state.secrets.clone();
    let pid = process_id.clone();
    let src_ids = body.source_ids.clone();

    let job = registry.run_in_background("process", move |job| {
        web_log(format!(
            "job {} process started process_id={}",
            job.job_id, pid
        ));
        let saved_secrets = secrets.load();
        saved_secrets.apply_to_env();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            run_process_outputs(
                &pid,
                &src_ids,
                &outputs,
                &process_store,
                &source_store,
                &job.job_id,
            )
            .await;
        });

        update_process_terminal_status(&process_store, &pid, &job.job_id);

        web_log(format!(
            "job {} process finished process_id={}",
            job.job_id, pid
        ));
    });

    // Update process with job ID.
    let _ = state.process_store.update(&process_id, |r| {
        r.job_id = Some(job.job_id.clone());
    });

    Json(serde_json::json!({
        "status": "processing",
        "process_id": process_id,
        "job_id": job.job_id,
    }))
}

// ---------------------------------------------------------------------------
// Process / Output retry
// ---------------------------------------------------------------------------

/// Query params for `POST /api/processes/{id}/retry`.
#[derive(Debug, Deserialize)]
struct RetryProcessQuery {
    /// When `true`, reset all outputs and rerun from scratch.
    #[serde(default)]
    force: bool,
}

/// `POST /api/processes/{id}/retry` — retry failed outputs or force full rerun.
async fn api_retry_process(
    State(state): State<AppState>,
    Path(process_id): Path<String>,
    Query(query): Query<RetryProcessQuery>,
) -> Json<serde_json::Value> {
    let saved_secrets = state.secrets.load();
    saved_secrets.apply_to_env();

    let proc = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Process not found"
            }));
        }
    };

    let outputs_to_retry: Vec<ProcessOutput> = if query.force {
        // Force: reset ALL outputs.
        let mut reset_outputs = Vec::new();
        let _ = state.process_store.update(&process_id, |r| {
            r.status = ProcessRecordStatus::Processing;
            r.last_error = None;
            for o in &mut r.outputs {
                o.status = ProcessRecordStatus::Processing;
                o.last_error = None;
                let meta = &mut o.metadata;
                meta["progress_current"] = serde_json::json!(0);
                meta["progress_label"] = serde_json::json!("queued");
                reset_outputs.push(o.clone());
            }
            r.updated_at = ProcessRecord::now_iso();
        });
        reset_outputs
    } else {
        // Smart retry: only failed outputs.
        let mut failed_outputs = Vec::new();
        let _ = state.process_store.update(&process_id, |r| {
            r.status = ProcessRecordStatus::Processing;
            r.last_error = None;
            for o in &mut r.outputs {
                if o.status == ProcessRecordStatus::Failed {
                    o.status = ProcessRecordStatus::Processing;
                    o.last_error = None;
                    let meta = &mut o.metadata;
                    meta["progress_current"] = serde_json::json!(0);
                    meta["progress_label"] = serde_json::json!("retrying");
                    failed_outputs.push(o.clone());
                }
            }
            r.updated_at = ProcessRecord::now_iso();
        });
        if failed_outputs.is_empty() {
            return Json(serde_json::json!({
                "status": "succeeded",
                "message": "No failed outputs to retry."
            }));
        }
        failed_outputs
    };

    // Launch background job.
    let registry = state.registry.clone();
    let process_store = state.process_store.clone();
    let source_store = state.source_store.clone();
    let pid = process_id.clone();
    let src_ids = proc.source_ids.clone();

    let retried_ids: Vec<String> = outputs_to_retry.iter().map(|o| o.id.clone()).collect();
    let job = registry.run_in_background("process-retry", move |job| {
        web_log(format!(
            "job {} process-retry started process_id={} force={}",
            job.job_id, pid, query.force
        ));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            run_process_outputs(
                &pid,
                &src_ids,
                &outputs_to_retry,
                &process_store,
                &source_store,
                &job.job_id,
            )
            .await;
        });

        update_process_terminal_status(&process_store, &pid, &job.job_id);

        web_log(format!(
            "job {} process-retry finished process_id={}",
            job.job_id, pid
        ));
    });

    Json(serde_json::json!({
        "status": "processing",
        "process_id": process_id,
        "job_id": job.job_id,
        "retried_outputs": retried_ids,
    }))
}

/// `POST /api/processes/{id}/outputs/{output_id}/retry` — retry a single output.
async fn api_retry_process_output(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    let saved_secrets = state.secrets.load();
    saved_secrets.apply_to_env();

    let proc = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Process not found"
            }));
        }
    };

    // Find and reset the specific output.
    let target_output = {
        let output_idx = proc.outputs.iter().position(|o| o.id == output_id);
        match output_idx {
            Some(idx) => {
                if proc.outputs[idx].status != ProcessRecordStatus::Failed
                    && proc.outputs[idx].status != ProcessRecordStatus::Ready
                {
                    return Json(serde_json::json!({
                        "status": "failed",
                        "error": format!(
                            "Output is not in a retryable state (status: {})",
                            proc.outputs[idx].status.to_string()
                        )
                    }));
                }
                let mut o = proc.outputs[idx].clone();
                o.status = ProcessRecordStatus::Processing;
                o.last_error = None;
                let meta = &mut o.metadata;
                meta["progress_current"] = serde_json::json!(0);
                meta["progress_label"] = serde_json::json!("retrying");
                o.updated_at = ProcessRecord::now_iso();
                o
            }
            None => {
                return Json(serde_json::json!({
                    "status": "failed",
                    "error": "Output not found"
                }));
            }
        }
    };

    // Persist the reset.
    let output_kind = target_output.kind.clone();
    let _ = state.process_store.update(&process_id, |r| {
        r.status = ProcessRecordStatus::Processing;
        r.last_error = None;
        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output_id) {
            o.status = ProcessRecordStatus::Processing;
            o.last_error = None;
            let meta = &mut o.metadata;
            meta["progress_current"] = serde_json::json!(0);
            meta["progress_label"] = serde_json::json!("retrying");
            o.updated_at = ProcessRecord::now_iso();
        }
        r.updated_at = ProcessRecord::now_iso();
    });

    // Launch background job for this single output.
    let registry = state.registry.clone();
    let process_store = state.process_store.clone();
    let source_store = state.source_store.clone();
    let pid = process_id.clone();
    let src_ids = proc.source_ids.clone();
    let out_id = output_id.clone();

    let job = registry.run_in_background("output-retry", move |job| {
        web_log(format!(
            "job {} output-retry started process_id={} output_id={}",
            job.job_id, pid, out_id
        ));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            match output_kind {
                ProcessOutputKind::NotePatch => {
                    run_note_patch(
                        &pid,
                        &src_ids,
                        &[target_output],
                        &process_store,
                        &source_store,
                        &job.job_id,
                    )
                    .await;
                }
                ProcessOutputKind::ReferenceDigest => {
                    run_reference_digest_outputs(
                        &pid,
                        &src_ids,
                        &[target_output],
                        &process_store,
                        &source_store,
                        &job.job_id,
                    )
                    .await;
                }
                ProcessOutputKind::CheatingSheet => {
                    run_cheating_sheet_outputs(&pid, &[target_output], &process_store, &job.job_id)
                        .await;
                }
            }
        });

        update_process_terminal_status(&process_store, &pid, &job.job_id);

        web_log(format!(
            "job {} output-retry finished process_id={} output_id={}",
            job.job_id, pid, out_id
        ));
    });

    Json(serde_json::json!({
        "status": "processing",
        "process_id": process_id,
        "output_id": output_id,
        "job_id": job.job_id,
    }))
}

// ---------------------------------------------------------------------------
// Streaming process output generation (SSE)
// ---------------------------------------------------------------------------

/// `GET /api/processes/{id}/outputs/{output_id}/stream`
///
/// Runs LLM generation for the given output and streams the result as
/// Server-Sent Events.  Uses the same prompt-building logic as the
/// background-job path but sends each token chunk to the client in
/// real-time via SSE.  The final result is saved to the process store.
async fn api_stream_process_output(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Response {
    let saved_secrets = state.secrets.load();
    saved_secrets.apply_to_env();

    if !crate::llm::is_available() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "LLM is not available"})),
        )
            .into_response();
    }

    let proc = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Process not found"})),
            )
                .into_response();
        }
    };

    let output = match proc.outputs.iter().find(|o| o.id == output_id) {
        Some(o) => o.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Output not found"})),
            )
                .into_response();
        }
    };

    let process_store = state.process_store.clone();
    let source_store = state.source_store.clone();
    let pid = process_id.clone();
    let oid = output_id.clone();
    let kind = output.kind.clone();

    let sources = source_store.load_all();
    // Owned data extracted before the SSE stream captures them.
    let note_content: Option<String> = sources
        .iter()
        .find(|s| proc.source_ids.contains(&s.id) && s.kind == SourceKind::Note)
        .and_then(|n| fs::read_to_string(&n.path).ok())
        .map(|c| c.chars().take(50000).collect::<String>());
    let transcript_paths: Vec<(String, String)> = sources
        .iter()
        .filter(|s| proc.source_ids.contains(&s.id) && s.kind == SourceKind::TranscriptDay)
        .map(|s| (s.title.clone(), s.path.clone()))
        .collect();

    let build_transcript_context = move |limit: usize| -> String {
        let mut ctx = String::new();
        let per_src = if transcript_paths.is_empty() {
            0
        } else {
            limit / transcript_paths.len()
        };
        for (title, path) in &transcript_paths {
            if ctx.chars().count() >= limit {
                break;
            }
            if let Ok(content) = fs::read_to_string(path) {
                let compact = crate::llm::compact_transcript_for_llm(&content, per_src);
                ctx.push_str(&format!("\n\n--- Source: {} ---\n", title));
                ctx.push_str(&compact);
            }
        }
        ctx
    };

    let stream = async_stream::stream! {
        let _ = process_store.update(&pid, |r| {
            r.status = ProcessRecordStatus::Processing;
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == oid) {
                o.status = ProcessRecordStatus::Processing;
                o.last_error = None;
                o.metadata["progress_label"] = serde_json::json!("streaming");
            }
        });

        yield Ok::<Event, Infallible>(
            Event::default().event("meta").data(serde_json::json!({
                "output_id": oid,
                "kind": kind.to_string(),
            }).to_string())
        );

        // Build prompt and call LLM with streaming based on output kind.
        let llm_result: Result<(String, mpsc::Receiver<Result<String>>)> = async {
            match &kind {
                ProcessOutputKind::NotePatch => {
                    let context = build_transcript_context(80000);
                    let (sys, usr) = build_note_patch_prompts(
                        note_content.is_some(),
                        &context,
                        80000,
                    );
                    let rx = crate::llm::chat_text_stream(&sys, &usr, 0.3, 32768).await?;
                    Ok(("note_patch".to_string(), rx))
                }
                ProcessOutputKind::ReferenceDigest => {
                    let context = build_transcript_context(100000);
                    let sys = build_ref_digest_system_prompt();
                    let usr = build_ref_digest_user_prompt(&note_content, &context, None);
                    let rx = crate::llm::chat_text_stream(&sys, &usr, 0.25, 65536).await?;
                    Ok(("ref_digest".to_string(), rx))
                }
                ProcessOutputKind::CheatingSheet => {
                    let ref_digest = proc.outputs.iter()
                        .find(|o| o.kind == ProcessOutputKind::ReferenceDigest && o.status == ProcessRecordStatus::Ready);
                    let digest_md = match ref_digest {
                        Some(o) => fs::read_to_string(&o.path).unwrap_or_default(),
                        None => return Err(anyhow::anyhow!("Cheat Sheet requires a completed Reference Digest")),
                    };
                    let max_pages = output
                        .metadata
                        .get("max_pages")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as usize)
                        .unwrap_or(2)
                        .clamp(1, 20);
                    let (sys, usr) = build_cheat_sheet_prompts(&digest_md, max_pages);
                    let rx = crate::llm::chat_text_stream(&sys, &usr, 0.2, 65536).await?;
                    Ok(("cheat_sheet".to_string(), rx))
                }
            }
        }.await;

        let (_label, mut rx) = match llm_result {
            Ok(v) => v,
            Err(e) => {
                let _ = process_store.update(&pid, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == oid) {
                        o.status = ProcessRecordStatus::Failed;
                        o.last_error = Some(e.to_string());
                    }
                });
                yield Ok::<Event, Infallible>(Event::default().event("error").data(e.to_string()));
                return;
            }
        };

        // Stream LLM chunks to the client.
        let mut accumulated = String::new();
        loop {
            match rx.recv().await {
                Some(Ok(text)) if !text.is_empty() => {
                    accumulated.push_str(&text);
                    yield Ok::<Event, Infallible>(Event::default().event("chunk").data(text));
                }
                Some(Ok(_)) => break, // empty = done
                Some(Err(e)) => {
                    let _ = process_store.update(&pid, |r| {
                        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == oid) {
                            o.status = ProcessRecordStatus::Failed;
                            o.last_error = Some(e.to_string());
                        }
                    });
                    yield Ok::<Event, Infallible>(Event::default().event("error").data(e.to_string()));
                    return;
                }
                None => break,
            }
        }

        // Save and finalise.
        let cleaned = strip_markdown_fences(&accumulated);
        let output_path = process_store.output_path(&pid, &oid);
        if let Some(parent) = output_path.parent() { let _ = fs::create_dir_all(parent); }
        let _ = fs::write(&output_path, &cleaned);
        let _ = process_store.update(&pid, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == oid) {
                o.status = ProcessRecordStatus::Ready;
                o.last_error = None;
                o.metadata["progress_current"] = serde_json::json!(1);
                o.metadata["progress_total"] = serde_json::json!(1);
                o.metadata["progress_label"] = serde_json::json!("complete");
                o.updated_at = ProcessRecord::now_iso();
            }
        });
        yield Ok::<Event, Infallible>(
            Event::default().event("done").data(serde_json::json!({
                "status": "succeeded",
                "path": output_path.to_string_lossy(),
                "generated_chars": cleaned.chars().count(),
            }).to_string())
        );
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn run_process_outputs(
    process_id: &str,
    source_ids: &[String],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
    // 1) Note Patch outputs if requested.
    let note_patch_outputs: Vec<ProcessOutput> = outputs
        .iter()
        .filter(|o| o.kind == ProcessOutputKind::NotePatch)
        .cloned()
        .collect();
    if !note_patch_outputs.is_empty() {
        run_note_patch(
            process_id,
            source_ids,
            &note_patch_outputs,
            process_store,
            source_store,
            job_id,
        )
        .await;
    }

    // 2) Reference Digest outputs.
    let reference_digest_outputs: Vec<ProcessOutput> = outputs
        .iter()
        .filter(|o| o.kind == ProcessOutputKind::ReferenceDigest)
        .cloned()
        .collect();
    if !reference_digest_outputs.is_empty() {
        run_reference_digest_outputs(
            process_id,
            source_ids,
            &reference_digest_outputs,
            process_store,
            source_store,
            job_id,
        )
        .await;
    }

    // 3) Cheating Sheet outputs.
    let cheating_sheet_outputs: Vec<ProcessOutput> = outputs
        .iter()
        .filter(|o| o.kind == ProcessOutputKind::CheatingSheet)
        .cloned()
        .collect();
    if !cheating_sheet_outputs.is_empty() {
        run_cheating_sheet_outputs(process_id, &cheating_sheet_outputs, process_store, job_id)
            .await;
    }
}

/// Build shared Note Patch prompts (used by both streaming and non-streaming paths).
fn build_note_patch_prompts(
    has_base_note: bool,
    context: &str,
    context_limit: usize,
) -> (String, String) {
    let system_prompt = if has_base_note {
        "You are an expert note editor. You are given an existing Markdown note and supplementary source materials (lecture transcripts, etc.). \
         Your job is to produce a COMPLETE updated Markdown note that incorporates key information from the sources into the existing note. \
         Make only the SMALLEST necessary modifications -- preserve the existing structure, wording, and formatting wherever possible. \
         Add missing details, correct factual errors, and supplement with important information from the sources. \
         Return ONLY the complete updated Markdown note, with no surrounding explanation, no code fences, and no commentary. \
         The output MUST be valid Markdown and MUST be the full note, not just the changes."
    } else {
        "You are an expert note writer. You are given lecture transcript materials and you need to generate a well-structured Markdown note. \
         Organize the content logically: group related topics, use headings (## for sections, ### for sub-sections), \
         include bullet points for key facts, and preserve timestamps in [MM:SS] format when citing specific moments. \
         Be comprehensive but well-organized. Do not invent facts not present in the sources. \
         Return ONLY the generated Markdown note, with no surrounding explanation, no code fences, and no commentary. \
         The output MUST be valid Markdown."
    };

    let user_prompt = if has_base_note {
        format!(
            "Update the existing note with information from the following source materials:\n\n{}\n\nRemember: return ONLY the complete updated Markdown note, no explanation or code fences.",
            truncate_for_llm(context, context_limit)
        )
    } else {
        format!(
            "Generate a note from the following source materials:\n\n{}\n\nRemember: return ONLY the Markdown note, no explanation or code fences.",
            truncate_for_llm(context, context_limit)
        )
    };

    (system_prompt.to_string(), user_prompt)
}

/// Run the Note Patch generation for a set of outputs.
async fn run_note_patch(
    process_id: &str,
    source_ids: &[String],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
    let sources = source_store.load_all();

    // Separate note and transcript sources.
    let note_source: Option<&SourceRecord> = sources
        .iter()
        .find(|s| source_ids.contains(&s.id) && s.kind == SourceKind::Note);
    let transcript_sources: Vec<&SourceRecord> = sources
        .iter()
        .filter(|s| source_ids.contains(&s.id) && s.kind == SourceKind::TranscriptDay)
        .collect();
    let course_sources: Vec<&SourceRecord> = sources
        .iter()
        .filter(|s| source_ids.contains(&s.id) && s.kind == SourceKind::TranscriptCourse)
        .collect();

    if transcript_sources.is_empty() && course_sources.is_empty() && note_source.is_none() {
        // This shouldn't happen due to validation, but guard.
        for output in outputs {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some("No valid sources found for Note Patch.".to_string());
                }
            });
        }
        return;
    }

    if !course_sources.is_empty() {
        run_course_note_patch(
            process_id,
            note_source,
            &course_sources,
            outputs,
            process_store,
            job_id,
        )
        .await;
        return;
    }

    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        0,
        3,
        "preparing transcript context",
    );

    // Check LLM availability.
    if !crate::llm::is_available() {
        let err_msg =
            "LLM is not available. Set OPENAI_API_KEY in Settings to enable Note Patch generation."
                .to_string();
        for output in outputs {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.clone());
                }
            });
        }
        return;
    }

    // Build context from transcript sources (compact, bounded).
    let mut context = String::new();
    let context_limit: usize = 80000;

    for src in &transcript_sources {
        if context.chars().count() >= context_limit {
            break;
        }
        match fs::read_to_string(&src.path) {
            Ok(content) => {
                let compact = crate::llm::compact_transcript_for_llm(
                    &content,
                    context_limit.saturating_sub(context.chars().count()),
                );
                context.push_str(&format!(
                    "\n\n--- Source: {} (type: {}) ---\n",
                    src.title,
                    src.kind.to_string()
                ));
                context.push_str(&compact);
            }
            Err(e) => {
                web_log(format!(
                    "job {} note-patch: failed to read source {}: {}",
                    job_id, src.path, e
                ));
            }
        }
    }

    // Read base note if present.
    let base_note_content: Option<String> = if let Some(note) = note_source {
        match fs::read_to_string(&note.path) {
            Ok(content) => {
                context.push_str(&format!(
                    "\n\n--- Base Note: {} (type: note) ---\n{}",
                    note.title, content
                ));
                Some(content)
            }
            Err(e) => {
                web_log(format!(
                    "job {} note-patch: failed to read note source {}: {}",
                    job_id, note.path, e
                ));
                None
            }
        }
    } else {
        None
    };

    // Build LLM prompt.
    let (system_prompt, user_prompt) =
        build_note_patch_prompts(base_note_content.is_some(), &context, context_limit);
    update_note_patch_progress(process_store, process_id, outputs, 1, 3, "calling LLM");

    // Call LLM.
    let markdown_output =
        match crate::llm::chat_text(&system_prompt, &user_prompt, 0.3, 32768).await {
            Ok(text) => {
                // Strip code fences if the LLM wrapped it anyway.
                let cleaned = text.trim();
                let cleaned = if cleaned.starts_with("```markdown") {
                    cleaned
                        .strip_prefix("```markdown")
                        .and_then(|s| s.strip_suffix("```"))
                        .map(|s| s.trim())
                        .unwrap_or(cleaned)
                } else if cleaned.starts_with("```") {
                    cleaned
                        .strip_prefix("```")
                        .and_then(|s| s.strip_suffix("```"))
                        .map(|s| s.trim())
                        .unwrap_or(cleaned)
                } else {
                    cleaned
                };
                cleaned.to_string()
            }
            Err(e) => {
                let err_msg = format!("LLM call failed: {}", e);
                for output in outputs {
                    let _ = process_store.update(process_id, |r| {
                        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                            o.status = ProcessRecordStatus::Failed;
                            o.last_error = Some(err_msg.clone());
                        }
                    });
                }
                return;
            }
        };

    // Save output and diff for each output record.
    for output in outputs {
        // Write the markdown output.
        let output_path = process_store.output_path(process_id, &output.id);
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&output_path, &markdown_output) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(format!("Failed to write output: {}", e));
                }
            });
            continue;
        }

        // Generate diff if base note exists.
        let diff_path = process_store.diff_path(process_id, &output.id);
        if let Some(ref base) = base_note_content {
            let unified = crate::diff::unified_diff(base, &markdown_output, 3);
            if let Err(e) = fs::write(&diff_path, &unified) {
                web_log(format!(
                    "job {} note-patch: failed to write diff: {}",
                    job_id, e
                ));
            }
        } else {
            // No base note, write empty diff.
            let _ = fs::write(&diff_path, "");
        }

        // Update output record to ready.
        let note_source_id = note_source.map(|n| n.id.clone());
        let _ = process_store.update(process_id, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                o.status = ProcessRecordStatus::Ready;
                o.base_source_id = note_source_id.clone();
                o.last_error = None;
                o.metadata = serde_json::json!({
                    "progress_current": 3,
                    "progress_total": 3,
                    "progress_label": "complete",
                });
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Reference Digest generation
// ---------------------------------------------------------------------------

/// Run Reference Digest generation for a set of outputs.
///
/// Loads current process Sources (optional Note, TranscriptDay list,
/// TranscriptCourse list).  Does **not** inspect process.outputs for Note Patch
/// and does **not** read Note Patch files.
async fn run_reference_digest_outputs(
    process_id: &str,
    source_ids: &[String],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
    let sources = source_store.load_all();

    let note_source: Option<&SourceRecord> = sources
        .iter()
        .find(|s| source_ids.contains(&s.id) && s.kind == SourceKind::Note);
    let transcript_sources: Vec<&SourceRecord> = sources
        .iter()
        .filter(|s| source_ids.contains(&s.id) && s.kind == SourceKind::TranscriptDay)
        .collect();
    let course_sources: Vec<&SourceRecord> = sources
        .iter()
        .filter(|s| source_ids.contains(&s.id) && s.kind == SourceKind::TranscriptCourse)
        .collect();

    if transcript_sources.is_empty() && course_sources.is_empty() && note_source.is_none() {
        for output in outputs {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some("No valid sources found for Reference Digest.".to_string());
                }
            });
        }
        return;
    }

    if !crate::llm::is_available() {
        let err_msg = "LLM is not available. Set OPENAI_API_KEY in Settings to enable Reference Digest generation.".to_string();
        for output in outputs {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.clone());
                }
            });
        }
        return;
    }

    // Dispatch to course path when course sources are present.
    if !course_sources.is_empty() {
        run_course_reference_digest(
            process_id,
            note_source,
            &course_sources,
            outputs,
            process_store,
            job_id,
        )
        .await;
        return;
    }

    // -------------------------------------------------------------------
    // Ordinary Note + TranscriptDay sources path
    // -------------------------------------------------------------------
    update_ref_digest_progress(
        process_store,
        process_id,
        outputs,
        0,
        2,
        "building transcript context",
    );

    // Read Note source as structure/priority/style reference (up to 50000 chars).
    let note_content: Option<String> = if let Some(note) = note_source {
        match fs::read_to_string(&note.path) {
            Ok(content) => {
                let truncated: String = content.chars().take(50000).collect();
                Some(truncated)
            }
            Err(e) => {
                web_log(format!(
                    "job {} reference-digest: failed to read note source {}: {}",
                    job_id, note.path, e
                ));
                None
            }
        }
    } else {
        None
    };

    // Build compact transcript context (target 100000 chars), sharing budget
    // across sources so later transcript files are not dropped.
    let mut context = String::new();
    let context_limit: usize = 100000;
    let per_source_budget = if transcript_sources.is_empty() {
        0
    } else {
        context_limit / transcript_sources.len()
    };

    for src in &transcript_sources {
        match fs::read_to_string(&src.path) {
            Ok(content) => {
                let compact = crate::llm::compact_transcript_for_llm(&content, per_source_budget);
                context.push_str(&format!(
                    "\n\n--- Source: {} (type: {}) ---\n",
                    src.title,
                    src.kind.to_string()
                ));
                context.push_str(&compact);
            }
            Err(e) => {
                web_log(format!(
                    "job {} reference-digest: failed to read source {}: {}",
                    job_id, src.path, e
                ));
            }
        }
    }

    let combined_chars = context.chars().count();
    let fits_in_one_call = combined_chars <= context_limit && transcript_sources.len() <= 2;

    let digest_markdown = if fits_in_one_call {
        update_ref_digest_progress(
            process_store,
            process_id,
            outputs,
            1,
            2,
            "generating digest",
        );
        match generate_ref_digest_single(&note_content, &context).await {
            Ok(md) => md,
            Err(e) => {
                let err_msg = format!("Reference Digest generation failed: {}", e);
                for output in outputs {
                    let _ = process_store.update(process_id, |r| {
                        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                            o.status = ProcessRecordStatus::Failed;
                            o.last_error = Some(err_msg.clone());
                        }
                    });
                }
                return;
            }
        }
    } else {
        // Generate per-source section digests, then merge.
        update_ref_digest_progress(
            process_store,
            process_id,
            outputs,
            1,
            2,
            "generating section digests",
        );
        let mut section_digests: Vec<String> = Vec::new();
        for src in &transcript_sources {
            let src_content = match fs::read_to_string(&src.path) {
                Ok(c) => c,
                Err(e) => {
                    web_log(format!(
                        "job {} reference-digest: failed to read source {}: {}",
                        job_id, src.path, e
                    ));
                    continue;
                }
            };
            let compact = crate::llm::compact_transcript_for_llm(&src_content, 90000);
            let src_context = format!(
                "Source: {} (type: {})\n{}",
                src.title,
                src.kind.to_string(),
                compact
            );
            match generate_ref_digest_section(&note_content, &src_context).await {
                Ok(md) => section_digests.push(md),
                Err(e) => {
                    web_log(format!(
                        "job {} reference-digest: section digest for {} failed: {}",
                        job_id, src.title, e
                    ));
                }
            }
        }
        if section_digests.is_empty() {
            let err_msg = "All section digest generations failed.".to_string();
            for output in outputs {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Failed;
                        o.last_error = Some(err_msg.clone());
                    }
                });
            }
            return;
        }
        match merge_ref_digest_sections(&section_digests).await {
            Ok(md) => md,
            Err(e) => {
                let err_msg = format!("Reference Digest merge failed: {}", e);
                for output in outputs {
                    let _ = process_store.update(process_id, |r| {
                        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                            o.status = ProcessRecordStatus::Failed;
                            o.last_error = Some(err_msg.clone());
                        }
                    });
                }
                return;
            }
        }
    };

    // Write output for each record.
    let source_counts = serde_json::json!({
        "note": note_source.is_some(),
        "transcript_day": transcript_sources.len(),
        "transcript_course": course_sources.len(),
    });
    let generated_chars = digest_markdown.chars().count();

    for output in outputs {
        let output_path = process_store.output_path(process_id, &output.id);
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&output_path, &digest_markdown) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(format!("Failed to write output: {}", e));
                }
            });
            continue;
        }

        let _ = process_store.update(process_id, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                o.status = ProcessRecordStatus::Ready;
                o.base_source_id = note_source.map(|n| n.id.clone());
                o.last_error = None;
                o.metadata = serde_json::json!({
                    "progress_current": 2,
                    "progress_total": 2,
                    "progress_label": "complete",
                    "source_note_used": note_source.is_some(),
                    "source_counts": source_counts,
                    "generated_chars": generated_chars,
                });
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }
}

/// Generate a single-pass Reference Digest from combined transcript context.
async fn generate_ref_digest_single(
    note_content: &Option<String>,
    transcript_context: &str,
) -> Result<String> {
    let system = build_ref_digest_system_prompt();
    let user = build_ref_digest_user_prompt(note_content, transcript_context, None);
    let text = crate::llm::chat_text(&system, &user, 0.25, 81920).await?;
    Ok(strip_markdown_fences(&text))
}

/// Generate a Reference Digest section for a single transcript source.
async fn generate_ref_digest_section(
    note_content: &Option<String>,
    src_context: &str,
) -> Result<String> {
    let system = build_ref_digest_system_prompt();
    let user = build_ref_digest_user_prompt(note_content, src_context, Some("section"));
    let text = crate::llm::chat_text(&system, &user, 0.25, 32768).await?;
    Ok(strip_markdown_fences(&text))
}

/// Merge independently-generated Reference Digest sections into one cohesive digest.
async fn merge_ref_digest_sections(sections: &[String]) -> Result<String> {
    let combined = sections
        .iter()
        .enumerate()
        .map(|(i, s)| format!("<!-- digest section {} -->\n{}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n\n");

    let truncated: String = combined.chars().take(160000).collect();

    let system = "\
You are an editor merging Reference Digest sections into one cohesive, detailed Markdown document.\n\
Normalize: heading hierarchy (## for top sections, ### for subsections), terminology consistency, \
formula formatting, remove duplicate content, and add smooth transitions between sections.\n\
Do not add new facts - only reorganize and normalize the provided content.\n\
Default language: Chinese, preserving English technical terms, formulas, symbols, and code identifiers.";

    let user = format!(
        "Merge and normalize these Reference Digest sections into one complete Markdown document:\n\n{}\n\nReturn only Markdown.",
        truncated
    );

    let text = crate::llm::chat_text(system, &user, 0.2, 81920).await?;
    Ok(strip_markdown_fences(&text))
}

/// Build the system prompt for Reference Digest generation.
fn build_ref_digest_system_prompt() -> String {
    "\
You are a lecture digest writer. Create a detailed, structured Markdown Reference Digest from \
lecture transcripts. The digest will be used downstream to compress into an exam cheat sheet, \
so be comprehensive and precise.\n\n\
Goal: detailed, structured Markdown covering definitions, formulas, conditions, algorithms, \
steps, comparisons, pitfalls, exam judgement rules, and timestamp evidence.\n\n\
Default language: Chinese, preserving English technical terms, identifiers, symbols, and formulas.\n\
Use ## for top-level sections and ### for subsections.\n\
Do not invent facts beyond the supplied sources.\n\
Include [MM:SS] timestamps when referencing specific moments.\n\
Return ONLY Markdown, no code fences, no explanations.".to_string()
}

/// Build the user prompt for Reference Digest generation.
fn build_ref_digest_user_prompt(
    note_content: &Option<String>,
    transcript_context: &str,
    _mode: Option<&str>,
) -> String {
    let mut user = String::new();

    if let Some(ref note) = note_content {
        user.push_str(&format!(
            "Reference Note (structure / priority / style reference only; not a length constraint):\n\n{}\n\n---\n\n",
            truncate_for_llm(note, 50000)
        ));
    }

    user.push_str(&format!(
        "Transcript sources:\n\n{}\n\n---\n\n\
        Create a comprehensive Reference Digest Markdown document. \
        Prioritize: definitions, formulas, conditions, algorithms, steps, comparisons, \
        pitfalls, exam judgement rules, and timestamp evidence. \
        Be detailed and precise - this will be further compressed into an exam cheat sheet. \
        Return ONLY Markdown.",
        transcript_context
    ));

    user
}

/// Course-based Reference Digest generation using staged outline -> section -> merge pipeline.
async fn run_course_reference_digest(
    process_id: &str,
    note_source: Option<&SourceRecord>,
    course_sources: &[&SourceRecord],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    job_id: &str,
) {
    // Helper: mark every output as failed.
    fn fail_rd_outputs(
        process_store: &ProcessStore,
        process_id: &str,
        outputs: &[ProcessOutput],
        err_msg: &str,
    ) {
        for output in outputs {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.to_string());
                    o.updated_at = ProcessRecord::now_iso();
                }
            });
        }
    }

    // Step 1 - Load course index.
    update_ref_digest_progress(
        process_store,
        process_id,
        outputs,
        0,
        4,
        "loading course index",
    );

    let mut indexes: Vec<CourseDateIndex> = Vec::new();
    for source in course_sources {
        match read_manifest(&source.path) {
            Ok(manifest) => indexes.extend(read_indexes(&manifest)),
            Err(e) => {
                web_log(format!(
                    "job {} reference-digest: failed to read course manifest {}: {}",
                    job_id, source.path, e
                ));
            }
        }
    }
    if indexes.is_empty() {
        fail_rd_outputs(
            process_store,
            process_id,
            outputs,
            "No ready Course Transcript indexes found.",
        );
        return;
    }

    if !crate::llm::is_available() {
        fail_rd_outputs(
            process_store,
            process_id,
            outputs,
            "LLM is not available. Set OPENAI_API_KEY in Settings to enable Reference Digest generation.",
        );
        return;
    }

    // Step 2 - Plan outline from index metadata + optional note structure.
    update_ref_digest_progress(
        process_store,
        process_id,
        outputs,
        1,
        4,
        "planning digest outline",
    );

    // Build optional note heading inventory for richer outline context.
    let note_heading_inventory = if let Some(note) = note_source {
        match fs::read_to_string(&note.path) {
            Ok(content) => {
                let headings = crate::notes::extract_headings(&content);
                if headings.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nExisting note heading inventory:\n{}",
                        headings.join("\n- ")
                    )
                }
            }
            Err(_) => String::new(),
        }
    } else {
        String::new()
    };

    let (planned_sections, outline_fallback_used, outline_fallback_reason) =
        match generate_course_ref_digest_outline(&indexes, &note_heading_inventory).await {
            Ok(s) => (s, false, None),
            Err(e) => {
                let llm_error = e.to_string();
                match generate_fallback_outline(&indexes) {
                    Ok(sections) => {
                        web_log(format!(
                            "job {} reference-digest: using fallback outline ({} sections) after LLM outline failed: {}",
                            job_id, sections.len(), llm_error
                        ));
                        (sections, true, Some(llm_error))
                    }
                    Err(fallback_err) => {
                        let err_msg = format!(
                            "Failed to generate Reference Digest outline: {}. Fallback also failed: {}",
                            llm_error, fallback_err
                        );
                        fail_rd_outputs(process_store, process_id, outputs, &err_msg);
                        return;
                    }
                }
            }
        };

    let total_sections = planned_sections.len();
    let total_steps = total_sections + 3; // loading + outline + N sections + merge

    // Step 3 - Generate each section independently.
    let style_guide = "Write detailed Reference Digest sections. Default language: Chinese, preserving English technical terms, formulas, symbols, and code identifiers. Include definitions, formulas, conditions, algorithms, steps, comparisons, pitfalls, exam judgement rules, and timestamp evidence.";

    let mut generated_sections: Vec<String> = Vec::new();
    let mut retrieval_traces: Vec<RetrievalTrace> = Vec::new();
    let mut success_count: usize = 0;

    for (i, section) in planned_sections.iter().enumerate() {
        let label = format!("generating digest section {}/{}", i + 1, total_sections);
        update_ref_digest_progress(
            process_store,
            process_id,
            outputs,
            2 + i,
            total_steps,
            &label,
        );

        let (context, trace) = retrieve_course_context_for_section_ref_digest(section, &indexes);
        retrieval_traces.push(trace);

        match generate_course_ref_digest_section(section, &context, style_guide).await {
            Ok(markdown) => {
                generated_sections.push(markdown);
                success_count += 1;
            }
            Err(e) => {
                web_log(format!(
                    "job {} reference-digest: section '{}' generation failed: {}",
                    job_id, section.title, e
                ));
                let err_msg = format!(
                    "Section '{}' generation failed: {}. {}/{} sections succeeded.",
                    section.title, e, success_count, total_sections
                );
                fail_rd_outputs(process_store, process_id, outputs, &err_msg);
                return;
            }
        }
    }

    // Step 4 - Merge sections into one cohesive Reference Digest.
    update_ref_digest_progress(
        process_store,
        process_id,
        outputs,
        1 + total_sections,
        total_steps,
        "merging digest sections",
    );

    let digest_markdown =
        match merge_course_ref_digest_sections(&generated_sections, style_guide).await {
            Ok(md) => md,
            Err(e) => {
                let err_msg = format!("Failed to merge Reference Digest sections: {}", e);
                fail_rd_outputs(process_store, process_id, outputs, &err_msg);
                return;
            }
        };

    // Step 5 - Write outputs.
    update_ref_digest_progress(
        process_store,
        process_id,
        outputs,
        total_steps,
        total_steps,
        "complete",
    );

    let generated_chars = digest_markdown.chars().count();
    let source_counts = serde_json::json!({
        "note": note_source.is_some(),
        "transcript_day": 0,
        "transcript_course": course_sources.len(),
    });

    for output in outputs {
        let output_path = process_store.output_path(process_id, &output.id);
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&output_path, &digest_markdown) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(format!("Failed to write output: {}", e));
                }
            });
            continue;
        }

        let retrieval_path = process_store
            .process_dir(process_id)
            .join(format!("{}.retrieval.json", output.id));
        let _ = fs::write(
            &retrieval_path,
            serde_json::to_string_pretty(&retrieval_traces).unwrap_or_default(),
        );

        let _ = process_store.update(process_id, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                o.status = ProcessRecordStatus::Ready;
                o.base_source_id = note_source.map(|n| n.id.clone());
                o.last_error = None;
                let mut meta = serde_json::json!({
                    "progress_current": total_steps,
                    "progress_total": total_steps,
                    "progress_label": "complete",
                    "source_note_used": note_source.is_some(),
                    "source_counts": source_counts,
                    "generated_chars": generated_chars,
                    "section_count": total_sections,
                    "outline_fallback_used": outline_fallback_used,
                });
                if let Some(ref reason) = outline_fallback_reason {
                    meta["outline_fallback_reason"] = serde_json::json!(reason);
                }
                o.metadata = meta;
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }
}

/// Generate a course Reference Digest outline from index metadata + optional note structure.
async fn generate_course_ref_digest_outline(
    indexes: &[CourseDateIndex],
    note_heading_inventory: &str,
) -> Result<Vec<PlannedSection>> {
    let context = build_outline_context(indexes);

    let system = "\
You are a curriculum designer. Given course lecture index data, plan a structured Reference Digest outline.\n\
\n\
Output **only** a JSON object with this exact shape:\n\
{\"sections\":[{\"title\":\"\",\"purpose\":\"\",\"date_hints\":[],\"video_hints\":[],\"query_terms\":[],\"must_include\":[]}]}\n\
\n\
Rules:\n\
- Produce 4-24 main sections. Choose the count based on the course span and topic diversity.\n\
- title: concise section heading (no body prose, no Markdown).\n\
- purpose: one short sentence describing what this section should cover in the Reference Digest.\n\
- date_hints: date substrings that help select relevant lecture days (max 5 items).\n\
- video_hints: video ID substrings from the range previews (max 5 items).\n\
- query_terms: search keywords for retrieval (max 5 items).\n\
- must_include: key concepts / formulas / definitions that must appear (max 5 items).\n\
\n\
All arrays max 5 items. No body prose, no Markdown, no explanations outside the JSON.\n\
Organize by topic, not by date. Each section should cover a coherent topic that may span multiple lecture days.";

    let user = format!(
        "Course lecture index data:\n{}\n{}Plan a Reference Digest outline with sections organized by topic. Return only the JSON object described above.",
        truncate_for_llm(&context, 32000),
        if note_heading_inventory.is_empty() {
            String::new()
        } else {
            format!("\nNote heading inventory for reference:\n{}\n\n", note_heading_inventory)
        }
    );

    let messages = vec![
        crate::llm::ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        },
        crate::llm::ChatMessage {
            role: "user".into(),
            content: user.clone(),
        },
    ];

    // First attempt: max_tokens = 49152.
    let (text, finish_reason) =
        crate::llm::chat_completion_with_metadata(&messages, 0.2, 49152, Some("json_object"))
            .await
            .context("Reference Digest outline LLM call failed")?;

    match parse_outline_sections(&text, finish_reason.as_deref()) {
        Ok(sections) if (4..=24).contains(&sections.len()) => return Ok(sections),
        Ok(sections) => {
            let diagnostic = format!(
                "Outline had {} section(s), expected 4-24. response_len={}",
                sections.len(),
                text.chars().count()
            );
            web_log(format!("Reference Digest outline retry: {}", diagnostic));
        }
        Err(diagnostic) => {
            web_log(format!(
                "Reference Digest outline parse failure: {}",
                diagnostic
            ));
        }
    }

    // Retry: max_tokens = 57344.
    let retry_user = {
        let char_count = text.chars().count();
        let head: String = text.chars().take(300).collect();
        let tail: String = text
            .chars()
            .rev()
            .take(300)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!(
            "The previous outline attempt produced invalid or unusable JSON.\n\
             Diagnostic: response_len={} finish_reason={:?}\n\
             Head snippet: {}\n\
             Tail snippet: {}\n\n\
             Original course index context:\n{}\n{}\n\
             Plan a Reference Digest outline with 4-24 sections organized by topic. Return only the JSON object.",
            char_count, finish_reason, head, tail,
            truncate_for_llm(&context, 28000),
            if note_heading_inventory.is_empty() {
                String::new()
            } else {
                format!("\nNote heading inventory:\n{}\n", note_heading_inventory)
            }
        )
    };

    let retry_messages = vec![
        crate::llm::ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        },
        crate::llm::ChatMessage {
            role: "user".into(),
            content: retry_user,
        },
    ];

    let (retry_text, retry_finish_reason) =
        crate::llm::chat_completion_with_metadata(&retry_messages, 0.2, 57344, Some("json_object"))
            .await
            .context("Reference Digest outline retry LLM call failed")?;

    match parse_outline_sections(&retry_text, retry_finish_reason.as_deref()) {
        Ok(sections) if (4..=24).contains(&sections.len()) => Ok(sections),
        Ok(sections) => {
            anyhow::bail!(
                "Reference Digest outline retry produced {} section(s), expected 4-24",
                sections.len()
            );
        }
        Err(diagnostic) => {
            anyhow::bail!(
                "Reference Digest outline retry parse failed: {}",
                diagnostic
            );
        }
    }
}

/// Retrieve compact transcript context for a planned Reference Digest section.
///
/// Uses a 32000 char per-section context cap (wider than Note Patch).
fn retrieve_course_context_for_section_ref_digest(
    section: &PlannedSection,
    indexes: &[CourseDateIndex],
) -> (String, RetrievalTrace) {
    let max_indexes: usize = 4;
    let context_budget: usize = 32000;
    let mut trace_matches = Vec::new();
    let mut selected: Vec<(&CourseDateIndex, f64)> = Vec::new();

    // 1) Date-hint match.
    if !section.date_hints.is_empty() {
        for index in indexes {
            if selected.len() >= max_indexes {
                break;
            }
            let date_matches = section.date_hints.iter().any(|hint| {
                index.date.contains(hint.as_str()) || hint.contains(index.date.as_str())
            });
            if date_matches {
                selected.push((index, 1.0));
            }
        }
    }

    // 2) Fallback to BM25.
    if selected.is_empty() {
        let query = format!(
            "{} {} {} {}",
            section.title,
            section.purpose,
            section.query_terms.join(" "),
            section.must_include.join(" ")
        );
        let hits = bm25_search(indexes, &query, max_indexes);
        for hit in hits {
            selected.push((hit.index, hit.score));
        }
    }

    let mut context = String::new();
    for (index, score) in &selected {
        let ranges = index
            .timestamp_ranges
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<TimestampRange>>();
        trace_matches.push(RetrievalMatch {
            date: index.date.clone(),
            score: *score,
            timestamp_ranges: ranges.clone(),
        });

        let remaining = context_budget.saturating_sub(context.chars().count());
        if remaining < 500 {
            break;
        }
        let source_text = fs::read_to_string(&index.source_path).unwrap_or_default();
        let transcript_budget = remaining.min(12000);
        context.push_str(&format!(
            "\n\n--- Date: {} | {} ---\nSummary: {}\nKeywords: {}\nConcepts: {}\nRanges:\n{}\nTranscript excerpt:\n{}",
            index.date,
            index.title,
            index.summary,
            index.keywords.join(", "),
            index.concepts.join(", "),
            ranges
                .iter()
                .map(|r| format!(
                    "{} [{:.0}-{:.0}] {}",
                    r.video_id, r.start, r.end, r.text_preview
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            compact_transcript_for_llm(&source_text, transcript_budget)
        ));
    }

    let trace = RetrievalTrace {
        section: section.title.clone(),
        matches: trace_matches,
        skipped_reason: if selected.is_empty() {
            Some("no relevant indexes found for section".to_string())
        } else {
            None
        },
    };

    (context, trace)
}

/// Generate Markdown for a single planned Reference Digest section.
async fn generate_course_ref_digest_section(
    section: &PlannedSection,
    context: &str,
    style_guide: &str,
) -> Result<String> {
    let system = format!(
        "You are a Reference Digest writer. Write a detailed Markdown section.\n\
        Section title: {}\nPurpose: {}\n\
        Use ## {} for the section heading and ### for subsections.\n\
        Write detailed Reference Digest content - include definitions, formulas, conditions, \
        algorithms, steps, comparisons, pitfalls, exam judgement rules, and timestamp evidence.\n\
        Default language: Chinese, preserving English technical terms, formulas, symbols, and code identifiers.\n\
        Must include these concepts: {}\n\
        Do not invent facts beyond the provided context.\n\
        {}",
        section.title,
        section.purpose,
        section.title,
        section.must_include.join(", "),
        style_guide
    );

    let user = format!(
        "Write the Reference Digest Markdown section. Context:\n{}",
        truncate_for_llm(context, 32000)
    );

    let text = crate::llm::chat_text(&system, &user, 0.25, 24576).await?;
    Ok(strip_markdown_fences(&text))
}

/// Merge independently-generated Reference Digest sections into one cohesive digest.
async fn merge_course_ref_digest_sections(
    sections: &[String],
    style_guide: &str,
) -> Result<String> {
    let combined = sections
        .iter()
        .enumerate()
        .map(|(i, s)| format!("<!-- section {} -->\n{}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n\n");

    let truncated: String = combined.chars().take(160000).collect();

    let system = format!(
        "You are an editor normalizing Reference Digest sections into one cohesive Markdown document.\n\
        Normalize: title hierarchy (## for top sections, ### for subsections), terminology consistency, \
        formula formatting, duplicate content removal, and smooth transitions between sections.\n\
        Do not add new facts - only reorganize and normalize the provided content.\n\
        Default language: Chinese, preserving English technical terms, formulas, symbols, and code identifiers.\n\
        {}",
        style_guide
    );

    let user = format!(
        "Merge and normalize these Reference Digest sections into one complete Markdown document:\n\n{}\n\nReturn only Markdown.",
        truncated
    );

    let text = crate::llm::chat_text(&system, &user, 0.2, 81920).await?;
    Ok(strip_markdown_fences(&text))
}

/// Update progress for Reference Digest outputs.
fn update_ref_digest_progress(
    process_store: &ProcessStore,
    process_id: &str,
    outputs: &[ProcessOutput],
    current: usize,
    total: usize,
    label: &str,
) {
    let output_ids: Vec<String> = outputs.iter().map(|o| o.id.clone()).collect();
    let current = current.min(total);
    let total = total.max(1);
    let label = label.to_string();
    let _ = process_store.update(process_id, |r| {
        for output in &mut r.outputs {
            if output_ids.contains(&output.id) {
                output.metadata = serde_json::json!({
                    "progress_current": current,
                    "progress_total": total,
                    "progress_label": label,
                });
                output.updated_at = ProcessRecord::now_iso();
            }
        }
    });
}

async fn run_course_note_patch(
    process_id: &str,
    note_source: Option<&SourceRecord>,
    course_sources: &[&SourceRecord],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    job_id: &str,
) {
    if note_source.is_none() {
        run_course_note_generation(process_id, course_sources, outputs, process_store, job_id)
            .await;
        return;
    }

    let note = match note_source {
        Some(note) => note,
        None => unreachable!("handled by course note generation path above"),
    };

    let base_note = match fs::read_to_string(&note.path) {
        Ok(content) => content,
        Err(e) => {
            let err_msg = format!("Failed to read note source: {}", e);
            for output in outputs {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Failed;
                        o.last_error = Some(err_msg.clone());
                    }
                });
            }
            return;
        }
    };

    let mut indexes = Vec::new();
    for source in course_sources {
        match read_manifest(&source.path) {
            Ok(manifest) => indexes.extend(read_indexes(&manifest)),
            Err(e) => {
                web_log(format!(
                    "job {} note-patch: failed to read course manifest {}: {}",
                    job_id, source.path, e
                ));
            }
        }
    }
    if indexes.is_empty() {
        let err_msg = "No ready Course Transcript indexes found.".to_string();
        for output in outputs {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.clone());
                }
            });
        }
        return;
    }

    let sections = split_note_sections(&base_note);
    let total_section_chars: usize = sections
        .iter()
        .map(|s| s.heading.chars().count() + s.body.chars().count())
        .sum::<usize>()
        .max(1);
    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        0,
        total_section_chars,
        "retrieving course index",
    );
    let mut all_patches: Vec<PatchEntry> = Vec::new();
    let mut retrieval_traces: Vec<RetrievalTrace> = Vec::new();
    let mut completed_section_chars = 0usize;

    for section in sections {
        let section_chars = (section.heading.chars().count() + section.body.chars().count()).max(1);
        let query = format!(
            "{}\n{}",
            section.heading,
            truncate_chars(&section.body, 1600)
        );
        let hits = bm25_search(&indexes, &query, 3);
        let meaningful_hits: Vec<_> = hits.into_iter().filter(|h| h.score >= 0.2).collect();
        if meaningful_hits.is_empty() {
            retrieval_traces.push(RetrievalTrace {
                section: section.heading.clone(),
                matches: Vec::new(),
                skipped_reason: Some("no relevant date above threshold".to_string()),
            });
            completed_section_chars =
                (completed_section_chars + section_chars).min(total_section_chars);
            update_note_patch_progress(
                process_store,
                process_id,
                outputs,
                completed_section_chars,
                total_section_chars,
                &format!("skipped {}", section.heading),
            );
            continue;
        }

        let mut trace_matches = Vec::new();
        let mut context = String::new();
        for hit in &meaningful_hits {
            let ranges = hit
                .index
                .timestamp_ranges
                .iter()
                .take(8)
                .cloned()
                .collect::<Vec<TimestampRange>>();
            trace_matches.push(RetrievalMatch {
                date: hit.index.date.clone(),
                score: hit.score,
                timestamp_ranges: ranges.clone(),
            });
            let source_text = fs::read_to_string(&hit.index.source_path).unwrap_or_default();
            context.push_str(&format!(
                "\n\n--- Date: {} score {:.3} ---\nSummary: {}\nKeywords: {}\nConcepts: {}\nRanges:\n{}\nTranscript excerpt:\n{}",
                hit.index.date,
                hit.score,
                hit.index.summary,
                hit.index.keywords.join(", "),
                hit.index.concepts.join(", "),
                ranges
                    .iter()
                    .map(|r| format!(
                        "{} [{:.0}-{:.0}] {}",
                        r.video_id, r.start, r.end, r.text_preview
                    ))
                    .collect::<Vec<_>>()
                    .join("\n"),
                compact_transcript_for_llm(&source_text, 12000)
            ));
        }
        retrieval_traces.push(RetrievalTrace {
            section: section.heading.clone(),
            matches: trace_matches,
            skipped_reason: None,
        });

        match generate_section_patches(&section.heading, &section.body, &context).await {
            Ok(mut patches) => all_patches.append(&mut patches),
            Err(e) => {
                web_log(format!(
                    "job {} note-patch: section {} patch generation failed: {}",
                    job_id, section.heading, e
                ));
            }
        }
        completed_section_chars =
            (completed_section_chars + section_chars).min(total_section_chars);
        update_note_patch_progress(
            process_store,
            process_id,
            outputs,
            completed_section_chars,
            total_section_chars,
            &format!("processed {}", section.heading),
        );
    }

    let markdown_output = apply_structured_patches_to_note(&base_note, &all_patches);

    for output in outputs {
        let output_path = process_store.output_path(process_id, &output.id);
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&output_path, &markdown_output) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(format!("Failed to write output: {}", e));
                }
            });
            continue;
        }

        let diff_path = process_store.diff_path(process_id, &output.id);
        let unified = crate::diff::unified_diff(&base_note, &markdown_output, 3);
        let _ = fs::write(&diff_path, &unified);

        let retrieval_path = process_store
            .process_dir(process_id)
            .join(format!("{}.retrieval.json", output.id));
        let _ = fs::write(
            &retrieval_path,
            serde_json::to_string_pretty(&retrieval_traces).unwrap_or_default(),
        );

        let note_source_id = note_source.map(|n| n.id.clone());
        let _ = process_store.update(process_id, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                o.status = ProcessRecordStatus::Ready;
                o.base_source_id = note_source_id.clone();
                o.last_error = None;
                o.metadata = serde_json::json!({
                    "progress_current": total_section_chars,
                    "progress_total": total_section_chars,
                    "progress_label": "complete",
                });
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Staged course-note generation helpers
// ---------------------------------------------------------------------------

/// Generate a topic-based section outline from lightweight course index data.
///
/// The prompt is built **only** from index metadata (date, title, summary,
/// keywords, concepts, and a small `timestamp_ranges` preview).  Raw transcript
/// excerpts are deliberately excluded here.
/// Build compact outline context from `CourseDateIndex` metadata **only**.
///
/// Does **not** include transcript source text or `Transcript excerpt`.
/// Timestamp range preview is limited to at most 3 ranges per index with
/// each `text_preview` trimmed to 120 chars (char-safe).
fn build_outline_context(indexes: &[CourseDateIndex]) -> String {
    let mut context = String::new();
    for index in indexes {
        let ranges_preview = index
            .timestamp_ranges
            .iter()
            .take(3)
            .map(|r| {
                format!(
                    "{} [{:.0}-{:.0}] {}",
                    r.video_id,
                    r.start,
                    r.end,
                    truncate_chars(&r.text_preview, 120)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        context.push_str(&format!(
            "Date: {} | {}\nSummary: {}\nKeywords: {}\nConcepts: {}\nRanges preview:\n{}\n\n",
            index.date,
            index.title,
            index.summary,
            index.keywords.join(", "),
            index.concepts.join(", "),
            ranges_preview
        ));
    }
    context
}

/// Parse the LLM outline response into `PlannedSection` items.
///
/// Returns `Ok(Vec<PlannedSection>)` on success or `Err(diagnostic: String)`
/// with parse error detail, response character count, char-safe head/tail
/// snippets, and `[truncated_by_length]` marker when `finish_reason ==
/// "length"`.
fn parse_outline_sections(
    text: &str,
    finish_reason: Option<&str>,
) -> std::result::Result<Vec<PlannedSection>, String> {
    let char_count = text.chars().count();
    // Char-safe head (first 200 chars) and tail (last 200 chars).
    let head: String = text.chars().take(200).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(200)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    // Strip markdown code fences (same logic as llm::strip_code_fences_and_parse).
    let cleaned = {
        let t = text.trim();
        if let Some(rest) = t.strip_prefix("```json") {
            rest.strip_suffix("```").unwrap_or(rest).trim().to_string()
        } else if let Some(rest) = t.strip_prefix("```") {
            rest.strip_suffix("```").unwrap_or(rest).trim().to_string()
        } else {
            t.to_string()
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(e) => {
            let mut diag = format!("JSON parse error: {}. response_len={}", e, char_count);
            if let Some("length") = finish_reason {
                diag.push_str(" [truncated_by_length]");
            }
            if let Some(fr) = finish_reason {
                if fr != "length" {
                    diag.push_str(&format!(" finish_reason={}", fr));
                }
            }
            diag.push_str(&format!(" head={:?} tail={:?}", head, tail));
            return Err(diag);
        }
    };

    let sections_arr = json
        .get("sections")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "JSON missing 'sections' array. response_len={} finish_reason={:?} head={:?} tail={:?}",
                char_count, finish_reason, head, tail
            )
        })?;

    let mut sections = Vec::new();
    for item in sections_arr {
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let purpose = item
            .get("purpose")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let date_hints = json_array_strings(item.get("date_hints"));
        let video_hints = json_array_strings(item.get("video_hints"));
        let query_terms = json_array_strings(item.get("query_terms"));
        let must_include = json_array_strings(item.get("must_include"));

        sections.push(PlannedSection {
            title,
            purpose,
            date_hints,
            video_hints,
            query_terms,
            must_include,
        });
    }

    if sections.is_empty() {
        return Err(format!(
            "Outline JSON contained no valid sections. response_len={} head={:?} tail={:?}",
            char_count, head, tail
        ));
    }

    Ok(sections)
}

/// Generate a deterministic fallback outline from `CourseDateIndex` metadata.
///
/// Groups dates chronologically into 4-24 sections.  Section titles are
/// derived from title/keywords/concepts where available; `date_hints`,
/// `query_terms`, and `must_include` are derived from group keywords/concepts
/// capped to a few items.
///
/// Does **not** read transcript source files.
fn generate_fallback_outline(indexes: &[CourseDateIndex]) -> Result<Vec<PlannedSection>> {
    if indexes.is_empty() {
        anyhow::bail!("Cannot generate fallback outline: no indexes available");
    }

    let n = indexes.len();
    // 4-24 sections when enough indexes exist; fewer otherwise, but ≥ 1.
    let section_count = ((n as f64).sqrt().ceil() as usize).clamp(if n >= 4 { 4 } else { 1 }, 24);

    let chunk_size = ((n as f64) / (section_count as f64)).ceil() as usize;

    let mut sections = Vec::new();
    for i in 0..section_count {
        let start = i * chunk_size;
        let end = (start + chunk_size).min(n);
        if start >= n {
            break;
        }
        let group = &indexes[start..end];

        // Derive a section title from the group metadata.
        let title = if group.len() == 1 {
            let idx = &group[0];
            if !idx.title.is_empty() {
                idx.title.clone()
            } else if !idx.summary.is_empty() {
                truncate_chars(&idx.summary, 80)
            } else {
                format!("Course Topics {}", i + 1)
            }
        } else {
            // Collect keywords/concepts from the group to form a title.
            let mut terms: Vec<&str> = Vec::new();
            for idx in group {
                for kw in &idx.keywords {
                    if terms.len() < 3 && !terms.contains(&kw.as_str()) {
                        terms.push(kw);
                    }
                }
            }
            if terms.is_empty() {
                for idx in group {
                    for c in &idx.concepts {
                        if terms.len() < 3 && !terms.contains(&c.as_str()) {
                            terms.push(c);
                        }
                    }
                }
            }
            if !terms.is_empty() {
                terms.join(" / ")
            } else if !group[0].title.is_empty() {
                group[0].title.clone()
            } else {
                format!("Course Topics {}", i + 1)
            }
        };

        // date_hints: all dates in the group.
        let date_hints: Vec<String> = group.iter().map(|idx| idx.date.clone()).collect();

        // query_terms: keywords from the group, capped.
        let mut query_terms: Vec<String> = Vec::new();
        for idx in group {
            for kw in &idx.keywords {
                if query_terms.len() < 5 && !query_terms.contains(kw) {
                    query_terms.push(kw.clone());
                }
            }
        }

        // must_include: concepts from the group, capped.
        let mut must_include: Vec<String> = Vec::new();
        for idx in group {
            for c in &idx.concepts {
                if must_include.len() < 5 && !must_include.contains(c) {
                    must_include.push(c.clone());
                }
            }
        }

        sections.push(PlannedSection {
            title,
            purpose: format!(
                "Fallback section covering dates {}-{}",
                group[0].date,
                group[group.len() - 1].date
            ),
            date_hints,
            video_hints: Vec::new(),
            query_terms,
            must_include,
        });
    }

    if sections.is_empty() {
        anyhow::bail!("Fallback outline produced zero sections");
    }

    Ok(sections)
}

async fn generate_course_note_outline(indexes: &[CourseDateIndex]) -> Result<Vec<PlannedSection>> {
    let context = build_outline_context(indexes);

    let system = "\
You are a curriculum designer. Given course lecture index data, plan a structured note outline.\n\
\n\
Output **only** a JSON object with this exact shape:\n\
{\"sections\":[{\"title\":\"\",\"purpose\":\"\",\"date_hints\":[],\"video_hints\":[],\"query_terms\":[],\"must_include\":[]}]}\n\
\n\
Rules:\n\
- Produce 4-24 main sections. Choose the count based on the course span and topic diversity.\n\
- title: concise section heading (no body prose, no Markdown).\n\
- purpose: one short sentence describing why this section exists.\n\
- date_hints: date substrings that help select relevant lecture days (max 5 items).\n\
- video_hints: video ID substrings from the range previews (max 5 items).\n\
- query_terms: search keywords for retrieval (max 5 items).\n\
- must_include: key concepts / formulas / definitions that must appear (max 5 items).\n\
\n\
All arrays max 5 items. No body prose, no Markdown, no explanations outside the JSON.\n\
Organize by topic, not by date. Each section should cover a coherent topic that may span multiple lecture days.";

    let user = format!(
        "Course lecture index data:\n{}\n\nPlan a note outline with sections organized by topic. Return only the JSON object described above.",
        truncate_for_llm(&context, 32000)
    );

    let messages = vec![
        crate::llm::ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        },
        crate::llm::ChatMessage {
            role: "user".into(),
            content: user,
        },
    ];

    // --- First attempt: max_tokens = 49152, temperature = 0.2 ---
    let (text, finish_reason) =
        crate::llm::chat_completion_with_metadata(&messages, 0.2, 49152, Some("json_object"))
            .await
            .context("Outline LLM call failed")?;

    match parse_outline_sections(&text, finish_reason.as_deref()) {
        Ok(sections) if (4..=24).contains(&sections.len()) => return Ok(sections),
        Ok(sections) => {
            // LLM returned sections but outside the 4-24 range — retry.
            let diagnostic = format!(
                "Outline had {} section(s), expected 4-24. response_len={}",
                sections.len(),
                text.chars().count()
            );
            web_log(format!("Outline retry: {}", diagnostic));
        }
        Err(diagnostic) => {
            web_log(format!("Outline parse failure: {}", diagnostic));
        }
    }

    // --- Retry: max_tokens = 57344, include diagnostic in user prompt ---
    // The retry does NOT append the invalid output as an assistant message.
    // It starts a fresh conversation with the diagnostic embedded in the user
    // prompt.
    let retry_user = {
        let char_count = text.chars().count();
        let head: String = text.chars().take(300).collect();
        let tail: String = text
            .chars()
            .rev()
            .take(300)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        format!(
            "The previous outline attempt produced invalid or unusable JSON.\n\
             Diagnostic: response_len={} finish_reason={:?}\n\
             Head snippet: {}\n\
             Tail snippet: {}\n\n\
             Original course index context:\n{}\n\n\
             Plan a note outline with 4-24 sections organized by topic. Return only the JSON object.",
            char_count, finish_reason, head, tail,
            truncate_for_llm(&context, 28000)
        )
    };

    let retry_messages = vec![
        crate::llm::ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        },
        crate::llm::ChatMessage {
            role: "user".into(),
            content: retry_user,
        },
    ];

    let (retry_text, retry_finish_reason) =
        crate::llm::chat_completion_with_metadata(&retry_messages, 0.2, 57344, Some("json_object"))
            .await
            .context("Outline retry LLM call failed")?;

    match parse_outline_sections(&retry_text, retry_finish_reason.as_deref()) {
        Ok(sections) if (4..=24).contains(&sections.len()) => Ok(sections),
        Ok(sections) => {
            anyhow::bail!(
                "Outline retry produced {} section(s), expected 4-24",
                sections.len()
            );
        }
        Err(diagnostic) => {
            anyhow::bail!("Outline retry parse failed: {}", diagnostic);
        }
    }
}

/// Retrieve compact transcript context for a single planned section.
///
/// Prefers indexes whose date appears in `section.date_hints`; falls back to
/// BM25 search.  Limits selected indexes per section to 4 and enforces a
/// per-section transcript/context budget of 24 000 chars.
fn retrieve_course_context_for_section(
    section: &PlannedSection,
    indexes: &[CourseDateIndex],
) -> (String, RetrievalTrace) {
    let max_indexes: usize = 4;
    let context_budget: usize = 32000;
    let mut trace_matches = Vec::new();
    let mut selected: Vec<(&CourseDateIndex, f64)> = Vec::new();

    // 1) Date-hint match.
    if !section.date_hints.is_empty() {
        for index in indexes {
            if selected.len() >= max_indexes {
                break;
            }
            let date_matches = section.date_hints.iter().any(|hint| {
                index.date.contains(hint.as_str()) || hint.contains(index.date.as_str())
            });
            if date_matches {
                selected.push((index, 1.0));
            }
        }
    }

    // 2) Fallback to BM25.
    if selected.is_empty() {
        let query = format!(
            "{} {} {} {}",
            section.title,
            section.purpose,
            section.query_terms.join(" "),
            section.must_include.join(" ")
        );
        let hits = bm25_search(indexes, &query, max_indexes);
        for hit in hits {
            selected.push((hit.index, hit.score));
        }
    }

    let mut context = String::new();
    for (index, score) in &selected {
        let ranges = index
            .timestamp_ranges
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<TimestampRange>>();
        trace_matches.push(RetrievalMatch {
            date: index.date.clone(),
            score: *score,
            timestamp_ranges: ranges.clone(),
        });

        let remaining = context_budget.saturating_sub(context.chars().count());
        if remaining < 500 {
            break;
        }
        let source_text = fs::read_to_string(&index.source_path).unwrap_or_default();
        let transcript_budget = remaining.min(8000);
        context.push_str(&format!(
            "\n\n--- Date: {} | {} ---\nSummary: {}\nKeywords: {}\nConcepts: {}\nRanges:\n{}\nTranscript excerpt:\n{}",
            index.date,
            index.title,
            index.summary,
            index.keywords.join(", "),
            index.concepts.join(", "),
            ranges
                .iter()
                .map(|r| format!(
                    "{} [{:.0}-{:.0}] {}",
                    r.video_id, r.start, r.end, r.text_preview
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            compact_transcript_for_llm(&source_text, transcript_budget)
        ));
    }

    let trace = RetrievalTrace {
        section: section.title.clone(),
        matches: trace_matches,
        skipped_reason: if selected.is_empty() {
            Some("no relevant indexes found for section".to_string())
        } else {
            None
        },
    };

    (context, trace)
}

/// Generate Markdown for a single planned section from its retrieved context.
///
/// The prompt writes **only** that section; it must not invent facts beyond the
/// given context.  Defaults to Chinese while preserving English technical terms,
/// formulas, symbols, and code identifiers.
async fn generate_course_note_section(
    section: &PlannedSection,
    context: &str,
    style_guide: &str,
) -> Result<String> {
    let system = format!(
        "You are a lecture note writer. Write a single Markdown section.\n\
        Section title: {}\nPurpose: {}\n\
        Use ## {} for the section heading and ### for subsections.\n\
        Default language: Chinese, preserving English technical terms, formulas, symbols, and code identifiers.\n\
        Must include these concepts: {}\n\
        Do not invent facts beyond the provided context.\n\
        {}",
        section.title,
        section.purpose,
        section.title,
        section.must_include.join(", "),
        style_guide
    );

    let user = format!(
        "Write the Markdown section. Context:\n{}",
        truncate_for_llm(context, 32000)
    );

    let text = crate::llm::chat_text(&system, &user, 0.25, 16384).await?;
    Ok(strip_markdown_fences(&text))
}

/// Merge independently-generated section Markdown into one cohesive note.
///
/// The prompt normalises title hierarchy, terminology, formula formatting,
/// duplicate content, and transitions **without** adding new facts.  Only the
/// section texts and style guide are fed in — raw transcripts are excluded.
async fn merge_course_note_sections(sections: &[String], style_guide: &str) -> Result<String> {
    let combined = sections
        .iter()
        .enumerate()
        .map(|(i, s)| format!("<!-- section {} -->\n{}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n\n");

    let system = format!(
        "You are an editor normalizing lecture notes into a single cohesive Markdown document.\n\
        Normalize: title hierarchy (## for top sections, ### for subsections), terminology consistency, \
        formula formatting, duplicate content removal, and smooth transitions between sections.\n\
        Do not add new facts — only reorganize and normalize the provided content.\n\
        Default language: Chinese, preserving English technical terms, formulas, symbols, and code identifiers.\n\
        {}",
        style_guide
    );

    let user = format!(
        "Merge and normalize these note sections into one complete Markdown document:\n\n{}\n\nReturn only Markdown.",
        truncate_for_llm(&combined, 120000)
    );

    let text = crate::llm::chat_text(&system, &user, 0.2, 57344).await?;
    Ok(strip_markdown_fences(&text))
}

async fn run_course_note_generation(
    process_id: &str,
    course_sources: &[&SourceRecord],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    job_id: &str,
) {
    // Helper: mark every output as failed with the given error message.
    fn fail_outputs(
        process_store: &ProcessStore,
        process_id: &str,
        outputs: &[ProcessOutput],
        err_msg: &str,
    ) {
        for output in outputs {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.to_string());
                    o.updated_at = ProcessRecord::now_iso();
                }
            });
        }
    }

    // -----------------------------------------------------------------------
    // Step 1 — Load course index
    // -----------------------------------------------------------------------
    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        0,
        4,
        "loading course index",
    );

    let mut indexes = Vec::new();
    for source in course_sources {
        match read_manifest(&source.path) {
            Ok(manifest) => indexes.extend(read_indexes(&manifest)),
            Err(e) => {
                web_log(format!(
                    "job {} note-patch: failed to read course manifest {}: {}",
                    job_id, source.path, e
                ));
            }
        }
    }
    if indexes.is_empty() {
        fail_outputs(
            process_store,
            process_id,
            outputs,
            "No ready Course Transcript indexes found.",
        );
        return;
    }

    if !crate::llm::is_available() {
        fail_outputs(
            process_store,
            process_id,
            outputs,
            "LLM is not available. Set OPENAI_API_KEY in Settings to enable Note Patch generation.",
        );
        return;
    }

    // -----------------------------------------------------------------------
    // Step 2 — Plan outline from lightweight index metadata
    // -----------------------------------------------------------------------
    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        1,
        4,
        "planning note outline",
    );

    let (planned_sections, outline_fallback_used, outline_fallback_reason) =
        match generate_course_note_outline(&indexes).await {
            Ok(s) => (s, false, None),
            Err(e) => {
                // Try deterministic fallback before failing completely.
                let llm_error = e.to_string();
                match generate_fallback_outline(&indexes) {
                    Ok(sections) => {
                        web_log(format!(
                            "job {} note-patch: using fallback outline ({} sections) after LLM outline failed: {}",
                            job_id,
                            sections.len(),
                            llm_error
                        ));
                        (sections, true, Some(llm_error))
                    }
                    Err(fallback_err) => {
                        let err_msg = format!(
                            "Failed to generate course note outline: {}. Fallback also failed: {}",
                            llm_error, fallback_err
                        );
                        fail_outputs(process_store, process_id, outputs, &err_msg);
                        return;
                    }
                }
            }
        };

    let total_sections = planned_sections.len();
    let total_steps = total_sections + 3; // loading + outline + N sections + merge = N+3
    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        1,
        total_steps,
        "planning note outline",
    );

    // -----------------------------------------------------------------------
    // Step 3 — Generate each section independently
    // -----------------------------------------------------------------------
    let style_guide =
        "Use Chinese, preserving English technical terms, formulas, symbols, and code identifiers.";
    let mut generated_sections: Vec<String> = Vec::new();
    let mut retrieval_traces: Vec<RetrievalTrace> = Vec::new();
    let mut success_count: usize = 0;

    for (i, section) in planned_sections.iter().enumerate() {
        let label = format!("generating section {}/{}", i + 1, total_sections);
        update_note_patch_progress(
            process_store,
            process_id,
            outputs,
            2 + i,
            total_steps,
            &label,
        );

        let (context, trace) = retrieve_course_context_for_section(section, &indexes);
        retrieval_traces.push(trace);

        match generate_course_note_section(section, &context, style_guide).await {
            Ok(markdown) => {
                generated_sections.push(markdown);
                success_count += 1;
            }
            Err(e) => {
                web_log(format!(
                    "job {} note-patch: section '{}' generation failed: {}",
                    job_id, section.title, e
                ));
                let failed_count = total_sections.saturating_sub(success_count);
                let err_msg = format!(
                    "Section '{}' generation failed: {}. {}/{} sections succeeded.",
                    section.title, e, success_count, total_sections
                );
                fail_outputs(process_store, process_id, outputs, &err_msg);
                for output in outputs {
                    let _ = process_store.update(process_id, |r| {
                        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                            let mut meta = serde_json::json!({
                                "generated_without_note_source": true,
                                "section_count": total_sections,
                                "section_success_count": success_count,
                                "section_failed_count": failed_count,
                                "outline_fallback_used": outline_fallback_used,
                            });
                            if let Some(ref reason) = outline_fallback_reason {
                                meta["outline_fallback_reason"] = serde_json::json!(reason);
                            }
                            o.metadata = meta;
                        }
                    });
                }
                return;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Step 4 — Merge sections into one cohesive note
    // -----------------------------------------------------------------------
    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        1 + total_sections,
        total_steps,
        "merging sections",
    );

    let markdown_output = match merge_course_note_sections(&generated_sections, style_guide).await {
        Ok(md) => md,
        Err(e) => {
            let err_msg = format!("Failed to merge note sections: {}", e);
            fail_outputs(process_store, process_id, outputs, &err_msg);
            let failed_count = total_sections.saturating_sub(success_count);
            for output in outputs {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        let mut meta = serde_json::json!({
                            "generated_without_note_source": true,
                            "section_count": total_sections,
                            "section_success_count": success_count,
                            "section_failed_count": failed_count,
                            "outline_fallback_used": outline_fallback_used,
                        });
                        if let Some(ref reason) = outline_fallback_reason {
                            meta["outline_fallback_reason"] = serde_json::json!(reason);
                        }
                        o.metadata = meta;
                    }
                });
            }
            return;
        }
    };

    // -----------------------------------------------------------------------
    // Step 5 — Write outputs
    // -----------------------------------------------------------------------
    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        total_steps,
        total_steps,
        "complete",
    );

    let failed_count = total_sections.saturating_sub(success_count);
    for output in outputs {
        let output_path = process_store.output_path(process_id, &output.id);
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&output_path, &markdown_output) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(format!("Failed to write output: {}", e));
                    o.updated_at = ProcessRecord::now_iso();
                }
            });
            continue;
        }

        let diff_path = process_store.diff_path(process_id, &output.id);
        let _ = fs::write(&diff_path, "");

        let retrieval_path = process_store
            .process_dir(process_id)
            .join(format!("{}.retrieval.json", output.id));
        let _ = fs::write(
            &retrieval_path,
            serde_json::to_string_pretty(&retrieval_traces).unwrap_or_default(),
        );

        let _ = process_store.update(process_id, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                o.status = ProcessRecordStatus::Ready;
                o.base_source_id = None;
                o.last_error = None;
                let mut meta = serde_json::json!({
                    "progress_current": total_steps,
                    "progress_total": total_steps,
                    "progress_label": "complete",
                    "generated_without_note_source": true,
                    "section_count": total_sections,
                    "section_success_count": success_count,
                    "section_failed_count": failed_count,
                    "outline_fallback_used": outline_fallback_used,
                });
                if let Some(ref reason) = outline_fallback_reason {
                    meta["outline_fallback_reason"] = serde_json::json!(reason);
                }
                o.metadata = meta;
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Cheating Sheet capacity estimation and section inventory
// ---------------------------------------------------------------------------

/// Budget estimates for generating a cheating sheet of `max_pages` pages.
///
/// The template is 4-column A4 with 5pt body text. Budget estimates:
/// - ~11000 chars/page is a comfortable fill target.
/// - ~13500 chars/page is the soft maximum before content overflows.
#[derive(Debug, Clone)]
struct CheatingSheetBudget {
    target_chars: usize,
    soft_max_chars: usize,
    min_acceptable_chars: usize,
}

/// Estimate the character budget for a cheating sheet of `max_pages` pages.
///
/// Clamps `max_pages` to `1..=20` (matching the caller's policy).
fn estimate_cheating_sheet_budget(max_pages: usize) -> CheatingSheetBudget {
    let pages = max_pages.clamp(1, 20);
    let target_chars = pages.saturating_mul(11000);
    let soft_max_chars = pages.saturating_mul(15000);
    let min_acceptable_chars = target_chars.saturating_mul(3) / 4;
    CheatingSheetBudget {
        target_chars,
        soft_max_chars,
        min_acceptable_chars,
    }
}

// ---------------------------------------------------------------------------
// Markdown section inventory
// ---------------------------------------------------------------------------

/// A parsed section heading from source Markdown.
#[derive(Debug, Clone)]
struct CheatSection {
    /// Heading level (1 for `#`, 2 for `##`, 3 for `###`).
    level: usize,
    /// Heading text (without the `#` prefix).
    heading: String,
    /// Short preview of the section body (first ~120 chars).
    body_preview: String,
}

/// Build a compact inventory of Markdown sections for prompting.
///
/// Parses `#`, `##`, and `###` headings in order and captures a short body
/// preview for each section. The result is a plain-text inventory string
/// suitable for inclusion in an LLM prompt.
fn build_section_inventory(markdown: &str) -> (Vec<CheatSection>, String) {
    let heading_re = regex::Regex::new(r"^(#{1,3})\s+(.+)$").unwrap();
    let mut sections: Vec<CheatSection> = Vec::new();
    let mut current_body = String::new();

    for line in markdown.lines() {
        if let Some(caps) = heading_re.captures(line) {
            // Flush the current body into the previous section, if any.
            if let Some(last) = sections.last_mut() {
                if last.body_preview.is_empty() {
                    last.body_preview = truncate_chars(&current_body, 120);
                }
            }
            current_body.clear();

            let level = caps.get(1).unwrap().as_str().len();
            let heading = caps.get(2).unwrap().as_str().trim().to_string();
            sections.push(CheatSection {
                level,
                heading,
                body_preview: String::new(),
            });
        } else if !sections.is_empty() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if !current_body.is_empty() {
                    current_body.push(' ');
                }
                current_body.push_str(trimmed);
            }
        }
    }

    // Flush the last section.
    if let Some(last) = sections.last_mut() {
        if last.body_preview.is_empty() {
            last.body_preview = truncate_chars(&current_body, 120);
        }
    }

    // Build a compact inventory string.
    let inventory = sections
        .iter()
        .map(|s| {
            let prefix = "#".repeat(s.level);
            format!(
                "{} {}\n  body: {}\n",
                prefix,
                s.heading,
                if s.body_preview.is_empty() {
                    "(empty)"
                } else {
                    &s.body_preview
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    (sections, inventory)
}

// ---------------------------------------------------------------------------
// Cheating Sheet generation result
// ---------------------------------------------------------------------------

/// Result of a cheating sheet Markdown generation pass.
#[derive(Debug, Clone)]
struct CheatingSheetGenerationResult {
    markdown: String,
    target_chars: usize,
    generated_chars: usize,
    harness_attempts: usize,
    expansion_used: bool,
    underfilled_reason: Option<String>,
}

/// Decide whether to attempt an expansion pass.
///
/// Expansion is warranted only when the rendered page count is below max_pages
/// AND the source is long enough to support meaningful additions.
/// We prefer compression of over-generated content to expansion of under-generated content.
fn should_attempt_expansion(
    _generated_chars: usize,
    _min_acceptable_chars: usize,
    page_count: usize,
    max_pages: usize,
    source_too_short: bool,
) -> bool {
    if source_too_short {
        return false;
    }
    page_count < max_pages
}

/// Build metadata JSON for a cheating sheet output.
///
/// Preserves the existing metadata keys and adds the new capacity-aware fields.
fn build_cheatsheet_metadata(
    progress_current: usize,
    progress_total: usize,
    progress_label: &str,
    max_pages: usize,
    page_count: usize,
    compression_attempts: usize,
    template_used: &str,
    markdown_path: &str,
    reference_digest_output_id: &str,
    target_chars: usize,
    generated_chars: usize,
    harness_attempts: usize,
    expansion_used: bool,
    final_page_count: usize,
    underfilled_reason: Option<&str>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "progress_current": progress_current,
        "progress_total": progress_total,
        "progress_label": progress_label,
        "max_pages": max_pages,
        "page_count": page_count,
        "compression_attempts": compression_attempts,
        "template_used": template_used,
        "markdown_path": markdown_path,
        "reference_digest_output_id": reference_digest_output_id,
        "target_chars": target_chars,
        "generated_chars": generated_chars,
        "harness_attempts": harness_attempts,
        "expansion_used": expansion_used,
        "final_page_count": final_page_count,
    });
    if let Some(reason) = underfilled_reason {
        meta["underfilled_reason"] = serde_json::json!(reason);
    }
    meta
}

// ---------------------------------------------------------------------------
// Expansion prompt builder
// ---------------------------------------------------------------------------

/// Build an expansion prompt that asks the LLM to add high-value material
/// from the Reference Digest to an existing cheating sheet.
fn build_expansion_prompt(
    current_cheat: &str,
    section_inventory: &str,
    ref_digest_excerpt: &str,
    target_add_chars: usize,
) -> (String, String) {
    let system_prompt = "You expand an existing exam cheat-sheet Markdown by adding high-value material \
from the Reference Digest. Preserve the existing structure, headings, and content exactly as-is. \
Only add new material where it fits naturally: definitions, formulas, conditions, algorithm steps, \
pitfalls, comparisons, and exam judgement rules that are in the Reference Digest but missing from \
the cheat sheet. It is better to add slightly too much than too little — the renderer will compress \
if needed. \
Do not fabricate facts, do not rewrite existing sections, and do not change the document structure. \
Return ONLY Markdown, with no code fences, no explanations, and no raw LaTeX commands except ordinary \
math delimited by $...$ or $$...$$. The Markdown will be inserted into a XeLaTeX/xeCJK four-column A4 \
template, so avoid syntax that commonly breaks LaTeX: no HTML, images, footnotes, Markdown tables, \
nested tables, Mermaid, TikZ, custom macros, \\begin blocks, or unbalanced braces. \
Use only #, ##, ### headings, -, 1. lists, inline code for identifiers, bold for key terms, \
and standard Markdown math.";

    let user_prompt = format!(
        "The existing cheat sheet below should be expanded with missing high-value content \
from the Reference Digest. \
Add approximately {} more characters of high-value material. \
Maintain the exact same heading hierarchy and document structure.\n\n\
Section inventory (all sections must remain covered):\n{}\n\n\
Existing cheat sheet:\n{}\n\n\
Reference Digest excerpt:\n{}\n\n\
Add the most exam-critical missing material: definitions, formulas, conditions, pitfalls, \
algorithm steps, comparisons, and judgement rules. Do NOT add narrative, examples without \
reusable patterns, or anything already covered. \
Return the complete expanded cheat-sheet Markdown.",
        target_add_chars, section_inventory, current_cheat, ref_digest_excerpt
    );

    (system_prompt.to_string(), user_prompt)
}

async fn run_cheating_sheet_outputs(
    process_id: &str,
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    job_id: &str,
) {
    let process = match process_store.get(process_id) {
        Some(process) => process,
        None => return,
    };
    let ref_digest = process.outputs.iter().find(|o| {
        o.kind == ProcessOutputKind::ReferenceDigest && o.status == ProcessRecordStatus::Ready
    });
    let ref_digest = match ref_digest {
        Some(output) => output,
        None => {
            let err_msg =
                "Cheating Sheet requires a completed Reference Digest output.".to_string();
            for output in outputs {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Failed;
                        o.last_error = Some(err_msg.clone());
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
            return;
        }
    };

    let ref_digest_markdown = match fs::read_to_string(&ref_digest.path) {
        Ok(content) => content,
        Err(e) => {
            let err_msg = format!("Failed to read Reference Digest output: {}", e);
            for output in outputs {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Failed;
                        o.last_error = Some(err_msg.clone());
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
            return;
        }
    };

    for output in outputs {
        update_single_output_progress(
            process_store,
            process_id,
            &output.id,
            1,
            4,
            "condensing markdown",
        );
        let max_pages = output
            .metadata
            .get("max_pages")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(2)
            .clamp(1, 20);

        let ref_digest_chars = ref_digest_markdown.chars().count();
        let budget = estimate_cheating_sheet_budget(max_pages);
        let source_too_short = ref_digest_chars < budget.min_acceptable_chars;

        web_log(format!(
            "job {} cheating-sheet: budget max_pages={} target={} soft_max={} min_acceptable={} ref_digest_chars={} source_too_short={}",
            job_id, max_pages, budget.target_chars, budget.soft_max_chars,
            budget.min_acceptable_chars, ref_digest_chars, source_too_short,
        ));

        let gen_result = match generate_cheating_sheet_markdown(&ref_digest_markdown, max_pages)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                web_log(format!(
                    "job {} cheating-sheet: LLM condensation failed, rendering compressed Reference Digest directly: {}",
                    job_id, e
                ));
                let fallback_md = latex::compress_content(&ref_digest_markdown, 1);
                let fallback_chars = fallback_md.chars().count();
                // Fallback with no expansion — cannot expand a non-LLM draft.
                let markdown_path =
                    cheating_sheet_markdown_path(process_store, process_id, &output.id);
                if let Some(parent) = markdown_path.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                if let Err(write_err) = fs::write(&markdown_path, &fallback_md) {
                    let _ = process_store.update(process_id, |r| {
                        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                            o.status = ProcessRecordStatus::Failed;
                            o.last_error = Some(format!(
                                "Failed to write Cheating Sheet Markdown: {}",
                                write_err
                            ));
                            o.updated_at = ProcessRecord::now_iso();
                        }
                    });
                    continue;
                }
                // Render fallback without expansion.
                finish_cheating_sheet_render(
                    process_store,
                    process_id,
                    output,
                    ref_digest,
                    &fallback_md,
                    &markdown_path,
                    max_pages,
                    budget.target_chars,
                    fallback_chars,
                    0,
                    false,
                    Some("llm_failed".to_string()),
                );
                continue;
            }
        };

        let markdown_path = cheating_sheet_markdown_path(process_store, process_id, &output.id);
        if let Some(parent) = markdown_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&markdown_path, &gen_result.markdown) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(format!("Failed to write Cheating Sheet Markdown: {}", e));
                    o.updated_at = ProcessRecord::now_iso();
                }
            });
            continue;
        }

        update_single_output_progress(
            process_store,
            process_id,
            &output.id,
            2,
            4,
            "rendering LaTeX",
        );
        let pdf_path = process_output_path_for(
            process_store,
            process_id,
            &output.id,
            &ProcessOutputKind::CheatingSheet,
        );
        let render_result = latex::render_cheatsheet(
            &markdown_path.to_string_lossy(),
            None,
            &pdf_path.to_string_lossy(),
            max_pages,
        );

        // Determine if expansion is needed.
        let should_expand = match &render_result {
            Ok(artifact) => {
                let decision = should_attempt_expansion(
                    gen_result.generated_chars,
                    budget.min_acceptable_chars,
                    artifact.page_count,
                    max_pages,
                    source_too_short,
                );
                web_log(format!(
                    "job {} cheating-sheet: first render page_count={} max_pages={} generated_chars={} source_too_short={} -> expand={}",
                    job_id, artifact.page_count, max_pages, gen_result.generated_chars,
                    source_too_short, decision,
                ));
                decision
            }
            Err(e) => {
                web_log(format!(
                    "job {} cheating-sheet: first render FAILED: {} — skipping expansion",
                    job_id, e,
                ));
                false
            }
        };

        let (final_result, expansion_used, final_page_count) = if should_expand {
            update_single_output_progress(
                process_store,
                process_id,
                &output.id,
                3,
                4,
                "expanding content",
            );

            let current_chars = gen_result.generated_chars;
            let target_add_chars = (budget.target_chars.saturating_sub(current_chars))
                .min(6000)
                .max(2000);

            web_log(format!(
                "job {} cheating-sheet: expansion triggered — current_chars={} target_chars={} target_add_chars={}",
                job_id, current_chars, budget.target_chars, target_add_chars,
            ));
            let (_sections, inventory) = build_section_inventory(&ref_digest_markdown);
            let ref_digest_excerpt =
                truncate_ref_digest_for_cheatsheet(&ref_digest_markdown, 90000).0;

            let (exp_system, exp_user) = build_expansion_prompt(
                &gen_result.markdown,
                &inventory,
                &ref_digest_excerpt,
                target_add_chars,
            );

            match crate::llm::chat_text(&exp_system, &exp_user, 0.2, 81920).await {
                Ok(expanded_text) => {
                    let expanded_md = strip_markdown_fences(&expanded_text);
                    let expanded_chars = expanded_md.chars().count();

                    // Write expanded markdown.
                    if let Err(e) = fs::write(&markdown_path, &expanded_md) {
                        web_log(format!(
                            "job {} cheating-sheet: failed to write expansion markdown: {}",
                            job_id, e
                        ));
                        // Fall back to first render result.
                        (
                            gen_result,
                            false,
                            render_result.as_ref().ok().map(|a| a.page_count),
                        )
                    } else {
                        let exp_render = latex::render_cheatsheet(
                            &markdown_path.to_string_lossy(),
                            None,
                            &pdf_path.to_string_lossy(),
                            max_pages,
                        );

                        match exp_render {
                            Ok(exp_artifact) => {
                                let mut result = gen_result.clone();
                                result.markdown = expanded_md;
                                result.generated_chars = expanded_chars;
                                result.harness_attempts = 2;
                                result.expansion_used = true;
                                (result, true, Some(exp_artifact.page_count))
                            }
                            Err(e) => {
                                web_log(format!(
                                    "job {} cheating-sheet: expansion render failed, keeping first draft: {}",
                                    job_id, e
                                ));
                                // Restore first draft markdown.
                                let _ = fs::write(&markdown_path, &gen_result.markdown);
                                (
                                    gen_result,
                                    false,
                                    render_result.as_ref().ok().map(|a| a.page_count),
                                )
                            }
                        }
                    }
                }
                Err(e) => {
                    web_log(format!(
                        "job {} cheating-sheet: expansion LLM call failed, keeping first draft: {}",
                        job_id, e
                    ));
                    (
                        gen_result,
                        false,
                        render_result.as_ref().ok().map(|a| a.page_count),
                    )
                }
            }
        } else {
            let page_count = render_result.as_ref().ok().map(|a| a.page_count);
            let underfilled_reason = if source_too_short {
                Some("source_too_short".to_string())
            } else {
                None
            };
            if let Some(ref reason) = underfilled_reason {
                web_log(format!(
                    "job {} cheating-sheet: expansion skipped — reason={} ref_digest_chars={} min_acceptable={}",
                    job_id, reason, ref_digest_chars, budget.min_acceptable_chars,
                ));
            }
            let mut result = gen_result;
            result.underfilled_reason = underfilled_reason;
            (result, false, page_count)
        };

        // Final render if needed (when expansion was used, render already done above).
        let final_render_result = if expansion_used {
            // Re-render to get fresh artifact (the expansion already wrote the file).
            latex::render_cheatsheet(
                &markdown_path.to_string_lossy(),
                None,
                &pdf_path.to_string_lossy(),
                max_pages,
            )
        } else {
            render_result
        };

        match final_render_result {
            Ok(artifact) => {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Ready;
                        o.path = artifact.pdf_path.clone();
                        o.diff_path = None;
                        o.base_source_id = Some(ref_digest.id.clone());
                        o.last_error = None;
                        o.metadata = build_cheatsheet_metadata(
                            4,
                            4,
                            "complete",
                            max_pages,
                            artifact.page_count,
                            artifact.compression_attempts,
                            &artifact.template_used,
                            &markdown_path.to_string_lossy().to_string(),
                            &ref_digest.id,
                            final_result.target_chars,
                            final_result.generated_chars,
                            final_result.harness_attempts,
                            final_result.expansion_used,
                            final_page_count.unwrap_or(artifact.page_count),
                            final_result.underfilled_reason.as_deref(),
                        );
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
            Err(e) => {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Failed;
                        o.last_error = Some(format!("Cheating Sheet render failed: {}", e));
                        o.metadata = build_cheatsheet_metadata(
                            3,
                            4,
                            "render failed",
                            max_pages,
                            0,
                            0,
                            "",
                            &markdown_path.to_string_lossy().to_string(),
                            &ref_digest.id,
                            final_result.target_chars,
                            final_result.generated_chars,
                            final_result.harness_attempts,
                            final_result.expansion_used,
                            0,
                            final_result.underfilled_reason.as_deref(),
                        );
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
        }
    }
}

/// Helper to finalize a cheating sheet render result (used for fallback path).
fn finish_cheating_sheet_render(
    process_store: &ProcessStore,
    process_id: &str,
    output: &ProcessOutput,
    ref_digest: &ProcessOutput,
    _cheat_markdown: &str,
    markdown_path: &std::path::Path,
    max_pages: usize,
    target_chars: usize,
    generated_chars: usize,
    harness_attempts: usize,
    expansion_used: bool,
    underfilled_reason: Option<String>,
) {
    let pdf_path = process_output_path_for(
        process_store,
        process_id,
        &output.id,
        &ProcessOutputKind::CheatingSheet,
    );
    let render_result = latex::render_cheatsheet(
        &markdown_path.to_string_lossy(),
        None,
        &pdf_path.to_string_lossy(),
        max_pages,
    );

    match render_result {
        Ok(artifact) => {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Ready;
                    o.path = artifact.pdf_path.clone();
                    o.diff_path = None;
                    o.base_source_id = Some(ref_digest.id.clone());
                    o.last_error = None;
                    o.metadata = build_cheatsheet_metadata(
                        4,
                        4,
                        "complete",
                        max_pages,
                        artifact.page_count,
                        artifact.compression_attempts,
                        &artifact.template_used,
                        &markdown_path.to_string_lossy().to_string(),
                        &ref_digest.id,
                        target_chars,
                        generated_chars,
                        harness_attempts,
                        expansion_used,
                        artifact.page_count,
                        underfilled_reason.as_deref(),
                    );
                    o.updated_at = ProcessRecord::now_iso();
                }
            });
        }
        Err(e) => {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(format!("Cheating Sheet render failed: {}", e));
                    o.metadata = build_cheatsheet_metadata(
                        3,
                        4,
                        "render failed",
                        max_pages,
                        0,
                        0,
                        "",
                        &markdown_path.to_string_lossy().to_string(),
                        &ref_digest.id,
                        target_chars,
                        generated_chars,
                        harness_attempts,
                        expansion_used,
                        0,
                        underfilled_reason.as_deref(),
                    );
                    o.updated_at = ProcessRecord::now_iso();
                }
            });
        }
    }
}

/// Truncate a Reference Digest for the cheat-sheet prompt by taking
/// proportional excerpts from each `##` section rather than only the
/// first N characters.  This ensures every topic has at least a minimal
/// presence in the prompt, avoiding total omission of later sections.
///
/// Returns the truncated Markdown and the number of sections included.
fn truncate_ref_digest_for_cheatsheet(markdown: &str, max_chars: usize) -> (String, usize) {
    let min_per_section: usize = 600;
    let preamble_budget: usize = 1200;

    // Split on `## ` boundaries.
    let mut sections: Vec<(usize, &str)> = Vec::new(); // (start_byte_offset, full_text)
    let mut preamble_end: usize = 0;

    // Find the first `\n## ` boundary.
    if let Some(first_h2) = markdown.find("\n## ") {
        preamble_end = first_h2;
        let body = &markdown[first_h2..];
        // Split body into h2-headed sections.
        let mut prev_start: usize = 0;
        for m in regex::Regex::new(r"\n## ").unwrap().find_iter(body) {
            if prev_start > 0 {
                sections.push((
                    first_h2 + prev_start,
                    &markdown[first_h2 + prev_start..first_h2 + m.start()],
                ));
            }
            prev_start = m.start();
        }
        // Last section.
        if prev_start < body.len() {
            sections.push((first_h2 + prev_start, &markdown[first_h2 + prev_start..]));
        }
    }

    if sections.is_empty() {
        // No h2 headings — fall back to head-only truncation.
        return (truncate_for_llm(markdown, max_chars), 0);
    }

    let total_sections = sections.len();

    // Allocate budget: preamble gets preamble_budget chars, remainder
    // is split proportionally among sections, with min_per_section floor.
    let preamble = truncate_chars(&markdown[..preamble_end], preamble_budget);
    let body_budget = max_chars.saturating_sub(preamble.chars().count());

    let min_floor = total_sections.saturating_mul(min_per_section);
    if body_budget <= min_floor {
        // Tight budget: give each section exactly min_per_section chars.
        let mut result = preamble;
        result.push('\n');
        for (_offset, section_text) in &sections {
            if let Some(heading_end) = section_text.find('\n') {
                result.push_str(&section_text[..heading_end]);
                result.push('\n');
                let body_start = heading_end + 1;
                let body = &section_text[body_start..];
                result.push_str(&truncate_chars(body.trim_start(), min_per_section));
                result.push_str("\n\n");
            }
        }
        return (result, total_sections);
    }

    // Proportional allocation: each section gets its share of the remaining
    // budget based on its character count.
    let total_chars: usize = sections.iter().map(|(_, t)| t.chars().count()).sum();
    let mut result = preamble;
    result.push('\n');

    for (_offset, section_text) in &sections {
        let section_chars = section_text.chars().count();
        let share = if total_chars > 0 {
            ((section_chars as f64 / total_chars as f64) * body_budget as f64) as usize
        } else {
            body_budget / total_sections
        };
        let alloc = share.max(min_per_section);

        if let Some(heading_end) = section_text.find('\n') {
            result.push_str(&section_text[..heading_end]);
            result.push('\n');
            let body_start = heading_end + 1;
            let body = &section_text[body_start..];
            result.push_str(&truncate_chars(body.trim_start(), alloc));
            result.push_str("\n\n");
        }
    }

    (result, total_sections)
}

/// Build shared Cheat Sheet prompts (used by both streaming and non-streaming paths).
fn build_cheat_sheet_prompts(ref_digest_markdown: &str, max_pages: usize) -> (String, String) {
    let budget = estimate_cheating_sheet_budget(max_pages);
    let (sections, inventory) = build_section_inventory(ref_digest_markdown);

    let section_count = sections.len();
    let section_list: Vec<String> = sections.iter().map(|s| s.heading.clone()).collect();
    let section_names = section_list.join(" | ");

    let system_prompt = "You convert a Reference Digest into an exam reference cheat-sheet Markdown \
for a fixed LaTeX template. Cover every topic comprehensively: for each section, include its essential \
definitions, formulas, conditions, algorithm steps, comparisons, pitfalls, and exam judgement rules. \
It is better to include slightly too much content than too little — the renderer will compress if \
needed. Do not omit topics.\n\
Return ONLY Markdown, with no code fences, no explanations, and no raw LaTeX commands except ordinary \
math delimited by $...$ or $$...$$. \
The Markdown will be escaped and inserted into a XeLaTeX/xeCJK four-column A4 template, so avoid \
syntax that commonly breaks LaTeX: no HTML, images, footnotes, Markdown tables, nested tables, \
Mermaid, TikZ, custom macros, \\begin blocks, or unbalanced braces. \
Use Chinese for Chinese source material, keep standard English technical terms, identifiers, and formulas. \
Prefer short headings, information-rich bullets, definitions, theorem/condition/result patterns, \
formulas, contrasts, and procedure steps. \
Use only #, ##, ### headings, -, 1. lists, inline code for identifiers, bold for key terms, \
and standard Markdown math. \
Every formula must be syntactically balanced. Keep underscores and percent signs inside math or code \
when possible.";

    let user_prompt = format!(
        "Target: a {} page(s) exam cheat sheet.\n\
Roughly {} characters typically fills {} page(s); up to {} is fine — the renderer will \
compress if needed.\n\n\
Coverage requirement: the Reference Digest has {} main sections in order: {}\n\
Every main section MUST contribute at least one of: definition, formula, condition, algorithm step, \
pitfall, comparison, or exam judgement rule.\n\n\
Do not invent new facts. Extract and organize the essential content from the Reference Digest below. \
Prefer completeness over conciseness.\n\n\
Section inventory:\n{}\n\n\
Reference Digest Markdown:\n\n{}\n\n\
Return only the complete cheating-sheet Markdown. Aim for roughly {} characters; more is acceptable.",
        max_pages,
        budget.target_chars,
        max_pages,
        budget.soft_max_chars,
        section_count,
        section_names,
        inventory,
        truncate_ref_digest_for_cheatsheet(ref_digest_markdown, 90000).0,
        budget.target_chars,
    );

    (system_prompt.to_string(), user_prompt)
}

async fn generate_cheating_sheet_markdown(
    ref_digest_markdown: &str,
    max_pages: usize,
) -> Result<CheatingSheetGenerationResult> {
    if !crate::llm::is_available() {
        bail!("LLM is not available");
    }

    let budget = estimate_cheating_sheet_budget(max_pages);
    let (system_prompt, user_prompt) = build_cheat_sheet_prompts(ref_digest_markdown, max_pages);

    let text = crate::llm::chat_text(&system_prompt, &user_prompt, 0.2, 81920).await?;
    let markdown = strip_markdown_fences(&text);
    let generated_chars = markdown.chars().count();

    Ok(CheatingSheetGenerationResult {
        markdown,
        target_chars: budget.target_chars,
        generated_chars,
        harness_attempts: 1,
        expansion_used: false,
        underfilled_reason: None,
    })
}

fn strip_markdown_fences(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.starts_with("```markdown") {
        cleaned
            .strip_prefix("```markdown")
            .and_then(|s| s.strip_suffix("```"))
            .map(|s| s.trim())
            .unwrap_or(cleaned)
            .to_string()
    } else if cleaned.starts_with("```") {
        cleaned
            .strip_prefix("```")
            .and_then(|s| s.strip_suffix("```"))
            .map(|s| s.trim())
            .unwrap_or(cleaned)
            .to_string()
    } else {
        cleaned.to_string()
    }
}

fn update_single_output_progress(
    process_store: &ProcessStore,
    process_id: &str,
    output_id: &str,
    current: usize,
    total: usize,
    label: &str,
) {
    let current = current.min(total);
    let total = total.max(1);
    let label = label.to_string();
    let _ = process_store.update(process_id, |r| {
        if let Some(output) = r.outputs.iter_mut().find(|o| o.id == output_id) {
            let mut metadata = output.metadata.clone();
            if !metadata.is_object() {
                metadata = serde_json::json!({});
            }
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert("progress_current".to_string(), serde_json::json!(current));
                obj.insert("progress_total".to_string(), serde_json::json!(total));
                obj.insert("progress_label".to_string(), serde_json::json!(label));
            }
            output.metadata = metadata;
            output.updated_at = ProcessRecord::now_iso();
        }
    });
}

fn update_note_patch_progress(
    process_store: &ProcessStore,
    process_id: &str,
    outputs: &[ProcessOutput],
    current: usize,
    total: usize,
    label: &str,
) {
    let output_ids: Vec<String> = outputs.iter().map(|o| o.id.clone()).collect();
    let current = current.min(total);
    let total = total.max(1);
    let label = label.to_string();
    let _ = process_store.update(process_id, |r| {
        for output in &mut r.outputs {
            if output_ids.contains(&output.id) {
                output.metadata = serde_json::json!({
                    "progress_current": current,
                    "progress_total": total,
                    "progress_label": label,
                });
                output.updated_at = ProcessRecord::now_iso();
            }
        }
    });
}

async fn generate_section_patches(
    heading: &str,
    body: &str,
    context: &str,
) -> Result<Vec<PatchEntry>> {
    let system = "You produce structured note patches from retrieved lecture transcript excerpts. \
                  Output JSON only with a patches array. Each patch has location, new_text, source_date, source_video_id, source_timestamp, confidence. \
                  Only add facts that are clearly supported by the transcript context and missing from the note section.";
    let user = format!(
        "Note section heading: {}\n\nCurrent section body:\n{}\n\nRetrieved transcript context:\n{}",
        heading,
        truncate_chars(body, 4000),
        truncate_for_llm(context, 45000)
    );
    let json = crate::llm::chat_json(system, &user, 0.2, 8192).await?;
    let mut patches = Vec::new();
    if let Some(arr) = json.get("patches").and_then(|v| v.as_array()) {
        for item in arr {
            let new_text = item
                .get("new_text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if new_text.is_empty() {
                continue;
            }
            let confidence = item
                .get("confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.7);
            if confidence < 0.55 {
                continue;
            }
            let location = item
                .get("location")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(heading)
                .to_string();
            patches.push(PatchEntry {
                location,
                original_text: None,
                new_text: new_text.to_string(),
                source_video_id: item
                    .get("source_video_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(ToString::to_string),
                source_timestamp: item.get("source_timestamp").and_then(|v| v.as_f64()),
                keep_level: KeepLevel::Compress,
            });
        }
    }
    Ok(patches)
}

fn apply_structured_patches_to_note(base_note: &str, patches: &[PatchEntry]) -> String {
    if patches.is_empty() {
        return base_note.to_string();
    }
    let mut grouped: BTreeMap<&str, Vec<&PatchEntry>> = BTreeMap::new();
    for patch in patches {
        grouped
            .entry(patch.location.as_str())
            .or_default()
            .push(patch);
    }

    let mut lines: Vec<String> = base_note.lines().map(ToString::to_string).collect();
    let mut insertions: Vec<(usize, Vec<String>)> = Vec::new();

    for (location, entries) in grouped {
        let insertion_index = find_section_insert_index(&lines, location).unwrap_or(lines.len());
        let mut addition_lines = Vec::new();
        if insertion_index > 0
            && !lines
                .get(insertion_index.saturating_sub(1))
                .is_some_and(|l| l.trim().is_empty())
        {
            addition_lines.push(String::new());
        }
        for entry in entries {
            let source = match (&entry.source_video_id, entry.source_timestamp) {
                (Some(video), Some(ts)) => format!(" (source: {} @ {:.0}s)", video, ts),
                (Some(video), None) => format!(" (source: {})", video),
                _ => String::new(),
            };
            addition_lines.push(format!("- {}{}", entry.new_text.trim(), source));
        }
        addition_lines.push(String::new());
        insertions.push((insertion_index, addition_lines));
    }

    insertions.sort_by(|a, b| b.0.cmp(&a.0));
    for (idx, new_lines) in insertions {
        for line in new_lines.into_iter().rev() {
            lines.insert(idx, line);
        }
    }

    let mut out = lines.join("\n");
    if base_note.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn find_section_insert_index(lines: &[String], location: &str) -> Option<usize> {
    let target = normalize_heading_text(location);
    if target.is_empty() {
        return None;
    }
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if level == 0 || level > 6 || !trimmed[level..].starts_with(' ') {
            continue;
        }
        let heading = normalize_heading_text(&trimmed[level..]);
        if heading != target && !target.contains(&heading) && !heading.contains(&target) {
            continue;
        }
        for next_idx in idx + 1..lines.len() {
            let next = lines[next_idx].trim_start();
            if next.starts_with('#') {
                let next_level = next.chars().take_while(|c| *c == '#').count();
                if next_level > 0 && next_level <= level && next[next_level..].starts_with(' ') {
                    return Some(next_idx);
                }
            }
        }
        return Some(lines.len());
    }
    None
}

fn normalize_heading_text(value: &str) -> String {
    value.trim().trim_matches('#').trim().to_lowercase()
}

/// `GET /api/processes/{id}/outputs/{output_id}` -- get output content.
async fn api_get_process_output(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Response {
    let process = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Process not found"})),
            )
                .into_response();
        }
    };

    let output = match process.outputs.iter().find(|o| o.id == output_id) {
        Some(o) => o,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Output not found"})),
            )
                .into_response();
        }
    };

    let markdown_path = if output.kind == ProcessOutputKind::CheatingSheet {
        output
            .metadata
            .get("markdown_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        output.path.clone()
    };
    let markdown = if markdown_path.is_empty() {
        String::new()
    } else {
        fs::read_to_string(&markdown_path).unwrap_or_default()
    };
    let diff = output
        .diff_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    let retrieval_path = state
        .process_store
        .process_dir(&process_id)
        .join(format!("{}.retrieval.json", output_id));
    let retrieval: serde_json::Value = fs::read_to_string(&retrieval_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!([]));
    let has_base_note = output.base_source_id.is_some();

    Json(serde_json::json!({
        "process_id": process_id,
        "output": output,
        "markdown": markdown,
        "diff": diff,
        "retrieval": retrieval,
        "has_base_note": has_base_note,
        "artifact_path": output.path,
    }))
    .into_response()
}

/// `GET /api/processes/{id}/outputs/{output_id}/file` — serve the output file (PDF etc.).
async fn api_get_process_output_file(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Response {
    let process = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Process not found"})),
            )
                .into_response();
        }
    };

    let output = match process.outputs.iter().find(|o| o.id == output_id) {
        Some(o) => o,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Output not found"})),
            )
                .into_response();
        }
    };

    let file_path = &output.path;
    if file_path.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Output file path is empty"})),
        )
            .into_response();
    }

    let bytes = match fs::read(file_path) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Failed to read output file: {}", e)})),
            )
                .into_response();
        }
    };

    let content_type = match std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
    {
        Some("pdf") => "application/pdf",
        Some("tex") | Some("typ") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type)],
        bytes,
    )
        .into_response()
}

/// `POST /api/processes/{id}/outputs` -- add an output method to a process.
///
/// Body: `{ kind: "note_patch" | "cheating_sheet", max_pages?: 2 }`
#[derive(Debug, Deserialize)]
struct AddOutputBody {
    kind: String,
    #[serde(default)]
    max_pages: Option<usize>,
}

async fn api_add_process_output(
    State(state): State<AppState>,
    Path(process_id): Path<String>,
    Json(body): Json<AddOutputBody>,
) -> Json<serde_json::Value> {
    let requested_kind = match parse_process_output_kind(&body.kind) {
        Some(kind) => kind,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Unsupported output kind: {}. Supported kinds: note_patch, reference_digest, cheating_sheet.", body.kind),
            }));
        }
    };

    let process = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Process not found",
            }));
        }
    };

    let has_note_patch = process
        .outputs
        .iter()
        .any(|o| o.kind == ProcessOutputKind::NotePatch);
    let has_reference_digest = process
        .outputs
        .iter()
        .any(|o| o.kind == ProcessOutputKind::ReferenceDigest);
    let has_cheating_sheet = process
        .outputs
        .iter()
        .any(|o| o.kind == ProcessOutputKind::CheatingSheet);

    if requested_kind == ProcessOutputKind::NotePatch && has_note_patch {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Note Patch output already exists for this process. Only one note_patch output is supported.",
        }));
    }

    if requested_kind == ProcessOutputKind::ReferenceDigest && has_reference_digest {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Reference Digest output already exists for this process. Only one reference_digest output is supported.",
        }));
    }

    if requested_kind == ProcessOutputKind::CheatingSheet && has_cheating_sheet {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Cheating Sheet output already exists for this process. Only one cheating_sheet output is supported.",
        }));
    }

    // Check if there is an existing Reference Digest in Ready or Processing state for reuse.
    let existing_rd_usable =
        if requested_kind == ProcessOutputKind::CheatingSheet && !has_reference_digest {
            // Also check for Processing reference digest that could be reused.
            process.outputs.iter().any(|o| {
                o.kind == ProcessOutputKind::ReferenceDigest
                    && (o.status == ProcessRecordStatus::Ready
                        || o.status == ProcessRecordStatus::Processing)
            })
        } else {
            has_reference_digest
        };

    let now = ProcessRecord::now_iso();
    let mut new_outputs = Vec::new();
    let mut kinds_to_add = Vec::new();
    if requested_kind == ProcessOutputKind::CheatingSheet && !existing_rd_usable {
        kinds_to_add.push((ProcessOutputKind::ReferenceDigest, 0usize));
    }
    kinds_to_add.push((
        requested_kind.clone(),
        body.max_pages.unwrap_or(2).clamp(1, 20),
    ));

    for (kind, max_pages) in kinds_to_add {
        let output_id = uuid::Uuid::new_v4().to_string();
        let output_path =
            process_output_path_for(&state.process_store, &process_id, &output_id, &kind);
        let diff_path = if kind == ProcessOutputKind::NotePatch {
            Some(state.process_store.diff_path(&process_id, &output_id))
        } else {
            None
        };
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let markdown_path =
            cheating_sheet_markdown_path(&state.process_store, &process_id, &output_id);
        let metadata = if kind == ProcessOutputKind::ReferenceDigest {
            serde_json::json!({
                "progress_current": 0,
                "progress_total": 2,
                "progress_label": "queued",
            })
        } else if kind == ProcessOutputKind::CheatingSheet {
            serde_json::json!({
                "progress_current": 0,
                "progress_total": 4,
                "progress_label": "queued",
                "max_pages": max_pages,
                "markdown_path": markdown_path.to_string_lossy().to_string(),
            })
        } else {
            serde_json::json!({
                "progress_current": 0,
                "progress_total": 1,
                "progress_label": "queued",
            })
        };
        new_outputs.push(ProcessOutput {
            id: output_id,
            kind: kind.clone(),
            status: ProcessRecordStatus::Processing,
            title: process_output_title(&kind).to_string(),
            path: output_path.to_string_lossy().to_string(),
            diff_path: diff_path.map(|p| p.to_string_lossy().to_string()),
            base_source_id: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_error: None,
            metadata,
        });
    }

    let src_ids = process.source_ids.clone();
    let outputs_for_job = new_outputs.clone();
    let _ = state.process_store.update(&process_id, |r| {
        r.outputs.extend(new_outputs);
        r.status = ProcessRecordStatus::Processing;
        r.last_error = None;
    });

    // Launch background job for the new output.
    let process_store = state.process_store.clone();
    let source_store = state.source_store.clone();
    let secrets = state.secrets.clone();
    let registry = state.registry.clone();
    let pid = process_id.clone();

    let job = registry.run_in_background("process-output", move |job| {
        let saved_secrets = secrets.load();
        saved_secrets.apply_to_env();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            run_process_outputs(
                &pid,
                &src_ids,
                &outputs_for_job,
                &process_store,
                &source_store,
                &job.job_id,
            )
            .await;
        });
        update_process_terminal_status(&process_store, &pid, &job.job_id);
    });

    Json(serde_json::json!({
        "status": "processing",
        "job_id": job.job_id,
    }))
}

/// `DELETE /api/processes/{id}/outputs/{output_id}` -- remove an output.
async fn api_delete_process_output(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Response {
    let mut process = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "Process not found"})),
            )
                .into_response();
        }
    };

    if let Some(output) = process.outputs.iter().find(|o| o.id == output_id) {
        // Best-effort delete artifact files.
        if !output.path.is_empty() {
            let _ = fs::remove_file(&output.path);
        }
        if let Some(ref diff_path) = output.diff_path {
            let _ = fs::remove_file(diff_path);
        }
        if let Some(markdown_path) = output
            .metadata
            .get("markdown_path")
            .and_then(|v| v.as_str())
        {
            let _ = fs::remove_file(markdown_path);
        }
        let retrieval_path = state
            .process_store
            .process_dir(&process_id)
            .join(format!("{}.retrieval.json", output_id));
        let _ = fs::remove_file(retrieval_path);
    }

    process.outputs.retain(|o| o.id != output_id);

    // If no outputs remain, remove the entire process record.
    if process.outputs.is_empty() {
        // Best-effort clean up process directory.
        let proc_dir = state.process_store.process_dir(&process_id);
        let _ = fs::remove_dir_all(&proc_dir);
        state.process_store.delete(&process_id);
        return Json(serde_json::json!({
            "status": "deleted",
            "process_removed": true,
        }))
        .into_response();
    }

    let _ = state.process_store.update(&process_id, |r| {
        r.outputs.retain(|o| o.id != output_id);
    });

    Json(serde_json::json!({
        "status": "deleted",
        "process_removed": false,
    }))
    .into_response()
}

/// `POST /api/processes/{id}/outputs/{output_id}/revise` -- revise with LLM.
///
/// Body: `{ instruction: string }`
#[derive(Debug, Deserialize)]
struct ReviseOutputBody {
    instruction: String,
}

async fn api_revise_process_output(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
    Json(body): Json<ReviseOutputBody>,
) -> Json<serde_json::Value> {
    if body.instruction.trim().is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Instruction is required."
        }));
    }

    let process = match state.process_store.get(&process_id) {
        Some(p) => p,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Process not found",
            }));
        }
    };

    let output = match process.outputs.iter().find(|o| o.id == output_id) {
        Some(o) => o,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "Output not found",
            }));
        }
    };
    if output.kind != ProcessOutputKind::NotePatch {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Only Note Patch outputs can be revised directly. Regenerate the Cheating Sheet after revising Note Patch.",
        }));
    }

    // Read current markdown.
    let current_md = match fs::read_to_string(&output.path) {
        Ok(c) => c,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to read output: {}", e),
            }));
        }
    };

    // Read base note if it exists for context.
    let base_note_content = output
        .base_source_id
        .as_ref()
        .and_then(|sid| state.source_store.get(sid))
        .and_then(|src| fs::read_to_string(&src.path).ok());

    // Check LLM availability.
    if !crate::llm::is_available() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "LLM is not available. Set OPENAI_API_KEY in Settings to enable revision.",
        }));
    }

    let system_prompt =
        "You are an expert Markdown note editor. You will be given the current Markdown note and a revision instruction. \
         Apply the instruction to produce an updated complete Markdown note. \
         Preserve the Markdown format. Make only the changes requested; do not rewrite unrelated sections. \
         Return ONLY the complete updated Markdown note, with no surrounding explanation, no code fences, and no commentary.";

    let mut user_prompt = format!(
        "Current Markdown note:\n\n{}\n\n---\n\nRevision instruction: {}\n\n---\n\n\
         Return the complete updated Markdown note (no code fences, no explanation).",
        current_md, body.instruction
    );

    if let Some(ref base) = base_note_content {
        user_prompt.push_str(&format!(
            "\n\nFor reference, here is the original base note (before patching):\n\n{}",
            truncate_for_llm(base, 40000)
        ));
    }

    // Call LLM.
    let updated_md = match crate::llm::chat_text(system_prompt, &user_prompt, 0.3, 32768).await {
        Ok(text) => {
            let cleaned = text.trim();
            let cleaned = if cleaned.starts_with("```markdown") {
                cleaned
                    .strip_prefix("```markdown")
                    .and_then(|s| s.strip_suffix("```"))
                    .map(|s| s.trim())
                    .unwrap_or(cleaned)
            } else if cleaned.starts_with("```") {
                cleaned
                    .strip_prefix("```")
                    .and_then(|s| s.strip_suffix("```"))
                    .map(|s| s.trim())
                    .unwrap_or(cleaned)
            } else {
                cleaned
            };
            cleaned.to_string()
        }
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("LLM call failed: {}", e),
            }));
        }
    };

    // Write updated markdown.
    if let Err(e) = fs::write(&output.path, &updated_md) {
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Failed to write updated output: {}", e),
        }));
    }

    // Update diff if base note exists.
    let diff_updated = if let Some(ref base) = base_note_content {
        let unified = crate::diff::unified_diff(base, &updated_md, 3);
        if let Some(ref diff_path) = output.diff_path {
            let _ = fs::write(diff_path, &unified);
        }
        true
    } else {
        false
    };

    // Update the output record.
    let _ = state.process_store.update(&process_id, |r| {
        if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output_id) {
            o.updated_at = ProcessRecord::now_iso();
        }
    });

    let diff_content = output
        .diff_path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();

    Json(serde_json::json!({
        "status": "revised",
        "markdown": updated_md,
        "diff": diff_content,
        "has_base_note": output.base_source_id.is_some(),
        "diff_updated": diff_updated,
    }))
}

// ---------------------------------------------------------------------------
// Canvas LMS courses API
// ---------------------------------------------------------------------------

/// `GET /api/canvas/courses` -- list available Canvas courses.
///
/// Uses the saved Canvas token from secrets, or a token provided via
/// `Authorization` header or `?token=` query parameter.
///
/// Calls the official Canvas LMS REST API:
/// `GET https://oc.sjtu.edu.cn/api/v1/courses?include[]=term&include[]=teachers&state[]=available&per_page=100`
#[derive(Debug, Deserialize)]
struct CanvasCoursesQuery {
    #[serde(default)]
    token: String,
}

async fn api_canvas_courses(
    State(state): State<AppState>,
    Query(query): Query<CanvasCoursesQuery>,
) -> Json<serde_json::Value> {
    // Resolve token: query param > secrets. We do NOT read Authorization header
    // here to keep the implementation simple; the query param or saved token
    // covers the primary use case.
    let saved_secrets = state.secrets.load();
    let token = if query.token.trim().is_empty() {
        if saved_secrets.canvas_token.is_empty() {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "No Canvas API token available. Save a Canvas token in Settings or pass ?token=... query parameter.",
                "courses": [],
            }));
        }
        saved_secrets.canvas_token.clone()
    } else {
        query.token.trim().to_string()
    };

    let url = "https://oc.sjtu.edu.cn/api/v1/courses?include[]=term&include[]=teachers&state[]=available&per_page=100";

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to create HTTP client: {}", e),
                "courses": [],
            }));
        }
    };

    let resp = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to connect to Canvas API: {}", e),
                "courses": [],
            }));
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return Json(serde_json::json!({
            "status": "failed",
            "error": format!("Canvas API returned HTTP {}: {}", status.as_u16(), body_text),
            "courses": [],
        }));
    }

    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to parse Canvas API response: {}", e),
                "courses": [],
            }));
        }
    };

    // Parse courses into a simple list.
    let courses: Vec<serde_json::Value> = json
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|course| {
                    let term_name = course
                        .get("term")
                        .and_then(|t| t.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let teachers: Vec<&str> = course
                        .get("teachers")
                        .and_then(|t| t.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|t| {
                                    t.get("display_name").and_then(|v| v.as_str())
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    serde_json::json!({
                        "id": course.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
                        "name": course.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                        "course_code": course.get("course_code").and_then(|v| v.as_str()).unwrap_or(""),
                        "start_at": course.get("start_at").and_then(|v| v.as_str()),
                        "end_at": course.get("end_at").and_then(|v| v.as_str()),
                        "workflow_state": course.get("workflow_state").and_then(|v| v.as_str()).unwrap_or(""),
                        "enrollment_term_id": course.get("enrollment_term_id").and_then(|v| v.as_u64()),
                        "term_name": term_name,
                        "teachers": teachers,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Json(serde_json::json!({
        "status": "succeeded",
        "courses": courses,
        "count": courses.len(),
    }))
}

// ---------------------------------------------------------------------------
// Canvas course dates API
// ---------------------------------------------------------------------------

/// `GET /api/canvas/course-dates?course_id=...`
///
/// Uses the saved Canvas/JAccount cookie from Settings to list all videos for a
/// course, then groups them by date (derived from `course_begin_time`).
/// Returns a sorted date list suitable for a dropdown picker.
#[derive(Debug, Deserialize)]
struct CourseDatesQuery {
    course_id: String,
}

async fn api_canvas_course_dates(
    State(state): State<AppState>,
    Query(query): Query<CourseDatesQuery>,
) -> Json<serde_json::Value> {
    if query.course_id.is_empty() {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "course_id query parameter is required.",
        }));
    }

    let saved_secrets = state.secrets.load();
    let cookie = match saved_secrets.canvas_auth_cookie() {
        Some(c) => c,
        None => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": "No Canvas video credential saved. Go to Settings and save Canvas credentials first.",
            }));
        }
    };

    let mut client =
        crate::canvas_sjtu::CanvasSJTUVideoClient::new(query.course_id.clone(), cookie);

    let videos = match client.list_videos().await {
        Ok(v) => v,
        Err(e) => {
            return Json(serde_json::json!({
                "status": "failed",
                "error": format!("Failed to list videos: {}", e),
            }));
        }
    };

    // Group videos by date and build the date list.
    let dates = group_videos_by_date(&videos);

    Json(serde_json::json!({
        "status": "succeeded",
        "course_id": query.course_id,
        "total_videos": videos.len(),
        "dates": dates,
    }))
}

/// Group video infos by date, returning a sorted list of date entries.
///
/// Each entry contains the date, video count, and the first/last video title.
/// Dates are extracted from `course_begin_time` (first 10 chars = YYYY-MM-DD),
/// with a fallback to title-based date extraction.
pub fn group_videos_by_date(
    videos: &[crate::canvas_sjtu::CanvasVideoInfo],
) -> Vec<serde_json::Value> {
    let mut date_map: HashMap<String, Vec<&crate::canvas_sjtu::CanvasVideoInfo>> = HashMap::new();

    for v in videos {
        let date = extract_date_from_video(v);
        date_map.entry(date).or_default().push(v);
    }

    let mut dates: Vec<serde_json::Value> = date_map
        .into_iter()
        .map(|(date, vids)| {
            let mut sorted_vids = vids.clone();
            sorted_vids.sort_by(|a, b| {
                a.course_begin_time
                    .cmp(&b.course_begin_time)
                    .then_with(|| a.title.cmp(&b.title))
            });
            let first_title = sorted_vids.first().map(|v| v.title.clone());
            let last_title = sorted_vids.last().map(|v| v.title.clone());
            serde_json::json!({
                "date": date,
                "video_count": sorted_vids.len(),
                "first_title": first_title,
                "last_title": last_title,
            })
        })
        .collect();

    // Sort newest first.
    dates.sort_by(|a, b| {
        b["date"]
            .as_str()
            .unwrap_or("")
            .cmp(a["date"].as_str().unwrap_or(""))
    });

    dates
}

/// Extract a date string (YYYY-MM-DD) from a video info.
///
/// Uses `course_begin_time` first, falling back to date extraction from the
/// video title if the begin time is not parseable.
fn extract_date_from_video(v: &crate::canvas_sjtu::CanvasVideoInfo) -> String {
    // course_begin_time is typically "2023-09-18 08:00:00" or similar.
    if v.course_begin_time.len() >= 10 {
        let date_part = &v.course_begin_time[..10];
        // Validate it looks like YYYY-MM-DD.
        if date_part.len() == 10
            && date_part.chars().nth(4) == Some('-')
            && date_part.chars().nth(7) == Some('-')
            && date_part[..4].chars().all(|c| c.is_ascii_digit())
        {
            return date_part.to_string();
        }
    }

    // Fallback: try to extract a date from the video title.
    // Common patterns: "2023-09-18", "20230918", "09-18", etc.
    extract_date_from_title(&v.title)
}

/// Try to extract a YYYY-MM-DD date from a video title string.
fn extract_date_from_title(title: &str) -> String {
    use regex::Regex;
    // Try YYYY-MM-DD or YYYY/MM/DD.
    if let Ok(re) = Regex::new(r"(\d{4})[-/](\d{2})[-/](\d{2})") {
        if let Some(caps) = re.captures(title) {
            return format!("{}-{}-{}", &caps[1], &caps[2], &caps[3]);
        }
    }
    // Try YYYYMMDD.
    if let Ok(re) = Regex::new(r"(\d{4})(\d{2})(\d{2})") {
        if let Some(caps) = re.captures(title) {
            return format!("{}-{}-{}", &caps[1], &caps[2], &caps[3]);
        }
    }
    // Fallback: use a placeholder.
    "unknown-date".to_string()
}

// ---------------------------------------------------------------------------
// SPA serving (embedded via rust-embed, filesystem fallback)
// ---------------------------------------------------------------------------

/// Guess the MIME type from a file extension.
fn mime_from_path(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "eot" => "application/vnd.ms-fontobject",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serve an asset from the SPA build (embedded or filesystem).
async fn serve_spa_asset(Path(path): Path<String>) -> Response {
    let asset_path = path.trim_start_matches('/');
    let embedded_path = if asset_path.starts_with("assets/") {
        asset_path.to_string()
    } else {
        format!("assets/{asset_path}")
    };

    // 1. Try embedded assets.
    if let Some(asset) = WebAssets::get(&embedded_path) {
        let mime = mime_from_path(&embedded_path);
        return Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "public, max-age=3600")
            .body(Body::from(asset.data.into_owned()))
            .unwrap();
    }

    // 2. Fall back to filesystem.
    let fs_path = FsPath::new("web/dist").join(&embedded_path);
    if fs_path.exists() && fs_path.is_file() {
        match fs::read(&fs_path) {
            Ok(data) => {
                let mime = mime_from_path(&embedded_path);
                return Response::builder()
                    .header(header::CONTENT_TYPE, mime)
                    .header(header::CACHE_CONTROL, "public, max-age=3600")
                    .body(Body::from(data))
                    .unwrap();
            }
            Err(_) => {}
        }
    }

    (StatusCode::NOT_FOUND, "Not Found").into_response()
}

/// Serve the SPA index.html (embedded or filesystem), or a startup-error page
/// if nothing is available.
async fn serve_spa_index() -> Response {
    serve_index_html()
}

/// Catch-all fallback for SPA client-side routing.
///
/// If the path looks like an API/action route that wasn't matched, return 404.
/// Otherwise serve `index.html` so React Router can handle it.
async fn serve_spa_fallback(_uri: Uri, Path(path): Path<String>) -> Response {
    let path = path.trim_start_matches('/');

    // Known non-SPA paths that should 404 instead of serving index.html.
    if path.is_empty() {
        return serve_index_html();
    }

    // If the path has a file extension, try to serve it as a static asset.
    if path.contains('.') {
        return serve_spa_asset(Path(path.to_string())).await;
    }

    // Otherwise, serve index.html for client-side routing.
    serve_index_html()
}

/// Try embedded, then filesystem, then show a clear error page.
fn serve_index_html() -> Response {
    // 1. Try embedded.
    if let Some(asset) = WebAssets::get("index.html") {
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(asset.data.into_owned()))
            .unwrap();
    }

    // 2. Try filesystem.
    let fs_path = FsPath::new("web/dist/index.html");
    if fs_path.exists() {
        match fs::read_to_string(&fs_path) {
            Ok(content) => {
                return Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(Body::from(content))
                    .unwrap();
            }
            Err(_) => {}
        }
    }

    // 3. Neither is available 闁?show a clear error page.
    let version = env!("CARGO_PKG_VERSION");
    let body = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>lecture-distill 闁?GUI not built</title>
<style>
  body {{ font-family: system-ui, sans-serif; max-width: 640px; margin: 4rem auto; padding: 0 1.5rem; line-height: 1.6; color: #212529; }}
  h1 {{ font-size: 1.25rem; }}
  code {{ background: #f1f3f5; padding: 0.15em 0.4em; border-radius: 4px; font-size: 0.9em; }}
  pre {{ background: #f8f9fa; border: 1px solid #dee2e6; border-radius: 6px; padding: 1rem; overflow-x: auto; }}
  .badge {{ display: inline-block; padding: 0.2em 0.6em; border-radius: 9999px; font-size: 0.75rem; font-weight: 500; background: #fee2e2; color: #dc2626; }}
</style>
</head>
<body>
<h1>lecture-distill v{version}</h1>
<p><span class="badge">GUI Not Built</span></p>
<p>The React frontend has not been built or embedded.</p>
<p>To build the GUI:</p>
<pre>cd web/
npm install
npm run build
cd ..
cargo build --release</pre>
<p>After building, restart the server. The dashboard will then be served at <a href="/">/</a>.</p>
<p><small>If you are a developer, you can also run the Vite dev server separately:
<code>cd web/ && npm run dev</code> and use it as a standalone frontend during development.</small></p>
</body>
</html>"#,
        version = version,
    );
    Response::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests for helper functions
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::TranscriptSegment;
    use crate::canvas_sjtu::CanvasVideoInfo;

    fn make_video(begin_time: &str, title: &str, video_id: &str) -> CanvasVideoInfo {
        CanvasVideoInfo {
            video_id: video_id.to_string(),
            title: title.to_string(),
            duration: 3600,
            course_begin_time: begin_time.to_string(),
            course_end_time: format!("{} 10:00:00", &begin_time[..10.min(begin_time.len())]),
            teacher: "Test Teacher".to_string(),
            classroom: "Room 101".to_string(),
        }
    }

    fn requested_output(kind: &str, max_pages: Option<usize>) -> CreateProcessOutputBody {
        CreateProcessOutputBody {
            kind: kind.to_string(),
            max_pages,
        }
    }

    #[test]
    fn test_reference_digest_output_kind_parse_and_display() {
        assert_eq!(
            parse_process_output_kind("reference_digest"),
            Some(ProcessOutputKind::ReferenceDigest)
        );
        assert_eq!(
            ProcessOutputKind::ReferenceDigest.to_string(),
            "reference_digest"
        );
        assert_eq!(
            process_output_title(&ProcessOutputKind::ReferenceDigest),
            "Reference Digest"
        );
    }

    #[test]
    fn test_reference_digest_dependency_expansion_for_cheating_sheet() {
        let outputs = vec![requested_output("cheating_sheet", Some(3))];
        let expanded = expand_output_kinds(&outputs).expect("expand outputs");

        assert_eq!(
            expanded,
            vec![
                (ProcessOutputKind::ReferenceDigest, 0),
                (ProcessOutputKind::CheatingSheet, 3)
            ]
        );
        assert!(!expanded
            .iter()
            .any(|(kind, _)| *kind == ProcessOutputKind::NotePatch));
    }

    #[test]
    fn test_reference_digest_dependency_expansion_keeps_explicit_note_patch_parallel() {
        let outputs = vec![
            requested_output("note_patch", None),
            requested_output("cheating_sheet", Some(2)),
        ];
        let expanded = expand_output_kinds(&outputs).expect("expand outputs");

        assert_eq!(
            expanded,
            vec![
                (ProcessOutputKind::NotePatch, 2),
                (ProcessOutputKind::ReferenceDigest, 0),
                (ProcessOutputKind::CheatingSheet, 2)
            ]
        );
    }

    #[test]
    fn test_reference_digest_prompt_uses_note_as_structure_reference() {
        let note = Some("# Important\n\nExisting Note content".to_string());
        let prompt = build_ref_digest_user_prompt(&note, "Transcript context", None);

        assert!(prompt.contains("structure / priority / style reference only"));
        assert!(prompt.contains("not a length constraint"));
        assert!(prompt.contains("Transcript context"));
    }

    #[test]
    fn test_extract_date_from_begin_time() {
        let v = make_video("2024-09-18 08:00:00", "Lecture 1", "v1");
        assert_eq!(extract_date_from_video(&v), "2024-09-18");
    }

    #[test]
    fn test_extract_date_from_title_fallback() {
        let v = CanvasVideoInfo {
            video_id: "v1".to_string(),
            title: "2024-03-15 Course Introduction".to_string(),
            duration: 3600,
            course_begin_time: "".to_string(),
            course_end_time: "".to_string(),
            teacher: "Teacher".to_string(),
            classroom: "Room".to_string(),
        };
        assert_eq!(extract_date_from_video(&v), "2024-03-15");
    }

    #[test]
    fn test_extract_date_fallback_to_unknown() {
        let v = CanvasVideoInfo {
            video_id: "v1".to_string(),
            title: "No date here".to_string(),
            duration: 3600,
            course_begin_time: "invalid".to_string(),
            course_end_time: "".to_string(),
            teacher: "Teacher".to_string(),
            classroom: "Room".to_string(),
        };
        assert_eq!(extract_date_from_video(&v), "unknown-date");
    }

    #[test]
    fn test_group_videos_by_date() {
        let videos = vec![
            make_video("2024-09-18 08:00:00", "Morning Lecture", "v1"),
            make_video("2024-09-18 10:00:00", "Afternoon Lecture", "v2"),
            make_video("2024-09-19 08:00:00", "Next Day", "v3"),
        ];

        let dates = group_videos_by_date(&videos);
        assert_eq!(dates.len(), 2);

        // Newest first.
        assert_eq!(dates[0]["date"].as_str().unwrap(), "2024-09-19");
        assert_eq!(dates[0]["video_count"].as_u64().unwrap(), 1);

        assert_eq!(dates[1]["date"].as_str().unwrap(), "2024-09-18");
        assert_eq!(dates[1]["video_count"].as_u64().unwrap(), 2);
    }

    #[test]
    fn test_group_videos_empty() {
        let dates = group_videos_by_date(&[]);
        assert!(dates.is_empty());
    }

    #[test]
    fn test_group_videos_single_date() {
        let videos = vec![
            make_video("2024-09-18 08:00:00", "First", "v1"),
            make_video("2024-09-18 09:00:00", "Last", "v2"),
        ];

        let dates = group_videos_by_date(&videos);
        assert_eq!(dates.len(), 1);
        assert_eq!(dates[0]["date"].as_str().unwrap(), "2024-09-18");
        assert_eq!(dates[0]["video_count"].as_u64().unwrap(), 2);
        assert_eq!(dates[0]["first_title"].as_str().unwrap(), "First");
        assert_eq!(dates[0]["last_title"].as_str().unwrap(), "Last");
    }

    // -----------------------------------------------------------------------
    // Compact transcript tests (also testing llm::compact_transcript_for_llm)
    // -----------------------------------------------------------------------

    #[test]
    fn test_compact_transcript_basic() {
        let input =
            "# CS101 - 2024-09-18\n\n## 08:00 Lecture 1 - v1\n\n[00:00] Hello\n[00:05] World\n";
        let result = crate::llm::compact_transcript_for_llm(input, 10000);
        assert!(result.contains("# CS101"));
        assert!(result.contains("## 08:00 Lecture 1 - v1"));
        assert!(result.contains("[00:00] Hello [00:05] World"));
    }

    fn make_transcript(segments: Vec<TranscriptSegment>) -> TranscriptArtifact {
        TranscriptArtifact {
            video_id: "video-1".to_string(),
            video_title: "Lecture".to_string(),
            course_id: "course-1".to_string(),
            language: "zh".to_string(),
            segments,
            fetched_at: "2026-01-01T00:00:00Z".to_string(),
            recorded_at: None,
            source_url: None,
        }
    }

    #[test]
    fn test_transcript_markdown_uses_ppt_boundaries_without_splitting_sentence() {
        let artifact = make_transcript(vec![
            TranscriptSegment {
                index: 1,
                start_time: 1.0,
                end_time: 3.0,
                text: "First half ".to_string(),
            },
            TranscriptSegment {
                index: 2,
                start_time: 11.0,
                end_time: 12.0,
                text: "continues.".to_string(),
            },
            TranscriptSegment {
                index: 3,
                start_time: 13.0,
                end_time: 14.0,
                text: " Next sentence.".to_string(),
            },
        ]);
        let ppt_slices = vec![
            CanvasPptSlice {
                create_sec: 0.0,
                ppt_img_url: None,
                ocr_words: vec!["Title".to_string()],
            },
            CanvasPptSlice {
                create_sec: 10.0,
                ppt_img_url: None,
                ocr_words: Vec::new(),
            },
        ];

        let markdown = transcript_markdown_for_video(&artifact, &ppt_slices);

        assert!(markdown.contains("### Slide 1 [00:00-00:10]"));
        assert!(markdown.contains("_Slide OCR:_ Title"));
        assert!(markdown.contains("[00:01] First half continues."));
        assert!(markdown.contains("### Slide 2 [00:10-00:14]"));
        assert!(markdown.contains("[00:13] Next sentence."));
    }

    #[test]
    fn test_transcript_markdown_long_ppt_range_falls_back_to_sentence_chunks() {
        let artifact = make_transcript(vec![TranscriptSegment {
            index: 1,
            start_time: 2.0,
            end_time: 5.0,
            text: "One. Two. Three. Four. Five.".to_string(),
        }]);
        let ppt_slices = vec![
            CanvasPptSlice {
                create_sec: 0.0,
                ppt_img_url: None,
                ocr_words: Vec::new(),
            },
            CanvasPptSlice {
                create_sec: 400.0,
                ppt_img_url: None,
                ocr_words: Vec::new(),
            },
        ];

        let markdown = transcript_markdown_for_video(&artifact, &ppt_slices);

        assert!(markdown.contains("### Slide 1 [00:00-00:05]"));
        assert!(markdown.contains("[00:02] One.Two.Three.Four."));
        assert!(markdown.contains("[00:02] Five."));
    }

    // -----------------------------------------------------------------------
    // PlannedSection outline JSON parse / convert
    // -----------------------------------------------------------------------

    #[test]
    fn test_planned_section_deserialize_success() {
        let json = r#"{
            "title": "Optimization Basics",
            "purpose": "Introduce gradient descent",
            "date_hints": ["2024-09-18", "2024-09-19"],
            "video_hints": ["v1", "v2"],
            "query_terms": ["gradient", "convex"],
            "must_include": ["gradient descent formula", "convexity definition"]
        }"#;
        let section: PlannedSection = serde_json::from_str(json).unwrap();
        assert_eq!(section.title, "Optimization Basics");
        assert_eq!(section.purpose, "Introduce gradient descent");
        assert_eq!(section.date_hints.len(), 2);
        assert_eq!(section.video_hints.len(), 2);
        assert_eq!(section.query_terms.len(), 2);
        assert_eq!(section.must_include.len(), 2);
    }

    #[test]
    fn test_planned_section_deserialize_minimal() {
        // A section with only required fields.
        let json = r#"{"title": "Single Topic", "purpose": "Cover basics"}"#;
        let section: PlannedSection = serde_json::from_str(json).unwrap();
        assert_eq!(section.title, "Single Topic");
        assert!(section.date_hints.is_empty());
        assert!(section.must_include.is_empty());
    }

    #[test]
    fn test_planned_section_deserialize_missing_title_is_caught() {
        // Missing the required "title" field should fail.
        let json = r#"{"purpose": "No title provided"}"#;
        let result: Result<PlannedSection, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_outline_json_invalid_missing_sections() {
        // Simulate what generate_course_note_outline would parse:
        // a valid JSON object but with no "sections" key.
        let json = serde_json::json!({"other": []});
        let sections_arr = json.get("sections").and_then(|v| v.as_array());
        assert!(sections_arr.is_none());
    }

    // -----------------------------------------------------------------------
    // Section retrieval: date-hint preference and index limit
    // -----------------------------------------------------------------------

    fn make_test_index(
        date: &str,
        title: &str,
        summary: &str,
        keywords: &[&str],
    ) -> CourseDateIndex {
        CourseDateIndex {
            date: date.to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            concepts: Vec::new(),
            timestamp_ranges: Vec::new(),
            char_count: 100,
            token_count: 20,
            source_path: "test.md".to_string(),
            status: "ready".to_string(),
        }
    }

    #[test]
    fn test_retrieval_prefers_date_hints_over_bm25() {
        let indexes = vec![
            make_test_index("2024-09-10", "Unrelated", "nothing useful", &[]),
            make_test_index(
                "2024-09-18",
                "Optimization",
                "gradient descent and convexity",
                &["gradient", "convex"],
            ),
            make_test_index(
                "2024-09-19",
                "More Optimization",
                "stochastic gradient descent",
                &["sgd"],
            ),
        ];

        let section = PlannedSection {
            title: "Optimization".into(),
            purpose: "Learn gradient descent".into(),
            date_hints: vec!["2024-09-18".into()],
            video_hints: vec![],
            query_terms: vec![],
            must_include: vec![],
        };

        let (context, trace) = retrieve_course_context_for_section(&section, &indexes);
        // The date-hint match should prefer the 2024-09-18 index.
        assert!(!context.is_empty(), "context should not be empty");
        assert!(
            trace.matches.iter().any(|m| m.date == "2024-09-18"),
            "should include the date-hint matched index"
        );
        // Should NOT include all indexes — only 4 max (here: just the matched one).
        assert!(
            trace.matches.len() <= 4,
            "should not include more than 4 indexes"
        );
    }

    #[test]
    fn test_retrieval_falls_back_to_bm25_when_no_date_hints() {
        let indexes = vec![
            make_test_index(
                "2024-09-18",
                "Intro",
                "syllabus and logistics",
                &["syllabus"],
            ),
            make_test_index(
                "2024-09-19",
                "Optimization",
                "gradient descent algorithm",
                &["gradient", "descent"],
            ),
        ];

        let section = PlannedSection {
            title: "Gradient Descent".into(),
            purpose: "Learn optimization".into(),
            date_hints: vec![],
            video_hints: vec![],
            query_terms: vec!["gradient".into(), "descent".into()],
            must_include: vec![],
        };

        let (context, trace) = retrieve_course_context_for_section(&section, &indexes);
        // Should fall back to BM25 and find the Optimization index.
        assert!(
            context.contains("gradient descent"),
            "context should contain relevant transcript content"
        );
        assert!(
            trace.matches.iter().any(|m| m.date == "2024-09-19"),
            "should find the relevant index via BM25 fallback"
        );
    }

    #[test]
    fn test_retrieval_does_not_include_all_indexes() {
        let mut indexes = Vec::new();
        for day in 1..=10 {
            indexes.push(make_test_index(
                &format!("2024-09-{:02}", day),
                &format!("Lecture {}", day),
                "various topics",
                &["topic"],
            ));
        }

        let section = PlannedSection {
            title: "Comprehensive Topic".into(),
            purpose: "Everything".into(),
            date_hints: vec![],
            video_hints: vec![],
            query_terms: vec!["topic".into()],
            must_include: vec![],
        };

        let (_context, trace) = retrieve_course_context_for_section(&section, &indexes);
        assert!(
            trace.matches.len() <= 4,
            "should limit to at most 4 indexes, got {}",
            trace.matches.len()
        );
        assert!(
            trace.matches.len() < indexes.len(),
            "should not include all 10 indexes"
        );
    }

    // -----------------------------------------------------------------------
    // Merge input assembly preserves all section strings
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_sections_input_has_all_sections() {
        let section_texts: Vec<String> = vec![
            "## Intro\n\nContent A".into(),
            "## Methods\n\nContent B".into(),
            "## Results\n\nContent C".into(),
        ];

        // Simulate the merge assembly logic (without the LLM call).
        let combined = section_texts
            .iter()
            .enumerate()
            .map(|(i, s)| format!("<!-- section {} -->\n{}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n\n");

        // Every section body must appear in the combined text.
        assert!(combined.contains("Content A"));
        assert!(combined.contains("Content B"));
        assert!(combined.contains("Content C"));
        // The section markers must appear.
        assert!(combined.contains("<!-- section 1 -->"));
        assert!(combined.contains("<!-- section 2 -->"));
        assert!(combined.contains("<!-- section 3 -->"));
        // Number of sections preserved.
        assert_eq!(combined.matches("<!-- section ").count(), 3);
    }

    // -----------------------------------------------------------------------
    // Outline context builder
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_outline_context_no_transcript_source() {
        let indexes = vec![CourseDateIndex {
            date: "2024-09-18".into(),
            title: "Optimization".into(),
            summary: "Gradient descent and convexity".into(),
            keywords: vec!["gradient".into(), "convex".into()],
            concepts: vec!["gradient descent formula".into()],
            timestamp_ranges: vec![TimestampRange {
                video_id: "v1".into(),
                label: "Slide 1".into(),
                start: 0.0,
                end: 30.0,
                text_preview: "Today we discuss gradient descent which is a first-order iterative optimization algorithm...".into(),
            }],
            char_count: 5000,
            token_count: 1000,
            source_path: "/fake/path/transcript.md".into(),
            status: "ready".into(),
        }];

        let ctx = build_outline_context(&indexes);
        // Must contain metadata (date, title, summary, keywords, concepts).
        assert!(ctx.contains("2024-09-18"));
        assert!(ctx.contains("Optimization"));
        assert!(ctx.contains("gradient"));
        assert!(ctx.contains("convex"));
        // Must NOT contain the source_path or transcript excerpt strings.
        assert!(!ctx.contains("/fake/path/transcript.md"));
        assert!(!ctx.contains("Transcript excerpt"));
        // text_preview from timestamp_ranges is metadata and is allowed;
        // truncation to 120 chars is exercised by the timestamp-limit test below.
    }

    #[test]
    fn test_build_outline_context_timestamp_limit() {
        let mut ranges = Vec::new();
        for i in 0..10 {
            ranges.push(TimestampRange {
                video_id: format!("v{}", i),
                label: format!("Slide {}", i),
                start: i as f64 * 10.0,
                end: i as f64 * 10.0 + 5.0,
                text_preview: format!("Content for slide {}", i),
            });
        }
        let indexes = vec![CourseDateIndex {
            date: "2024-09-18".into(),
            title: "Test".into(),
            summary: "Test".into(),
            keywords: vec![],
            concepts: vec![],
            timestamp_ranges: ranges,
            char_count: 100,
            token_count: 20,
            source_path: "fake.md".into(),
            status: "ready".into(),
        }];

        let ctx = build_outline_context(&indexes);
        // Only 3 ranges should appear (take(3)). Slide 3 should not appear.
        for i in 0..3 {
            assert!(
                ctx.contains(&format!("Content for slide {}", i)),
                "should include slide {}",
                i
            );
        }
        assert!(
            !ctx.contains("Content for slide 3"),
            "should limit to 3 ranges"
        );
        assert!(
            !ctx.contains("Content for slide 9"),
            "should limit to 3 ranges"
        );
    }

    // -----------------------------------------------------------------------
    // parse_outline_sections
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_outline_sections_success() {
        let text = r#"{"sections":[{"title":"Optimization Basics","purpose":"Introduce gradient descent","date_hints":["2024-09-18"],"video_hints":[],"query_terms":["gradient"],"must_include":["GD formula"]}]}"#;
        let result = parse_outline_sections(text, Some("stop"));
        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let sections = result.unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "Optimization Basics");
        assert_eq!(sections[0].purpose, "Introduce gradient descent");
    }

    #[test]
    fn test_parse_outline_sections_code_fenced() {
        let text = "```json\n{\"sections\":[{\"title\":\"Intro\",\"purpose\":\"Basics\",\"date_hints\":[],\"video_hints\":[],\"query_terms\":[],\"must_include\":[]}]}\n```";
        let result = parse_outline_sections(text, None);
        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
        let sections = result.unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "Intro");
    }

    #[test]
    fn test_parse_outline_sections_invalid_json_diagnostic() {
        let text = "not valid json at all {";
        let result = parse_outline_sections(text, None);
        assert!(result.is_err());
        let diag = result.unwrap_err();
        // Diagnostic must include the parse error text.
        assert!(diag.contains("JSON parse error"), "diagnostic: {}", diag);
        // Must include response length.
        assert!(diag.contains("response_len="), "diagnostic: {}", diag);
        // Must include head/tail snippets.
        assert!(diag.contains("head="), "diagnostic: {}", diag);
        assert!(diag.contains("tail="), "diagnostic: {}", diag);
    }

    #[test]
    fn test_parse_outline_sections_missing_sections_key() {
        let text = r#"{"other_key": []}"#;
        let result = parse_outline_sections(text, None);
        assert!(result.is_err());
        let diag = result.unwrap_err();
        assert!(diag.contains("missing 'sections'"), "diagnostic: {}", diag);
    }

    #[test]
    fn test_parse_outline_sections_truncated_by_length() {
        let text = r#"{"sections":[{"title":"Test","purpose":"Test"}]}"#;
        let result = parse_outline_sections(text, Some("length"));
        // This should succeed because the JSON is valid.
        assert!(result.is_ok());
        // Test the diagnostic path for truncated JSON specifically.
        let truncated = r#"{"sections":[{"title":"Tes"#;
        let result2 = parse_outline_sections(truncated, Some("length"));
        assert!(result2.is_err());
        let diag = result2.unwrap_err();
        assert!(diag.contains("truncated_by_length"), "diagnostic: {}", diag);
    }

    #[test]
    fn test_parse_outline_sections_empty_sections_is_error() {
        let text = r#"{"sections":[]}"#;
        let result = parse_outline_sections(text, None);
        assert!(result.is_err());
        let diag = result.unwrap_err();
        assert!(diag.contains("no valid sections"), "diagnostic: {}", diag);
    }

    // -----------------------------------------------------------------------
    // Fallback outline
    // -----------------------------------------------------------------------

    fn make_fake_index(
        date: &str,
        title: &str,
        keywords: &[&str],
        concepts: &[&str],
    ) -> CourseDateIndex {
        CourseDateIndex {
            date: date.to_string(),
            title: title.to_string(),
            summary: format!("Summary for {}", date),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            concepts: concepts.iter().map(|s| s.to_string()).collect(),
            timestamp_ranges: Vec::new(),
            char_count: 100,
            token_count: 20,
            // Use a fake source path that won't be read by the fallback.
            source_path: "/nonexistent/transcript.md".into(),
            status: "ready".into(),
        }
    }

    #[test]
    fn test_fallback_outline_produces_4_to_24_sections_with_enough_indexes() {
        let mut indexes = Vec::new();
        for day in 1..=30 {
            indexes.push(make_fake_index(
                &format!("2024-09-{:02}", day.min(30)),
                &format!("Lecture {}", day),
                &[&format!("kw{}", day)],
                &[&format!("concept{}", day)],
            ));
        }

        let sections = generate_fallback_outline(&indexes).unwrap();
        assert!(
            sections.len() >= 4 && sections.len() <= 24,
            "expected 4-24 sections, got {}",
            sections.len()
        );
        // Every section should have a non-empty title and date_hints.
        for s in &sections {
            assert!(!s.title.is_empty(), "section title must not be empty");
            assert!(!s.date_hints.is_empty(), "date_hints must not be empty");
        }
    }

    #[test]
    fn test_fallback_outline_small_index_set() {
        // Only 2 indexes — should produce at least 1 section.
        let indexes = vec![
            make_fake_index("2024-09-18", "Lecture 1", &["intro"], &["basics"]),
            make_fake_index("2024-09-19", "Lecture 2", &["advanced"], &["details"]),
        ];

        let sections = generate_fallback_outline(&indexes).unwrap();
        assert!(
            sections.len() >= 1,
            "expected at least 1 section, got {}",
            sections.len()
        );
        assert!(
            sections.len() <= 2,
            "expected at most 2 sections for 2 indexes"
        );
        // All dates should be covered in date_hints across all sections.
        let all_dates: std::collections::HashSet<String> = sections
            .iter()
            .flat_map(|s| s.date_hints.iter().cloned())
            .collect();
        assert!(all_dates.contains("2024-09-18"));
        assert!(all_dates.contains("2024-09-19"));
    }

    #[test]
    fn test_fallback_outline_does_not_read_transcript_files() {
        // Indexes have non-existent source_paths — fallback must still work
        // because it only uses metadata.
        let indexes = vec![
            make_fake_index("2024-09-18", "Optimization", &["gradient"], &["GD"]),
            make_fake_index("2024-09-19", "SGD", &["stochastic"], &["SGD"]),
            make_fake_index("2024-09-20", "Convexity", &["convex"], &["convex set"]),
            make_fake_index("2024-09-21", "Lagrange", &["dual"], &["KKT"]),
            make_fake_index("2024-09-22", "SVM", &["margin"], &["hinge loss"]),
        ];

        let sections = generate_fallback_outline(&indexes).unwrap();
        assert!(!sections.is_empty());
        // The fallback must not call fs::read_to_string on source_path.
        // If it did with /nonexistent/ path, the test would still work
        // (read returns empty), but the point is the function signature
        // only takes indexes, not source files.
    }

    #[test]
    fn test_fallback_outline_empty_indexes_fails() {
        let indexes: Vec<CourseDateIndex> = Vec::new();
        let result = generate_fallback_outline(&indexes);
        assert!(result.is_err());
    }

    #[test]
    fn test_fallback_outline_single_index() {
        let indexes = vec![make_fake_index(
            "2024-09-18",
            "Solo Lecture",
            &["solo"],
            &["concept"],
        )];
        let sections = generate_fallback_outline(&indexes).unwrap();
        assert_eq!(sections.len(), 1);
        assert!(!sections[0].title.is_empty());
        assert_eq!(sections[0].date_hints, vec!["2024-09-18"]);
    }

    // -----------------------------------------------------------------------
    // Section retrieval: context budget and index limit
    // -----------------------------------------------------------------------

    #[test]
    fn test_retrieval_respects_context_budget() {
        let indexes = vec![make_test_index(
            "2024-09-18",
            "Long Lecture",
            "A lecture with a long summary that goes on and on about many topics",
            &["long"],
        )];

        let section = PlannedSection {
            title: "Long Topic".into(),
            purpose: "Test budget".into(),
            date_hints: vec!["2024-09-18".into()],
            video_hints: vec![],
            query_terms: vec![],
            must_include: vec![],
        };

        let (context, _trace) = retrieve_course_context_for_section(&section, &indexes);
        // Context should not exceed 24000 chars.
        assert!(
            context.chars().count() <= 24000,
            "context budget exceeded: {}",
            context.chars().count()
        );
    }

    // -----------------------------------------------------------------------
    // Cheating Sheet capacity estimation
    // -----------------------------------------------------------------------

    #[test]
    fn test_budget_estimation_one_page() {
        let budget = estimate_cheating_sheet_budget(1);
        assert_eq!(budget.target_chars, 11000);
        assert_eq!(budget.soft_max_chars, 15000);
        assert_eq!(budget.min_acceptable_chars, 8250); // 11000 * 3/4
    }

    #[test]
    fn test_budget_estimation_two_pages() {
        let budget = estimate_cheating_sheet_budget(2);
        assert_eq!(budget.target_chars, 22000);
        assert_eq!(budget.soft_max_chars, 30000);
        assert_eq!(budget.min_acceptable_chars, 16500);
    }

    #[test]
    fn test_budget_estimation_three_pages() {
        let budget = estimate_cheating_sheet_budget(3);
        assert_eq!(budget.target_chars, 33000);
        assert_eq!(budget.soft_max_chars, 45000);
        assert_eq!(budget.min_acceptable_chars, 24750);
    }

    #[test]
    fn test_budget_clamped_at_20() {
        let budget = estimate_cheating_sheet_budget(100);
        assert_eq!(budget.target_chars, 20 * 11000);
    }

    #[test]
    fn test_budget_minimum_one() {
        let budget = estimate_cheating_sheet_budget(0);
        assert_eq!(budget.target_chars, 11000);
    }

    // -----------------------------------------------------------------------
    // Markdown section inventory
    // -----------------------------------------------------------------------

    #[test]
    fn test_section_inventory_extracts_headings_in_order() {
        let md = "\
# Course Title
intro text here

## Key Concepts
- concept one
- concept two

### Sub Concept
more details here

## Another Section
final content
";
        let (sections, inventory) = build_section_inventory(md);
        assert_eq!(
            sections.len(),
            4,
            "expected 4 sections, got {}",
            sections.len()
        );
        assert_eq!(sections[0].heading, "Course Title");
        assert_eq!(sections[0].level, 1);
        assert_eq!(sections[1].heading, "Key Concepts");
        assert_eq!(sections[1].level, 2);
        assert_eq!(sections[2].heading, "Sub Concept");
        assert_eq!(sections[2].level, 3);
        assert_eq!(sections[3].heading, "Another Section");
        assert_eq!(sections[3].level, 2);

        // Inventory string must mention all headings.
        assert!(inventory.contains("Course Title"));
        assert!(inventory.contains("Key Concepts"));
        assert!(inventory.contains("Sub Concept"));
        assert!(inventory.contains("Another Section"));
    }

    #[test]
    fn test_section_inventory_body_previews() {
        let md = "\
# Title

## Section A
This is the body of section A with enough content to make a reasonable preview for the inventory.

## Section B
Section B body is shorter.
";
        let (sections, _inventory) = build_section_inventory(md);
        assert_eq!(sections.len(), 3);

        // Section A should have a body preview.
        assert!(!sections[1].body_preview.is_empty());
        assert!(sections[1].body_preview.contains("This is the body"));

        // Section B should have a body preview.
        assert!(!sections[2].body_preview.is_empty());
        assert!(sections[2].body_preview.contains("Section B body"));
    }

    #[test]
    fn test_section_inventory_empty_markdown() {
        let (sections, inventory) = build_section_inventory("");
        assert!(sections.is_empty());
        assert!(inventory.is_empty());
    }

    #[test]
    fn test_section_inventory_no_headings() {
        let md = "Just some text\nwithout any headings.\nMore text here.";
        let (sections, _inventory) = build_section_inventory(md);
        assert!(sections.is_empty());
    }

    // -----------------------------------------------------------------------
    // Expansion decision logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_underfilled_triggers_expansion_when_pages_below_max() {
        // 12000 generated, min is 8250 (above), but only 1 page of 2.
        let result = should_attempt_expansion(12000, 8250, 1, 2, false);
        assert!(result, "should expand when page count < max pages");
    }

    #[test]
    fn test_no_expansion_when_char_count_low_but_pages_full() {
        // 3000 generated (well below min 8250), but page count already at max.
        // Expansion should NOT trigger — page count is the sole criterion.
        let result = should_attempt_expansion(3000, 8250, 2, 2, false);
        assert!(
            !result,
            "should NOT expand when pages are full, even if char count is low"
        );
    }

    #[test]
    fn test_no_expansion_when_source_too_short() {
        // 5000 generated, page count 1 of 2, but source is too short.
        let result = should_attempt_expansion(5000, 8250, 1, 2, true);
        assert!(!result, "should NOT expand when source is too short");
    }

    #[test]
    fn test_no_expansion_when_filled() {
        // 20000 generated, min is 8250, 2 pages of 2, source is long.
        let result = should_attempt_expansion(20000, 8250, 2, 2, false);
        assert!(!result, "should NOT expand when already filled");
    }

    // -----------------------------------------------------------------------
    // Expansion prompt builder
    // -----------------------------------------------------------------------

    #[test]
    fn test_expansion_prompt_contains_current_cheat() {
        let cheat = "# Title\n\n- bullet one\n- bullet two\n";
        let inventory = "# Title\n  body: some preview\n";
        let excerpt = "original note excerpt content";
        let target_add = 3000;

        let (_system, user) = build_expansion_prompt(cheat, inventory, excerpt, target_add);

        // Should contain the existing cheat markdown.
        assert!(user.contains("bullet one"));
        assert!(user.contains("bullet two"));
    }

    #[test]
    fn test_expansion_prompt_contains_section_inventory() {
        let cheat = "# Cheat\ncontent\n";
        let inventory = "## Key Concepts\n  body: gradient descent definition\n";
        let excerpt = "original notes";
        let target_add = 3000;

        let (_system, user) = build_expansion_prompt(cheat, inventory, excerpt, target_add);

        assert!(user.contains("Key Concepts"));
        assert!(user.contains("gradient descent"));
    }

    #[test]
    fn test_expansion_prompt_contains_target_add_chars() {
        let cheat = "minimal";
        let inventory = "inventory";
        let excerpt = "notes";
        let target_add = 5000;

        let (_system, user) = build_expansion_prompt(cheat, inventory, excerpt, target_add);

        assert!(user.contains("5000"));
    }

    #[test]
    fn test_expansion_prompt_contains_latex_safety_constraints() {
        let cheat = "content";
        let inventory = "inv";
        let excerpt = "notes";
        let target_add = 2000;

        let (system, _user) = build_expansion_prompt(cheat, inventory, excerpt, target_add);

        // System prompt must prohibit raw LaTeX commands.
        assert!(
            system.contains("\\begin"),
            "must mention \\begin prohibition"
        );
        assert!(
            !system.contains("\\begin{"),
            "should not contain an actual begin block"
        );

        // User prompt must mention LaTeX safety.
        // The system prompt carries the constraints; user prompt focuses on content.
        // Verify system has the key constraints.
        assert!(system.contains("no HTML"));
        assert!(system.contains("Markdown tables"));
        assert!(system.contains("unbalanced braces"));
    }

    // -----------------------------------------------------------------------
    // Metadata builder
    // -----------------------------------------------------------------------

    #[test]
    fn test_metadata_includes_target_generated_harness_expansion_fields() {
        let meta = build_cheatsheet_metadata(
            4,
            4,
            "complete",
            2,
            2,
            0,
            "default_cheatsheet.tex",
            "/path/to/cheatsheet.md",
            "reference_digest_id",
            22000,
            19500,
            1,
            false,
            2,
            None,
        );

        assert_eq!(meta["target_chars"], serde_json::json!(22000));
        assert_eq!(meta["generated_chars"], serde_json::json!(19500));
        assert_eq!(meta["harness_attempts"], serde_json::json!(1));
        assert_eq!(meta["expansion_used"], serde_json::json!(false));
        assert_eq!(meta["final_page_count"], serde_json::json!(2));
    }

    #[test]
    fn test_metadata_includes_underfilled_reason_when_present() {
        let meta = build_cheatsheet_metadata(
            4,
            4,
            "complete",
            2,
            1,
            0,
            "default_cheatsheet.tex",
            "/path/to/cheatsheet.md",
            "note_patch_id",
            22000,
            5000,
            1,
            false,
            1,
            Some("source_too_short"),
        );

        assert_eq!(
            meta["underfilled_reason"],
            serde_json::json!("source_too_short")
        );
        assert_eq!(meta["target_chars"], serde_json::json!(22000));
        assert_eq!(meta["generated_chars"], serde_json::json!(5000));
    }

    #[test]
    fn test_metadata_omits_underfilled_reason_when_none() {
        let meta = build_cheatsheet_metadata(
            4,
            4,
            "complete",
            2,
            2,
            0,
            "default_cheatsheet.tex",
            "/path/to/cheatsheet.md",
            "note_patch_id",
            22000,
            23000,
            1,
            false,
            2,
            None,
        );

        assert!(meta.get("underfilled_reason").is_none());
    }

    #[test]
    fn test_metadata_preserves_existing_keys() {
        let meta = build_cheatsheet_metadata(
            4,
            4,
            "complete",
            3,
            3,
            1,
            "custom.tex",
            "/p/cheatsheet.md",
            "rd_id",
            33000,
            31000,
            2,
            true,
            3,
            None,
        );

        assert_eq!(meta["progress_current"], serde_json::json!(4));
        assert_eq!(meta["progress_total"], serde_json::json!(4));
        assert_eq!(meta["progress_label"], serde_json::json!("complete"));
        assert_eq!(meta["max_pages"], serde_json::json!(3));
        assert_eq!(meta["page_count"], serde_json::json!(3));
        assert_eq!(meta["compression_attempts"], serde_json::json!(1));
        assert_eq!(meta["template_used"], serde_json::json!("custom.tex"));
        assert_eq!(meta["markdown_path"], serde_json::json!("/p/cheatsheet.md"));
        assert_eq!(
            meta["reference_digest_output_id"],
            serde_json::json!("rd_id")
        );
    }

    #[test]
    fn test_expansion_used_metadata_true() {
        let meta = build_cheatsheet_metadata(
            4,
            4,
            "complete",
            2,
            2,
            0,
            "default_cheatsheet.tex",
            "/path/to/cheatsheet.md",
            "reference_digest_id",
            22000,
            22000,
            2,
            true,
            2,
            None,
        );

        assert_eq!(meta["expansion_used"], serde_json::json!(true));
        assert_eq!(meta["harness_attempts"], serde_json::json!(2));
        assert_eq!(meta["final_page_count"], serde_json::json!(2));
    }
}
