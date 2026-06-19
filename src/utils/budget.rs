//! Cheating Sheet capacity estimation and section inventory.
//!
//! Budget estimation, Markdown section parsing, expansion decision logic,
//! and metadata construction for cheating sheet outputs.

use crate::web::course::truncate_chars;

/// Budget estimates for generating a cheating sheet of `max_pages` pages.
///
/// The template is 4-column A4 with 5pt body text. Budget estimates:
/// - ~11000 chars/page is a comfortable fill target.
/// - ~13500 chars/page is the soft maximum before content overflows.
#[derive(Debug, Clone)]
pub(crate) struct CheatingSheetBudget {
    pub(crate) target_chars: usize,
    pub(crate) soft_max_chars: usize,
    pub(crate) min_acceptable_chars: usize,
}

/// Estimate the character budget for a cheating sheet of `max_pages` pages.
///
/// Clamps `max_pages` to `1..=20` (matching the caller's policy).
pub(crate) fn estimate_cheating_sheet_budget(max_pages: usize) -> CheatingSheetBudget {
    let pages = max_pages.clamp(1, 20);
    let target_chars = pages.saturating_mul(11000);
    let soft_max_chars = pages.saturating_mul(15000);
    let min_acceptable_chars = target_chars.saturating_mul(3) / 4;
    CheatingSheetBudget {
        target_chars,
        soft_max_chars,
        min_acceptable_chars,
    }
}

/// A parsed section heading from source Markdown.
#[derive(Debug, Clone)]
pub(crate) struct CheatSection {
    /// Heading level (1 for `#`, 2 for `##`, 3 for `###`).
    pub(crate) level: usize,
    /// Heading text (without the `#` prefix).
    pub(crate) heading: String,
    /// Short preview of the section body (first ~120 chars).
    pub(crate) body_preview: String,
}

