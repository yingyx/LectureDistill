//! Data models for lecture-distill artifacts.
//!
//! Equivalent to the Python `artifacts.py` module.
//! All models derive `Serialize`/`Deserialize` for JSON artifact storage.

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// Distillation keep level - mirrors the Python `KeepLevel` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepLevel {
    /// Critical content that must appear in the final cheat sheet.
    #[serde(rename = "must_keep")]
    MustKeep,
    /// Content that should be condensed/summarised.
    #[serde(rename = "compress")]
    Compress,
    /// Content safe to omit.
    #[serde(rename = "drop")]
    Drop,
}

impl Default for KeepLevel {
    fn default() -> Self {
        Self::Compress
    }
}

// ---------------------------------------------------------------------------
// Transcript artifacts
// ---------------------------------------------------------------------------

/// A single subtitle segment (SRT entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    /// 1-based segment index.
    pub index: usize,
    /// Start time in seconds.
    pub start_time: f64,
    /// End time in seconds.
    pub end_time: f64,
    /// Subtitle text content (whitespace-normalised).
    pub text: String,
}

/// Full transcript for one video.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptArtifact {
    /// Video identifier from the SJTU video platform.
    pub video_id: String,
    /// Human-readable video title.
    pub video_title: String,
    /// Canvas course ID.
    pub course_id: String,
    /// Language code (default `"zh"`).
    #[serde(default = "default_language")]
    pub language: String,
    /// Subtitle segments.
    #[serde(default)]
    pub segments: Vec<TranscriptSegment>,
    /// ISO-8601 timestamp when the transcript was fetched.
    pub fetched_at: String,
    /// Recording start time, if available from Canvas video metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_at: Option<String>,
    /// Source video URL, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

fn default_language() -> String {
    "zh".to_string()
}

impl TranscriptArtifact {
    /// Create a new transcript artifact with the current timestamp.
    pub fn new(video_id: String, video_title: String, course_id: String) -> Self {
        Self {
            video_id,
            video_title,
            course_id,
            language: "zh".to_string(),
            segments: Vec::new(),
            fetched_at: Utc::now().to_rfc3339(),
            recorded_at: None,
            source_url: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Note / patch artifacts
// ---------------------------------------------------------------------------

/// Loaded Markdown notes file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteArtifact {
    /// Original file path.
    pub path: String,
    /// Raw Markdown content.
    pub content: String,
    /// Extracted heading texts.
    #[serde(default)]
    pub headings: Vec<String>,
}

/// A single patch applied to the notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchEntry {
    /// Heading or section where the patch was applied.
    pub location: String,
    /// Original text that was modified (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_text: Option<String>,
    /// New text inserted.
    pub new_text: String,
    /// Source video identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_video_id: Option<String>,
    /// Timestamp in seconds within the source video.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_timestamp: Option<f64>,
    /// Distillation keep level.
    #[serde(default)]
    pub keep_level: KeepLevel,
}

/// Result of patching notes with transcript data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchArtifact {
    /// Original notes file path.
    pub source_notes_path: String,
    /// Transcript artifact paths used.
    #[serde(default)]
    pub source_transcripts: Vec<String>,
    /// Applied patches.
    #[serde(default)]
    pub patches: Vec<PatchEntry>,
    /// Unresolved conflicts for manual review.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// ISO-8601 timestamp of patching.
    pub patched_at: String,
}

impl PatchArtifact {
    pub fn new(source_notes_path: String) -> Self {
        Self {
            source_notes_path,
            source_transcripts: Vec::new(),
            patches: Vec::new(),
            conflicts: Vec::new(),
            patched_at: Utc::now().to_rfc3339(),
        }
    }
}

// ---------------------------------------------------------------------------
// Ranking / distillation artifacts
// ---------------------------------------------------------------------------

/// A ranked concept with exam relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedConcept {
    /// Concept name or short description.
    pub name: String,
    /// Distillation keep level.
    #[serde(default)]
    pub keep_level: KeepLevel,
    /// Exam relevance score 0.0-1.0.
    #[serde(default = "default_relevance")]
    pub relevance_score: f64,
    /// Related heading names.
    #[serde(default)]
    pub source_headings: Vec<String>,
    /// Video timestamp references.
    #[serde(default)]
    pub timestamp_references: Vec<f64>,
    /// Explanation for the score.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

fn default_relevance() -> f64 {
    0.5
}

// ---------------------------------------------------------------------------
// Cheat sheet artifact
// ---------------------------------------------------------------------------

/// Result of rendering a LaTeX cheat sheet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheatSheetArtifact {
    /// Path to the compiled PDF.
    pub pdf_path: String,
    /// Number of pages in the final PDF.
    #[serde(default)]
    pub page_count: usize,
    /// Template filename used.
    #[serde(default = "default_template_name")]
    pub template_used: String,
    /// Path to the distilled markdown input.
    pub distilled_content_path: String,
    /// ISO-8601 rendering timestamp.
    pub rendered_at: String,
    /// Number of compression attempts applied.
    #[serde(default)]
    pub compression_attempts: usize,
}

fn default_template_name() -> String {
    "default_cheatsheet.tex".to_string()
}

impl CheatSheetArtifact {
    pub fn new(pdf_path: String, distilled_content_path: String, template_used: String) -> Self {
        Self {
            pdf_path,
            page_count: 0,
            template_used,
            distilled_content_path,
            rendered_at: Utc::now().to_rfc3339(),
            compression_attempts: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keep_level_serialization() {
        let must = serde_json::to_string(&KeepLevel::MustKeep).unwrap();
        assert_eq!(must, r#""must_keep""#);

        let compress = serde_json::to_string(&KeepLevel::Compress).unwrap();
        assert_eq!(compress, r#""compress""#);

        let drop = serde_json::to_string(&KeepLevel::Drop).unwrap();
        assert_eq!(drop, r#""drop""#);
    }

    #[test]
    fn test_keep_level_default() {
        assert_eq!(KeepLevel::default(), KeepLevel::Compress);
    }

    #[test]
    fn test_transcript_artifact_serialization() {
        let ta = TranscriptArtifact {
            video_id: "vid1".into(),
            video_title: "Test Video".into(),
            course_id: "course1".into(),
            language: "zh".into(),
            segments: vec![TranscriptSegment {
                index: 1,
                start_time: 0.0,
                end_time: 2.5,
                text: "Hello world".into(),
            }],
            fetched_at: "2025-01-01T00:00:00Z".into(),
            recorded_at: None,
            source_url: None,
        };
        let json = serde_json::to_string_pretty(&ta).unwrap();
        assert!(json.contains("vid1"));
        assert!(json.contains("Test Video"));
        // source_url should be absent when None
        assert!(!json.contains("source_url"));
    }

    #[test]
    fn test_patch_artifact_defaults() {
        let pa = PatchArtifact::new("notes.md".into());
        assert!(pa.patches.is_empty());
        assert!(pa.conflicts.is_empty());
        assert!(!pa.patched_at.is_empty());
    }

    #[test]
    fn test_cheat_sheet_artifact_defaults() {
        let cs =
            CheatSheetArtifact::new("out.pdf".into(), "distilled.md".into(), "mytpl.tex".into());
        assert_eq!(cs.page_count, 0);
        assert_eq!(cs.compression_attempts, 0);
        assert_eq!(cs.template_used, "mytpl.tex");
    }
}
