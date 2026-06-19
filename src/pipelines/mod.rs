//! Pipeline dispatch strategies extracted from `crate::web::app`.
//!
//! Pipelines orchestrate multiple processors — for example, the unified
//! pipeline runs Reference Digest and Cheat Sheet in a single multi-turn
//! LLM conversation for DeepSeek prefix caching.

pub mod separate;
pub mod unified_ref_cheat;

use crate::processors::cheating_sheet::run_cheating_sheet_outputs;
use crate::processors::note_patch::run_note_patch;
use crate::processors::reference_digest::run_reference_digest_outputs;
use crate::web::processes::{ProcessOutput, ProcessOutputKind, ProcessStore};
use crate::web::sources::SourceStore;

use unified_ref_cheat::run_unified_cheat_pipeline;

// ---------------------------------------------------------------------------
// Top-level dispatch
// ---------------------------------------------------------------------------

/// Top-level process-outputs dispatcher.
///
/// Filters outputs into Note Patch, Reference Digest, and Cheating Sheet
/// groups, then routes them to the appropriate pipeline:
///
/// - When **both** Reference Digest and Cheating Sheet outputs are requested,
///   the unified multi-turn pipeline is used (enables DeepSeek prefix caching).
/// - Otherwise, each processor runs independently (the "separate" path).
pub async fn run_process_outputs(
    process_id: &str,
    source_ids: &[String],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
    // 1) Note Patch outputs if requested.
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

    // 2) Reference Digest outputs.
    let reference_digest_outputs: Vec<ProcessOutput> = outputs
        .iter()
        .filter(|o| o.kind == ProcessOutputKind::ReferenceDigest)
        .cloned()
        .collect();

    // 3) Cheating Sheet outputs.
    let cheating_sheet_outputs: Vec<ProcessOutput> = outputs
        .iter()
        .filter(|o| o.kind == ProcessOutputKind::CheatingSheet)
        .cloned()
        .collect();

    // When both Ref Digest AND Cheat Sheet are requested, use the unified
    // multi-turn pipeline for DeepSeek prefix caching.
    if !reference_digest_outputs.is_empty() && !cheating_sheet_outputs.is_empty() {
        run_unified_cheat_pipeline(
            process_id,
            source_ids,
            &reference_digest_outputs,
            &cheating_sheet_outputs,
            process_store,
            source_store,
            job_id,
        )
        .await;
    } else {
        run_process_outputs_separate(
            process_id,
            source_ids,
            &reference_digest_outputs,
            &cheating_sheet_outputs,
            process_store,
            source_store,
            job_id,
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// Separate-path dispatch
// ---------------------------------------------------------------------------

/// Run Reference Digest and/or Cheating Sheet outputs independently.
///
/// This is the "else" branch of `run_process_outputs`: used when only one of
/// the two output kinds is requested (or neither).
pub async fn run_process_outputs_separate(
    process_id: &str,
    source_ids: &[String],
    reference_digest_outputs: &[ProcessOutput],
    cheating_sheet_outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
    if !reference_digest_outputs.is_empty() {
        run_reference_digest_outputs(
            process_id,
            source_ids,
            reference_digest_outputs,
            process_store,
            source_store,
            job_id,
        )
        .await;
    }
    if !cheating_sheet_outputs.is_empty() {
        run_cheating_sheet_outputs(process_id, cheating_sheet_outputs, process_store, job_id).await;
    }
}
