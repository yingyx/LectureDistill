//! Exam relevance scoring for lecture notes.
//!
//! Provides both deterministic (rule-based) and LLM-powered scoring strategies
//! to rank concepts by their exam relevance.

use crate::artifacts::{KeepLevel, RankedConcept};
use crate::llm;
use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};

/// Score concepts for exam relevance.
///
/// If `use_llm` is true and an LLM is available, tries LLM-based scoring first.
/// Falls back to deterministic scoring on any error or empty result.
pub async fn score_concepts(content: &str, use_llm: bool) -> Vec<RankedConcept> {
    if use_llm && llm::is_available() {
        match llm_score(content).await {
            Ok(concepts) if !concepts.is_empty() => return concepts,
            Ok(_) => {
                log::warn!("LLM scoring returned empty result, falling back to deterministic");
            }
            Err(e) => {
                log::warn!("LLM scoring failed, falling back to deterministic: {}", e);
            }
        }
    }
    deterministic_score(content)
}

/// Deterministic scoring using four strategies:
///
/// 1. **Headings** (lines starting with `#`) -> `MUST_KEEP`, score 0.9,
///    rationale "Section heading - likely an exam topic".
/// 2. **Bold text** (`**...**`) -> `COMPRESS`, score 0.7, no rationale.
/// 3. **LaTeX formulas** (`$$...$$` or `$...$`) -> `MUST_KEEP`, score 0.95,
///    rationale "Mathematical formula - likely exam-critical", name prefixed
///    with `"Formula: "`.
/// 4. **Repeated significant terms** - capitalized words of 3+ characters
///    appearing 3+ times -> `COMPRESS`, score `min(0.5 + count * 0.05, 0.95)`,
///    rationale `"Repeated {count} times"`.
///
/// All concepts are deduplicated via a `HashSet` (case-insensitive for names).
fn deterministic_score(content: &str) -> Vec<RankedConcept> {
    let mut concepts: Vec<RankedConcept> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // ------------------------------------------------------------------
    // Strategy 1: Headings
    // ------------------------------------------------------------------
    let heading_re = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    for cap in heading_re.captures_iter(content) {
        let name = cap[1].trim().to_string();
        let lower = name.to_lowercase();
        if seen.insert(lower) {
            concepts.push(RankedConcept {
                name,
                keep_level: KeepLevel::MustKeep,
                relevance_score: 0.9,
                source_headings: Vec::new(),
                timestamp_references: Vec::new(),
                rationale: Some("Section heading - likely an exam topic".into()),
            });
        }
    }

    // ------------------------------------------------------------------
    // Strategy 2: Bold text
    // ------------------------------------------------------------------
    let bold_re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    for cap in bold_re.captures_iter(content) {
        let name = cap[1].trim().to_string();
        let lower = name.to_lowercase();
        if !name.is_empty() && seen.insert(lower) {
            concepts.push(RankedConcept {
                name,
                keep_level: KeepLevel::Compress,
                relevance_score: 0.7,
                source_headings: Vec::new(),
                timestamp_references: Vec::new(),
                rationale: None,
            });
        }
    }

    // ------------------------------------------------------------------
    // Strategy 3: LaTeX formulas ($$...$$ first, then $...$)
    // ------------------------------------------------------------------
    let formula_re = Regex::new(r"\$\$(.+?)\$\$|\$(.+?)\$").unwrap();
    for cap in formula_re.captures_iter(content) {
        let formula = cap.get(1).or_else(|| cap.get(2));
        if let Some(m) = formula {
            let name = format!("Formula: {}", m.as_str().trim());
            let lower = name.to_lowercase();
            if seen.insert(lower) {
                concepts.push(RankedConcept {
                    name,
                    keep_level: KeepLevel::MustKeep,
                    relevance_score: 0.95,
                    source_headings: Vec::new(),
                    timestamp_references: Vec::new(),
                    rationale: Some("Mathematical formula - likely exam-critical".into()),
                });
            }
        }
    }

    // ------------------------------------------------------------------
    // Strategy 4: Repeated significant terms
    // ------------------------------------------------------------------
    let term_re = Regex::new(r"\b([A-Z][a-zA-Z0-9]{2,})\b").unwrap();
    let mut term_counts: HashMap<String, (String, usize)> = HashMap::new();
    for cap in term_re.captures_iter(content) {
        let original = cap[1].to_string();
        let lower = original.to_lowercase();
        let entry = term_counts.entry(lower).or_insert_with(|| (original, 0));
        entry.1 += 1;
    }

    for (_lower, (original, count)) in term_counts {
        if count >= 3 && seen.insert(original.to_lowercase()) {
            let score = (0.5 + count as f64 * 0.05).min(0.95);
            concepts.push(RankedConcept {
                name: original,
                keep_level: KeepLevel::Compress,
                relevance_score: score,
                source_headings: Vec::new(),
                timestamp_references: Vec::new(),
                rationale: Some(format!("Repeated {} times", count)),
            });
        }
    }

    concepts
}

