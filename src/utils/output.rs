//! Process output helpers and path utilities.
//!
//! Functions for output kind parsing, path construction, progress reporting,
//! output kind expansion, and filesystem walking.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::web::processes::{
    ProcessOutput, ProcessOutputKind, ProcessRecord, ProcessStatus as ProcessRecordStatus,
    ProcessStore,
};

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

pub(crate) fn web_log(message: impl AsRef<str>) {
    eprintln!("[lecture-distill:web] {}", message.as_ref());
}

// ---------------------------------------------------------------------------
// Output kind helpers
// ---------------------------------------------------------------------------

pub fn parse_process_output_kind(kind: &str) -> Option<ProcessOutputKind> {
    match kind {
        "note_patch" => Some(ProcessOutputKind::NotePatch),
        "builtin.note_patch.note" => Some(ProcessOutputKind::NotePatch),
        "reference_digest" => Some(ProcessOutputKind::ReferenceDigest),
        "builtin.ref_cheat.ref" => Some(ProcessOutputKind::ReferenceDigest),
        "cheating_sheet" => Some(ProcessOutputKind::CheatingSheet),
        "builtin.ref_cheat.cheat" => Some(ProcessOutputKind::CheatingSheet),
        _ => None,
    }
}

pub fn process_output_title(kind: &ProcessOutputKind) -> &'static str {
    match kind {
        ProcessOutputKind::NotePatch => "Note Patch",
        ProcessOutputKind::ReferenceDigest => "Reference Digest",
        ProcessOutputKind::CheatingSheet => "Cheating Sheet",
    }
}

pub fn process_output_path_for(
    process_store: &ProcessStore,
    process_id: &str,
    output_id: &str,
    kind: &ProcessOutputKind,
) -> PathBuf {
    match kind {
        ProcessOutputKind::NotePatch | ProcessOutputKind::ReferenceDigest => {
            process_store.output_path(process_id, output_id)
        }
        ProcessOutputKind::CheatingSheet => process_store
            .process_dir(process_id)
            .join(format!("{}.pdf", output_id)),
    }
}

pub fn cheating_sheet_markdown_path(
    process_store: &ProcessStore,
    process_id: &str,
    output_id: &str,
) -> PathBuf {
    process_store
        .process_dir(process_id)
        .join(format!("{}.cheatsheet.md", output_id))
}

/// Body for requesting an output kind in `CreateProcessBody`.
#[derive(Debug, serde::Deserialize)]
pub struct CreateProcessOutputBody {
    pub kind: String,
    #[serde(default)]
    pub max_pages: Option<usize>,
}

/// Expand requested output kinds, automatically adding dependencies.
///
/// For example, if CheatingSheet is requested without ReferenceDigest,
/// a ReferenceDigest output is automatically added since CheatingSheet
/// depends on it.
pub fn expand_output_kinds(
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
    if has_note_patch {
        kinds.push((ProcessOutputKind::NotePatch, 2));
    }
    if has_reference_digest {
        kinds.push((ProcessOutputKind::ReferenceDigest, 0));
    }
    if has_cheating_sheet {
        if !has_reference_digest {
            // Auto-add Reference Digest as dependency for Cheat Sheet.
            kinds.push((ProcessOutputKind::ReferenceDigest, 0));
        }
        kinds.push((ProcessOutputKind::CheatingSheet, cheating_sheet_pages));
    }
    Ok(kinds)
}

// ---------------------------------------------------------------------------
// Progress helpers
// ---------------------------------------------------------------------------

pub(crate) fn update_single_output_progress(
    process_store: &ProcessStore,
    process_id: &str,
    output_id: &str,
    current: usize,
    total: usize,
    label: &str,
) {
    let current = current.min(total);
    let total = total.max(1);
    let label = label.to_string();
    let _ = process_store.update(process_id, |r| {
        if let Some(output) = r.outputs.iter_mut().find(|o| o.id == output_id) {
            let mut metadata = output.metadata.clone();
            if !metadata.is_object() {
                metadata = serde_json::json!({});
            }
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert("progress_current".to_string(), serde_json::json!(current));
                obj.insert("progress_total".to_string(), serde_json::json!(total));
                obj.insert("progress_label".to_string(), serde_json::json!(label));
            }
            output.metadata = metadata;
            output.updated_at = ProcessRecord::now_iso();
        }
    });
}

