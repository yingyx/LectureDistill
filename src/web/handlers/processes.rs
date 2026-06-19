//! Process handlers: CRUD for /api/processes/*.

use crate::web::app::AppState;
use crate::web::processes::{
    ProcessOutput, ProcessOutputKind, ProcessRecord, ProcessStatus as ProcessRecordStatus,
    ProcessStore,
};
use crate::web::sources::{truncate_for_llm, SourceKind};
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use std::fs;

// ---------------------------------------------------------------------------
// Streaming deterministic fallback (SSE)
// ---------------------------------------------------------------------------

/// Stream a deterministic fallback answer as SSE chunks.
pub(crate) fn stream_deterministic_fallback(
    source_id: String,
    answer: String,
    reason: &str,
) -> Response {
    use std::convert::Infallible;
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
// GET /api/processes, GET /api/processes/{id}
// ---------------------------------------------------------------------------

/// `GET /api/processes` -- list all processes, newest first.
pub(crate) async fn api_get_processes(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut processes = state.process_store.load_all();
    processes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Json(serde_json::json!({
        "processes": processes,
    }))
}

/// `GET /api/processes/{id}` -- get a single process record.
pub(crate) async fn api_get_process(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.process_store.get(&id) {
        Some(process) => Json(serde_json::to_value(&process).unwrap_or_default()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Process not found"})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Process output helper types and functions
// ---------------------------------------------------------------------------

/// `POST /api/processes` -- create a new process and start background jobs.
///
/// Body: `{ title?: string, source_ids: string[], outputs: [{ kind: "note_patch" | "reference_digest" | "cheating_sheet", max_pages?: 2 }] }`
#[derive(Debug, Deserialize)]
pub(crate) struct CreateProcessBody {
    #[serde(default)]
    pub title: String,
    pub source_ids: Vec<String>,
    pub outputs: Vec<CreateProcessOutputBody>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateProcessOutputBody {
    pub kind: String,
    #[serde(default)]
    pub max_pages: Option<usize>,
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

pub(crate) fn update_process_terminal_status(
    process_store: &ProcessStore,
    process_id: &str,
    job_id: &str,
) {
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

// ---------------------------------------------------------------------------
// POST /api/processes
// ---------------------------------------------------------------------------

pub(crate) async fn api_create_process(
    State(state): State<AppState>,
    Json(body): Json<CreateProcessBody>,
) -> Json<serde_json::Value> {
    use crate::pipelines::run_process_outputs as run_outputs;

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
        crate::utils::output::web_log(format!(
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
            run_outputs(
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

        crate::utils::output::web_log(format!(
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
// POST /api/processes/{id}/retry
// ---------------------------------------------------------------------------

/// Query params for `POST /api/processes/{id}/retry`.
#[derive(Debug, Deserialize)]
pub(crate) struct RetryProcessQuery {
    /// When `true`, reset all outputs and rerun from scratch.
    #[serde(default)]
    pub force: bool,
}

/// `POST /api/processes/{id}/retry` — retry failed outputs or force full rerun.
pub(crate) async fn api_retry_process(
    State(state): State<AppState>,
    Path(process_id): Path<String>,
    Query(query): Query<RetryProcessQuery>,
) -> Json<serde_json::Value> {
    use crate::pipelines::run_process_outputs as run_outputs;
    use crate::utils::output::web_log;

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
            run_outputs(
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

// ---------------------------------------------------------------------------
// POST /api/processes/{id}/outputs/{output_id}/retry
// ---------------------------------------------------------------------------

/// `POST /api/processes/{id}/outputs/{output_id}/retry` — retry a single output.
pub(crate) async fn api_retry_process_output(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Json<serde_json::Value> {
    use crate::processors::cheating_sheet::run_cheating_sheet_outputs;
    use crate::processors::note_patch::run_note_patch;
    use crate::processors::reference_digest::run_reference_digest_outputs;
    use crate::utils::output::web_log;

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
// GET /api/processes/{id}/outputs/{output_id}/stream (SSE)
// ---------------------------------------------------------------------------

/// `GET /api/processes/{id}/outputs/{output_id}/stream`
///
/// Runs LLM generation for the given output and streams the result as
/// Server-Sent Events.  Uses the same prompt-building logic as the
/// background-job path but sends each token chunk to the client in
/// real-time via SSE.  The final result is saved to the process store.
pub(crate) async fn api_stream_process_output(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Response {
    use crate::prompts::cheat_sheet::build_cheat_sheet_prompts;
    use crate::prompts::note_patch::build_note_patch_prompts;
    use crate::prompts::ref_digest::{
        build_ref_digest_system_prompt, build_ref_digest_user_prompt,
    };
    use crate::utils::markdown::strip_markdown_fences;
    use std::convert::Infallible;
    use tokio::sync::mpsc;

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

// ---------------------------------------------------------------------------
// GET /api/processes/{id}/outputs/{output_id}
// ---------------------------------------------------------------------------

/// `GET /api/processes/{id}/outputs/{output_id}` -- get output content.
pub(crate) async fn api_get_process_output(
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

// ---------------------------------------------------------------------------
// GET /api/processes/{id}/outputs/{output_id}/file
// ---------------------------------------------------------------------------

/// `GET /api/processes/{id}/outputs/{output_id}/file` — serve the output file (PDF etc.).
pub(crate) async fn api_get_process_output_file(
    State(state): State<AppState>,
    Path((process_id, output_id)): Path<(String, String)>,
) -> Response {
    use axum::http::header;

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

// ---------------------------------------------------------------------------
// POST /api/processes/{id}/outputs
// ---------------------------------------------------------------------------

/// `POST /api/processes/{id}/outputs` -- add an output method to a process.
///
/// Body: `{ kind: "note_patch" | "cheating_sheet", max_pages?: 2 }`
#[derive(Debug, Deserialize)]
pub(crate) struct AddOutputBody {
    pub kind: String,
    #[serde(default)]
    pub max_pages: Option<usize>,
}

pub(crate) async fn api_add_process_output(
    State(state): State<AppState>,
    Path(process_id): Path<String>,
    Json(body): Json<AddOutputBody>,
) -> Json<serde_json::Value> {
    use crate::pipelines::run_process_outputs as run_outputs;

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
            run_outputs(
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

// ---------------------------------------------------------------------------
// DELETE /api/processes/{id}/outputs/{output_id}
// ---------------------------------------------------------------------------

/// `DELETE /api/processes/{id}/outputs/{output_id}` -- remove an output.
pub(crate) async fn api_delete_process_output(
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

// ---------------------------------------------------------------------------
// POST /api/processes/{id}/outputs/{output_id}/revise
// ---------------------------------------------------------------------------

/// `POST /api/processes/{id}/outputs/{output_id}/revise` -- revise with LLM.
///
/// Body: `{ instruction: string }`
#[derive(Debug, Deserialize)]
pub(crate) struct ReviseOutputBody {
    pub instruction: String,
}

pub(crate) async fn api_revise_process_output(
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
