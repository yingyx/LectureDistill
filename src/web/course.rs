use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CourseManifest {
    pub source_id: String,
    pub course_id: String,
    pub course_name: String,
    #[serde(default)]
    pub dates: Vec<CourseManifestDate>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CourseManifestDate {
    pub date: String,
    pub title: String,
    pub source_path: String,
    pub index_path: String,
    pub video_count: usize,
    pub segment_count: usize,
    pub char_count: usize,
    pub token_count: usize,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CourseDateIndex {
    pub date: String,
    pub title: String,
    pub summary: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub concepts: Vec<String>,
    #[serde(default)]
    pub timestamp_ranges: Vec<TimestampRange>,
    pub char_count: usize,
    pub token_count: usize,
    pub source_path: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampRange {
    #[serde(default)]
    pub video_id: String,
    pub label: String,
    pub start: f64,
    pub end: f64,
    pub text_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalTrace {
    pub section: String,
    #[serde(default)]
    pub matches: Vec<RetrievalMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalMatch {
    pub date: String,
    pub score: f64,
    #[serde(default)]
    pub timestamp_ranges: Vec<TimestampRange>,
}

#[derive(Debug, Clone)]
pub struct NoteSection {
    pub heading: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct Bm25Hit<'a> {
    pub index: &'a CourseDateIndex,
    pub score: f64,
}

pub fn estimate_token_count(text: &str) -> usize {
    let mut count = 0usize;
    let mut in_ascii_word = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if !in_ascii_word {
                count += 1;
                in_ascii_word = true;
            }
        } else {
            in_ascii_word = false;
            if is_cjk(ch) {
                count += 1;
            }
        }
    }
    count
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF
    )
}

fn token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\p{Han}]|[A-Za-z0-9_]+").unwrap())
}

fn heading_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.+)$").unwrap())
}

fn video_heading_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^##\s+.*-\s+([A-Za-z0-9_-]+)\s*$").unwrap())
}

fn slide_heading_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^###\s+(.+?)\s+\[(\d{2}):(\d{2})-(\d{2}):(\d{2})\]\s*$").unwrap()
    })
}

fn timestamp_line_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\[(\d{2}):(\d{2})\]\s+(.+)$").unwrap())
}

pub fn tokenize(text: &str) -> Vec<String> {
    token_regex()
        .find_iter(text)
        .map(|m| m.as_str().to_lowercase())
        .filter(|t| !is_stopword(t))
        .collect()
}

fn is_stopword(token: &str) -> bool {
    matches!(
        token,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "for"
            | "from"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "the"
            | "to"
            | "with"
            | "this"
            | "that"
            | "这些"
            | "这个"
            | "我们"
            | "就是"
            | "一个"
    )
}

pub fn split_note_sections(markdown: &str) -> Vec<NoteSection> {
    let mut sections = Vec::new();
    let mut current_heading = "Document".to_string();
    let mut current_body = String::new();

    for line in markdown.lines() {
        if let Some(caps) = heading_regex().captures(line) {
            if !current_body.trim().is_empty() || !sections.is_empty() {
                sections.push(NoteSection {
                    heading: current_heading,
                    body: current_body.trim().to_string(),
                });
                current_body.clear();
            }
            current_heading = caps
                .get(2)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_else(|| "Untitled".to_string());
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    if !current_body.trim().is_empty() || sections.is_empty() {
        sections.push(NoteSection {
            heading: current_heading,
            body: current_body.trim().to_string(),
        });
    }

    sections
}

pub fn extract_timestamp_ranges(markdown: &str) -> Vec<TimestampRange> {
    let mut ranges = Vec::new();
    let mut current_video = String::new();
    let mut current_label = String::new();
    let mut current_start = 0.0;
    let mut current_end = 0.0;
    let mut current_text = String::new();

    let flush = |ranges: &mut Vec<TimestampRange>,
                 current_video: &str,
                 current_label: &str,
                 current_start: f64,
                 current_end: f64,
                 current_text: &mut String| {
        let preview = current_text.trim();
        if !preview.is_empty() {
            ranges.push(TimestampRange {
                video_id: current_video.to_string(),
                label: current_label.to_string(),
                start: current_start,
                end: current_end.max(current_start),
                text_preview: truncate_chars(preview, 260),
            });
        }
        current_text.clear();
    };

    for line in markdown.lines() {
        if let Some(caps) = video_heading_regex().captures(line) {
            flush(
                &mut ranges,
                &current_video,
                &current_label,
                current_start,
                current_end,
                &mut current_text,
            );
            current_video = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            current_label = line.trim_start_matches('#').trim().to_string();
            current_start = 0.0;
            current_end = 0.0;
            continue;
        }

        if let Some(caps) = slide_heading_regex().captures(line) {
            flush(
                &mut ranges,
                &current_video,
                &current_label,
                current_start,
                current_end,
                &mut current_text,
            );
            current_label = caps
                .get(1)
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| "Range".to_string());
            current_start = mmss_to_seconds(&caps[2], &caps[3]);
            current_end = mmss_to_seconds(&caps[4], &caps[5]);
            continue;
        }

        if let Some(caps) = timestamp_line_regex().captures(line.trim()) {
            let ts = mmss_to_seconds(&caps[1], &caps[2]);
            if current_text.is_empty() {
                current_start = if current_start > 0.0 {
                    current_start
                } else {
                    ts
                };
            }
            current_end = current_end.max(ts);
            if let Some(text) = caps.get(3) {
                current_text.push_str(text.as_str());
                current_text.push(' ');
            }
        } else if !line.trim().is_empty() && !line.trim_start().starts_with("_Slide OCR:_") {
            current_text.push_str(line.trim());
            current_text.push(' ');
        }
    }

    flush(
        &mut ranges,
        &current_video,
        &current_label,
        current_start,
        current_end,
        &mut current_text,
    );

    ranges
}

fn mmss_to_seconds(mm: &str, ss: &str) -> f64 {
    let minutes = mm.parse::<f64>().unwrap_or(0.0);
    let seconds = ss.parse::<f64>().unwrap_or(0.0);
    minutes * 60.0 + seconds
}

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max_chars).collect();
        s.push_str("...");
        s
    }
}