pub(crate) fn update_note_patch_progress(
    process_store: &ProcessStore,
    process_id: &str,
    outputs: &[ProcessOutput],
    current: usize,
    total: usize,
    label: &str,
) {
    let output_ids: Vec<String> = outputs.iter().map(|o| o.id.clone()).collect();
    let current = current.min(total);
    let total = total.max(1);
    let label = label.to_string();
    let _ = process_store.update(process_id, |r| {
        r.status = ProcessRecordStatus::Processing;
        for output in &mut r.outputs {
            if output_ids.contains(&output.id) {
                output.status = ProcessRecordStatus::Processing;
                output.last_error = None;
                output.metadata = serde_json::json!({
                    "progress_current": current,
                    "progress_total": total,
                    "progress_label": label,
                });
                output.updated_at = ProcessRecord::now_iso();
            }
        }
    });
}

pub(crate) fn update_ref_digest_progress(
    process_store: &ProcessStore,
    process_id: &str,
    outputs: &[ProcessOutput],
    current: usize,
    total: usize,
    label: &str,
) {
    // Same pattern as update_note_patch_progress.
    update_note_patch_progress(process_store, process_id, outputs, current, total, label);
}

pub fn update_process_terminal_status(
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
// Output directory walker
// ---------------------------------------------------------------------------

pub(crate) fn walk_output_dir(
    base: &Path,
    dir: &Path,
    groups: &mut HashMap<String, Vec<serde_json::Value>>,
) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                // Skip internal source artifact directories and job dirs.
                if file_name == "sources"
                    || file_name == "transcripts"
                    || file_name == "jobs"
                    || file_name == "processes"
                {
                    continue;
                }
                walk_output_dir(base, &path, groups);
            } else if path.is_file() {
                // Skip sources.json at the artifacts level.
                if file_name == "sources.json" {
                    continue;
                }
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_string();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let category = match ext.as_str() {
                    "json" => "transcripts",
                    "srt" => "transcripts",
                    "md" => "notes",
                    "pdf" => "outputs",
                    "tex" => "outputs",
                    _ => "other",
                };
                groups
                    .entry(category.to_string())
                    .or_default()
                    .push(serde_json::json!({
                        "name": file_name,
                        "path": rel,
                        "size": size,
                        "ext": ext,
                    }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested_output(kind: &str, max_pages: Option<usize>) -> CreateProcessOutputBody {
        CreateProcessOutputBody {
            kind: kind.to_string(),
            max_pages,
        }
    }

    #[test]
    fn test_reference_digest_output_kind_parse_and_display() {
        let kind = parse_process_output_kind("reference_digest").unwrap();
        assert_eq!(kind, ProcessOutputKind::ReferenceDigest);
        assert_eq!(process_output_title(&kind), "Reference Digest");
    }

    #[test]
    fn test_reference_digest_dependency_expansion_for_cheating_sheet() {
        // CheatingSheet without ReferenceDigest → auto-add ReferenceDigest.
        let kinds = expand_output_kinds(&[requested_output("cheating_sheet", Some(2))]).unwrap();
        let has_rd = kinds
            .iter()
            .any(|(k, _)| *k == ProcessOutputKind::ReferenceDigest);
        let has_cs = kinds
            .iter()
            .any(|(k, _)| *k == ProcessOutputKind::CheatingSheet);
        assert!(has_rd, "ReferenceDigest should be auto-added");
        assert!(has_cs, "CheatingSheet should be present");
    }

    #[test]
    fn test_reference_digest_dependency_expansion_keeps_explicit_reference_digest() {
        let kinds = expand_output_kinds(&[
            requested_output("reference_digest", None),
            requested_output("cheating_sheet", Some(2)),
        ])
        .unwrap();
        assert_eq!(kinds.len(), 2);
        assert_eq!(kinds[0].0, ProcessOutputKind::ReferenceDigest);
        assert_eq!(kinds[1].0, ProcessOutputKind::CheatingSheet);
    }

    #[test]
    fn test_reference_digest_dependency_expansion_keeps_explicit_note_patch_parallel() {
        let kinds = expand_output_kinds(&[
            requested_output("note_patch", None),
            requested_output("cheating_sheet", Some(2)),
        ])
        .unwrap();
        let has_np = kinds
            .iter()
            .any(|(k, _)| *k == ProcessOutputKind::NotePatch);
        let has_rd = kinds
            .iter()
            .any(|(k, _)| *k == ProcessOutputKind::ReferenceDigest);
        let has_cs = kinds
            .iter()
            .any(|(k, _)| *k == ProcessOutputKind::CheatingSheet);
        assert!(has_np, "NotePatch should be present");
        assert!(has_rd, "ReferenceDigest should be auto-added");
        assert!(has_cs, "CheatingSheet should be present");
    }
}
