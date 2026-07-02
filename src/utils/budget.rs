//! Cheating Sheet capacity estimation and section inventory.
//!
//! Budget estimation, Markdown section parsing, expansion decision logic,
//! and metadata construction for cheating sheet outputs.

use crate::utils::calibration::CalibrationData;
use crate::utils::calibration::SOFT_MAX_RATIO;
use crate::web::course::truncate_chars;

// ---------------------------------------------------------------------------
// Language-aware content counting
// ---------------------------------------------------------------------------

/// Effective character count after stripping Markdown syntax.
#[derive(Debug, Clone)]
pub(crate) struct EffectiveCount {
    /// CJK (Chinese/Japanese/Korean) ideographs.
    pub cjk_chars: usize,
    /// Latin alphabet letters (a-z, A-Z).
    pub latin_chars: usize,
    /// Total significant characters (cjk + latin).
    pub total_significant: usize,
}

/// Dominant language of the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Chinese,
    English,
}

/// Check if a character is a CJK Unified Ideograph.
fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
    )
}

/// Check if a character is a Latin letter.
fn is_latin(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// Count effective (visible) characters in Markdown, stripping syntax.
///
/// Rules:
/// 1. Skip blank lines.
/// 2. Strip structural prefixes: `#` heading markers, `-`/`*` list markers,
///    `N.`/`N)` ordered list markers, `>` blockquote markers.
/// 3. Skip code-fence content entirely.
/// 4. Skip HTML comments.
/// 5. For inline content, classify each char as CJK or Latin (ignore others).
pub(crate) fn count_effective(md: &str) -> EffectiveCount {
    let heading_re = regex::Regex::new(r"^(#{1,6})\s+").unwrap();
    let unordered_re = regex::Regex::new(r"^\s*[-*]\s+").unwrap();
    let ordered_re = regex::Regex::new(r"^\s*\d+[\.\)]\s+").unwrap();
    let blockquote_re = regex::Regex::new(r"^\s*>\s?").unwrap();

    let mut cjk = 0usize;
    let mut latin = 0usize;
    let mut in_code_fence = false;

    for line in md.lines() {
        let trimmed = line.trim();

        // Toggle code fence state; skip everything inside.
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }

        // Skip blank lines, horizontal rules, HTML comments.
        if trimmed.is_empty()
            || trimmed == "---"
            || trimmed == "***"
            || trimmed == "___"
            || trimmed.starts_with("<!--")
        {
            continue;
        }

        // Strip structural prefix to get the "content" part.
        let content = if let Some(caps) = heading_re.captures(trimmed) {
            // Content after the heading marker
            let full = caps.get(0).unwrap().as_str();
            &trimmed[full.len()..]
        } else if let Some(caps) = unordered_re.captures(trimmed) {
            &trimmed[caps.get(0).unwrap().as_str().len()..]
        } else if let Some(caps) = ordered_re.captures(trimmed) {
            &trimmed[caps.get(0).unwrap().as_str().len()..]
        } else if let Some(caps) = blockquote_re.captures(trimmed) {
            &trimmed[caps.get(0).unwrap().as_str().len()..]
        } else {
            trimmed
        };

        // Count chars, classifying each.
        for c in content.chars() {
            if is_cjk(c) {
                cjk += 1;
            } else if is_latin(c) {
                latin += 1;
            }
            // Math ($...$), digits, punctuation, whitespace — not counted.
        }
    }

    EffectiveCount {
        cjk_chars: cjk,
        latin_chars: latin,
        total_significant: cjk + latin,
    }
}

/// Determine the dominant language of the content.
///
/// CJK > 50% of `total_significant` → `Chinese`;
/// 50/50 tie → `Chinese` (safe default for SJTU course content).
pub(crate) fn dominant_language(count: &EffectiveCount) -> Language {
    if count.total_significant == 0 {
        return Language::Chinese; // default for empty content
    }
    let cjk_ratio = count.cjk_chars as f64 / count.total_significant as f64;
    if cjk_ratio >= 0.5 {
        Language::Chinese
    } else {
        Language::English
    }
}

// ---------------------------------------------------------------------------
// Budget computation
// ---------------------------------------------------------------------------

