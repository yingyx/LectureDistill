//! Notes loading and patching module.
//!
//! Loads Markdown notes and transcript artifacts, then patches the notes
//! with information extracted from video transcripts.  Supports both
//! LLM-based and deterministic patching strategies.
//!
//! Equivalent to the Python `notes.py` module.

use crate::artifacts::{KeepLevel, NoteArtifact, PatchArtifact, PatchEntry, TranscriptArtifact};
use crate::llm;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Regex singleton
// ---------------------------------------------------------------------------

fn heading_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^(#{1,6})\s+(.+)$").unwrap())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load all `TranscriptArtifact` JSON files from a directory.
///
/// Skips files that fail to parse (prints a warning to stderr).  Returns an
/// empty `Vec` when the directory does not exist.
pub fn load_transcripts(transcripts_dir: &str) -> Result<Vec<TranscriptArtifact>> {
    let dir = Path::new(transcripts_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut transcripts = Vec::new();
    for entry in fs::read_dir(dir).context("Failed to read transcripts directory")? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<TranscriptArtifact>(&content) {
                Ok(artifact) => transcripts.push(artifact),
                Err(e) => eprintln!(
                    "Warning: failed to parse transcript {}: {}",
                    path.display(),
                    e
                ),
            },
            Err(e) => eprintln!("Warning: failed to read {}: {}", path.display(), e),
        }
    }

    Ok(transcripts)
}

/// Load a Markdown notes file and extract its headings.
pub fn load_notes(notes_path: &str) -> Result<NoteArtifact> {
    let content = fs::read_to_string(notes_path).context("Failed to read notes file")?;
    let headings = extract_headings(&content);
    Ok(NoteArtifact {
        path: notes_path.to_string(),
        content,
        headings,
    })
}

/// Extract Markdown heading texts (ATX-style `#` through `######`) from
/// `content`.  Returns only the heading text without the leading `#` prefix.
pub fn extract_headings(content: &str) -> Vec<String> {
    heading_regex()
        .captures_iter(content)
        .filter_map(|caps| caps.get(2).map(|m| m.as_str().to_string()))
        .collect()
}

/// Main entry point for patching notes with transcript data.
///
/// 1. Loads notes and transcripts.
/// 2. If no transcripts are found, writes the notes unchanged with a conflict
///    message and returns the `PatchArtifact`.
/// 3. Otherwise, patches via LLM (when `llm::is_available()`) with
///    deterministic `fallback` on error, or uses deterministic patching
///    directly when the LLM is not available.
/// 4. Applies patches to produce the final Markdown and writes both output
///    files.
pub async fn patch_notes(
    notes_path: &str,
    transcripts_dir: &str,
    output_notes_path: &str,
    output_patches_path: &str,
) -> Result<PatchArtifact> {
    let note = load_notes(notes_path)?;
    let transcripts = load_transcripts(transcripts_dir)?;

    if transcripts.is_empty() {
        let conflict_msg = format!("No transcript files found in {}", transcripts_dir);
        let patch_artifact = PatchArtifact {
            source_notes_path: notes_path.to_string(),
            source_transcripts: Vec::new(),
            patches: Vec::new(),
            conflicts: vec![conflict_msg],
            patched_at: chrono::Utc::now().to_rfc3339(),
        };
        write_outputs(
            &note.content,
            &patch_artifact,
            output_notes_path,
            output_patches_path,
        )?;
        return Ok(patch_artifact);
    }

    let patch_artifact = if llm::is_available() {
        match llm_patch(&note, &transcripts).await {
            Ok(pa) => pa,
            Err(e) => {
                eprintln!(
                    "LLM patching failed: {}. Falling back to deterministic patching.",
                    e
                );
                deterministic_patch(&note, &transcripts)
            }
        }
    } else {
        deterministic_patch(&note, &transcripts)
    };

    let patched_content = apply_patches(&note.content, &patch_artifact);
    write_outputs(
        &patched_content,
        &patch_artifact,
        output_notes_path,
        output_patches_path,
    )?;

    Ok(patch_artifact)
}