/// Build a compact inventory of Markdown sections for prompting.
///
/// Parses `#`, `##`, and `###` headings in order and captures a short body
/// preview for each section. The result is a plain-text inventory string
/// suitable for inclusion in an LLM prompt.
pub(crate) fn build_section_inventory(markdown: &str) -> (Vec<CheatSection>, String) {
    let heading_re = regex::Regex::new(r"^(#{1,3})\s+(.+)$").unwrap();
    let mut sections: Vec<CheatSection> = Vec::new();
    let mut current_body = String::new();

    for line in markdown.lines() {
        if let Some(caps) = heading_re.captures(line) {
            // Flush the current body into the previous section, if any.
            if let Some(last) = sections.last_mut() {
                if last.body_preview.is_empty() {
                    last.body_preview = truncate_chars(&current_body, 120);
                }
            }
            current_body.clear();

            let level = caps.get(1).unwrap().as_str().len();
            let heading = caps.get(2).unwrap().as_str().trim().to_string();
            sections.push(CheatSection {
                level,
                heading,
                body_preview: String::new(),
            });
        } else if !sections.is_empty() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                if !current_body.is_empty() {
                    current_body.push(' ');
                }
                current_body.push_str(trimmed);
            }
        }
    }

    // Flush the last section.
    if let Some(last) = sections.last_mut() {
        if last.body_preview.is_empty() {
            last.body_preview = truncate_chars(&current_body, 120);
        }
    }

    // Build a compact inventory string.
    let inventory = sections
        .iter()
        .map(|s| {
            let prefix = "#".repeat(s.level);
            format!(
                "{} {}\n  body: {}\n",
                prefix,
                s.heading,
                if s.body_preview.is_empty() {
                    "(empty)"
                } else {
                    &s.body_preview
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    (sections, inventory)
}

/// Result of a cheating sheet Markdown generation pass.
#[derive(Debug, Clone)]
pub(crate) struct CheatingSheetGenerationResult {
    pub(crate) markdown: String,
    pub(crate) target_chars: usize,
    pub(crate) generated_chars: usize,
    pub(crate) harness_attempts: usize,
    pub(crate) expansion_used: bool,
    pub(crate) underfilled_reason: Option<String>,
}

/// Decide whether to attempt an expansion pass.
///
/// Expansion is warranted when:
/// - The source is long enough to support meaningful additions, AND
/// - Either the page count is below max_pages (traditional check), OR
/// - The last page is under-utilised (precise check from `typst query`).
///
/// We prefer compression of over-generated content to expansion of
/// under-generated content.
pub(crate) fn should_attempt_expansion(
    _generated_chars: usize,
    _min_acceptable_chars: usize,
    page_count: usize,
    max_pages: usize,
    source_too_short: bool,
    space_utilization: Option<&crate::artifacts::SpaceUtilization>,
) -> bool {
    if source_too_short {
        return false;
    }
    // Use precise utilisation data when available.
    if let Some(su) = space_utilization {
        return su.last_page_under_utilized;
    }
    // Fall back to simple page-count heuristic.
    page_count < max_pages
}

/// Build metadata JSON for a cheating sheet output.
///
/// Preserves the existing metadata keys and adds the new capacity-aware fields.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_cheatsheet_metadata(
    progress_current: usize,
    progress_total: usize,
    progress_label: &str,
    max_pages: usize,
    page_count: usize,
    compression_attempts: usize,
    template_used: &str,
    markdown_path: &str,
    reference_digest_output_id: &str,
    target_chars: usize,
    generated_chars: usize,
    harness_attempts: usize,
    expansion_used: bool,
    final_page_count: usize,
    underfilled_reason: Option<&str>,
    space_utilization: Option<&crate::artifacts::SpaceUtilization>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "progress_current": progress_current,
        "progress_total": progress_total,
        "progress_label": progress_label,
        "max_pages": max_pages,
        "page_count": page_count,
        "compression_attempts": compression_attempts,
        "template_used": template_used,
        "markdown_path": markdown_path,
        "reference_digest_output_id": reference_digest_output_id,
        "target_chars": target_chars,
        "generated_chars": generated_chars,
        "harness_attempts": harness_attempts,
        "expansion_used": expansion_used,
        "final_page_count": final_page_count,
    });
    if let Some(reason) = underfilled_reason {
        meta["underfilled_reason"] = serde_json::json!(reason);
    }
    if let Some(su) = space_utilization {
        meta["fill_pct"] = serde_json::json!(format!("{:.1}", su.last_page_utilization_pct));
        meta["total_content_pages"] = serde_json::json!(format!("{:.2}", su.total_content_pages));
        meta["last_page_under_utilized"] = serde_json::json!(su.last_page_under_utilized);
        if let Some(ratio) = su.overflow_ratio {
            meta["overflow_ratio"] = serde_json::json!(format!("{:.2}", ratio));
        }
        // Per-page utilization breakdown for diagnostics.
        let per_page: Vec<serde_json::Value> = su
            .page_utilizations
            .iter()
            .map(|p| {
                serde_json::json!({
                    "page": p.page,
                    "utilization_pct": format!("{:.1}", p.utilization_pct),
                })
            })
            .collect();
        meta["page_utilizations"] = serde_json::json!(per_page);
    }
    meta
}