/// Budget for a cheat sheet output, computed from calibration + content analysis.
#[derive(Debug, Clone)]
pub(crate) struct ContentBudget {
    /// Target in language-appropriate units.
    /// Chinese: CJK characters (字).  English: words.
    pub target: usize,
    /// Soft maximum before overflow is likely.
    pub soft_max: usize,
    /// Minimum acceptable (75% of target).
    pub min_acceptable: usize,
    /// Dominant language (drives prompt formatting).
    pub language: Language,
}

/// Compute a content budget from calibration data and effective character counts.
///
/// The per-page rate is chosen based on the dominant language.
pub(crate) fn compute_budget(
    calib: &CalibrationData,
    effective: &EffectiveCount,
    max_pages: usize,
) -> ContentBudget {
    let pages = max_pages.clamp(1, 20);
    let lang = dominant_language(effective);

    let per_page = match lang {
        Language::Chinese => calib.cjk_chars_per_page,
        Language::English => calib.english_chars_per_page,
    };

    let target = pages.saturating_mul(per_page);
    let soft_max = (target as f64 * SOFT_MAX_RATIO).round() as usize;
    let min_acceptable = target.saturating_mul(3) / 4;

    ContentBudget {
        target,
        soft_max,
        min_acceptable,
        language: lang,
    }
}

/// Format the budget target as a language-appropriate prompt fragment.
///
/// Chinese: "约 {字} 字"
/// English: "approximately {words} words"
pub(crate) fn format_target(budget: &ContentBudget) -> String {
    match budget.language {
        Language::Chinese => format!("约 {} 字", budget.target),
        Language::English => format!("approximately {} words", budget.target),
    }
}

// ---------------------------------------------------------------------------
// Deprecated: old hardcoded budget API
// ---------------------------------------------------------------------------

/// Budget estimates for generating a cheating sheet of `max_pages` pages.
///
/// The template is 4-column A4 with 5pt body text. Budget estimates:
/// - ~8500 chars/page is a comfortable fill target for Chinese+math content.
///   (Empirically calibrated: 16,440 chars fills 2 pages at ~95%).
/// - ~12000 chars/page is the soft maximum before content overflows.
/// - Models are calibrated lower than previous (11000/15000) based on
///   observed Typst rendering of Chinese-dominant exam-review content.
#[deprecated(note = "Use compute_budget() with CalibrationData instead")]
#[derive(Debug, Clone)]
pub(crate) struct CheatingSheetBudget {
    pub(crate) target_chars: usize,
    pub(crate) soft_max_chars: usize,
    pub(crate) min_acceptable_chars: usize,
}

/// Calibrated chars-per-page constant for Chinese+math content in 4-col 5pt layout.
#[deprecated(note = "Use CalibrationData::cjk_chars_per_page instead")]
pub(crate) const CHARS_PER_PAGE_TARGET: usize = 8500;

#[deprecated(note = "Use SOFT_MAX_RATIO with calibration data instead")]
pub(crate) const CHARS_PER_PAGE_SOFT_MAX: usize = 12000;

