//! Outline parsing and deterministic fallback generation.
//!
//! Functions for building course outline context, parsing LLM JSON outline
//! responses, and generating deterministic fallback outlines.

use anyhow::Result;

use crate::web::course::{truncate_chars, CourseDateIndex};

/// A planned section in a course note outline.
///
/// Built by the LLM during the outline phase; used to drive per-section
/// retrieval and generation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PlannedSection {
    /// Section heading (used as `## <title>` in the output).
    pub title: String,
    /// Why this section exists in the outline.
    pub purpose: String,
    /// Date sub-strings that hint which indexes to retrieve first.
    #[serde(default)]
    pub date_hints: Vec<String>,
    /// Video-ID sub-strings that hint which indexes to retrieve first.
    #[serde(default)]
    pub video_hints: Vec<String>,
    /// Extra search terms for BM25 fallback retrieval.
    #[serde(default)]
    pub query_terms: Vec<String>,
    /// Key concepts / formulas / definitions that must appear.
    #[serde(default)]
    pub must_include: Vec<String>,
}

/// Extract string arrays from JSON values (for date_hints, query_terms, etc.).
pub(crate) fn json_array_strings(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Build a context string from course date indexes for outline planning.
pub(crate) fn build_outline_context(indexes: &[CourseDateIndex]) -> String {
    let mut context = String::new();
    for index in indexes {
        let ranges_preview = index
            .timestamp_ranges
            .iter()
            .take(3)
            .map(|r| {
                format!(
                    "{} [{:.0}-{:.0}] {}",
                    r.video_id,
                    r.start,
                    r.end,
                    truncate_chars(&r.text_preview, 120)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        context.push_str(&format!(
            "Date: {} | {}\nSummary: {}\nKeywords: {}\nConcepts: {}\nRanges preview:\n{}\n\n",
            index.date,
            index.title,
            index.summary,
            index.keywords.join(", "),
            index.concepts.join(", "),
            ranges_preview
        ));
    }
    context
}

/// Parse the LLM outline response into `PlannedSection` items.
///
/// Returns `Ok(Vec<PlannedSection>)` on success or `Err(diagnostic: String)`
/// with parse error detail, response character count, char-safe head/tail
/// snippets, and `[truncated_by_length]` marker when `finish_reason ==
/// "length"`.
pub(crate) fn parse_outline_sections(
    text: &str,
    finish_reason: Option<&str>,
) -> std::result::Result<Vec<PlannedSection>, String> {
    let char_count = text.chars().count();
    // Char-safe head (first 200 chars) and tail (last 200 chars).
    let head: String = text.chars().take(200).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(200)
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    // Strip markdown code fences.
    let cleaned = {
        let t = text.trim();
        if let Some(rest) = t.strip_prefix("```json") {
            rest.strip_suffix("```").unwrap_or(rest).trim().to_string()
        } else if let Some(rest) = t.strip_prefix("```") {
            rest.strip_suffix("```").unwrap_or(rest).trim().to_string()
        } else {
            t.to_string()
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&cleaned) {
        Ok(v) => v,
        Err(e) => {
            let mut diag = format!("JSON parse error: {}. response_len={}", e, char_count);
            if let Some("length") = finish_reason {
                diag.push_str(" [truncated_by_length]");
            }
            if let Some(fr) = finish_reason {
                if fr != "length" {
                    diag.push_str(&format!(" finish_reason={}", fr));
                }
            }
            diag.push_str(&format!(" head={:?} tail={:?}", head, tail));
            return Err(diag);
        }
    };

    let sections_arr = json
        .get("sections")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "JSON missing 'sections' array. response_len={} finish_reason={:?} head={:?} tail={:?}",
                char_count, finish_reason, head, tail
            )
        })?;

    let mut sections = Vec::new();
    for item in sections_arr {
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            continue;
        }
        let purpose = item
            .get("purpose")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let date_hints = json_array_strings(item.get("date_hints"));
        let video_hints = json_array_strings(item.get("video_hints"));
        let query_terms = json_array_strings(item.get("query_terms"));
        let must_include = json_array_strings(item.get("must_include"));

        sections.push(PlannedSection {
            title,
            purpose,
            date_hints,
            video_hints,
            query_terms,
            must_include,
        });
    }

    if sections.is_empty() {
        return Err(format!(
            "Outline JSON contained no valid sections. response_len={} head={:?} tail={:?}",
            char_count, head, tail
        ));
    }

    Ok(sections)
}

/// Generate a deterministic fallback outline from `CourseDateIndex` metadata.
///
/// Groups dates chronologically into 4-24 sections. Section titles are
/// derived from title/keywords/concepts where available; `date_hints`,
/// `query_terms`, and `must_include` are derived from group keywords/concepts
/// capped to a few items.
///
/// Does **not** read transcript source files.
pub(crate) fn generate_fallback_outline(
    indexes: &[CourseDateIndex],
) -> Result<Vec<PlannedSection>> {
    if indexes.is_empty() {
        anyhow::bail!("Cannot generate fallback outline: no indexes available");
    }

    let n = indexes.len();
    // 4-24 sections when enough indexes exist; fewer otherwise, but ≥ 1.
    let section_count = ((n as f64).sqrt().ceil() as usize).clamp(if n >= 4 { 4 } else { 1 }, 24);

    let chunk_size = ((n as f64) / (section_count as f64)).ceil() as usize;

    let mut sections = Vec::new();
    for i in 0..section_count {
        let start = i * chunk_size;
        let end = (start + chunk_size).min(n);
        if start >= n {
            break;
        }
        let group = &indexes[start..end];

        // Derive a section title from the group metadata.
        let title = if group.len() == 1 {
            let idx = &group[0];
            if !idx.title.is_empty() {
                idx.title.clone()
            } else if !idx.summary.is_empty() {
                truncate_chars(&idx.summary, 80)
            } else {
                format!("Course Topics {}", i + 1)
            }
        } else {
            // Collect keywords/concepts from the group to form a title.
            let mut terms: Vec<&str> = Vec::new();
            for idx in group {
                for kw in &idx.keywords {
                    if terms.len() < 3 && !terms.contains(&kw.as_str()) {
                        terms.push(kw);
                    }
                }
            }
            if terms.is_empty() {
                for idx in group {
                    for c in &idx.concepts {
                        if terms.len() < 3 && !terms.contains(&c.as_str()) {
                            terms.push(c);
                        }
                    }
                }
            }
            if !terms.is_empty() {
                terms.join(" / ")
            } else if !group[0].title.is_empty() {
                group[0].title.clone()
            } else {
                format!("Course Topics {}", i + 1)
            }
        };

        // date_hints: all dates in the group.
        let date_hints: Vec<String> = group.iter().map(|idx| idx.date.clone()).collect();

        // query_terms: keywords from the group, capped.
        let mut query_terms: Vec<String> = Vec::new();
        for idx in group {
            for kw in &idx.keywords {
                if query_terms.len() < 5 && !query_terms.contains(kw) {
                    query_terms.push(kw.clone());
                }
            }
        }

        // must_include: concepts from the group, capped.
        let mut must_include: Vec<String> = Vec::new();
        for idx in group {
            for c in &idx.concepts {
                if must_include.len() < 5 && !must_include.contains(c) {
                    must_include.push(c.clone());
                }
            }
        }

        sections.push(PlannedSection {
            title,
            purpose: format!(
                "Fallback section covering dates {}-{}",
                group[0].date,
                group[group.len() - 1].date
            ),
            date_hints,
            video_hints: Vec::new(),
            query_terms,
            must_include,
        });
    }

    if sections.is_empty() {
        anyhow::bail!("Fallback outline produced zero sections");
    }

    Ok(sections)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::course::TimestampRange;

    fn make_test_index(
        date: &str,
        title: &str,
        summary: &str,
        keywords: &[&str],
    ) -> CourseDateIndex {
        CourseDateIndex {
            date: date.to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            keywords: keywords.iter().map(|s| s.to_string()).collect(),
            concepts: vec![],
            timestamp_ranges: vec![TimestampRange {
                video_id: "v1".to_string(),
                label: String::new(),
                start: 0.0,
                end: 60.0,
                text_preview: "sample".to_string(),
            }],
            char_count: 100,
            token_count: 50,
            source_path: "/tmp/test.md".to_string(),
            status: "ready".to_string(),
        }
    }

    fn make_fake_index(
        date: &str,
        title: &str,
        summary: &str,
        keywords: &[&str],
    ) -> CourseDateIndex {
        make_test_index(date, title, summary, keywords)
    }

    #[test]
    fn test_planned_section_deserialize_success() {
        let json = serde_json::json!({
            "title": "Introduction",
            "purpose": "Set the stage",
            "date_hints": ["2023-09"],
            "video_hints": [],
            "query_terms": ["intro"],
            "must_include": ["Definition of X"]
        });
        let section: PlannedSection = serde_json::from_value(json).unwrap();
        assert_eq!(section.title, "Introduction");
        assert_eq!(section.purpose, "Set the stage");
        assert_eq!(section.date_hints, vec!["2023-09"]);
        assert_eq!(section.query_terms, vec!["intro"]);
        assert_eq!(section.must_include, vec!["Definition of X"]);
    }

    #[test]
    fn test_planned_section_deserialize_minimal() {
        let json = serde_json::json!({"title": "Solo", "purpose": ""});
        let section: PlannedSection = serde_json::from_value(json).unwrap();
        assert_eq!(section.title, "Solo");
        assert!(section.date_hints.is_empty());
        assert!(section.must_include.is_empty());
    }

    #[test]
    fn test_planned_section_deserialize_missing_title_is_caught() {
        let json = serde_json::json!({"purpose": "no title"});
        let result: Result<PlannedSection, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_outline_json_invalid_missing_sections() {
        let json = r#"{"not_sections": []}"#;
        let result = parse_outline_sections(json, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing 'sections'"));
    }

    #[test]
    fn test_parse_outline_sections_success() {
        let json = r#"{"sections":[{"title":"A","purpose":"pA","date_hints":[],"video_hints":[],"query_terms":[],"must_include":[]},{"title":"B","purpose":"pB","date_hints":[],"video_hints":[],"query_terms":[],"must_include":[]}]}"#;
        let sections = parse_outline_sections(json, None).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "A");
        assert_eq!(sections[1].title, "B");
    }

    #[test]
    fn test_parse_outline_sections_code_fenced() {
        let json = "```json\n{\"sections\":[{\"title\":\"C\",\"purpose\":\"\",\"date_hints\":[],\"video_hints\":[],\"query_terms\":[],\"must_include\":[]}]}\n```";
        let sections = parse_outline_sections(json, None).unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title, "C");
    }

    #[test]
    fn test_parse_outline_sections_invalid_json_diagnostic() {
        let err = parse_outline_sections("not json at all", None).unwrap_err();
        assert!(err.contains("JSON parse error"));
        assert!(err.contains("response_len="));
    }

    #[test]
    fn test_parse_outline_sections_empty_sections_is_error() {
        let err = parse_outline_sections(r#"{"sections":[]}"#, None).unwrap_err();
        assert!(err.contains("no valid sections"));
    }

    #[test]
    fn test_parse_outline_sections_truncated_by_length() {
        let json = r#"{"sections":[{"title":"T","purpose":"","date_hints":[],"video_hints":[],"query_terms":[],"must_include":[]}]}"#;
        // finish_reason == "length" is recorded in the error.
        let result = parse_outline_sections(json, Some("length"));
        // This should succeed because the JSON is valid.
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_outline_sections_missing_sections_key() {
        let err = parse_outline_sections(r#"{"other":[]}"#, None).unwrap_err();
        assert!(err.contains("missing 'sections'"));
    }

    #[test]
    fn test_fallback_outline_produces_4_to_24_sections_with_enough_indexes() {
        let mut indexes = vec![];
        for i in 1..=20 {
            indexes.push(make_fake_index(
                &format!("2023-09-{:02}", i),
                &format!("Lecture {}", i),
                &format!("Summary {}", i),
                &[&format!("kw{}", i)],
            ));
        }
        let outline = generate_fallback_outline(&indexes).unwrap();
        assert!(
            (4..=24).contains(&outline.len()),
            "expected 4-24 sections, got {}",
            outline.len()
        );
    }

    #[test]
    fn test_fallback_outline_small_index_set() {
        let indexes = vec![make_fake_index("2023-09-01", "L1", "S1", &["a"])];
        let outline = generate_fallback_outline(&indexes).unwrap();
        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].title, "L1");
    }

    #[test]
    fn test_fallback_outline_does_not_read_transcript_files() {
        let indexes = vec![make_fake_index("2023-09-01", "Title", "Summary", &["kw"])];
        // Should succeed without accessing the filesystem for transcripts.
        let outline = generate_fallback_outline(&indexes).unwrap();
        assert!(!outline.is_empty());
    }

    #[test]
    fn test_fallback_outline_empty_indexes_fails() {
        let result = generate_fallback_outline(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_fallback_outline_single_index() {
        let indexes = vec![make_fake_index(
            "2023-09-01",
            "Test",
            "Test summary",
            &["test"],
        )];
        let outline = generate_fallback_outline(&indexes).unwrap();
        assert_eq!(outline.len(), 1);
    }

    #[test]
    fn test_build_outline_context_no_transcript_source() {
        let index = make_fake_index("2023-09-01", "L1", "Summary text", &["kw1"]);
        let context = build_outline_context(&[index]);
        assert!(context.contains("2023-09-01"));
        assert!(context.contains("L1"));
        assert!(context.contains("Summary text"));
        assert!(context.contains("kw1"));
    }

    #[test]
    fn test_build_outline_context_timestamp_limit() {
        let mut ranges = Vec::new();
        for i in 0..10 {
            ranges.push(TimestampRange {
                video_id: format!("v{}", i),
                label: String::new(),
                start: i as f64 * 10.0,
                end: i as f64 * 10.0 + 5.0,
                text_preview: format!("text {}", i),
            });
        }
        let index = CourseDateIndex {
            date: "2023-09-01".to_string(),
            title: "L1".to_string(),
            summary: "Summary".to_string(),
            keywords: vec![],
            concepts: vec![],
            timestamp_ranges: ranges,
            char_count: 200,
            token_count: 100,
            source_path: "/tmp/t.md".to_string(),
            status: "ready".to_string(),
        };
        let context = build_outline_context(&[index]);
        // Only first 3 ranges should appear.
        assert!(context.contains("text 0"));
        assert!(context.contains("text 1"));
        assert!(context.contains("text 2"));
        assert!(!context.contains("text 9"));
    }
}
