//! Course context retrieval for planned sections.
//!
//! Functions for retrieving compact transcript context for a single planned
//! section, preferring date-hint matches and falling back to BM25 search.

use std::fs;

use crate::llm::compact_transcript_for_llm;
use crate::utils::outline::PlannedSection;
use crate::web::course::{
    bm25_search, CourseDateIndex, RetrievalMatch, RetrievalTrace, TimestampRange,
};

/// Retrieve compact transcript context for a single planned Reference Digest
/// section.
///
/// Prefers indexes whose date appears in `section.date_hints`; falls back to
/// BM25 search. Limits selected indexes to 4 and enforces a per-section
/// context budget of 32 000 chars.
pub(crate) fn retrieve_course_context_for_section_ref_digest(
    section: &PlannedSection,
    indexes: &[CourseDateIndex],
) -> (String, RetrievalTrace) {
    let max_indexes: usize = 4;
    let context_budget: usize = 32000;
    let mut trace_matches = Vec::new();
    let mut selected: Vec<(&CourseDateIndex, f64)> = Vec::new();

    // 1) Date-hint match.
    if !section.date_hints.is_empty() {
        for index in indexes {
            if selected.len() >= max_indexes {
                break;
            }
            let date_matches = section.date_hints.iter().any(|hint| {
                index.date.contains(hint.as_str()) || hint.contains(index.date.as_str())
            });
            if date_matches {
                selected.push((index, 1.0));
            }
        }
    }

    // 2) Fallback to BM25.
    if selected.is_empty() {
        let query = format!(
            "{} {} {} {}",
            section.title,
            section.purpose,
            section.query_terms.join(" "),
            section.must_include.join(" ")
        );
        let hits = bm25_search(indexes, &query, max_indexes);
        for hit in hits {
            selected.push((hit.index, hit.score));
        }
    }

    let mut context = String::new();
    for (index, score) in &selected {
        let ranges = index
            .timestamp_ranges
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<TimestampRange>>();
        trace_matches.push(RetrievalMatch {
            date: index.date.clone(),
            score: *score,
            timestamp_ranges: ranges.clone(),
        });

        let remaining = context_budget.saturating_sub(context.chars().count());
        if remaining < 500 {
            break;
        }
        let source_text = fs::read_to_string(&index.source_path).unwrap_or_default();
        let transcript_budget = remaining.min(12000);
        context.push_str(&format!(
            "\n\n--- Date: {} | {} ---\nSummary: {}\nKeywords: {}\nConcepts: {}\nRanges:\n{}\nTranscript excerpt:\n{}",
            index.date,
            index.title,
            index.summary,
            index.keywords.join(", "),
            index.concepts.join(", "),
            ranges
                .iter()
                .map(|r| format!(
                    "{} [{:.0}-{:.0}] {}",
                    r.video_id, r.start, r.end, r.text_preview
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            compact_transcript_for_llm(&source_text, transcript_budget)
        ));
    }

    let trace = RetrievalTrace {
        section: section.title.clone(),
        matches: trace_matches,
        skipped_reason: if selected.is_empty() {
            Some("no relevant indexes found for section".to_string())
        } else {
            None
        },
    };

    (context, trace)
}

/// Retrieve compact transcript context for a single planned note section.
///
/// Prefers indexes whose date appears in `section.date_hints`; falls back to
/// BM25 search. Limits selected indexes per section to 4 and enforces a
/// per-section transcript/context budget of 32 000 chars.
pub(crate) fn retrieve_course_context_for_section(
    section: &PlannedSection,
    indexes: &[CourseDateIndex],
) -> (String, RetrievalTrace) {
    let max_indexes: usize = 4;
    let context_budget: usize = 32000;
    let mut trace_matches = Vec::new();
    let mut selected: Vec<(&CourseDateIndex, f64)> = Vec::new();

    // 1) Date-hint match.
    if !section.date_hints.is_empty() {
        for index in indexes {
            if selected.len() >= max_indexes {
                break;
            }
            let date_matches = section.date_hints.iter().any(|hint| {
                index.date.contains(hint.as_str()) || hint.contains(index.date.as_str())
            });
            if date_matches {
                selected.push((index, 1.0));
            }
        }
    }

    // 2) Fallback to BM25.
    if selected.is_empty() {
        let query = format!(
            "{} {} {} {}",
            section.title,
            section.purpose,
            section.query_terms.join(" "),
            section.must_include.join(" ")
        );
        let hits = bm25_search(indexes, &query, max_indexes);
        for hit in hits {
            selected.push((hit.index, hit.score));
        }
    }

    let mut context = String::new();
    for (index, score) in &selected {
        let ranges = index
            .timestamp_ranges
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<TimestampRange>>();
        trace_matches.push(RetrievalMatch {
            date: index.date.clone(),
            score: *score,
            timestamp_ranges: ranges.clone(),
        });

        let remaining = context_budget.saturating_sub(context.chars().count());
        if remaining < 500 {
            break;
        }
        let source_text = fs::read_to_string(&index.source_path).unwrap_or_default();
        let transcript_budget = remaining.min(8000);
        context.push_str(&format!(
            "\n\n--- Date: {} | {} ---\nSummary: {}\nKeywords: {}\nConcepts: {}\nRanges:\n{}\nTranscript excerpt:\n{}",
            index.date,
            index.title,
            index.summary,
            index.keywords.join(", "),
            index.concepts.join(", "),
            ranges
                .iter()
                .map(|r| format!(
                    "{} [{:.0}-{:.0}] {}",
                    r.video_id, r.start, r.end, r.text_preview
                ))
                .collect::<Vec<_>>()
                .join("\n"),
            compact_transcript_for_llm(&source_text, transcript_budget)
        ));
    }

    let trace = RetrievalTrace {
        section: section.title.clone(),
        matches: trace_matches,
        skipped_reason: if selected.is_empty() {
            Some("no relevant indexes found for section".to_string())
        } else {
            None
        },
    };

    (context, trace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::course::{CourseManifest, CourseManifestDate, TimestampRange};

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
                text_preview: "sample transcript text".to_string(),
            }],
            char_count: 100,
            token_count: 50,
            source_path: "/tmp/test_transcript.md".to_string(),
            status: "ready".to_string(),
        }
    }

    #[test]
    fn test_retrieval_prefers_date_hints_over_bm25() {
        let indexes = vec![
            make_test_index("2023-09-01", "Lecture A", "Topic X", &["x"]),
            make_test_index("2023-09-02", "Lecture B", "Topic Y", &["y"]),
            make_test_index("2023-09-03", "Lecture C", "Topic Z", &["z"]),
        ];
        let section = PlannedSection {
            title: "Section 1".to_string(),
            purpose: "Test".to_string(),
            date_hints: vec!["2023-09-02".to_string()],
            video_hints: vec![],
            query_terms: vec![],
            must_include: vec![],
        };
        let (context, trace) = retrieve_course_context_for_section(&section, &indexes);
        // The date-hint should match the 2023-09-02 index.
        assert!(
            context.contains("Lecture B"),
            "expected context for Lecture B, got: {}",
            context
        );
        assert_eq!(trace.matches.len(), 1);
        assert_eq!(trace.matches[0].date, "2023-09-02");
    }

    #[test]
    fn test_retrieval_falls_back_to_bm25_when_no_date_hints() {
        let indexes = vec![
            make_test_index("2023-09-01", "Lecture A", "Topic X", &["x"]),
            make_test_index("2023-09-02", "Lecture B", "Topic Y", &["y"]),
        ];
        let section = PlannedSection {
            title: "Y".to_string(),
            purpose: "Find Y".to_string(),
            date_hints: vec![],
            video_hints: vec![],
            query_terms: vec!["Y".to_string()],
            must_include: vec![],
        };
        let (_context, trace) = retrieve_course_context_for_section(&section, &indexes);
        // Should find at least Lecture B via BM25.
        assert!(!trace.matches.is_empty());
    }

    #[test]
    fn test_retrieval_does_not_include_all_indexes() {
        let mut indexes = vec![];
        for i in 1..=20 {
            indexes.push(make_test_index(
                &format!("2023-09-{:02}", i),
                &format!("L{}", i),
                &format!("Summary {}", i),
                &[&format!("kw{}", i)],
            ));
        }
        let section = PlannedSection {
            title: "General".to_string(),
            purpose: "All".to_string(),
            date_hints: vec![],
            video_hints: vec![],
            query_terms: vec![],
            must_include: vec![],
        };
        let (_context, trace) = retrieve_course_context_for_section(&section, &indexes);
        // At most 4 indexes selected.
        assert!(
            trace.matches.len() <= 4,
            "expected ≤4 matches, got {}",
            trace.matches.len()
        );
    }

    #[test]
    fn test_retrieval_respects_context_budget() {
        let indexes = vec![make_test_index("2023-09-01", "L1", "Summary", &["kw"])];
        let section = PlannedSection {
            title: "T".to_string(),
            purpose: "P".to_string(),
            date_hints: vec!["2023-09-01".to_string()],
            video_hints: vec![],
            query_terms: vec![],
            must_include: vec![],
        };
        let (context, _trace) = retrieve_course_context_for_section(&section, &indexes);
        // Context should not exceed the budget significantly.
        assert!(
            context.chars().count() <= 35000,
            "context too large: {} chars",
            context.chars().count()
        );
    }
}