/// Estimate the character budget for a cheating sheet of `max_pages` pages.
///
/// Clamps `max_pages` to `1..=20` (matching the caller's policy).
#[deprecated(note = "Use compute_budget() instead")]
pub(crate) fn estimate_cheating_sheet_budget(max_pages: usize) -> CheatingSheetBudget {
    #[allow(deprecated)]
    {
        let pages = max_pages.clamp(1, 20);
        let target_chars = pages.saturating_mul(CHARS_PER_PAGE_TARGET);
        let soft_max_chars = pages.saturating_mul(CHARS_PER_PAGE_SOFT_MAX);
        let min_acceptable_chars = target_chars.saturating_mul(3) / 4;
        CheatingSheetBudget {
            target_chars,
            soft_max_chars,
            min_acceptable_chars,
        }
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
    pub(crate) language: Language,
}

/// Decide whether to attempt an expansion pass.
///
/// Expansion is warranted when:
/// - The source is long enough to support meaningful additions, AND
/// - Either the page count is below max_pages (traditional check), OR
/// - The last page is under-utilised (precise check from `typst query`).
///
/// Exception: even when the source is short, if the cheat sheet itself is
/// severely underfilled (< 60% of target chars), expansion is still worth
/// attempting — the LLM may have stopped prematurely.
///
/// We prefer compression of over-generated content to expansion of
/// under-generated content.
pub(crate) fn should_attempt_expansion(
    generated_chars: usize,
    min_acceptable_chars: usize,
    page_count: usize,
    max_pages: usize,
    source_too_short: bool,
    space_utilization: Option<&crate::artifacts::SpaceUtilization>,
) -> bool {
    // Rule 0: If we haven't filled all available pages, always attempt expansion.
    // Even if the last existing page appears full, we want content to spill
    // onto the next page — that's the whole point of having max_pages > 1.
    if page_count < max_pages {
        // Guard: only expand if there's enough source material.
        if source_too_short {
            return generated_chars >= min_acceptable_chars;
        }
        return true;
    }

    // Rule 1: When pages are at max, check if the last page is under-utilised.
    // (source_too_short is irrelevant here — if pages are full, we're done).
    if let Some(su) = space_utilization {
        return su.last_page_under_utilized;
    }

    // Rule 2: Without precise data, never expand when pages are full.
    false
}

/// Decide whether the cheat sheet LLM generation needs a retry.
///
/// Returns true when the generated char count is below 60% of target AND
/// the ref_digest has enough content to support a meaningful retry.
pub(crate) fn should_retry_cheat_sheet_generation(
    generated_chars: usize,
    target_chars: usize,
    ref_digest_chars: usize,
) -> bool {
    let ratio = if target_chars > 0 {
        generated_chars as f64 / target_chars as f64
    } else {
        1.0
    };
    // Retry if severely underfilled AND source has content to draw from.
    ratio < 0.60 && ref_digest_chars > generated_chars
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

        let body_chars: String = body
            .chars()
            .take(remaining.saturating_sub(collected))
            .collect();
        collected += body_chars.chars().count();
        result.push_str(&body_chars);
        result.push_str("\n\n");
        collected += 2; // for the double newline
    }

    result
}

