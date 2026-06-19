//! Course Reference Digest: outline → section → merge pipeline.
//!
//! Handles the Reference Digest path when course transcript sources are present.
//! Builds a structured outline from index metadata, retrieves per-section
//! transcript context, generates each section independently via LLM, then
//! merges into one cohesive Reference Digest document.

use std::fs;

use anyhow::{Context, Result};
use serde_json;

use crate::llm::{self, ChatMessage};
use crate::notes;
use crate::utils::markdown::strip_markdown_fences;
use crate::utils::outline::{
    build_outline_context, generate_fallback_outline, parse_outline_sections, PlannedSection,
};
use crate::utils::output::{update_ref_digest_progress, web_log};
use crate::utils::retrieval::retrieve_course_context_for_section_ref_digest;
use crate::web::course::{read_indexes, read_manifest, CourseDateIndex, RetrievalTrace};
use crate::web::processes::{
    ProcessOutput, ProcessRecord, ProcessStatus as ProcessRecordStatus, ProcessStore,
};
use crate::web::sources::{truncate_for_llm, SourceRecord};

// ---------------------------------------------------------------------------
// Main course entry point
// ---------------------------------------------------------------------------

pub(crate) async fn run_course_reference_digest(
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

    if !llm::is_available() {
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
                let headings = notes::extract_headings(&content);
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

// ---------------------------------------------------------------------------
// Course outline generation
// ---------------------------------------------------------------------------

/// Generate a course Reference Digest outline from index metadata + optional note structure.
pub(crate) async fn generate_course_ref_digest_outline(
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
        ChatMessage {
            role: "system".into(),
            content: system.to_string(),
        },
        ChatMessage {
            role: "user".into(),
            content: user.clone(),
        },
    ];

    // First attempt: max_tokens = 49152.
    let (text, finish_reason) =
        llm::chat_completion_with_metadata(&messages, 0.2, 49152, Some("json_object"))
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

// ---------------------------------------------------------------------------
// Single section generation
// ---------------------------------------------------------------------------

/// Generate Markdown for a single planned Reference Digest section.
pub(crate) async fn generate_course_ref_digest_section(
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

    let text = llm::chat_text(&system, &user, 0.25, 24576).await?;
    Ok(strip_markdown_fences(&text))
}

// ---------------------------------------------------------------------------
// Section merging
// ---------------------------------------------------------------------------

/// Merge independently-generated Reference Digest sections into one cohesive digest.
pub(crate) async fn merge_course_ref_digest_sections(
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

    let text = llm::chat_text(&system, &user, 0.2, 81920).await?;
    Ok(strip_markdown_fences(&text))
}
