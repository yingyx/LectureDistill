//! Unified Ref Digest → Cheat Sheet pipeline (multi-turn LLM conversation).
//!
//! When both Reference Digest and Cheat Sheet outputs are requested, this
//! pipeline runs them in a single multi-turn LLM conversation to exploit
//! DeepSeek prefix caching: the system prompt is identical across turns so
//! the prefix can be cached and reused.
//!
//! The pipeline has three stages:
//! 1. Reference Digest (Turn 1)
//! 2. Cheat Sheet compression (Turn 2)
//! 3. Expansion (Turn 3, only when pages are underfilled)
//!
//! Two paths exist:
//! - **Single-pass**: all transcripts fit in one LLM call (≤2 sources, ≤100k chars)
//! - **Multi-section**: per-source section digests → merge → cheat sheet

use std::fs;

use crate::latex;
// LLM calls use fully-qualified paths (crate::llm::...) for clarity.
use crate::processors::cheating_sheet::finish_cheating_sheet_render;
use crate::processors::reference_digest::{
    generate_ref_digest_section, run_reference_digest_outputs,
};
use crate::prompts::cheat_sheet::{build_cheat_sheet_turn2_prompt, build_expansion_turn3_prompt};
use crate::prompts::ref_digest::build_ref_digest_user_prompt;
use crate::prompts::unified_pipeline::build_unified_pipeline_system_prompt;
use crate::utils::budget::{
    build_cheatsheet_metadata, build_section_inventory, compute_budget, count_effective,
    should_attempt_expansion, CheatingSheetGenerationResult,
};
use crate::utils::calibration::ensure_calibration;
use crate::utils::markdown::strip_markdown_fences;
use crate::utils::output::{
    cheating_sheet_markdown_path, process_output_path_for, update_single_output_progress, web_log,
};
use crate::web::processes::{
    ProcessOutput, ProcessOutputKind, ProcessRecord, ProcessStatus as ProcessRecordStatus,
    ProcessStore,
};
use crate::web::sources::{truncate_for_llm, SourceKind, SourceRecord, SourceStore};

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the unified multi-turn pipeline: Reference Digest → Cheat Sheet → Expansion.
///
/// When both Reference Digest and Cheat Sheet outputs are requested, this
/// function orchestrates a single multi-turn LLM conversation that generates
/// the digest on Turn 1, compresses it into a cheat sheet on Turn 2, and
/// optionally expands underfilled pages on Turn 3.
///
/// Falls back to the separate pipeline when course-level transcript sources
/// are present.
pub(crate) async fn run_unified_cheat_pipeline(
    process_id: &str,
    source_ids: &[String],
    ref_digest_outputs: &[ProcessOutput],
    cheat_sheet_outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
    let sources = source_store.load_all();

    // Separate note and transcript sources (mirrors run_reference_digest_outputs).
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

    if transcript_sources.is_empty() && course_sources.is_empty() {
        let err_msg = "No valid transcript sources found.".to_string();
        for output in ref_digest_outputs.iter().chain(cheat_sheet_outputs.iter()) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.clone());
                }
            });
        }
        return;
    }

    // Check LLM availability.
    if !crate::llm::is_available() {
        let err_msg = "LLM is not available. Set OPENAI_API_KEY in Settings.".to_string();
        for output in ref_digest_outputs.iter().chain(cheat_sheet_outputs.iter()) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.clone());
                }
            });
        }
        return;
    }

    // Course-based path: still runs the existing separate pipeline (deferred).
    if !course_sources.is_empty() {
        web_log(format!(
            "job {} unified-pipeline: course sources present — falling back to separate pipeline",
            job_id,
        ));
        run_reference_digest_outputs(
            process_id,
            source_ids,
            ref_digest_outputs,
            process_store,
            source_store,
            job_id,
        )
        .await;
        crate::processors::cheating_sheet::run_cheating_sheet_outputs(
            process_id,
            cheat_sheet_outputs,
            process_store,
            job_id,
        )
        .await;
        return;
    }

    // Read note content for style/structure reference.
    let note_content: Option<String> = match note_source {
        Some(note) => match fs::read_to_string(&note.path) {
            Ok(c) => Some(truncate_for_llm(&c, 50000)),
            Err(e) => {
                web_log(format!(
                    "job {} unified-pipeline: failed to read note source {}: {}",
                    job_id, note.path, e,
                ));
                None
            }
        },
        None => None,
    };

    // Build combined transcript context.
    let context_limit: usize = 100000;
    let mut combined_context = String::new();
    for src in &transcript_sources {
        if combined_context.chars().count() >= context_limit {
            break;
        }
        match fs::read_to_string(&src.path) {
            Ok(content) => {
                let compact = crate::llm::compact_transcript_for_llm(
                    &content,
                    context_limit.saturating_sub(combined_context.chars().count()),
                );
                combined_context.push_str(&format!(
                    "\n\n--- Source: {} (type: {}) ---\n",
                    src.title,
                    src.kind.to_string()
                ));
                combined_context.push_str(&compact);
            }
            Err(e) => {
                web_log(format!(
                    "job {} unified-pipeline: failed to read source {}: {}",
                    job_id, src.path, e
                ));
            }
        }
    }

    let combined_chars = combined_context.chars().count();
    let fits_in_one_call = combined_chars <= 100000 && transcript_sources.len() <= 2;

    // Determine max_pages from the first cheat_sheet_output (for the system prompt).
    let max_pages = cheat_sheet_outputs
        .first()
        .and_then(|o| o.metadata.get("max_pages"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(2)
        .clamp(1, 20);

    let source_counts = serde_json::json!({
        "note": note_source.is_some(),
        "transcript_day": transcript_sources.len(),
        "transcript_course": course_sources.len(),
    });

    if fits_in_one_call {
        run_cheat_pipeline_single_pass(
            process_id,
            note_content,
            &combined_context,
            max_pages,
            source_counts,
            transcript_sources.len(),
            ref_digest_outputs,
            cheat_sheet_outputs,
            process_store,
            job_id,
        )
        .await;
    } else {
        run_cheat_pipeline_multi_section(
            process_id,
            note_content,
            &transcript_sources,
            max_pages,
            source_counts,
            ref_digest_outputs,
            cheat_sheet_outputs,
            process_store,
            job_id,
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Single-pass path
// ---------------------------------------------------------------------------

/// Single-pass unified pipeline: Ref Digest generation → Cheat Sheet → Expansion.
async fn run_cheat_pipeline_single_pass(
    process_id: &str,
    note_content: Option<String>,
    transcript_context: &str,
    max_pages: usize,
    source_counts: serde_json::Value,
    _transcript_count: usize,
    ref_digest_outputs: &[ProcessOutput],
    cheat_sheet_outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    job_id: &str,
) {
    // --- Stage 1: Reference Digest (Turn 1) ---

    // Update progress for ref_digest outputs.
    for output in ref_digest_outputs {
        let _ = process_store.update(process_id, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                o.metadata = serde_json::json!({
                    "progress_current": 1,
                    "progress_total": 2,
                    "progress_label": "generating digest (unified)",
                });
            }
        });
    }

    let system_prompt = build_unified_pipeline_system_prompt(max_pages);
    let turn1_user = build_ref_digest_user_prompt(&note_content, transcript_context, None);

    let mut conversation = crate::llm::ChatConversation::new(&system_prompt);

    let digest_markdown = match conversation.turn(&turn1_user, 0.25, 81920).await {
        Ok(text) => strip_markdown_fences(&text),
        Err(e) => {
            let err_msg = format!("Reference Digest generation failed: {}", e);
            for output in ref_digest_outputs.iter().chain(cheat_sheet_outputs.iter()) {
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

    // Write digest to disk and mark Ready for each ref_digest output.
    let digest_markdown = {
        let source_chars = transcript_context.chars().count();
        let (quality_ok, quality_reason) = crate::utils::budget::check_ref_digest_quality(
            digest_markdown.chars().count(),
            source_chars,
            max_pages,
        );
        web_log(format!(
            "job {} unified-pipeline: quality gate — {}",
            job_id, quality_reason,
        ));

        if !quality_ok {
            let target_min = max_pages.saturating_mul(11000).saturating_mul(60) / 100;
            let retry_prompt = crate::prompts::ref_digest::build_ref_digest_retry_prompt(
                &digest_markdown,
                source_chars,
                digest_markdown.chars().count(),
                target_min.max(source_chars.saturating_mul(75) / 1000), // 7.5%
            );
            match conversation.turn(&retry_prompt, 0.25, 81920).await {
                Ok(text) => {
                    let retry_md = strip_markdown_fences(&text);
                    let retry_chars = retry_md.chars().count();
                    web_log(format!(
                        "job {} unified-pipeline: quality retry — {} chars (was {})",
                        job_id,
                        retry_chars,
                        digest_markdown.chars().count(),
                    ));
                    retry_md
                }
                Err(e) => {
                    web_log(format!(
                        "job {} unified-pipeline: quality retry failed: {} — using original",
                        job_id, e,
                    ));
                    digest_markdown
                }
            }
        } else {
            digest_markdown
        }
    };
    let generated_chars = digest_markdown.chars().count();
    for output in ref_digest_outputs {
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
                o.last_error = None;
                o.metadata = serde_json::json!({
                    "progress_current": 2,
                    "progress_total": 2,
                    "progress_label": "complete",
                    "source_note_used": note_content.is_some(),
                    "source_counts": source_counts,
                    "generated_chars": generated_chars,
                });
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }

    // Persist conversation for downstream retries (Cheat Sheet, Expansion).
    let conv_path = process_store
        .process_dir(process_id)
        .join("conversation.json");
    if let Err(e) = conversation.save_to_file(&conv_path) {
        web_log(format!(
            "job {} unified-pipeline: failed to save conversation: {}",
            job_id, e,
        ));
    }

    // Build section inventory once for all cheat sheet outputs.
    let (sections, inventory) = build_section_inventory(&digest_markdown);
    let section_count = sections.len();
    let section_names = sections
        .iter()
        .map(|s| s.heading.clone())
        .collect::<Vec<_>>()
        .join(" | ");

    // --- Stage 2 & 3: Cheat Sheet + optional Expansion (per output) ---

    for output in cheat_sheet_outputs {
        let output_max_pages = output
            .metadata
            .get("max_pages")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(max_pages)
            .clamp(1, 20);

        let ref_digest_chars = digest_markdown.chars().count();
        let calibration = ensure_calibration(None, &process_store.project_dir());
        let effective = count_effective(&digest_markdown);
        let budget = compute_budget(&calibration, &effective, output_max_pages);
        let lang = budget.language;
        let source_too_short = ref_digest_chars < budget.min_acceptable;

        web_log(format!(
            "job {} unified-pipeline: output {} budget max_pages={} target={} soft_max={} ref_digest_chars={} source_too_short={} lang={:?}",
            job_id, output.id, output_max_pages, budget.target, budget.soft_max,
            ref_digest_chars, source_too_short, lang,
        ));

        // Fork conversation for this output.
        let mut fork = conversation.fork();

        update_single_output_progress(
            process_store,
            process_id,
            &output.id,
            1,
            4,
            "condensing markdown (unified)",
        );

        // Turn 2: Cheat Sheet compression.
        let turn2_user = build_cheat_sheet_turn2_prompt(
            &inventory,
            &section_names,
            section_count,
            output_max_pages,
            &budget,
        );

        let gen_result = match fork.turn_with_metadata(&turn2_user, 0.2, 81920).await {
            Ok((text, finish_reason)) => {
                let md = strip_markdown_fences(&text);
                let chars = md.chars().count();
                web_log(format!(
                    "job {} unified-pipeline: Turn 2 — raw_chars={} stripped_chars={} target={} finish_reason={:?}",
                    job_id, text.chars().count(), chars, budget.target, finish_reason,
                ));
                CheatingSheetGenerationResult {
                    markdown: md,
                    target_chars: budget.target,
                    generated_chars: chars,
                    harness_attempts: 1,
                    expansion_used: false,
                    underfilled_reason: None,
                    language: lang,
                }
            }
            Err(e) => {
                web_log(format!(
                    "job {} unified-pipeline: Turn 2 LLM failed for output {}, falling back to compress_content: {}",
                    job_id, output.id, e,
                ));
                let fallback_md = latex::compress_content(&digest_markdown, 1);
                let fallback_chars = fallback_md.chars().count();
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
                finish_cheating_sheet_render(
                    process_store,
                    process_id,
                    output,
                    ref_digest_outputs.first().unwrap_or(output),
                    &fallback_md,
                    &markdown_path,
                    output_max_pages,
                    budget.target,
                    fallback_chars,
                    0,
                    false,
                    Some("llm_failed".to_string()),
                );
                continue;
            }
        };

        // Write markdown to disk.
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

        // Render.
        update_single_output_progress(process_store, process_id, &output.id, 2, 4, "rendering PDF");
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
            output_max_pages,
        );

        // Expansion decision.
        let should_expand = match &render_result {
            Ok(artifact) => {
                let decision = should_attempt_expansion(
                    gen_result.generated_chars,
                    budget.min_acceptable,
                    artifact.page_count,
                    output_max_pages,
                    source_too_short,
                    artifact.space_utilization.as_ref(),
                );
                web_log(format!(
                    "job {} unified-pipeline: first render page_count={} max_pages={} generated_chars={} source_too_short={} space_util={:?} -> expand={}",
                    job_id, artifact.page_count, output_max_pages, gen_result.generated_chars,
                    source_too_short,
                    artifact.space_utilization.as_ref().map(|su| format!(
                        "last_page={:.1}% under={}",
                        su.last_page_utilization_pct,
                        su.last_page_under_utilized
                    )),
                    decision,
                ));
                decision
            }
            Err(e) => {
                web_log(format!(
                    "job {} unified-pipeline: first render FAILED: {} — skipping expansion",
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
                "expanding content (unified)",
            );

            // ── Multi-pass expansion loop ──
            const MAX_EXPANSION_PASSES: usize = 3;
            const MIN_FILL_RATIO: f64 = 0.70;

            let mut current_md = gen_result.markdown.clone();
            let mut total_harness = gen_result.harness_attempts;
            let mut expansion_used_flag = false;
            let mut last_page_count = render_result.as_ref().ok().map(|a| a.page_count);

            for pass in 0..MAX_EXPANSION_PASSES {
                let current_chars = current_md.chars().count();
                let gap = budget.target.saturating_sub(current_chars);

                if gap == 0 {
                    web_log(format!(
                        "job {} unified-pipeline: expansion pass {} — target reached",
                        job_id, pass,
                    ));
                    break;
                }

                // Check for overflow only after the output has reached the
                // allowed page count. If unused pages remain, expansion should
                // try to spill content onto those pages.
                let current_pages = last_page_count.unwrap_or(1).max(1);
                let chars_per_page = current_chars as f64 / current_pages as f64;
                let soft_max_per_page = budget.soft_max / output_max_pages.max(1);
                if current_pages >= output_max_pages && chars_per_page > soft_max_per_page as f64 {
                    web_log(format!(
                        "job {} unified-pipeline: expansion pass {} — stopping: chars/page={:.0} > soft_max_per_page={} (content likely overflowing)",
                        job_id, pass + 1, chars_per_page, soft_max_per_page,
                    ));
                    break;
                }

                let target_add = if gap < 3000 {
                    gap.max(2000)
                } else if gap <= 8000 {
                    gap
                } else {
                    gap.min(12000).max(4000)
                };

                web_log(format!(
                    "job {} unified-pipeline: expansion pass {}/{} — current={} target={} gap={} target_add={} chars/page={:.0}",
                    job_id, pass + 1, MAX_EXPANSION_PASSES, current_chars, budget.target, gap, target_add, chars_per_page,
                ));

                let turn3_user = build_expansion_turn3_prompt(target_add, &inventory, lang);
                match fork.turn_with_metadata(&turn3_user, 0.2, 81920).await {
                    Ok((expanded_text, finish_reason)) => {
                        let raw_chars = expanded_text.chars().count();
                        let expanded_md = strip_markdown_fences(&expanded_text);
                        let expanded_chars = expanded_md.chars().count();
                        let added = expanded_chars.saturating_sub(current_chars);

                        web_log(format!(
                            "job {} unified-pipeline: expansion pass {} result — raw={} stripped={} added={} target_add={} finish_reason={:?}",
                            job_id, pass + 1, raw_chars, expanded_chars, added, target_add, finish_reason,
                        ));

                        if expanded_chars <= current_chars {
                            web_log(format!(
                                "job {} unified-pipeline: expansion pass {} no growth — stopping",
                                job_id,
                                pass + 1,
                            ));
                            break;
                        }

                        current_md = expanded_md;
                        total_harness += 1;
                        expansion_used_flag = true;

                        if let Err(e) = fs::write(&markdown_path, &current_md) {
                            web_log(format!(
                                "job {} unified-pipeline: write expansion pass {} failed: {}",
                                job_id,
                                pass + 1,
                                e,
                            ));
                            break;
                        }
                        let exp_render = latex::render_cheatsheet(
                            &markdown_path.to_string_lossy(),
                            None,
                            &pdf_path.to_string_lossy(),
                            output_max_pages,
                        );
                        match exp_render {
                            Ok(artifact) => {
                                last_page_count = Some(artifact.page_count);
                                if artifact.page_count >= output_max_pages {
                                    web_log(format!(
                                        "job {} unified-pipeline: expansion pass {} filled to {} pages — done",
                                        job_id, pass + 1, artifact.page_count,
                                    ));
                                    break;
                                }
                            }
                            Err(e) => {
                                web_log(format!(
                                    "job {} unified-pipeline: expansion pass {} render failed: {}",
                                    job_id,
                                    pass + 1,
                                    e,
                                ));
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        web_log(format!(
                            "job {} unified-pipeline: expansion pass {} LLM failed: {}",
                            job_id,
                            pass + 1,
                            e,
                        ));
                        break;
                    }
                }
            }

            // ── Fallback: extract from Reference Digest if still underfilled ──
            let final_chars = current_md.chars().count();
            let fill_ratio = final_chars as f64 / budget.target as f64;
            if fill_ratio < MIN_FILL_RATIO && final_chars < budget.target {
                let gap = budget.target.saturating_sub(final_chars);
                web_log(format!(
                    "job {} unified-pipeline: fill ratio {:.1}% < {:.0}% — extracting {} chars from Ref Digest",
                    job_id, fill_ratio * 100.0, MIN_FILL_RATIO * 100.0, gap,
                ));
                use crate::utils::budget::extract_high_density_content;
                let extracted = extract_high_density_content(&digest_markdown, gap);
                let augmented = format!("{}\n\n{}", current_md, extracted);
                let aug_chars = augmented.chars().count();
                web_log(format!(
                    "job {} unified-pipeline: extracted fallback — final_chars={} (was {})",
                    job_id, aug_chars, final_chars,
                ));
                current_md = augmented;
                total_harness += 1;
                expansion_used_flag = true;
                if let Err(e) = fs::write(&markdown_path, &current_md) {
                    web_log(format!(
                        "job {} unified-pipeline: write fallback failed: {}",
                        job_id, e,
                    ));
                } else {
                    let fb_render = latex::render_cheatsheet(
                        &markdown_path.to_string_lossy(),
                        None,
                        &pdf_path.to_string_lossy(),
                        output_max_pages,
                    );
                    if let Ok(artifact) = fb_render {
                        last_page_count = Some(artifact.page_count);
                    }
                }
            }

            let mut result = gen_result.clone();
            result.markdown = current_md;
            result.generated_chars = result.markdown.chars().count();
            result.harness_attempts = total_harness;
            result.expansion_used = expansion_used_flag;
            (result, expansion_used_flag, last_page_count)
        } else {
            let page_count = render_result.as_ref().ok().map(|a| a.page_count);
            let underfilled_reason = if source_too_short {
                Some("source_too_short".to_string())
            } else {
                None
            };
            if let Some(ref reason) = underfilled_reason {
                web_log(format!(
                    "job {} unified-pipeline: expansion skipped — reason={} ref_digest_chars={} min_acceptable={}",
                    job_id, reason, ref_digest_chars, budget.min_acceptable,
                ));
            }
            let mut result = gen_result;
            result.underfilled_reason = underfilled_reason;
            (result, false, page_count)
        };

        // Final render.
        let final_render_result = if expansion_used {
            latex::render_cheatsheet(
                &markdown_path.to_string_lossy(),
                None,
                &pdf_path.to_string_lossy(),
                output_max_pages,
            )
        } else {
            render_result
        };

        // Find the reference digest output for linking.
        let ref_digest_id = ref_digest_outputs
            .first()
            .map(|o| o.id.clone())
            .unwrap_or_default();

        match final_render_result {
            Ok(artifact) => {
                let _ = process_store.update(process_id, |r| {
                    if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                        o.status = ProcessRecordStatus::Ready;
                        o.path = artifact.pdf_path.clone();
                        o.diff_path = None;
                        o.base_source_id = Some(ref_digest_id.clone());
                        o.last_error = None;
                        o.metadata = build_cheatsheet_metadata(
                            4,
                            4,
                            "complete",
                            output_max_pages,
                            artifact.page_count,
                            artifact.compression_attempts,
                            &artifact.template_used,
                            &markdown_path.to_string_lossy().to_string(),
                            &ref_digest_id,
                            final_result.target_chars,
                            final_result.generated_chars,
                            final_result.harness_attempts,
                            final_result.expansion_used,
                            final_page_count.unwrap_or(artifact.page_count),
                            final_result.underfilled_reason.as_deref(),
                            artifact.space_utilization.as_ref(),
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
                            output_max_pages,
                            0,
                            0,
                            "",
                            &markdown_path.to_string_lossy().to_string(),
                            &ref_digest_id,
                            final_result.target_chars,
                            final_result.generated_chars,
                            final_result.harness_attempts,
                            final_result.expansion_used,
                            0,
                            final_result.underfilled_reason.as_deref(),
                            None,
                        );
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-section path
// ---------------------------------------------------------------------------

/// Multi-section unified pipeline: per-source section digests → merge → Cheat Sheet → Expansion.
async fn run_cheat_pipeline_multi_section(
    process_id: &str,
    note_content: Option<String>,
    transcript_sources: &[&SourceRecord],
    max_pages: usize,
    source_counts: serde_json::Value,
    ref_digest_outputs: &[ProcessOutput],
    cheat_sheet_outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    job_id: &str,
) {
    // --- Phase 1: Generate per-source section digests ---

    for output in ref_digest_outputs {
        let _ = process_store.update(process_id, |r| {
            if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                o.metadata = serde_json::json!({
                    "progress_current": 1,
                    "progress_total": 2,
                    "progress_label": "generating section digests",
                });
            }
        });
    }

    let mut section_digests: Vec<String> = Vec::new();
    for src in transcript_sources {
        let src_content = match fs::read_to_string(&src.path) {
            Ok(c) => c,
            Err(e) => {
                web_log(format!(
                    "job {} unified-pipeline: failed to read source {}: {}",
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
                    "job {} unified-pipeline: section digest for {} failed: {}",
                    job_id, src.title, e
                ));
            }
        }
    }

    if section_digests.is_empty() {
        let err_msg = "All section digest generations failed.".to_string();
        for output in ref_digest_outputs.iter().chain(cheat_sheet_outputs.iter()) {
            let _ = process_store.update(process_id, |r| {
                if let Some(o) = r.outputs.iter_mut().find(|o| o.id == output.id) {
                    o.status = ProcessRecordStatus::Failed;
                    o.last_error = Some(err_msg.clone());
                }
            });
        }
        return;
    }

    // Build the merge prompt content.
    let combined_sections = section_digests
        .iter()
        .enumerate()
        .map(|(i, s)| format!("<!-- digest section {} -->\n{}", i + 1, s))
        .collect::<Vec<_>>()
        .join("\n\n");
    let truncated_combined: String = combined_sections.chars().take(160000).collect();

    // --- Phase 2: Merge → Cheat Sheet → Expansion (multi-turn conversation) ---

    let system_prompt = build_unified_pipeline_system_prompt(max_pages);
    let turn1_user = format!(
        "STAGE 1 — REFERENCE DIGEST (MERGE).\n\n\
         Merge and normalize these Reference Digest sections into one complete \
         Markdown document:\n\n{}\n\n\
         Return only Markdown.",
        truncated_combined
    );
    // Add note as assistant context if present (for style reference).
    let note_context = note_content.as_deref().map(|n| {
        format!(
            "Reference Note (style/structure reference):\n\n{}\n\n---",
            n
        )
    });

    let mut conversation = crate::llm::ChatConversation::new(&system_prompt);
    if let Some(ref nc) = note_context {
        // Inject note context before the merge instruction.
        let turn1_with_note = format!("{}\n\n{}", nc, turn1_user);
        let user_msg = turn1_with_note;
        // We build a single combined user message.
        let digest_markdown = match conversation.turn(&user_msg, 0.2, 81920).await {
            Ok(text) => strip_markdown_fences(&text),
            Err(e) => {
                let err_msg = format!("Reference Digest merge failed: {}", e);
                for output in ref_digest_outputs.iter().chain(cheat_sheet_outputs.iter()) {
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
        // --- Write digest, build inventory, process cheat sheet outputs ---
        write_digest_and_process_cheat_sheets(
            process_id,
            &digest_markdown,
            note_content.is_some(),
            &source_counts,
            ref_digest_outputs,
            cheat_sheet_outputs,
            max_pages,
            &conversation,
            process_store,
            job_id,
        )
        .await;
    } else {
        let digest_markdown = match conversation.turn(&turn1_user, 0.2, 81920).await {
            Ok(text) => strip_markdown_fences(&text),
            Err(e) => {
                let err_msg = format!("Reference Digest merge failed: {}", e);
                for output in ref_digest_outputs.iter().chain(cheat_sheet_outputs.iter()) {
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
        write_digest_and_process_cheat_sheets(
            process_id,
            &digest_markdown,
            note_content.is_some(),
            &source_counts,
            ref_digest_outputs,
            cheat_sheet_outputs,
            max_pages,
            &conversation,
            process_store,
            job_id,
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Shared helper: write digest & process cheat sheet outputs
// ---------------------------------------------------------------------------

/// Helper: write digest to disk and process all cheat sheet outputs.
///
/// Shared between single-pass and multi-section paths to avoid code duplication.
async fn write_digest_and_process_cheat_sheets(
    process_id: &str,
    digest_markdown: &str,
    _note_used: bool,
    source_counts: &serde_json::Value,
    ref_digest_outputs: &[ProcessOutput],
    cheat_sheet_outputs: &[ProcessOutput],
    max_pages: usize,
    conversation: &crate::llm::ChatConversation,
    process_store: &ProcessStore,
    job_id: &str,
) {
    let generated_chars = digest_markdown.chars().count();

    // Write digest to disk for each ref_digest output.
    for output in ref_digest_outputs {
        let output_path = process_store.output_path(process_id, &output.id);
        if let Some(parent) = output_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&output_path, digest_markdown) {
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
                o.last_error = None;
                o.metadata = serde_json::json!({
                    "progress_current": 2,
                    "progress_total": 2,
                    "progress_label": "complete",
                    "source_note_used": _note_used,
                    "source_counts": source_counts,
                    "generated_chars": generated_chars,
                });
                o.updated_at = ProcessRecord::now_iso();
            }
        });
    }

    // Persist conversation for downstream retries.
    let conv_path = process_store
        .process_dir(process_id)
        .join("conversation.json");
    if let Err(e) = conversation.save_to_file(&conv_path) {
        web_log(format!(
            "job {} unified-pipeline: failed to save conversation: {}",
            job_id, e,
        ));
    }

    // Build section inventory once.
    let (sections, inventory) = build_section_inventory(digest_markdown);
    let section_count = sections.len();
    let section_names = sections
        .iter()
        .map(|s| s.heading.clone())
        .collect::<Vec<_>>()
        .join(" | ");

    let ref_digest_chars = digest_markdown.chars().count();
    let ref_digest_id = ref_digest_outputs
        .first()
        .map(|o| o.id.clone())
        .unwrap_or_default();

    // Process each cheat sheet output.
    for output in cheat_sheet_outputs {
        let output_max_pages = output
            .metadata
            .get("max_pages")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(max_pages)
            .clamp(1, 20);

        let calibration = ensure_calibration(None, &process_store.project_dir());
        let effective = count_effective(digest_markdown);
        let budget = compute_budget(&calibration, &effective, output_max_pages);
        let lang = budget.language;
        let source_too_short = ref_digest_chars < budget.min_acceptable;

        web_log(format!(
            "job {} unified-pipeline (multi): output {} budget max_pages={} target={} soft_max={} ref_digest_chars={} source_too_short={} lang={:?}",
            job_id, output.id, output_max_pages, budget.target, budget.soft_max,
            ref_digest_chars, source_too_short, lang,
        ));

        let mut fork = conversation.fork();

        update_single_output_progress(
            process_store,
            process_id,
            &output.id,
            1,
            4,
            "condensing markdown (unified)",
        );

        // Turn 2: Cheat Sheet compression.
        let turn2_user = build_cheat_sheet_turn2_prompt(
            &inventory,
            &section_names,
            section_count,
            output_max_pages,
            &budget,
        );

        let gen_result = match fork.turn_with_metadata(&turn2_user, 0.2, 81920).await {
            Ok((text, finish_reason)) => {
                let md = strip_markdown_fences(&text);
                let chars = md.chars().count();
                web_log(format!(
                    "job {} unified-pipeline: Turn 2 — raw_chars={} stripped_chars={} target={} finish_reason={:?}",
                    job_id, text.chars().count(), chars, budget.target, finish_reason,
                ));
                CheatingSheetGenerationResult {
                    markdown: md,
                    target_chars: budget.target,
                    generated_chars: chars,
                    harness_attempts: 1,
                    expansion_used: false,
                    underfilled_reason: None,
                    language: lang,
                }
            }
            Err(e) => {
                web_log(format!(
                    "job {} unified-pipeline (multi): Turn 2 LLM failed for output {}, falling back: {}",
                    job_id, output.id, e,
                ));
                let fallback_md = latex::compress_content(digest_markdown, 1);
                let fallback_chars = fallback_md.chars().count();
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
                finish_cheating_sheet_render(
                    process_store,
                    process_id,
                    output,
                    ref_digest_outputs.first().unwrap_or(output),
                    &fallback_md,
                    &markdown_path,
                    output_max_pages,
                    budget.target,
                    fallback_chars,
                    0,
                    false,
                    Some("llm_failed".to_string()),
                );
                continue;
            }
        };

        // Write markdown to disk.
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

        // Render.
        update_single_output_progress(process_store, process_id, &output.id, 2, 4, "rendering PDF");
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
            output_max_pages,
        );

        // Expansion decision.
        let should_expand = match &render_result {
            Ok(artifact) => {
                let decision = should_attempt_expansion(
                    gen_result.generated_chars,
                    budget.min_acceptable,
                    artifact.page_count,
                    output_max_pages,
                    source_too_short,
                    artifact.space_utilization.as_ref(),
                );
                web_log(format!(
                    "job {} unified-pipeline (multi): first render page_count={} -> expand={}",
                    job_id, artifact.page_count, decision,
                ));
                decision
            }
            Err(e) => {
                web_log(format!(
                    "job {} unified-pipeline (multi): first render FAILED: {} — skipping expansion",
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
                "expanding content (unified)",
            );
            let current_chars = gen_result.generated_chars;
            let target_add_chars = (budget.target.saturating_sub(current_chars))
                .min(6000)
                .max(2000);

            let turn3_user = build_expansion_turn3_prompt(target_add_chars, &inventory, lang);

            match fork.turn_with_metadata(&turn3_user, 0.2, 81920).await {
                Ok((expanded_text, finish_reason)) => {
                    let expanded_md = strip_markdown_fences(&expanded_text);
                    let expanded_chars = expanded_md.chars().count();
                    web_log(format!(
                        "job {} unified-pipeline (multi): expansion — raw={} stripped={} target_add={} finish_reason={:?}",
                        job_id, expanded_text.chars().count(), expanded_chars, target_add_chars, finish_reason,
                    ));
                    if let Err(e) = fs::write(&markdown_path, &expanded_md) {
                        web_log(format!(
                            "job {} unified-pipeline (multi): failed to write expansion: {}",
                            job_id, e
                        ));
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
                            output_max_pages,
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
                                    "job {} unified-pipeline (multi): expansion render failed: {}",
                                    job_id, e
                                ));
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
                        "job {} unified-pipeline (multi): expansion LLM failed: {}",
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
            let mut result = gen_result;
            result.underfilled_reason = underfilled_reason;
            (result, false, page_count)
        };

        // Final render.
        let final_render_result = if expansion_used {
            latex::render_cheatsheet(
                &markdown_path.to_string_lossy(),
                None,
                &pdf_path.to_string_lossy(),
                output_max_pages,
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
                        o.base_source_id = Some(ref_digest_id.clone());
                        o.last_error = None;
                        o.metadata = build_cheatsheet_metadata(
                            4,
                            4,
                            "complete",
                            output_max_pages,
                            artifact.page_count,
                            artifact.compression_attempts,
                            &artifact.template_used,
                            &markdown_path.to_string_lossy().to_string(),
                            &ref_digest_id,
                            final_result.target_chars,
                            final_result.generated_chars,
                            final_result.harness_attempts,
                            final_result.expansion_used,
                            final_page_count.unwrap_or(artifact.page_count),
                            final_result.underfilled_reason.as_deref(),
                            artifact.space_utilization.as_ref(),
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
                            output_max_pages,
                            0,
                            0,
                            "",
                            &markdown_path.to_string_lossy().to_string(),
                            &ref_digest_id,
                            final_result.target_chars,
                            final_result.generated_chars,
                            final_result.harness_attempts,
                            final_result.expansion_used,
                            0,
                            final_result.underfilled_reason.as_deref(),
                            None,
                        );
                        o.updated_at = ProcessRecord::now_iso();
                    }
                });
            }
        }
    }
}