/// Quality gate for Reference Digest generation.
///
/// Returns `(passed, reason)` where `reason` is a human-readable explanation.
/// The digest must meet BOTH:
/// - At least `min_absolute_chars` (max_pages × 11000 × 0.6)
/// - At least 7.5% of source transcript chars
pub(crate) fn check_ref_digest_quality(
    digest_chars: usize,
    source_chars: usize,
    max_pages: usize,
) -> (bool, String) {
    let min_absolute = max_pages
        .clamp(1, 20)
        .saturating_mul(CHARS_PER_PAGE_TARGET)
        .saturating_mul(60)
        / 100;
    let min_ratio = source_chars.saturating_mul(75) / 1000; // 7.5%
    let threshold = min_absolute.max(min_ratio);

    let mut reasons = Vec::new();
    if digest_chars < min_absolute {
        reasons.push(format!(
            "absolute: {} < min_absolute {} ({} pages × 11000 × 0.6)",
            digest_chars, min_absolute, max_pages
        ));
    }
    if digest_chars < min_ratio {
        reasons.push(format!(
            "ratio: {} < min_ratio {} (15% of source {} chars)",
            digest_chars, min_ratio, source_chars
        ));
    }

    if reasons.is_empty() {
        (
            true,
            format!("ok: {} chars >= threshold {}", digest_chars, threshold),
        )
    } else {
        (
            false,
            format!("FAIL: {} (threshold {})", reasons.join("; "), threshold),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // ContentBudget / compute_budget / format_target tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_compute_budget_cjk_dominant() {
        let calib = CalibrationData {
            template: "test.typ".into(),
            calibrated_at: "2026-01-01T00:00:00Z".into(),
            cjk_chars_per_page: 8000,
            english_words_per_page: 1400,
            english_chars_per_page: 7000,
            page_height_mm: 290.0,
            page_width_mm: 200.0,
        };
        let effective = EffectiveCount {
            cjk_chars: 600,
            latin_chars: 400,
            total_significant: 1000,
        };
        let budget = compute_budget(&calib, &effective, 2);
        // CJK dominant -> uses cjk_chars_per_page
        assert_eq!(budget.language, Language::Chinese);
        // target = cjk_chars_per_page * max_pages = 8000 * 2 = 16000
        assert_eq!(budget.target, 16000);
        // soft_max = target * 1.4 = 22400
        assert_eq!(budget.soft_max, 22400);
        // min_acceptable = 16000 * 3/4 = 12000
        assert_eq!(budget.min_acceptable, 12000);
    }

    #[test]
    fn test_compute_budget_english_dominant() {
        let calib = CalibrationData {
            template: "test.typ".into(),
            calibrated_at: "2026-01-01T00:00:00Z".into(),
            cjk_chars_per_page: 8000,
            english_words_per_page: 1400,
            english_chars_per_page: 7000,
            page_height_mm: 290.0,
            page_width_mm: 200.0,
        };
        let effective = EffectiveCount {
            cjk_chars: 300,
            latin_chars: 700,
            total_significant: 1000,
        };
        let budget = compute_budget(&calib, &effective, 1);
        assert_eq!(budget.language, Language::English);
        // English target uses latin chars budget = english_chars_per_page * max_pages = 7000
        assert_eq!(budget.target, 7000);
    }

    #[test]
    fn test_format_target_chinese() {
        let budget = ContentBudget {
            target: 16000,
            soft_max: 22400,
            min_acceptable: 12000,
            language: Language::Chinese,
        };
        let formatted = format_target(&budget);
        assert!(formatted.contains("16000"));
        assert!(formatted.contains("字"));
    }

    #[test]
    fn test_format_target_english() {
        let budget = ContentBudget {
            target: 1400, // words
            soft_max: 1960,
            min_acceptable: 1050,
            language: Language::English,
        };
        let formatted = format_target(&budget);
        assert!(formatted.contains("1400"));
        assert!(formatted.contains("words"));
    }

    #[test]
    fn test_budget_estimation_one_page() {
        #[allow(deprecated)]
        let b = estimate_cheating_sheet_budget(1);
        assert_eq!(b.target_chars, 8500);
        assert_eq!(b.soft_max_chars, 12000);
        assert_eq!(b.min_acceptable_chars, 6375);
    }

    #[test]
    fn test_budget_estimation_two_pages() {
        #[allow(deprecated)]
        let b = estimate_cheating_sheet_budget(2);
        assert_eq!(b.target_chars, 17000);
        assert_eq!(b.soft_max_chars, 24000);
    }

    #[test]
    fn test_budget_estimation_three_pages() {
        #[allow(deprecated)]
        let b = estimate_cheating_sheet_budget(3);
        assert_eq!(b.target_chars, 25500);
    }

    #[test]
    fn test_budget_clamped_at_20() {
        #[allow(deprecated)]
        let b = estimate_cheating_sheet_budget(100);
        assert_eq!(b.target_chars, 20 * 8500);
    }

    #[test]
    fn test_budget_minimum_one() {
        #[allow(deprecated)]
        let b = estimate_cheating_sheet_budget(0);
        assert_eq!(b.target_chars, 8500);
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
        // 1 page of 2 → always expand (unless source too short).
        assert!(should_attempt_expansion(5000, 8250, 1, 2, false, None));
    }

    #[test]
    fn test_expansion_when_pages_below_max_even_with_precise_full_data() {
        // 1 page of 2, page appears full (99%) → still expand to fill page 2.
        let su = crate::artifacts::SpaceUtilization {
            total_content_pages: 1.0,
            max_pages: 2,
            overflow_ratio: None,
            page_utilizations: vec![],
            last_page_under_utilized: false,
            last_page_utilization_pct: 99.0,
        };
        assert!(should_attempt_expansion(
            15000,
            8250,
            1,
            2,
            false,
            Some(&su)
        ));
    }

    #[test]
    fn test_no_expansion_when_pages_full_and_utilized() {
        // 2 pages of 2, well-utilised → no expansion.
        assert!(!should_attempt_expansion(22000, 8250, 2, 2, false, None));
    }

    #[test]
    fn test_no_expansion_when_source_too_short_and_pages_below_max() {
        // 1 page of 3, but source too short AND generated < min_acceptable.
        assert!(!should_attempt_expansion(5000, 8250, 1, 3, true, None));
    }

    #[test]
    fn test_expansion_when_source_too_short_but_generated_adequate() {
        // 1 page of 2, source borderline but LLM generated enough.
        assert!(should_attempt_expansion(9000, 8250, 1, 2, true, None));
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

    #[test]
    fn test_expansion_when_pages_full_but_under_utilized() {
        let su = crate::artifacts::SpaceUtilization {
            total_content_pages: 2.0,
            max_pages: 2,
            overflow_ratio: None,
            page_utilizations: vec![],
            last_page_under_utilized: true,
            last_page_utilization_pct: 60.0,
        };
        assert!(should_attempt_expansion(
            10000,
            8250,
            2,
            2,
            false,
            Some(&su)
        ));
    }

    // -------------------------------------------------------------------------
    // count_effective / dominant_language tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_count_effective_strips_heading_hashes() {
        let md = "## 中文标题\n\nEnglish paragraph text here.\n";
        let count = count_effective(md);
        // "中文标题" = 4 CJK chars; "English paragraph text here." = 24 Latin chars
        assert_eq!(count.cjk_chars, 4);
        assert!(count.latin_chars >= 24);
        // The "## " prefix should NOT be counted
    }

    #[test]
    fn test_count_effective_skips_code_fences() {
        let md = "## Title\n\n```\ncode inside fence\n```\n\nreal content here\n";
        let count = count_effective(md);
        // "Title" (5) + "real" (4) + "content" (7) + "here" (4) = 20
        assert_eq!(count.latin_chars, 20);
        assert_eq!(count.cjk_chars, 0);
    }

    #[test]
    fn test_count_effective_strips_list_markers() {
        let md = "- first item\n- second item\n1. numbered one\n2. numbered two\n";
        let count = count_effective(md);
        // "first" (5) + "item" (4) + "second" (6) + "item" (4) + "numbered" (8) + "one" (3) + "numbered" (8) + "two" (3) = 41
        assert_eq!(count.latin_chars, 41);
    }

    #[test]
    fn test_count_effective_classifies_cjk() {
        let md = "中文内容 here\n日本語も more text\n";
        let count = count_effective(md);
        assert_eq!(count.cjk_chars, 4 + 4); // 中文内容 + 日本語も = 8
        assert!(count.latin_chars >= 12); // "here" + "more" + "text" = 12
    }

    #[test]
    fn test_dominant_language_cjk_majority() {
        let count = EffectiveCount {
            cjk_chars: 60,
            latin_chars: 40,
            total_significant: 100,
        };
        assert_eq!(dominant_language(&count), Language::Chinese);
    }

    #[test]
    fn test_dominant_language_english_majority() {
        let count = EffectiveCount {
            cjk_chars: 40,
            latin_chars: 60,
            total_significant: 100,
        };
        assert_eq!(dominant_language(&count), Language::English);
    }

    #[test]
    fn test_dominant_language_tie_goes_to_chinese() {
        let count = EffectiveCount {
            cjk_chars: 50,
            latin_chars: 50,
            total_significant: 100,
        };
        assert_eq!(dominant_language(&count), Language::Chinese);
    }

    #[test]
    fn test_count_effective_empty() {
        let count = count_effective("");
        assert_eq!(count.cjk_chars, 0);
        assert_eq!(count.latin_chars, 0);
        assert_eq!(count.total_significant, 0);
    }

    #[test]
    fn test_count_effective_all_blank_lines() {
        let count = count_effective("\n\n\n");
        assert_eq!(count.total_significant, 0);
    }

    #[test]
    fn test_count_effective_math_is_skipped() {
        let md = "公式 $x^2 + y^2$ 推导\nformula $E = mc^2$ derivation\n";
        let count = count_effective(md);
        // Math content ($...$) should NOT be counted as chars
        // 公式推导 = 4 CJK; formula derivation = 17 Latin
        assert_eq!(count.cjk_chars, 4); // 公式 + 推导, math skipped
    }
}