/// LLM-based scoring.
///
/// Sends the content (truncated to 20000 characters) to the LLM for concept
/// extraction and scoring.  Skips individual entries that fail to parse.
async fn llm_score(content: &str) -> Result<Vec<RankedConcept>> {
    let system_prompt = "\
You are an exam preparation assistant. Analyze the following lecture notes and \
identify key concepts for exam review. Output JSON with a 'concepts' array. \
Each concept has: name (string), keep_level ('must_keep'/'compress'/'drop'), \
relevance_score (float 0.0-1.0), rationale (optional string), source_headings \
(optional string array). Focus on content likely to appear on exams: \
definitions, formulas, theorems, key dates, important procedures.";

    let truncated: String = content.chars().take(20000).collect();
    let user_prompt = format!(
        "Analyze the following lecture notes and identify key concepts for exam \
review:\n\n{}",
        truncated
    );

    let json = llm::chat_json(system_prompt, &user_prompt, 0.3, 4096).await?;

    let mut concepts = Vec::new();
    if let Some(arr) = json["concepts"].as_array() {
        for item in arr {
            // Skip entries whose name we cannot read.
            let name = match item["name"].as_str() {
                Some(n) => n.to_string(),
                None => continue,
            };

            let keep_level = match item["keep_level"].as_str() {
                Some("must_keep") => KeepLevel::MustKeep,
                Some("compress") => KeepLevel::Compress,
                Some("drop") => KeepLevel::Drop,
                _ => KeepLevel::Compress,
            };

            let relevance_score = item["relevance_score"].as_f64().unwrap_or(0.5);

            let rationale = item["rationale"].as_str().map(|s| s.to_string());

            let source_headings = item["source_headings"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            concepts.push(RankedConcept {
                name,
                keep_level,
                relevance_score,
                source_headings,
                timestamp_references: Vec::new(),
                rationale,
            });
        }
    }

    Ok(concepts)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_score_extracts_headings() {
        let content = "# Introduction\nSome text\n## Background\nMore text\n### Details\n";
        let concepts = deterministic_score(content);
        let names: Vec<&str> = concepts.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Introduction"));
        assert!(names.contains(&"Background"));
        assert!(names.contains(&"Details"));
        for c in &concepts {
            if c.name == "Introduction" {
                assert_eq!(c.keep_level, KeepLevel::MustKeep);
                assert_eq!(c.relevance_score, 0.9);
                assert_eq!(
                    c.rationale.as_deref(),
                    Some("Section heading - likely an exam topic")
                );
            }
        }
    }

    #[test]
    fn deterministic_score_extracts_bold_text() {
        let content = "The **Riemann Hypothesis** is important. Also see **Linear Algebra**.";
        let concepts = deterministic_score(content);
        let names: Vec<&str> = concepts.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Riemann Hypothesis"));
        assert!(names.contains(&"Linear Algebra"));
        for c in &concepts {
            if c.name == "Riemann Hypothesis" {
                assert_eq!(c.keep_level, KeepLevel::Compress);
                assert_eq!(c.relevance_score, 0.7);
                assert!(c.rationale.is_none());
            }
        }
    }

    #[test]
    fn deterministic_score_extracts_formulas() {
        let content = "The integral $$\\int_0^\\infty e^{-x^2} dx = \\frac{\\sqrt{\\pi}}{2}$$ is key. Also $E = mc^2$ matters.";
        let concepts = deterministic_score(content);
        let names: Vec<&str> = concepts.iter().map(|c| c.name.as_str()).collect();
        assert!(names.iter().any(|n| n.contains("\\int")));
        assert!(names.iter().any(|n| n.contains("E = mc^2")));
        for c in &concepts {
            if c.name.starts_with("Formula:") {
                assert_eq!(c.keep_level, KeepLevel::MustKeep);
                assert_eq!(c.relevance_score, 0.95);
                assert_eq!(
                    c.rationale.as_deref(),
                    Some("Mathematical formula - likely exam-critical")
                );
            }
        }
    }

    #[test]
    fn deterministic_score_finds_repeated_terms() {
        let content =
            "Newton invented calculus. Newton also worked on optics. Newton's laws are fundamental.";
        let concepts = deterministic_score(content);
        // "Newton" is a capitalized word with 3+ chars, appearing 3 times
        let newton = concepts.iter().find(|c| c.name == "Newton");
        assert!(newton.is_some());
        let n = newton.unwrap();
        assert_eq!(n.keep_level, KeepLevel::Compress);
        // score = 0.5 + 3 * 0.05 = 0.65
        assert!((n.relevance_score - 0.65).abs() < 0.01);
        assert_eq!(n.rationale.as_deref(), Some("Repeated 3 times"));
    }

    #[test]
    fn deterministic_score_deduplicates_concepts() {
        let content =
            "# Introduction\n# Introduction\n# INTRODUCTION\n**Bold Term**\n**Bold Term**\n";
        let concepts = deterministic_score(content);
        // All duplicates should be removed (case-insensitive)
        let intro_count = concepts
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case("Introduction"))
            .count();
        assert_eq!(intro_count, 1);
        let bold_count = concepts
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case("Bold Term"))
            .count();
        assert_eq!(bold_count, 1);
    }
}