/// Patch notes using an LLM (OpenAI-compatible chat completion).
///
/// Builds a combined transcript context (segments formatted as
/// `[{seconds}s] {text}`, videos joined with `\n\n---\n\n`), truncates to
/// 30 000 characters if needed, and sends system + user prompts to the LLM.
/// Parses the JSON response into [`PatchEntry`] items, skipping entries
/// that fail to parse.
pub async fn llm_patch(
    note: &NoteArtifact,
    transcripts: &[TranscriptArtifact],
) -> Result<PatchArtifact> {
    // --- Build transcript context -------------------------------------------
    let mut parts: Vec<String> = Vec::new();
    for t in transcripts {
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("## Video: {} ({})", t.video_title, t.video_id));
        for seg in &t.segments {
            lines.push(format!("[{:.0}s] {}", seg.start_time, seg.text));
        }
        parts.push(lines.join("\n"));
    }
    let transcript_ctx = truncate_chars(&parts.join("\n\n---\n\n"), 30000);

    // --- Build prompts -------------------------------------------------------
    let note_text = truncate_chars(&note.content, 15000);

    let system_prompt = concat!(
        "You are a lecture notes patching assistant. Given a set of lecture notes ",
        "and corresponding video transcripts, identify gaps and inconsistencies ",
        "where the transcripts contain information not yet captured in the notes.\n\n",
        "Output a JSON object with:\n",
        "- \"patches\": array of patch objects, each with:\n",
        "  - \"location\": (string, required) heading or section name where the patch applies\n",
        "  - \"original_text\": (string, optional) the text being modified or replaced\n",
        "  - \"new_text\": (string, required) the new text to insert or replace with\n",
        "  - \"source_video_id\": (string, optional) which video this comes from\n",
        "  - \"source_timestamp\": (number, optional) timestamp in seconds\n",
        "  - \"keep_level\": (string) one of: \"must_keep\", \"compress\", \"drop\"\n",
        "- \"conflicts\": array of strings describing any unresolved conflicts ",
        "for manual review"
    );

    let user_prompt = format!(
        "## Current Lecture Notes\n\n{}\n\n## Video Transcripts\n\n{}",
        note_text, transcript_ctx,
    );

    let response = llm::chat_json(system_prompt, &user_prompt, 0.3, 4096).await?;

    // --- Parse patches -------------------------------------------------------
    let mut patches: Vec<PatchEntry> = Vec::new();
    if let Some(arr) = response.get("patches").and_then(|v| v.as_array()) {
        for item in arr {
            let location = match item.get("location").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let new_text = match item.get("new_text").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => continue,
            };
            let original_text = item
                .get("original_text")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let source_video_id = item
                .get("source_video_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let source_timestamp = item.get("source_timestamp").and_then(|v| v.as_f64());
            let keep_level = match item.get("keep_level").and_then(|v| v.as_str()) {
                Some("must_keep") => KeepLevel::MustKeep,
                Some("compress") => KeepLevel::Compress,
                Some("drop") => KeepLevel::Drop,
                _ => KeepLevel::Compress,
            };

            patches.push(PatchEntry {
                location,
                original_text,
                new_text,
                source_video_id,
                source_timestamp,
                keep_level,
            });
        }
    }

    let conflicts: Vec<String> = response
        .get("conflicts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let source_transcripts: Vec<String> = transcripts
        .iter()
        .map(|t| format!("{}/{}", t.course_id, t.video_id))
        .collect();

    Ok(PatchArtifact {
        source_notes_path: note.path.clone(),
        source_transcripts,
        patches,
        conflicts,
        patched_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Deterministic (non-LLM) patching based on quoted terms (`"..."`), bold
/// terms (`**...**`), and inline code (`` `...` ``) extracted from transcript
/// segments.
///
/// Terms already present in the notes (case-insensitive check) are excluded.
/// All patches are placed under `"Key Terms / Glossary"` with
/// [`KeepLevel::Compress`].
pub fn deterministic_patch(
    note: &NoteArtifact,
    transcripts: &[TranscriptArtifact],
) -> PatchArtifact {
    let quoted_re = Regex::new(r#""([^"]+)""#).unwrap();
    let bold_re = Regex::new(r"\*\*([^*]+)\*\*").unwrap();
    let code_re = Regex::new(r"`([^`]+)`").unwrap();

    let mut terms: HashSet<String> = HashSet::new();
    for t in transcripts {
        for seg in &t.segments {
            for cap in quoted_re.captures_iter(&seg.text) {
                terms.insert(cap[1].to_string());
            }
            for cap in bold_re.captures_iter(&seg.text) {
                terms.insert(cap[1].to_string());
            }
            for cap in code_re.captures_iter(&seg.text) {
                terms.insert(cap[1].to_string());
            }
        }
    }

    // Filter out terms already present in the notes (case-insensitive).
    let note_lower = note.content.to_lowercase();
    let patches: Vec<PatchEntry> = terms
        .into_iter()
        .filter(|term| !contains_term(&note_lower, term))
        .map(|term| PatchEntry {
            location: "Key Terms / Glossary".to_string(),
            original_text: None,
            new_text: term,
            source_video_id: None,
            source_timestamp: None,
            keep_level: KeepLevel::Compress,
        })
        .collect();

    let source_transcripts: Vec<String> = transcripts
        .iter()
        .map(|t| format!("{}/{}", t.course_id, t.video_id))
        .collect();

    PatchArtifact {
        source_notes_path: note.path.clone(),
        source_transcripts,
        patches,
        conflicts: Vec::new(),
        patched_at: chrono::Utc::now().to_rfc3339(),
    }
}

fn contains_term(lowercase_content: &str, term: &str) -> bool {
    let term_lower = term.to_lowercase();
    if term_lower
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        let pattern = format!(r"\b{}\b", regex::escape(&term_lower));
        Regex::new(&pattern)
            .map(|re| re.is_match(lowercase_content))
            .unwrap_or_else(|_| lowercase_content.contains(&term_lower))
    } else {
        lowercase_content.contains(&term_lower)
    }
}

/// Apply patches to original content and produce the final patched Markdown.
///
/// The output is structured as:
/// 1. `<!-- Patched by lecture-distill -->` comment
/// 2. Original content unchanged
/// 3. `## Transcript Additions` section (grouped by patch location)
/// 4. `## Conflicts / Needs Review` section (only if conflicts exist)
pub fn apply_patches(original_content: &str, patch_artifact: &PatchArtifact) -> String {
    let mut out = String::new();

    // Header comment + original content.
    out.push_str("<!-- Patched by lecture-distill -->\n\n");
    out.push_str(original_content);
    out.push('\n');

    // Transcript Additions - grouped by location.
    if !patch_artifact.patches.is_empty() {
        out.push_str("## Transcript Additions\n\n");

        let mut grouped: BTreeMap<&str, Vec<&PatchEntry>> = BTreeMap::new();
        for p in &patch_artifact.patches {
            grouped.entry(p.location.as_str()).or_default().push(p);
        }

        for (location, entries) in &grouped {
            out.push_str(&format!("### {}\n\n", location));
            for entry in entries {
                let source = match (&entry.source_video_id, entry.source_timestamp) {
                    (Some(vid), Some(ts)) => format!(" (from {} at {:.1}s)", vid, ts),
                    (Some(vid), None) => format!(" (from {})", vid),
                    _ => String::new(),
                };

                if let Some(ref orig) = entry.original_text {
                    let orig_trunc = truncate_for_display(orig, 80);
                    let new_trunc = truncate_for_display(&entry.new_text, 200);
                    out.push_str(&format!(
                        "- **[[{}](#)]** ~~{}~~ -> {}{}\n",
                        location, orig_trunc, new_trunc, source,
                    ));
                } else {
                    let new_trunc = truncate_for_display(&entry.new_text, 200);
                    out.push_str(&format!(
                        "- **[[{}](#)]** {}{}\n",
                        location, new_trunc, source,
                    ));
                }
            }
            out.push('\n');
        }
    }

    // Conflicts section - only emitted when conflicts exist.
    if !patch_artifact.conflicts.is_empty() {
        out.push_str("## Conflicts / Needs Review\n\n");
        for conflict in &patch_artifact.conflicts {
            out.push_str(&format!("- {}\n", conflict));
        }
        out.push('\n');
    }

    out
}

/// Write the patched Markdown and the patch-artifact JSON to disk.
///
/// Creates parent directories as needed.
pub fn write_outputs(
    content: &str,
    patch_artifact: &PatchArtifact,
    output_notes_path: &str,
    output_patches_path: &str,
) -> Result<()> {
    if let Some(parent) = Path::new(output_notes_path).parent() {
        fs::create_dir_all(parent).context("Failed to create output directory for notes")?;
    }
    if let Some(parent) = Path::new(output_patches_path).parent() {
        fs::create_dir_all(parent).context("Failed to create output directory for patches")?;
    }

    fs::write(output_notes_path, content).context("Failed to write patched notes")?;
    let patches_json = serde_json::to_string_pretty(patch_artifact)
        .context("Failed to serialize patch artifact")?;
    fs::write(output_patches_path, patches_json).context("Failed to write patches JSON")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Truncate a string for display, appending `"..."` when shortened.
fn truncate_for_display(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        s.to_string()
    } else {
        format!("{}...", truncate_chars(s, max_len))
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::TranscriptSegment;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // extract_headings
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_headings_multiple_levels() {
        let content = "# H1\nSome text\n## H2\nMore\n### H3\nPlain\n#### H4\n##### H5\n###### H6\n";
        let headings = extract_headings(content);
        assert_eq!(headings, vec!["H1", "H2", "H3", "H4", "H5", "H6"]);
    }

    #[test]
    fn test_extract_headings_no_headings() {
        let content = "Just some plain\ntext without any\nheadings present.\n";
        let headings = extract_headings(content);
        assert!(headings.is_empty());
    }

    #[test]
    fn test_extract_headings_ignores_non_heading_hashes() {
        // Hashes in running text (not at line start) should not match.
        let content = "## Real heading\nThis is not a ## heading because it is inline.\n### Another heading\n";
        let headings = extract_headings(content);
        assert_eq!(headings, vec!["Real heading", "Another heading"]);
    }

    // -----------------------------------------------------------------------
    // apply_patches
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_patches_adds_sections_and_conflicts() {
        let original = "# Notes\n\nOriginal content here.\n";
        let pa = PatchArtifact {
            source_notes_path: "test.md".into(),
            source_transcripts: vec!["c1/v1".into()],
            patches: vec![PatchEntry {
                location: "Key Terms".into(),
                original_text: Some("old term".into()),
                new_text: "new term".into(),
                source_video_id: Some("v1".into()),
                source_timestamp: Some(10.5),
                keep_level: KeepLevel::Compress,
            }],
            conflicts: vec!["Conflict 1: needs manual review".into()],
            patched_at: "2025-01-01T00:00:00Z".into(),
        };

        let result = apply_patches(original, &pa);

        assert!(result.contains("<!-- Patched by lecture-distill -->"));
        assert!(result.contains("Original content here."));
        assert!(result.contains("## Transcript Additions"));
        assert!(result.contains("## Conflicts / Needs Review"));
        assert!(result.contains("Conflict 1: needs manual review"));
        assert!(result.contains("new term"));
        assert!(result.contains("old term"));
        assert!(result.contains("from v1 at 10.5s"));
    }

    #[test]
    fn test_apply_patches_no_conflicts_section_omitted() {
        let original = "# Notes\n\nContent.\n";
        let pa = PatchArtifact {
            source_notes_path: "test.md".into(),
            source_transcripts: vec![],
            patches: vec![],
            conflicts: vec![],
            patched_at: "2025-01-01T00:00:00Z".into(),
        };

        let result = apply_patches(original, &pa);

        assert!(result.contains("<!-- Patched by lecture-distill -->"));
        assert!(result.contains("Content."));
        assert!(!result.contains("## Conflicts / Needs Review"));
        // No Transcript Additions section when patches are empty.
        assert!(!result.contains("## Transcript Additions"));
    }

    #[test]
    fn test_apply_patches_groups_by_location() {
        let original = "# Notes\n\nSome content.\n";
        let pa = PatchArtifact {
            source_notes_path: "test.md".into(),
            source_transcripts: vec![],
            patches: vec![
                PatchEntry {
                    location: "Section A".into(),
                    original_text: None,
                    new_text: "term1".into(),
                    source_video_id: None,
                    source_timestamp: None,
                    keep_level: KeepLevel::Compress,
                },
                PatchEntry {
                    location: "Section B".into(),
                    original_text: None,
                    new_text: "term2".into(),
                    source_video_id: None,
                    source_timestamp: None,
                    keep_level: KeepLevel::Compress,
                },
                PatchEntry {
                    location: "Section A".into(),
                    original_text: None,
                    new_text: "term3".into(),
                    source_video_id: None,
                    source_timestamp: None,
                    keep_level: KeepLevel::Compress,
                },
            ],
            conflicts: vec![],
            patched_at: "2025-01-01T00:00:00Z".into(),
        };

        let result = apply_patches(original, &pa);

        // "Section A" heading should appear once as a group heading.
        let section_a_count = result.matches("### Section A").count();
        assert_eq!(section_a_count, 1);

        // term1 and term3 should both appear under Section A.
        assert!(result.contains("term1"));
        assert!(result.contains("term3"));
        assert!(result.contains("term2"));
    }

    #[test]
    fn test_apply_patches_truncates_long_text() {
        let original = "# Notes\n\nShort content.\n";
        let long_original = "a".repeat(120);
        let long_new = "b".repeat(250);

        let pa = PatchArtifact {
            source_notes_path: "test.md".into(),
            source_transcripts: vec![],
            patches: vec![PatchEntry {
                location: "Section".into(),
                original_text: Some(long_original.clone()),
                new_text: long_new.clone(),
                source_video_id: Some("v1".into()),
                source_timestamp: Some(5.0),
                keep_level: KeepLevel::Compress,
            }],
            conflicts: vec![],
            patched_at: "2025-01-01T00:00:00Z".into(),
        };

        let result = apply_patches(original, &pa);

        // Original text should be truncated to ~80 chars + "..."
        assert!(!result.contains(&long_original));
        assert!(result.contains(&long_original[..80]));
        assert!(result.contains(&format!("{}...", &long_original[..80])));

        // New text should be truncated to ~200 chars + "..."
        assert!(!result.contains(&long_new));
        assert!(result.contains(&long_new[..200]));
        assert!(result.contains(&format!("{}...", &long_new[..200])));
    }

    // -----------------------------------------------------------------------
    // deterministic_patch
    // -----------------------------------------------------------------------

    #[test]
    fn test_deterministic_patch_extracts_quoted_terms() {
        let note = NoteArtifact {
            path: "test.md".into(),
            content: "Some notes without special terms.\n".into(),
            headings: vec![],
        };

        let transcript = TranscriptArtifact {
            video_id: "v1".into(),
            video_title: "Test Video".into(),
            course_id: "c1".into(),
            language: "zh".into(),
            segments: vec![
                TranscriptSegment {
                    index: 1,
                    start_time: 0.0,
                    end_time: 2.0,
                    text: r#"The "new concept" is **important** and use `code_example`."#.into(),
                },
                TranscriptSegment {
                    index: 2,
                    start_time: 2.0,
                    end_time: 4.0,
                    text: r#"Another "term" here."#.into(),
                },
            ],
            fetched_at: "2025-01-01T00:00:00Z".into(),
            recorded_at: None,
            source_url: None,
        };

        let result = deterministic_patch(&note, &[transcript]);

        let new_texts: Vec<&str> = result.patches.iter().map(|p| p.new_text.as_str()).collect();
        assert!(new_texts.contains(&"new concept"));
        assert!(new_texts.contains(&"important"));
        assert!(new_texts.contains(&"code_example"));
        assert!(new_texts.contains(&"term"));

        // All patches should be under "Key Terms / Glossary" with Compress level.
        for patch in &result.patches {
            assert_eq!(patch.location, "Key Terms / Glossary");
            assert_eq!(patch.keep_level, KeepLevel::Compress);
        }

        assert!(result.conflicts.is_empty());
    }

    #[test]
    fn test_deterministic_patch_filters_existing_terms() {
        let note = NoteArtifact {
            path: "test.md".into(),
            content: "These notes already mention **existing** concepts.\n".into(),
            headings: vec![],
        };

        let transcript = TranscriptArtifact {
            video_id: "v1".into(),
            video_title: "Test Video".into(),
            course_id: "c1".into(),
            language: "zh".into(),
            segments: vec![TranscriptSegment {
                index: 1,
                start_time: 0.0,
                end_time: 2.0,
                text: r#"The "existing" term and a "new" one."#.into(),
            }],
            fetched_at: "2025-01-01T00:00:00Z".into(),
            recorded_at: None,
            source_url: None,
        };

        let result = deterministic_patch(&note, &[transcript]);

        let new_texts: Vec<&str> = result.patches.iter().map(|p| p.new_text.as_str()).collect();
        // "existing" is already in notes (case-insensitive), so it should NOT appear.
        assert!(!new_texts.contains(&"existing"));
        // "new" should appear.
        assert!(new_texts.contains(&"new"));
    }

    #[test]
    fn test_deterministic_patch_deduplicates_across_transcripts() {
        let note = NoteArtifact {
            path: "test.md".into(),
            content: "No existing terms.\n".into(),
            headings: vec![],
        };

        let t1 = TranscriptArtifact {
            video_id: "v1".into(),
            video_title: "Video 1".into(),
            course_id: "c1".into(),
            language: "zh".into(),
            segments: vec![TranscriptSegment {
                index: 1,
                start_time: 0.0,
                end_time: 2.0,
                text: r#"The "duplicate" term appears."#.into(),
            }],
            fetched_at: "2025-01-01T00:00:00Z".into(),
            recorded_at: None,
            source_url: None,
        };

        let t2 = TranscriptArtifact {
            video_id: "v2".into(),
            video_title: "Video 2".into(),
            course_id: "c1".into(),
            language: "zh".into(),
            segments: vec![TranscriptSegment {
                index: 1,
                start_time: 0.0,
                end_time: 2.0,
                text: r#"The same "duplicate" in another video."#.into(),
            }],
            fetched_at: "2025-01-01T00:00:00Z".into(),
            recorded_at: None,
            source_url: None,
        };

        let result = deterministic_patch(&note, &[t1, t2]);

        // "duplicate" should appear exactly once (deduplicated across transcripts).
        let count = result
            .patches
            .iter()
            .filter(|p| p.new_text == "duplicate")
            .count();
        assert_eq!(count, 1);
    }

    // -----------------------------------------------------------------------
    // load_notes
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_notes_parses_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.md");

        let content =
            "# Main Title\n\n## Section One\nSome text here.\n\n### Subsection\n\nMore text.\n## Section Two\nEnd.\n";
        fs::write(&file_path, content).unwrap();

        let note = load_notes(file_path.to_str().unwrap()).unwrap();
        assert_eq!(note.path, file_path.to_str().unwrap());
        assert_eq!(note.content, content);
        assert_eq!(
            note.headings,
            vec!["Main Title", "Section One", "Subsection", "Section Two"]
        );
    }

    #[test]
    fn test_load_notes_file_not_found() {
        let result = load_notes("/nonexistent/path/to/notes.md");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // load_transcripts
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_transcripts_valid_json() {
        let dir = TempDir::new().unwrap();
        let path = dir.path();

        // Write a valid transcript JSON file.
        let ta = TranscriptArtifact {
            video_id: "v1".into(),
            video_title: "Test Video".into(),
            course_id: "c1".into(),
            language: "zh".into(),
            segments: vec![],
            fetched_at: "2025-01-01T00:00:00Z".into(),
            recorded_at: None,
            source_url: None,
        };
        let json = serde_json::to_string(&ta).unwrap();
        fs::write(path.join("valid1.json"), &json).unwrap();

        // Write a second valid file.
        let ta2 = TranscriptArtifact {
            video_id: "v2".into(),
            video_title: "Second Video".into(),
            course_id: "c2".into(),
            language: "en".into(),
            segments: vec![],
            fetched_at: "2025-01-02T00:00:00Z".into(),
            recorded_at: None,
            source_url: Some("https://example.com".into()),
        };
        let json2 = serde_json::to_string(&ta2).unwrap();
        fs::write(path.join("valid2.json"), &json2).unwrap();

        // Write an invalid JSON file (should be skipped).
        fs::write(path.join("invalid.json"), "not valid {{{").unwrap();

        // Write a non-JSON file (should be ignored).
        fs::write(path.join("readme.txt"), "hello world").unwrap();

        let transcripts = load_transcripts(path.to_str().unwrap()).unwrap();
        assert_eq!(transcripts.len(), 2);

        let ids: Vec<&str> = transcripts.iter().map(|t| t.video_id.as_str()).collect();
        assert!(ids.contains(&"v1"));
        assert!(ids.contains(&"v2"));
    }

    #[test]
    fn test_load_transcripts_missing_dir_returns_empty() {
        let result = load_transcripts("/nonexistent/path/12345/67890").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_transcripts_empty_dir() {
        let dir = TempDir::new().unwrap();
        let transcripts = load_transcripts(dir.path().to_str().unwrap()).unwrap();
        assert!(transcripts.is_empty());
    }

    #[test]
    fn test_load_transcripts_no_json_files() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("notes.md"), "# Notes\n").unwrap();
        fs::write(dir.path().join("data.txt"), "text").unwrap();

        let transcripts = load_transcripts(dir.path().to_str().unwrap()).unwrap();
        assert!(transcripts.is_empty());
    }

    // -----------------------------------------------------------------------
    // patch_notes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_patch_notes_no_transcripts_writes_unchanged_and_preserves_source() {
        let dir = TempDir::new().unwrap();
        let notes_path = dir.path().join("notes.md");
        let output_notes = dir.path().join("patched.md");
        let output_patches = dir.path().join("patches.json");
        let transcripts_dir = dir.path().join("transcripts");

        // Source notes content.
        let original_content = "# Test Notes\n\nSome content here.\n";
        fs::write(&notes_path, original_content).unwrap();

        // Empty transcripts directory.
        fs::create_dir(&transcripts_dir).unwrap();

        let result = patch_notes(
            notes_path.to_str().unwrap(),
            transcripts_dir.to_str().unwrap(),
            output_notes.to_str().unwrap(),
            output_patches.to_str().unwrap(),
        )
        .await
        .unwrap();

        // Source file must be unchanged.
        let source_after = fs::read_to_string(&notes_path).unwrap();
        assert_eq!(source_after, original_content);

        // Output file must have the original content (unchanged).
        let output_content = fs::read_to_string(&output_notes).unwrap();
        assert_eq!(output_content, original_content);

        // Patch artifact must report the conflict.
        assert!(result.patches.is_empty());
        assert_eq!(result.conflicts.len(), 1);
        assert!(result.conflicts[0].contains("No transcript files found"));
        assert!(result.conflicts[0].contains(transcripts_dir.to_str().unwrap()));

        // Patches JSON must exist on disk.
        let patch_json_content = fs::read_to_string(&output_patches).unwrap();
        let parsed: PatchArtifact = serde_json::from_str(&patch_json_content).unwrap();
        assert!(parsed.patches.is_empty());
        assert_eq!(parsed.conflicts.len(), 1);
    }

    // -----------------------------------------------------------------------
    // write_outputs
    // -----------------------------------------------------------------------

    #[test]
    fn test_write_outputs_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let nested_notes = dir.path().join("sub").join("deep").join("out.md");
        let nested_patches = dir.path().join("sub").join("deep").join("out.json");

        let pa = PatchArtifact::new("source.md".into());
        write_outputs(
            "content",
            &pa,
            nested_notes.to_str().unwrap(),
            nested_patches.to_str().unwrap(),
        )
        .unwrap();

        assert!(nested_notes.exists());
        assert!(nested_patches.exists());
        assert_eq!(fs::read_to_string(&nested_notes).unwrap(), "content");
    }
}