/// Proportional truncation of a Reference Digest for cheat sheet prompting.
///
/// Splits on `## ` boundaries, allocates a preamble budget (1200 chars),
/// and distributes the remaining budget proportionally across sections with
/// a minimum of 600 chars per section. Returns the truncated markdown and
/// the total section count.
pub(crate) fn truncate_ref_digest_for_cheatsheet(
    markdown: &str,
    max_chars: usize,
) -> (String, usize) {
    let min_per_section: usize = 600;
    let preamble_budget: usize = 1200;

    // Split on `\n## ` boundaries.
    let mut sections: Vec<(usize, &str)> = Vec::new();
    let mut preamble_end: usize = 0;

    // Find the first `\n## ` boundary.
    if let Some(first_h2) = markdown.find("\n## ") {
        preamble_end = first_h2;
        let body = &markdown[first_h2..];
        // Split body into h2-headed sections.
        let mut prev_start: usize = 0;
        for m in regex::Regex::new(r"\n## ").unwrap().find_iter(body) {
            if prev_start > 0 {
                sections.push((
                    first_h2 + prev_start,
                    &markdown[first_h2 + prev_start..first_h2 + m.start()],
                ));
            }
            prev_start = m.start();
        }
        // Last section.
        if prev_start < body.len() {
            sections.push((first_h2 + prev_start, &markdown[first_h2 + prev_start..]));
        }
    }

    if sections.is_empty() {
        // No h2 headings — fall back to head-only truncation.
        return (
            crate::web::sources::truncate_for_llm(markdown, max_chars),
            0,
        );
    }

    let total_sections = sections.len();

    // Allocate budget: preamble gets preamble_budget chars, remainder
    // is split proportionally among sections, with min_per_section floor.
    let preamble = truncate_chars(&markdown[..preamble_end], preamble_budget);
    let body_budget = max_chars.saturating_sub(preamble.chars().count());

    let min_floor = total_sections.saturating_mul(min_per_section);
    if body_budget <= min_floor {
        // Tight budget: give each section exactly min_per_section chars.
        let mut result = preamble;
        result.push('\n');
        for (_offset, section_text) in &sections {
            if let Some(heading_end) = section_text.find('\n') {
                result.push_str(&section_text[..heading_end]);
                result.push('\n');
                let body_start = heading_end + 1;
                let body = &section_text[body_start..];
                result.push_str(&truncate_chars(body.trim_start(), min_per_section));
                result.push_str("\n\n");
            }
        }
        return (result, total_sections);
    }

    // Proportional allocation.
    let total_chars: usize = sections.iter().map(|(_, t)| t.chars().count()).sum();
    let mut result = preamble;
    result.push('\n');

    for (_offset, section_text) in &sections {
        let section_chars = section_text.chars().count();
        let share = if total_chars > 0 {
            ((section_chars as f64 / total_chars as f64) * body_budget as f64) as usize
        } else {
            body_budget / total_sections
        };
        let alloc = share.max(min_per_section);

        if let Some(heading_end) = section_text.find('\n') {
            result.push_str(&section_text[..heading_end]);
            result.push('\n');
            let body_start = heading_end + 1;
            let body = &section_text[body_start..];
            result.push_str(&truncate_chars(body.trim_start(), alloc));
            result.push_str("\n\n");
        }
    }

    (result, total_sections)
}

