//! Source handlers: CRUD for /api/sources/*.

use crate::artifacts::TranscriptArtifact;
use crate::canvas_sjtu::CanvasPptSlice;
use crate::llm::{self, compact_transcript_for_llm};
use crate::web::app::AppState;
use crate::web::course::{
    bm25_search, estimate_token_count, extract_timestamp_ranges, read_indexes, read_manifest,
    truncate_chars, write_index, write_manifest, CourseDateIndex, CourseManifest,
    CourseManifestDate,
};
use crate::web::jobs::{JobRegistry, JobStatus};
use crate::web::sources::{
    deterministic_answer as source_deterministic_answer, truncate_for_llm, SourceKind,
    SourceRecord, SourceStatus, SourceStore,
};
use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use std::collections::HashMap;
use std::convert::Infallible;
use std::fs;
use std::path::Path as FsPath;

// ---------------------------------------------------------------------------
// Tiny helpers
// ---------------------------------------------------------------------------

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
// Language helpers
// ---------------------------------------------------------------------------

fn default_processing_language() -> String {
    "zh".to_string()
}

pub(crate) fn normalize_processing_language(value: &str) -> String {
    match value {
        "en" => "en".to_string(),
        "bilingual" => "bilingual".to_string(),
        _ => "zh".to_string(),
    }
}

// ---------------------------------------------------------------------------
// JSON helper
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// GET /api/sources
// ---------------------------------------------------------------------------

/// `GET /api/sources` -- list all sources, sorted by creation time (newest
/// first). Also reconciles any stale `Processing` sources whose background
/// job has already finished.
pub(crate) async fn api_get_sources(State(state): State<AppState>) -> Json<serde_json::Value> {
    reconcile_stale_source_jobs(&state);
    let mut sources = state.source_store.load_all();
    sources.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(serde_json::json!({
        "sources": sources,
    }))
}

pub(crate) fn reconcile_stale_source_jobs(state: &AppState) {
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

// ---------------------------------------------------------------------------
// GET /api/sources/{id}
// ---------------------------------------------------------------------------

/// `GET /api/sources/{id}` — get a single source.
pub(crate) async fn api_get_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.source_store.get(&id) {
        Some(source) => Json(serde_json::to_value(&source).unwrap_or_default()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Source not found"})),
        )
            .into_response(),
    }
}

/// `GET /api/sources/{id}/index` — get the course date indexes for a
/// transcript-course or transcript-day source.
pub(crate) async fn api_get_source_index(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
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

// ---------------------------------------------------------------------------
// POST /api/sources/{id}/reindex
// ---------------------------------------------------------------------------

pub(crate) async fn api_reindex_source(
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

// ---------------------------------------------------------------------------
// DELETE /api/sources/{id}
// ---------------------------------------------------------------------------

/// `DELETE /api/sources/{id}` — delete a source record (does not delete
/// artifact files).
pub(crate) async fn api_delete_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
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

// ---------------------------------------------------------------------------
// POST /api/sources/note
// ---------------------------------------------------------------------------

/// `POST /api/sources/note` — create a note source from Markdown content.
///
/// Body: `{ name?: string, content: string }`
#[derive(Debug, Deserialize)]
pub(crate) struct CreateNoteBody {
    #[serde(default)]
    pub name: String,
    pub content: String,
}

pub(crate) async fn api_create_note_source(
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

// ---------------------------------------------------------------------------
// PUT /api/sources/{id}/note
// ---------------------------------------------------------------------------

/// `PUT /api/sources/{id}/note` — update an existing note source.
///
/// Body: `{ name?: string, content: string }`
#[derive(Debug, Deserialize)]
pub(crate) struct UpdateNoteBody {
    #[serde(default)]
    pub name: String,
    pub content: String,
}

pub(crate) async fn api_update_note_source(
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

// ---------------------------------------------------------------------------
// POST /api/sources/transcript-day
// ---------------------------------------------------------------------------

/// `POST /api/sources/transcript-day` — create a transcript-day source and
/// start background processing.
///
/// Body: `{ course_id, course_name?, date, cookie? }`
#[derive(Debug, Deserialize)]
pub(crate) struct CreateTranscriptDayBody {
    pub course_id: String,
    #[serde(default)]
    pub course_name: String,
    pub date: String,
    #[serde(default = "default_processing_language")]
    pub processing_language: String,
    /// Deprecated: cookie is no longer accepted from the request body.
    /// Credentials must be saved in Settings.
    #[serde(default)]
    #[allow(dead_code)]
    pub cookie: String,
}

pub(crate) async fn api_create_transcript_day_source(
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

// ---------------------------------------------------------------------------
// POST /api/sources/transcript-course
// ---------------------------------------------------------------------------

/// Body: `{ course_id, course_name? }`
#[derive(Debug, Deserialize)]
pub(crate) struct CreateTranscriptCourseBody {
    pub course_id: String,
    #[serde(default)]
    pub course_name: String,
    #[serde(default = "default_processing_language")]
    pub processing_language: String,
}

pub(crate) async fn api_create_transcript_course_source(
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

// ---------------------------------------------------------------------------
// sync_course_source — core logic shared by create and sync
// ---------------------------------------------------------------------------

pub(crate) async fn sync_course_source(
    source_id: &str,
    course_id: &str,
    course_name: &str,
    processing_language: &str,
    cookie: &str,
    source_store: &SourceStore,
    registry: &JobRegistry,
    job_id: &str,
) -> Result<()> {
    use crate::web::handlers::courses::extract_date_from_video;

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

// ---------------------------------------------------------------------------
// build_course_date_index_with_llm
// ---------------------------------------------------------------------------

pub(crate) async fn build_course_date_index_with_llm(
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

// ---------------------------------------------------------------------------
// POST /api/sources/{id}/sync
// ---------------------------------------------------------------------------

/// `POST /api/sources/{id}/sync` — re-sync a source.
///
/// For transcript_day sources, re-runs the download/merge using saved
/// metadata and secrets. For note sources, returns a message explaining
/// notes are updated via the upload endpoint.
pub(crate) async fn api_sync_source(
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

// ---------------------------------------------------------------------------
// POST /api/sources/{id}/ask
// ---------------------------------------------------------------------------

/// `POST /api/sources/{id}/ask` - ask a natural language question.
///
/// Body: `{ question: string }`
#[derive(Debug, Deserialize)]
pub(crate) struct AskSourceBody {
    pub question: String,
}

pub(crate) async fn api_ask_source(
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
// GET /api/sources/{id}/ask-stream (SSE)
// ---------------------------------------------------------------------------

/// `GET /api/sources/{id}/ask-stream?question=...`
///
/// Streams the answer as Server-Sent Events. Uses LLM streaming when
/// available, or deterministic fallback chunks otherwise.
#[derive(Debug, Deserialize)]
pub(crate) struct AskStreamQuery {
    pub question: String,
}

pub(crate) async fn api_source_ask_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AskStreamQuery>,
) -> Response {
    use super::processes::stream_deterministic_fallback;

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
