//! Note Patch processor: patches Markdown notes with transcript context.
//!
//! Loads sources, builds transcript context, calls the LLM to generate an
//! updated note (or a fresh note when no base note exists), and saves the
//! output along with a unified diff.
//!
//! Also provides the per-section patch generation function used by the course
//! note patch path.

use std::fs;

use crate::artifacts::{KeepLevel, PatchEntry};
use crate::diff;
use crate::llm;
use crate::processors::course_note_patch::run_course_note_patch;
use crate::prompts::note_patch::build_note_patch_prompts;
use crate::utils::output::{update_note_patch_progress, web_log};
use crate::web::course::truncate_chars;
use crate::web::processes::{
    ProcessOutput, ProcessRecord, ProcessStatus as ProcessRecordStatus, ProcessStore,
};
use crate::web::sources::{truncate_for_llm, SourceKind, SourceRecord, SourceStore};

/// Run Note Patch generation for a set of outputs.
///
/// Loads sources (Note, TranscriptDay, TranscriptCourse), builds compact
/// transcript context, calls the LLM to produce an updated Markdown note,
/// and writes the output along with a unified diff.
///
/// When course-level transcript sources are present, delegates to
/// `run_course_note_patch` for structured per-section patching.
pub(crate) async fn run_note_patch(
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
    if !llm::is_available() {
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
                let compact = llm::compact_transcript_for_llm(
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
    let markdown_output = match llm::chat_text(&system_prompt, &user_prompt, 0.3, 32768).await {
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
            let unified = diff::unified_diff(base, &markdown_output, 3);
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

/// Generate structured patches for a single note section from retrieved
/// transcript context.
///
/// Used by the course note patch path to produce per-section insertions.
/// Each patch includes a `location` (heading), `new_text`, source attribution
/// (`source_video_id`, `source_timestamp`), and confidence score.
pub(crate) async fn generate_section_patches(
    heading: &str,
    body: &str,
    context: &str,
) -> Result<Vec<PatchEntry>, anyhow::Error> {
    let system = "You produce structured note patches from retrieved lecture transcript excerpts. \
                  Output JSON only with a patches array. Each patch has location, new_text, source_date, source_video_id, source_timestamp, confidence. \
                  Only add facts that are clearly supported by the transcript context and missing from the note section.";
    let user = format!(
        "Note section heading: {}\n\nCurrent section body:\n{}\n\nRetrieved transcript context:\n{}",
        heading,
        truncate_chars(body, 4000),
        truncate_for_llm(context, 45000)
    );
    let json = llm::chat_json(system, &user, 0.2, 8192).await?;
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