/// Extract high-density content from a Reference Digest to fill a character
/// gap when LLM expansion fails to generate enough content.
///
/// Splits on `## ` section boundaries, picks sections with the highest
/// content density (chars per heading level), and extracts content until
/// the requested character budget is met.
pub(crate) fn extract_high_density_content(markdown: &str, target_chars: usize) -> String {
    if target_chars == 0 || markdown.is_empty() {
        return String::new();
    }

    // Split on `\n## ` boundaries to get individual sections.
    let mut sections: Vec<(&str, &str)> = Vec::new(); // (heading, body)

    if let Some(first_h2) = markdown.find("\n## ") {
        let body = &markdown[first_h2..];
        let mut prev_start: usize = 0;
        for m in regex::Regex::new(r"\n## ").unwrap().find_iter(body) {
            if prev_start > 0 {
                let section_text = &markdown[first_h2 + prev_start..first_h2 + m.start()];
                if let Some(nl) = section_text.find('\n') {
                    let heading = &section_text[..nl].trim();
                    // Strip "## " prefix.
                    let heading = heading.strip_prefix("## ").unwrap_or(heading);
                    let body_text = &section_text[nl + 1..];
                    sections.push((heading, body_text));
                }
            }
            prev_start = m.start();
        }
        if prev_start < body.len() {
            let section_text = &markdown[first_h2 + prev_start..];
            if let Some(nl) = section_text.find('\n') {
                let heading = &section_text[..nl].trim();
                let heading = heading.strip_prefix("## ").unwrap_or(heading);
                let body_text = &section_text[nl + 1..];
                sections.push((heading, body_text));
            }
        }
    }

    if sections.is_empty() {
        // No h2 sections — just truncate the whole document.
        return crate::web::sources::truncate_for_llm(markdown, target_chars);
    }

    // Score each section by content density (chars / estimated "importance").
    // Key Concepts and Content Summary sections get higher priority.
    let mut scored: Vec<(usize, f64, &str, &str)> = sections
        .iter()
        .enumerate()
        .map(|(i, (heading, body))| {
            let chars = body.chars().count() as f64;
            let priority = if heading.contains("Key Concept")
                || heading.contains("Content Summary")
                || heading.contains("Must Know")
            {
                2.0
            } else if heading.contains("Supporting") || heading.contains("Background") {
                0.5
            } else {
                1.0
            };
            (i, chars * priority, *heading, *body)
        })
        .collect();

    // Sort by score descending.
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Build result from highest-scoring sections.
    let mut result = String::with_capacity(target_chars + 256);
    let mut collected = 0usize;
    for (_idx, _score, heading, body) in &scored {
        if collected >= target_chars {
            break;
        }
        let remaining = target_chars.saturating_sub(collected);
        let section_header = format!("## {}\n", heading);
        result.push_str(&section_header);
        collected += section_header.chars().count();

        let body_chars: String = body.chars().take(remaining.saturating_sub(collected)).collect();
        collected += body_chars.chars().count();
        result.push_str(&body_chars);
        result.push_str("\n\n");
        collected += 2; // for the double newline
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_estimation_one_page() {
        let b = estimate_cheating_sheet_budget(1);
        assert_eq!(b.target_chars, 11000);
        assert_eq!(b.soft_max_chars, 15000);
        assert_eq!(b.min_acceptable_chars, 8250);
    }

    #[test]
    fn test_budget_estimation_two_pages() {
        let b = estimate_cheating_sheet_budget(2);
        assert_eq!(b.target_chars, 22000);
        assert_eq!(b.soft_max_chars, 30000);
    }

    #[test]
    fn test_budget_estimation_three_pages() {
        let b = estimate_cheating_sheet_budget(3);
        assert_eq!(b.target_chars, 33000);
    }

    #[test]
    fn test_budget_clamped_at_20() {
        let b = estimate_cheating_sheet_budget(100);
        assert_eq!(b.target_chars, 20 * 11000);
    }

    #[test]
    fn test_budget_minimum_one() {
        let b = estimate_cheating_sheet_budget(0);
        assert_eq!(b.target_chars, 11000);
    }

    #[test]
    fn test_section_inventory_extracts_headings_in_order() {
        let md = "# Top\ncontent\n## Sub A\nbody A\n## Sub B\nbody B\n";
        let (sections, inventory) = build_section_inventory(md);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].heading, "Top");
        assert_eq!(sections[1].heading, "Sub A");
        assert_eq!(sections[2].heading, "Sub B");
        assert!(inventory.contains("Top"));
        assert!(inventory.contains("Sub A"));
    }

    #[test]
    fn test_section_inventory_body_previews() {
        let md = "# H1\nsome body text here\n## H2\nanother body\n";
        let (sections, _) = build_section_inventory(md);
        assert_eq!(sections.len(), 2);
        assert!(sections[0].body_preview.contains("some body text here"));
        assert!(sections[1].body_preview.contains("another body"));
    }

    #[test]
    fn test_section_inventory_empty_markdown() {
        let (sections, _) = build_section_inventory("");
        assert!(sections.is_empty());
    }

    #[test]
    fn test_section_inventory_no_headings() {
        let (sections, _) = build_section_inventory("just text\nno headings\n");
        assert!(sections.is_empty());
    }

    #[test]
    fn test_underfilled_triggers_expansion_when_pages_below_max() {
        assert!(should_attempt_expansion(5000, 8250, 1, 2, false, None));
    }

    #[test]
    fn test_no_expansion_when_char_count_low_but_pages_full() {
        assert!(!should_attempt_expansion(5000, 8250, 2, 2, false, None));
    }

    #[test]
    fn test_no_expansion_when_source_too_short() {
        assert!(!should_attempt_expansion(5000, 8250, 1, 3, true, None));
    }

    #[test]
    fn test_no_expansion_when_filled() {
        let su = crate::artifacts::SpaceUtilization {
            total_content_pages: 2.0,
            max_pages: 2,
            overflow_ratio: None,
            page_utilizations: vec![],
            last_page_under_utilized: false,
            last_page_utilization_pct: 90.0,
        };
        assert!(!should_attempt_expansion(
            15000,
            8250,
            2,
            2,
            false,
            Some(&su)
        ));
    }
}
