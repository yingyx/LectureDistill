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

use crate::diff;
use crate::latex;
use crate::llm::{self, compact_transcript_for_llm};
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
        .route("/api/processes/{id}/outputs", post(api_add_process_output))
        .route(
            "/api/processes/{id}/outputs/{output_id}",
            get(api_get_process_output).delete(api_delete_process_output),
        )
        .route(
            "/api/processes/{id}/outputs/{output_id}/revise",
            post(api_revise_process_output),
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
            match crate::llm::chat_text(system, &user, 0.3, 3072).await {
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
            let context = truncate_for_llm(&source_text, 20000);
            let system = "You answer questions about lecture transcript or note content. \
                          Be concise, cite specific sections and timestamps when relevant, \
                          and do not invent facts. Answer in the same language as the question.";
            let user = format!(
                "Source: {}\nQuestion: {}\nContent:\n{}",
                source.title, body.question, context
            );

            match crate::llm::chat_text(system, &user, 0.3, 2048).await {
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
                match crate::llm::chat_text(system, &user, 0.3, 3072).await {
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

        match llm::chat_text_stream(system, &user, 0.3, 4096).await {
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
/// Body: `{ title?: string, source_ids: string[], outputs: [{ kind: "note_patch" | "cheating_sheet", max_pages?: 2 }] }`
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
        "cheating_sheet" => Some(ProcessOutputKind::CheatingSheet),
        _ => None,
    }
}

fn process_output_title(kind: &ProcessOutputKind) -> &'static str {
    match kind {
        ProcessOutputKind::NotePatch => "Note Patch",
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
    let mut has_cheating_sheet = false;
    let mut cheating_sheet_pages = 2usize;

    for out in requested {
        match parse_process_output_kind(&out.kind) {
            Some(ProcessOutputKind::NotePatch) => has_note_patch = true,
            Some(ProcessOutputKind::CheatingSheet) => {
                has_cheating_sheet = true;
                cheating_sheet_pages = out.max_pages.unwrap_or(2).clamp(1, 20);
            }
            None => {
                return Err(format!(
                    "Unsupported output kind: {}. Supported kinds: note_patch, cheating_sheet.",
                    out.kind
                ));
            }
        }
    }

    let mut kinds = Vec::new();
    if has_note_patch || has_cheating_sheet {
        kinds.push((ProcessOutputKind::NotePatch, 2));
    }
    if has_cheating_sheet {
        kinds.push((ProcessOutputKind::CheatingSheet, cheating_sheet_pages));
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

async fn run_process_outputs(
    process_id: &str,
    source_ids: &[String],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
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
    let system_prompt = if base_note_content.is_some() {
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

    let user_prompt = format!(
        "Generate a note from the following source materials:\n\n{}\n\nRemember: return ONLY the Markdown note, no explanation or code fences.",
        truncate_for_llm(&context, context_limit)
    );
    update_note_patch_progress(process_store, process_id, outputs, 1, 3, "calling LLM");

    // Call LLM.
    let markdown_output = match crate::llm::chat_text(system_prompt, &user_prompt, 0.3, 16384).await
    {
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

async fn run_course_note_generation(
    process_id: &str,
    course_sources: &[&SourceRecord],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    job_id: &str,
) {
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
        let err_msg = "No ready Course Transcript indexes found.".to_string();
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

    if !crate::llm::is_available() {
        let err_msg =
            "LLM is not available. Set OPENAI_API_KEY in Settings to enable Note Patch generation."
                .to_string();
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

    update_note_patch_progress(
        process_store,
        process_id,
        outputs,
        1,
        4,
        "preparing course context",
    );

    let mut context = String::new();
    let context_limit = 80000usize;
    for index in &indexes {
        if context.chars().count() >= context_limit {
            break;
        }
        let ranges = index
            .timestamp_ranges
            .iter()
            .take(10)
            .map(|r| {
                format!(
                    "{} [{:.0}-{:.0}] {}",
                    r.video_id, r.start, r.end, r.text_preview
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let source_text = fs::read_to_string(&index.source_path).unwrap_or_default();
        let remaining = context_limit.saturating_sub(context.chars().count());
        let transcript_budget = remaining.min(10000);
        context.push_str(&format!(
            "\n\n--- Date: {} | {} ---\nSummary: {}\nKeywords: {}\nConcepts: {}\nImportant ranges:\n{}\nTranscript excerpt:\n{}",
            index.date,
            index.title,
            index.summary,
            index.keywords.join(", "),
            index.concepts.join(", "),
            ranges,
            compact_transcript_for_llm(&source_text, transcript_budget)
        ));
    }

    let system_prompt = "You are an expert lecture note writer. You are given indexed course transcript materials and must create a complete Markdown note from scratch. \
Use Chinese when the source is Chinese, while preserving standard English technical terms, symbols, formulas, and code identifiers. \
Organize by topic, not merely by date. Use ## for main sections and ### for subsections. \
Keep exam-relevant definitions, formulas, assumptions, algorithms, procedures, comparisons, and common pitfalls. \
Do not invent facts not supported by the transcript context. \
Return ONLY the complete Markdown note, with no surrounding explanation, no code fences, and no commentary.";

    let user_prompt = format!(
        "Generate a complete Markdown note from the following course transcript index/context. \
This note will be used as the Note Patch dependency for a later Cheating Sheet render, so make it well-structured and factual.\n\n{}\n\n\
Return only Markdown.",
        truncate_for_llm(&context, context_limit)
    );

    update_note_patch_progress(process_store, process_id, outputs, 2, 4, "calling LLM");

    let markdown_output = match crate::llm::chat_text(system_prompt, &user_prompt, 0.25, 16384).await
    {
        Ok(text) => strip_markdown_fences(&text),
        Err(e) => {
            let err_msg = format!("LLM call failed: {}", e);
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

    update_note_patch_progress(process_store, process_id, outputs, 3, 4, "writing note");

    let retrieval_traces = indexes
        .iter()
        .map(|index| RetrievalTrace {
            section: format!("Generated from {}", index.date),
            matches: vec![RetrievalMatch {
                date: index.date.clone(),
                score: 1.0,
                timestamp_ranges: index.timestamp_ranges.iter().take(10).cloned().collect(),
            }],
            skipped_reason: None,
        })
        .collect::<Vec<_>>();

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
                o.metadata = serde_json::json!({
                    "progress_current": 4,
                    "progress_total": 4,
                    "progress_label": "complete",
                    "generated_without_note_source": true,
                });
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }
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
    let note_patch = process
        .outputs
        .iter()
        .find(|o| o.kind == ProcessOutputKind::NotePatch && o.status == ProcessRecordStatus::Ready);
    let note_patch = match note_patch {
        Some(output) => output,
        None => {
            let err_msg = "Cheating Sheet requires a completed Note Patch output.".to_string();
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

    let note_markdown = match fs::read_to_string(&note_patch.path) {
        Ok(content) => content,
        Err(e) => {
            let err_msg = format!("Failed to read Note Patch output: {}", e);
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

        let cheat_markdown = match generate_cheating_sheet_markdown(&note_markdown, max_pages).await
        {
            Ok(markdown) => markdown,
            Err(e) => {
                web_log(format!(
                    "job {} cheating-sheet: LLM condensation failed, rendering compressed Note Patch directly: {}",
                    job_id, e
                ));
                latex::compress_content(&note_markdown, 1)
            }
        };

        let markdown_path = cheating_sheet_markdown_path(process_store, process_id, &output.id);
        if let Some(parent) = markdown_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&markdown_path, &cheat_markdown) {
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

        match render_result {
            Ok(artifact) => {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Ready;
                        o.path = artifact.pdf_path.clone();
                        o.diff_path = None;
                        o.base_source_id = Some(note_patch.id.clone());
                        o.last_error = None;
                        o.metadata = serde_json::json!({
                            "progress_current": 4,
                            "progress_total": 4,
                            "progress_label": "complete",
                            "max_pages": max_pages,
                            "page_count": artifact.page_count,
                            "compression_attempts": artifact.compression_attempts,
                            "template_used": artifact.template_used,
                            "markdown_path": markdown_path.to_string_lossy().to_string(),
                            "source_note_patch_output_id": note_patch.id.clone(),
                        });
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
            Err(e) => {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Failed;
                        o.last_error = Some(format!("Cheating Sheet render failed: {}", e));
                        o.metadata = serde_json::json!({
                            "progress_current": 3,
                            "progress_total": 4,
                            "progress_label": "render failed",
                            "max_pages": max_pages,
                            "markdown_path": markdown_path.to_string_lossy().to_string(),
                            "source_note_patch_output_id": note_patch.id.clone(),
                        });
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
        }
    }
}

async fn generate_cheating_sheet_markdown(note_markdown: &str, max_pages: usize) -> Result<String> {
    if !crate::llm::is_available() {
        bail!("LLM is not available");
    }

    let system_prompt = "You convert patched lecture notes into compact exam cheat-sheet Markdown for a fixed LaTeX template. \
Return ONLY Markdown, with no code fences, no explanations, and no raw LaTeX commands except ordinary math delimited by $...$ or $$...$$. \
The Markdown will be escaped and inserted into a XeLaTeX/xeCJK four-column A4 template, so avoid syntax that commonly breaks LaTeX: no HTML, images, footnotes, Markdown tables, nested tables, Mermaid, TikZ, custom macros, \\begin blocks, or unbalanced braces. \
Use Chinese for Chinese source material, keep standard English technical terms, identifiers, and formulas. \
Prefer short headings, dense bullets, definitions, theorem/condition/result patterns, formulas, contrasts, and procedure steps. \
Use only #, ##, ### headings, -, 1. lists, inline code for identifiers, bold for key terms, and standard Markdown math. \
Every formula must be syntactically balanced. Keep underscores and percent signs inside math or code when possible.";

    let user_prompt = format!(
        "Target page budget: at most {} A4 page(s), four columns, about 5pt body text. \
Create a compact Markdown cheating sheet from the patched note below.\n\n\
Compression policy:\n\
- Keep exam-critical definitions, equations, assumptions, algorithms, edge cases, and comparison tables converted to bullets.\n\
- Remove narrative transitions, timestamps, source citations, examples that do not add a reusable pattern, and repeated explanations.\n\
- If content is too long, prefer fewer words per bullet over dropping core formulas.\n\
- Use Chinese punctuation sparingly and keep lines short.\n\n\
Patched Note Markdown:\n\n{}\n\n\
Return only the complete cheating-sheet Markdown.",
        max_pages,
        truncate_for_llm(note_markdown, 60000)
    );

    let text = crate::llm::chat_text(system_prompt, &user_prompt, 0.2, 16384).await?;
    Ok(strip_markdown_fences(&text))
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
    let json = crate::llm::chat_json(system, &user, 0.2, 4096).await?;
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
                "error": format!("Unsupported output kind: {}. Supported kinds: note_patch, cheating_sheet.", body.kind),
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

    if requested_kind == ProcessOutputKind::CheatingSheet && has_cheating_sheet {
        return Json(serde_json::json!({
            "status": "failed",
            "error": "Cheating Sheet output already exists for this process. Only one cheating_sheet output is supported.",
        }));
    }

    let now = ProcessRecord::now_iso();
    let mut new_outputs = Vec::new();
    let mut kinds_to_add = Vec::new();
    if requested_kind == ProcessOutputKind::CheatingSheet && !has_note_patch {
        kinds_to_add.push((ProcessOutputKind::NotePatch, 2usize));
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
        let metadata = if kind == ProcessOutputKind::CheatingSheet {
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
            truncate_for_llm(base, 20000)
        ));
    }

    // Call LLM.
    let updated_md = match crate::llm::chat_text(system_prompt, &user_prompt, 0.3, 16384).await {
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
}
