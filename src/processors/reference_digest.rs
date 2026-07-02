//! Reference Digest processor: generates comprehensive exam review digests
//! from lecture transcripts.
//!
//! Supports two paths:
//! - **Ordinary**: Note + TranscriptDay sources — single or sectioned digest
//! - **Course**: TranscriptCourse sources — outline → section → merge pipeline
//!   (delegated to `course_reference_digest`)

use std::fs;

use anyhow::Result;

use crate::llm;
use crate::prompts::ref_digest::{build_ref_digest_system_prompt, build_ref_digest_user_prompt};
use crate::utils::markdown::strip_markdown_fences;
use crate::utils::output::{update_ref_digest_progress, web_log};
use crate::web::processes::{
    ProcessOutput, ProcessRecord, ProcessStatus as ProcessRecordStatus, ProcessStore,
};
use crate::web::sources::{SourceKind, SourceRecord, SourceStore};

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run Reference Digest generation for a set of outputs.
///
/// Loads sources (Note, TranscriptDay, TranscriptCourse), builds compact
/// transcript context, generates a comprehensive Markdown Reference Digest
/// via LLM, and writes the output.
///
/// When course-level transcript sources are present, delegates to
/// `run_course_reference_digest` for structured per-section generation.
pub(crate) async fn run_reference_digest_outputs(
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

    if !llm::is_available() {
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
        crate::processors::course_reference_digest::run_course_reference_digest(
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
                let compact = llm::compact_transcript_for_llm(&content, per_source_budget);
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
            let compact = llm::compact_transcript_for_llm(&src_content, 90000);
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

    // --- Ref Digest Quality Gate ---
    let source_chars = context.chars().count();
    let max_pages_for_quality = 2; // default for quality check
    let (quality_ok, quality_reason) = crate::utils::budget::check_ref_digest_quality(
        digest_markdown.chars().count(),
        source_chars,
        max_pages_for_quality,
    );
    web_log(format!(
        "job {} reference-digest: quality gate — {}",
        job_id, quality_reason,
    ));

    let digest_markdown = if !quality_ok {
        // Retry with corrective prompt.
        let target_min = max_pages_for_quality
            .saturating_mul(11000)
            .saturating_mul(60)
            / 100;
        let retry_prompt = crate::prompts::ref_digest::build_ref_digest_retry_prompt(
            &digest_markdown,
            source_chars,
            digest_markdown.chars().count(),
            target_min.max(source_chars.saturating_mul(75) / 1000), // 7.5%
        );
        let system = crate::prompts::ref_digest::build_ref_digest_system_prompt();
        match llm::chat_text(&system, &retry_prompt, 0.25, 81920).await {
            Ok(text) => {
                let retry_md = strip_markdown_fences(&text);
                let retry_chars = retry_md.chars().count();
                web_log(format!(
                    "job {} reference-digest: quality retry — {} chars (was {})",
                    job_id,
                    retry_chars,
                    digest_markdown.chars().count(),
                ));
                retry_md
            }
            Err(e) => {
                web_log(format!(
                    "job {} reference-digest: quality retry failed: {} — using original",
                    job_id, e,
                ));
                digest_markdown
            }
        }
    } else {
        digest_markdown
    };
    // --- End Quality Gate ---

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

// ---------------------------------------------------------------------------
// Single-pass generation
// ---------------------------------------------------------------------------

/// Generate a single-pass Reference Digest from combined transcript context.
pub(crate) async fn generate_ref_digest_single(
    note_content: &Option<String>,
    transcript_context: &str,
) -> Result<String> {
    let system = build_ref_digest_system_prompt();
    let user = build_ref_digest_user_prompt(note_content, transcript_context, None);
    let text = llm::chat_text(&system, &user, 0.25, 81920).await?;
    Ok(strip_markdown_fences(&text))
}

// ---------------------------------------------------------------------------
// Section generation
// ---------------------------------------------------------------------------

/// Generate a Reference Digest section for a single transcript source.
pub(crate) async fn generate_ref_digest_section(
    note_content: &Option<String>,
    src_context: &str,
) -> Result<String> {
    let system = build_ref_digest_system_prompt();
    let user = build_ref_digest_user_prompt(note_content, src_context, Some("section"));
    let text = llm::chat_text(&system, &user, 0.25, 32768).await?;
    Ok(strip_markdown_fences(&text))
}

// ---------------------------------------------------------------------------
// Section merging
// ---------------------------------------------------------------------------

/// Merge independently-generated Reference Digest sections into one cohesive digest.
pub(crate) async fn merge_ref_digest_sections(sections: &[String]) -> Result<String> {
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

    let text = llm::chat_text(system, &user, 0.2, 81920).await?;
    Ok(strip_markdown_fences(&text))
}
