//! Markdown manipulation utilities.
//!
//! Functions for stripping code fences, applying structured patches to
//! notes, and finding/normalizing heading text.

use std::collections::BTreeMap;

use crate::artifacts::PatchEntry;

/// Strip ```markdown / ``` fences from LLM output.
pub(crate) fn strip_markdown_fences(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.starts_with("```markdown") {
        cleaned
            .strip_prefix("```markdown")
            .and_then(|s| s.strip_suffix("```"))
            .map(|s| s.trim())
            .unwrap_or(cleaned)
            .to_string()
    } else if cleaned.starts_with("```") {
        cleaned
            .strip_prefix("```")
            .and_then(|s| s.strip_suffix("```"))
            .map(|s| s.trim())
            .unwrap_or(cleaned)
            .to_string()
    } else {
        cleaned.to_string()
    }
}

/// Apply structured patch entries to a base note, inserting them under the
/// appropriate headings.
pub(crate) fn apply_structured_patches_to_note(base_note: &str, patches: &[PatchEntry]) -> String {
    if patches.is_empty() {
        return base_note.to_string();
    }
    let mut grouped: BTreeMap<&str, Vec<&PatchEntry>> = BTreeMap::new();
    for patch in patches {
        grouped
            .entry(patch.location.as_str())
            .or_default()
            .push(patch);
    }

    let mut lines: Vec<String> = base_note.lines().map(ToString::to_string).collect();
    let mut insertions: Vec<(usize, Vec<String>)> = Vec::new();

    for (location, entries) in grouped {
        let insertion_index = find_section_insert_index(&lines, location).unwrap_or(lines.len());
        let mut addition_lines = Vec::new();
        if insertion_index > 0
            && !lines
                .get(insertion_index.saturating_sub(1))
                .is_some_and(|l| l.trim().is_empty())
        {
            addition_lines.push(String::new());
        }
        for entry in entries {
            let source = match (&entry.source_video_id, entry.source_timestamp) {
                (Some(video), Some(ts)) => format!(" (source: {} @ {:.0}s)", video, ts),
                (Some(video), None) => format!(" (source: {})", video),
                _ => String::new(),
            };
            addition_lines.push(format!("- {}{}", entry.new_text.trim(), source));
        }
        addition_lines.push(String::new());
        insertions.push((insertion_index, addition_lines));
    }

    insertions.sort_by(|a, b| b.0.cmp(&a.0));
    for (idx, new_lines) in insertions {
        for line in new_lines.into_iter().rev() {
            lines.insert(idx, line);
        }
    }

    let mut out = lines.join("\n");
    if base_note.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Find the insertion index after the section with the given heading.
pub(crate) fn find_section_insert_index(lines: &[String], location: &str) -> Option<usize> {
    let target = normalize_heading_text(location);
    if target.is_empty() {
        return None;
    }
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            continue;
        }
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        if level == 0 || level > 6 || !trimmed[level..].starts_with(' ') {
            continue;
        }
        let heading = normalize_heading_text(&trimmed[level..]);
        if heading != target && !target.contains(&heading) && !heading.contains(&target) {
            continue;
        }
        for next_idx in idx + 1..lines.len() {
            let next = lines[next_idx].trim_start();
            if next.starts_with('#') {
                let next_level = next.chars().take_while(|c| *c == '#').count();
                if next_level > 0 && next_level <= level && next[next_level..].starts_with(' ') {
                    return Some(next_idx);
                }
            }
        }
        return Some(lines.len());
    }
    None
}

/// Normalize heading text for comparison: trim whitespace, `#` marks, and
/// convert to lowercase.
pub(crate) fn normalize_heading_text(value: &str) -> String {
    value.trim().trim_matches('#').trim().to_lowercase()
}
