//! Course Note Patch processor: structured patching of Markdown notes using
//! course transcript indexes and BM25 retrieval, with fallback to full note
//! generation when no base note exists.
//!
//! This is the course-transcript variant of note patching.  It splits the
//! note into sections, retrieves matching transcript context per section via
//! the course index, generates per-section patches with an LLM, and applies
//! the patch list to produce the final updated note.

use std::fs;

use crate::diff;
use crate::llm::compact_transcript_for_llm;
use crate::processors::course_note::run_course_note_generation;
use crate::processors::note_patch::generate_section_patches;
use crate::utils::markdown::apply_structured_patches_to_note;
use crate::utils::output::{update_note_patch_progress, web_log};
use crate::web::course::{
    bm25_search, read_indexes, read_manifest, split_note_sections, truncate_chars, RetrievalMatch,
    RetrievalTrace, TimestampRange,
};
use crate::web::processes::{
    ProcessOutput, ProcessRecord, ProcessStatus as ProcessRecordStatus, ProcessStore,
};
use crate::web::sources::SourceRecord;

pub(crate) async fn run_course_note_patch(
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
    let mut all_patches: Vec<crate::artifacts::PatchEntry> = Vec::new();
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
        let unified = diff::unified_diff(&base_note, &markdown_output, 3);
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
