//! Course Note processor: generates Markdown notes from course transcript
//! indexes using LLM outline planning, per-section retrieval, and section merging.
//!
//! Handles the "generate from scratch" path when no base note source is available.
//! Depends on `crate::utils::outline` for outline planning/parsing and
//! `crate::utils::retrieval` for per-section transcript context retrieval.

use std::fs;

use anyhow::Context;

use crate::llm::{self, ChatMessage};
use crate::utils::markdown::strip_markdown_fences;
use crate::utils::outline::{
    build_outline_context, generate_fallback_outline, parse_outline_sections, PlannedSection,
};
use crate::utils::output::{update_note_patch_progress, web_log};
use crate::utils::retrieval::retrieve_course_context_for_section;
use crate::web::course::{read_indexes, read_manifest, CourseDateIndex, RetrievalTrace};
use crate::web::processes::{
    ProcessOutput, ProcessRecord, ProcessStatus as ProcessRecordStatus, ProcessStore,
};
use crate::web::sources::{truncate_for_llm, SourceRecord};

// ---------------------------------------------------------------------------
// Course note outline (LLM)
// ---------------------------------------------------------------------------

/// Plan a structured note outline via LLM, using lightweight course index
/// metadata. Returns 4-24 planned sections on success.
///
/// If the first attempt returns an invalid outline (wrong section count or
/// unparseable JSON), a single retry is made with a diagnostic hint embedded
/// in the user prompt.
pub(crate) async fn generate_course_note_outline(
    indexes: &[CourseDateIndex],
) -> anyhow::Result<Vec<PlannedSection>> {
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
        ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user,
        },
    ];

    // --- First attempt: max_tokens = 49152, temperature = 0.2 ---
    let (text, finish_reason) =
        llm::chat_completion_with_metadata(&messages, 0.2, 49152, Some("json_object"))
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
        ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: retry_user,
        },
    ];

    let (retry_text, retry_finish_reason) =
        llm::chat_completion_with_metadata(&retry_messages, 0.2, 57344, Some("json_object"))
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

// ---------------------------------------------------------------------------
// Single section generation
// ---------------------------------------------------------------------------

/// Generate Markdown for a single planned section from its retrieved context.
///
/// The prompt writes **only** that section; it must not invent facts beyond the
/// given context.  Defaults to Chinese while preserving English technical terms,
/// formulas, symbols, and code identifiers.
pub(crate) async fn generate_course_note_section(
    section: &PlannedSection,
    context: &str,
    style_guide: &str,
) -> anyhow::Result<String> {
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

    let text = llm::chat_text(&system, &user, 0.25, 16384).await?;
    Ok(strip_markdown_fences(&text))
}

// ---------------------------------------------------------------------------
// Section merging
// ---------------------------------------------------------------------------

/// Merge independently-generated section Markdown into one cohesive note.
///
/// The prompt normalises title hierarchy, terminology, formula formatting,
/// duplicate content, and transitions **without** adding new facts.  Only the
/// section texts and style guide are fed in — raw transcripts are excluded.
pub(crate) async fn merge_course_note_sections(
    sections: &[String],
    style_guide: &str,
) -> anyhow::Result<String> {
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

    let text = llm::chat_text(&system, &user, 0.2, 57344).await?;
    Ok(strip_markdown_fences(&text))
}

// ---------------------------------------------------------------------------
// Main course note generation entry point
// ---------------------------------------------------------------------------

/// Run the full course note generation pipeline:
///
/// 1. Load course indexes from source manifests.
/// 2. Plan an outline (LLM with deterministic fallback).
/// 3. Retrieve context for each section and generate Markdown independently.
/// 4. Merge sections into one cohesive note.
/// 5. Write outputs (Markdown, diff, retrieval traces) and update progress.
pub(crate) async fn run_course_note_generation(
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

    if !llm::is_available() {
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
