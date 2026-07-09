//! Cheating Sheet processor: compress Reference Digest into exam cheat sheet PDF.
//!
//! Supports multiple paths:
//! - **Fresh LLM generation**: standalone `generate_cheating_sheet_markdown`
//! - **Unified multi-turn**: continues a saved `ChatConversation` from the
//!   unified pipeline (Ref Digest → Turn 2 → Expansion Turn 3)
//! - **Fallback**: direct LaTeX compression without LLM when LLM fails
//! - **Expansion**: attempts to add missing content when pages are underfilled

use std::fs;

use anyhow::{bail, Result};

use crate::latex;
use crate::llm::{self, ChatConversation};
use crate::prompts::cheat_sheet::{
    build_cheat_sheet_prompts, build_cheat_sheet_turn2_prompt, build_expansion_prompt,
    build_expansion_turn3_prompt,
};
use crate::utils::budget::{
    build_cheatsheet_metadata, build_section_inventory, compute_budget, count_effective,
    extract_high_density_content, should_attempt_expansion, truncate_ref_digest_for_cheatsheet,
    CheatingSheetGenerationResult,
};
use crate::utils::calibration::{ensure_calibration, CalibrationData};
use crate::utils::markdown::strip_markdown_fences;
use crate::utils::output::{
    cheating_sheet_markdown_path, process_output_path_for, update_single_output_progress, web_log,
};
use crate::web::plugins::ref_cheat_default_template_path;
use crate::web::processes::{
    ProcessOutput, ProcessOutputKind, ProcessRecord, ProcessStatus as ProcessRecordStatus,
    ProcessStore,
};

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run Cheating Sheet generation for a set of outputs.
///
/// Loads the Reference Digest output, attempts LLM-based condensation (fresh
/// or continuing a saved conversation), renders to PDF, optionally expands
/// underfilled pages, and writes final artifacts.
///
/// Falls back to direct LaTeX compression when the LLM call fails.
pub(crate) async fn run_cheating_sheet_outputs(
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

    // Attempt to load a saved multi-turn conversation from a prior unified
    // pipeline run.  If present, we can continue the conversation instead of
    // starting a fresh LLM call — this preserves the Reference Digest in
    // context and enables DeepSeek prefix-cache hits.
    let conv_path = process_store
        .process_dir(process_id)
        .join("conversation.json");
    let saved_conversation = match ChatConversation::load_from_file(&conv_path) {
        Ok(Some(conv)) => {
            web_log(format!(
                "job {} cheating-sheet: loaded saved conversation ({} messages)",
                job_id,
                conv.messages().len(),
            ));
            Some(conv)
        }
        Ok(None) => None,
        Err(e) => {
            web_log(format!(
                "job {} cheating-sheet: failed to load conversation, starting fresh: {}",
                job_id, e,
            ));
            None
        }
    };
    let template_path = ref_cheat_default_template_path(&process_store.project_dir());
    let template_path_str = template_path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());

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

        // Load or auto-calibrate the template and compute a language-aware budget.
        let calibration =
            ensure_calibration(template_path.as_deref(), &process_store.project_dir());
        let effective = count_effective(&ref_digest_markdown);
        let budget = compute_budget(&calibration, &effective, max_pages);
        let lang = budget.language;
        let source_too_short = ref_digest_chars < budget.min_acceptable;

        web_log(format!(
            "job {} cheating-sheet: budget max_pages={} target={} soft_max={} min_acceptable={} ref_digest_chars={} lang={:?} source_too_short={}",
            job_id, max_pages, budget.target, budget.soft_max,
            budget.min_acceptable, ref_digest_chars, lang, source_too_short,
        ));

        // Saved fork with Turn 2 exchange, for use in Turn 3 (Expansion).
        let mut saved_turn2_fork: Option<ChatConversation> = None;

        let gen_result = {
            let llm_result = if let Some(ref conv) = saved_conversation {
                // Continue the saved conversation (Turn 2).
                let (sections, inventory) = build_section_inventory(&ref_digest_markdown);
                let section_count = sections.len();
                let section_names: String = sections
                    .iter()
                    .map(|s| s.heading.clone())
                    .collect::<Vec<_>>()
                    .join(" | ");
                let turn2_user = build_cheat_sheet_turn2_prompt(
                    &inventory,
                    &section_names,
                    section_count,
                    max_pages,
                    &budget,
                );
                let mut fork = conv.fork();
                let result = fork.turn_with_metadata(&turn2_user, 0.2, 81920).await.map(|(text, finish_reason)| {
                    let raw_chars = text.chars().count();
                    let md = strip_markdown_fences(&text);
                    let chars = md.chars().count();
                    web_log(format!(
                        "job {} cheating-sheet: Turn 2 LLM — raw_chars={} stripped_chars={} target={} finish_reason={:?}",
                        job_id, raw_chars, chars, budget.target, finish_reason,
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
                });
                // Save fork for potential Turn 3 (Expansion).
                saved_turn2_fork = Some(fork);
                result
            } else {
                generate_cheating_sheet_markdown(&ref_digest_markdown, max_pages, &calibration)
                    .await
            };

            match llm_result {
                Ok(result) => result,
                Err(e) => {
                    web_log(format!(
                        "job {} cheating-sheet: LLM condensation failed, rendering compressed Reference Digest directly: {}",
                        job_id, e
                    ));
                    let fallback_md = latex::compress_content(&ref_digest_markdown, 1);
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
                        ref_digest,
                        &fallback_md,
                        &markdown_path,
                        max_pages,
                        budget.target,
                        fallback_chars,
                        0,
                        false,
                        Some("llm_failed".to_string()),
                    );
                    continue;
                }
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

        update_single_output_progress(process_store, process_id, &output.id, 2, 4, "rendering PDF");
        let pdf_path = process_output_path_for(
            process_store,
            process_id,
            &output.id,
            &ProcessOutputKind::CheatingSheet,
        );
        let render_result = latex::render_cheatsheet(
            &markdown_path.to_string_lossy(),
            template_path_str.as_deref(),
            &pdf_path.to_string_lossy(),
            max_pages,
        );

        // Determine if expansion is needed.
        let should_expand = match &render_result {
            Ok(artifact) => {
                let decision = should_attempt_expansion(
                    gen_result.generated_chars,
                    budget.min_acceptable,
                    artifact.page_count,
                    max_pages,
                    source_too_short,
                    artifact.space_utilization.as_ref(),
                );
                web_log(format!(
                    "job {} cheating-sheet: first render page_count={} max_pages={} generated_chars={} source_too_short={} space_util={:?} -> expand={}",
                    job_id, artifact.page_count, max_pages, gen_result.generated_chars,
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

            let (_sections, inventory) = build_section_inventory(&ref_digest_markdown);

            // ── Multi-pass expansion loop ──
            // Try up to 3 expansion passes.  Each pass targets the remaining
            // gap between current content and the budget target.  If after all
            // passes the content is still < 70% of target, fill the remaining
            // gap by extracting high-density content from the Reference Digest.
            const MAX_EXPANSION_PASSES: usize = 3;
            const MIN_FILL_RATIO: f64 = 0.70; // 70% of target before fallback

            let mut current_md = gen_result.markdown.clone();
            let mut total_harness = gen_result.harness_attempts;
            let mut expansion_used_flag = false;
            let mut last_page_count = render_result.as_ref().ok().map(|a| a.page_count);

            for pass in 0..MAX_EXPANSION_PASSES {
                let current_chars = current_md.chars().count();
                let gap = budget.target.saturating_sub(current_chars);

                // Stop if we've reached or exceeded the target.
                if gap == 0 {
                    web_log(format!(
                        "job {} cheating-sheet: expansion pass {} — target reached ({} chars >= {} target)",
                        job_id, pass, current_chars, budget.target,
                    ));
                    break;
                }

                // Scale target_add_chars proportionally to the gap.
                let target_add = if gap < 3000 {
                    gap.max(2000)
                } else if gap <= 8000 {
                    gap
                } else {
                    gap.min(12000).max(4000)
                };

                // Check for overflow only after the output has reached the
                // allowed page count. If unused pages remain, expansion should
                // try to spill content onto those pages.
                let soft_max_per_page = budget.soft_max / max_pages.max(1);
                let current_pages = last_page_count.unwrap_or(1).max(1);
                let chars_per_page = current_chars as f64 / current_pages as f64;
                if current_pages >= max_pages && chars_per_page > soft_max_per_page as f64 {
                    web_log(format!(
                        "job {} cheating-sheet: expansion pass {} — stopping: chars/page={:.0} > soft_max_per_page={} (content likely overflowing)",
                        job_id, pass + 1, chars_per_page, soft_max_per_page,
                    ));
                    break;
                }

                web_log(format!(
                    "job {} cheating-sheet: expansion pass {}/{} — current_chars={} target={} gap={} target_add={} chars/page={:.0}",
                    job_id, pass + 1, MAX_EXPANSION_PASSES, current_chars, budget.target, gap, target_add, chars_per_page,
                ));

                let expanded_result = if let Some(ref mut fork) = saved_turn2_fork {
                    let turn3_user = build_expansion_turn3_prompt(target_add, &inventory, lang);
                    fork.turn_with_metadata(&turn3_user, 0.2, 81920)
                        .await
                        .map(|(text, fr)| {
                            web_log(format!(
                                "job {} cheating-sheet: expansion pass {} — finish_reason={:?}",
                                job_id,
                                pass + 1,
                                fr,
                            ));
                            text
                        })
                } else {
                    let ref_digest_excerpt =
                        truncate_ref_digest_for_cheatsheet(&ref_digest_markdown, 90000).0;
                    let (exp_system, exp_user) = build_expansion_prompt(
                        &current_md,
                        &inventory,
                        &ref_digest_excerpt,
                        target_add,
                        lang,
                    );
                    llm::chat_completion_with_metadata(
                        &[
                            crate::llm::ChatMessage { role: "system".into(), content: exp_system },
                            crate::llm::ChatMessage { role: "user".into(), content: exp_user },
                        ],
                        0.2, 81920, None,
                    ).await.map(|(text, fr)| {
                        web_log(format!(
                            "job {} cheating-sheet: expansion pass {} (standalone) — finish_reason={:?}",
                            job_id, pass + 1, fr,
                        ));
                        text
                    })
                };

                match expanded_result {
                    Ok(expanded_text) => {
                        let raw_chars = expanded_text.chars().count();
                        let expanded_md = strip_markdown_fences(&expanded_text);
                        let expanded_chars = expanded_md.chars().count();
                        let added = expanded_chars.saturating_sub(current_chars);

                        web_log(format!(
                            "job {} cheating-sheet: expansion pass {} result — raw={} stripped={} added={} target_add={}",
                            job_id, pass + 1, raw_chars, expanded_chars, added, target_add,
                        ));

                        if expanded_chars <= current_chars {
                            web_log(format!(
                                "job {} cheating-sheet: expansion pass {} produced no growth — stopping",
                                job_id, pass + 1,
                            ));
                            break;
                        }

                        current_md = expanded_md;
                        total_harness += 1;
                        expansion_used_flag = true;

                        // Write and render intermediate result.
                        if let Err(e) = fs::write(&markdown_path, &current_md) {
                            web_log(format!(
                                "job {} cheating-sheet: failed to write expansion pass {}: {}",
                                job_id,
                                pass + 1,
                                e,
                            ));
                            break;
                        }
                        let exp_render = latex::render_cheatsheet(
                            &markdown_path.to_string_lossy(),
                            template_path_str.as_deref(),
                            &pdf_path.to_string_lossy(),
                            max_pages,
                        );
                        match exp_render {
                            Ok(artifact) => {
                                last_page_count = Some(artifact.page_count);
                                // If pages are now at max, we're done.
                                if artifact.page_count >= max_pages {
                                    web_log(format!(
                                        "job {} cheating-sheet: expansion pass {} filled to {} pages — done",
                                        job_id, pass + 1, artifact.page_count,
                                    ));
                                    break;
                                }
                            }
                            Err(e) => {
                                web_log(format!(
                                    "job {} cheating-sheet: expansion pass {} render failed: {}",
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
                            "job {} cheating-sheet: expansion pass {} LLM failed: {}",
                            job_id,
                            pass + 1,
                            e,
                        ));
                        break;
                    }
                }
            }

            // ── Fallback: extract content from Reference Digest if still underfilled ──
            let final_chars = current_md.chars().count();
            let fill_ratio = final_chars as f64 / budget.target as f64;
            if fill_ratio < MIN_FILL_RATIO && final_chars < budget.target {
                let gap = budget.target.saturating_sub(final_chars);
                web_log(format!(
                    "job {} cheating-sheet: fill ratio {:.1}% < {:.0}% — extracting {} chars from Reference Digest",
                    job_id, fill_ratio * 100.0, MIN_FILL_RATIO * 100.0, gap,
                ));
                let extracted = extract_high_density_content(&ref_digest_markdown, gap);
                let augmented = format!("{}\n\n{}", current_md, extracted);
                let aug_chars = augmented.chars().count();
                web_log(format!(
                    "job {} cheating-sheet: extracted fallback — final_chars={} (was {})",
                    job_id, aug_chars, final_chars,
                ));
                current_md = augmented;
                total_harness += 1;
                expansion_used_flag = true;
                if let Err(e) = fs::write(&markdown_path, &current_md) {
                    web_log(format!(
                        "job {} cheating-sheet: failed to write fallback markdown: {}",
                        job_id, e,
                    ));
                } else {
                    let fb_render = latex::render_cheatsheet(
                        &markdown_path.to_string_lossy(),
                        template_path_str.as_deref(),
                        &pdf_path.to_string_lossy(),
                        max_pages,
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
                    "job {} cheating-sheet: expansion skipped — reason={} ref_digest_chars={} min_acceptable={}",
                    job_id, reason, ref_digest_chars, budget.min_acceptable,
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
                template_path_str.as_deref(),
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
// Rendering helpers
// ---------------------------------------------------------------------------

/// Finish a cheat sheet render: write the markdown, render to PDF, and update
/// the process record with the result.
///
/// Used as a fallback path when LLM condensation fails and we render the
/// compressed Reference Digest directly.
pub(crate) fn finish_cheating_sheet_render(
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
    let template_path = ref_cheat_default_template_path(&process_store.project_dir());
    let template_path_str = template_path
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let render_result = latex::render_cheatsheet(
        &markdown_path.to_string_lossy(),
        template_path_str.as_deref(),
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
                        None,
                    );
                    o.updated_at = ProcessRecord::now_iso();
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// LLM generation (standalone, non-unified path)
// ---------------------------------------------------------------------------

/// Generate a cheating sheet markdown from a Reference Digest via LLM.
///
/// This is the standalone (fresh) generation path used when no saved
/// conversation exists from a unified pipeline run.
///
/// Accepts `calib` for language-aware budget computation.
pub(crate) async fn generate_cheating_sheet_markdown(
    ref_digest_markdown: &str,
    max_pages: usize,
    calib: &CalibrationData,
) -> Result<CheatingSheetGenerationResult> {
    if !llm::is_available() {
        bail!("LLM is not available");
    }

    let effective = count_effective(ref_digest_markdown);
    let budget = compute_budget(calib, &effective, max_pages);
    let (system_prompt, user_prompt) =
        build_cheat_sheet_prompts(ref_digest_markdown, max_pages, calib);

    let (text, finish_reason) = llm::chat_completion_with_metadata(
        &[
            llm::ChatMessage {
                role: "system".into(),
                content: system_prompt.clone(),
            },
            llm::ChatMessage {
                role: "user".into(),
                content: user_prompt.clone(),
            },
        ],
        0.2,
        81920,
        None,
    )
    .await?;
    let raw_chars = text.chars().count();
    let markdown = strip_markdown_fences(&text);
    let generated_chars = markdown.chars().count();
    log::info!(
        "generate_cheating_sheet_markdown: raw_chars={} stripped_chars={} target={} finish_reason={:?}",
        raw_chars, generated_chars, budget.target, finish_reason,
    );

    // Retry if severely underfilled.
    let (markdown, generated_chars, harness_attempts) =
        if crate::utils::budget::should_retry_cheat_sheet_generation(
            generated_chars,
            budget.target,
            ref_digest_markdown.chars().count(),
        ) {
            log::info!(
                "generate_cheating_sheet_markdown: retrying — {} chars is {:.0}% of target {}",
                generated_chars,
                (generated_chars as f64 / budget.target as f64) * 100.0,
                budget.target,
            );
            let retry_user = format!(
                "{}\n\nIMPORTANT: Your previous response was only {} characters. \
                 The target is {} characters. Please generate a COMPLETE cheat sheet \
                 that fills the target. Include ALL key concepts, formulas, and exam \
                 points from the Reference Digest. Be thorough and comprehensive.",
                user_prompt, generated_chars, budget.target,
            );
            match llm::chat_completion_with_metadata(
                &[
                    llm::ChatMessage {
                        role: "system".into(),
                        content: system_prompt.clone(),
                    },
                    llm::ChatMessage {
                        role: "user".into(),
                        content: retry_user,
                    },
                ],
                0.2,
                81920,
                None,
            )
            .await
            {
                Ok((retry_text, fr)) => {
                    let retry_raw = retry_text.chars().count();
                    let retry_md = strip_markdown_fences(&retry_text);
                    let retry_chars = retry_md.chars().count();
                    log::info!(
                        "generate_cheating_sheet_markdown: retry result — raw={} stripped={} finish_reason={:?}",
                        retry_raw, retry_chars, fr,
                    );
                    (retry_md, retry_chars, 2)
                }
                Err(e) => {
                    log::warn!(
                        "generate_cheating_sheet_markdown: retry failed: {} — using original",
                        e
                    );
                    (markdown, generated_chars, 1)
                }
            }
        } else {
            (markdown, generated_chars, 1)
        };

    Ok(CheatingSheetGenerationResult {
        markdown,
        target_chars: budget.target,
        generated_chars,
        harness_attempts,
        expansion_used: false,
        underfilled_reason: None,
        language: budget.language,
    })
}