pub fn build_index_document(index: &CourseDateIndex) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        index.date,
        index.title,
        index.summary,
        index.keywords.join(" "),
        index
            .concepts
            .iter()
            .chain(index.timestamp_ranges.iter().map(|r| &r.text_preview))
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    )
}

pub fn bm25_search<'a>(
    indexes: &'a [CourseDateIndex],
    query: &str,
    limit: usize,
) -> Vec<Bm25Hit<'a>> {
    let query_terms = tokenize(query);
    if query_terms.is_empty() || indexes.is_empty() || limit == 0 {
        return Vec::new();
    }

    let docs: Vec<Vec<String>> = indexes
        .iter()
        .map(|idx| tokenize(&build_index_document(idx)))
        .collect();
    let avg_len = docs.iter().map(|d| d.len() as f64).sum::<f64>() / docs.len() as f64;
    let mut df: HashMap<&str, usize> = HashMap::new();
    for doc in &docs {
        let unique: HashSet<&str> = doc.iter().map(|s| s.as_str()).collect();
        for term in unique {
            *df.entry(term).or_insert(0) += 1;
        }
    }

    let n = docs.len() as f64;
    let k1 = 1.5;
    let b = 0.75;
    let mut hits = Vec::new();

    for (idx, doc) in docs.iter().enumerate() {
        if doc.is_empty() {
            continue;
        }
        let mut tf: HashMap<&str, usize> = HashMap::new();
        for token in doc {
            *tf.entry(token.as_str()).or_insert(0) += 1;
        }
        let dl = doc.len() as f64;
        let mut score = 0.0;
        for term in &query_terms {
            let freq = *tf.get(term.as_str()).unwrap_or(&0) as f64;
            if freq == 0.0 {
                continue;
            }
            let doc_freq = *df.get(term.as_str()).unwrap_or(&0) as f64;
            let idf = ((n - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln();
            let denom = freq + k1 * (1.0 - b + b * dl / avg_len.max(1.0));
            score += idf * freq * (k1 + 1.0) / denom;
        }
        if score > 0.0 {
            hits.push(Bm25Hit {
                index: &indexes[idx],
                score,
            });
        }
    }

    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    hits.truncate(limit);
    hits
}

pub fn read_manifest(path: &str) -> Result<CourseManifest> {
    let text = fs::read_to_string(path).with_context(|| format!("failed to read {path}"))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {path}"))
}

pub fn write_manifest(path: &Path, manifest: &CourseManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(manifest)?)?;
    Ok(())
}

pub fn read_indexes(manifest: &CourseManifest) -> Vec<CourseDateIndex> {
    manifest
        .dates
        .iter()
        .filter_map(|d| {
            fs::read_to_string(&d.index_path)
                .ok()
                .and_then(|text| serde_json::from_str::<CourseDateIndex>(&text).ok())
        })
        .collect()
}

pub fn write_index(path: &Path, index: &CourseDateIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(index)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_count_counts_ascii_words_and_cjk_chars() {
        assert_eq!(estimate_token_count("linear regression 线性回归"), 6);
    }

    #[test]
    fn bm25_finds_relevant_date() {
        let indexes = vec![
            CourseDateIndex {
                date: "2026-01-01".into(),
                title: "Intro".into(),
                summary: "overview of course logistics".into(),
                keywords: vec!["syllabus".into()],
                concepts: vec![],
                timestamp_ranges: vec![],
                char_count: 10,
                token_count: 2,
                source_path: "a.md".into(),
                status: "ready".into(),
            },
            CourseDateIndex {
                date: "2026-01-02".into(),
                title: "Optimization".into(),
                summary: "gradient descent and convex optimization".into(),
                keywords: vec!["gradient".into(), "convex".into()],
                concepts: vec![],
                timestamp_ranges: vec![],
                char_count: 10,
                token_count: 2,
                source_path: "b.md".into(),
                status: "ready".into(),
            },
        ];
        let hits = bm25_search(&indexes, "convex gradient methods", 3);
        assert_eq!(hits[0].index.date, "2026-01-02");
    }

    #[test]
    fn split_note_sections_handles_markdown_headings() {
        let sections = split_note_sections("# A\none\n## B\ntwo\n");
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].heading, "A");
        assert_eq!(sections[1].heading, "B");
    }
}
