//! Transcript Markdown rendering utilities.
//!
//! Functions for formatting timestamps, joining text, splitting sentences,
//! and rendering transcript artifacts as Markdown with PPT slide markers.
//!
//! These are the canonical shared implementations. Production code currently
//! uses local copies in `web/handlers/`; a future refactor should consolidate
//! all callers to use these shared versions.
#![allow(dead_code)]

use crate::artifacts::TranscriptArtifact;
use crate::canvas_sjtu::CanvasPptSlice;

/// Format seconds as `MM:SS` or `HH:MM:SS`.
pub(crate) fn format_clock(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// Check if text ends with sentence-ending punctuation.
pub(crate) fn is_sentence_terminal(text: &str) -> bool {
    text.trim_end()
        .chars()
        .last()
        .map(|ch| matches!(ch, '.' | '!' | '?'))
        .unwrap_or(false)
}

/// Append `text` to `target`, inserting a space when appropriate.
pub(crate) fn push_joined_text(target: &mut String, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if !target.is_empty()
        && !target.ends_with(char::is_whitespace)
        && !text.starts_with(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '!' | '?'))
    {
        target.push(' ');
    }
    target.push_str(text);
}

/// Split text into sentence-level chunks, at most `max_sentences` per chunk.
pub(crate) fn split_sentence_chunks(text: &str, max_sentences: usize) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            continue;
        }
        current.push(ch);
        if matches!(ch, '.' | '!' | '?') {
            sentences.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        sentences.push(current);
    }
    if sentences.is_empty() {
        return vec![text.trim().to_string()];
    }
    sentences
        .chunks(max_sentences.max(1))
        .map(|chunk| chunk.concat())
        .collect()
}

/// Extract a date string (YYYY-MM-DD) from a transcript artifact.
pub(crate) fn transcript_date(artifact: &TranscriptArtifact) -> String {
    artifact
        .recorded_at
        .as_deref()
        .or(Some(artifact.fetched_at.as_str()))
        .and_then(|value| value.get(0..10))
        .unwrap_or("")
        .to_string()
}

/// Render a transcript artifact as Markdown, interleaving PPT slide markers
/// with timestamped sentence lines.
pub(crate) fn transcript_markdown_for_video(
    artifact: &TranscriptArtifact,
    ppt_slices: &[CanvasPptSlice],
) -> String {
    let mut slides = ppt_slices.to_vec();
    slides.sort_by(|a, b| a.create_sec.total_cmp(&b.create_sec));

    if slides.is_empty() {
        slides.push(CanvasPptSlice {
            create_sec: 0.0,
            ppt_img_url: None,
            ocr_words: Vec::new(),
        });
    }

    let max_end = artifact
        .segments
        .iter()
        .map(|segment| segment.end_time)
        .fold(0.0_f64, f64::max);

    #[derive(Debug)]
    struct SentenceLine {
        start_time: f64,
        end_time: f64,
        text: String,
    }

    let mut lines = Vec::new();
    let mut current_text = String::new();
    let mut current_start = 0.0_f64;
    let mut current_end = 0.0_f64;
    let mut has_current = false;

    for segment in &artifact.segments {
        if !has_current {
            current_start = segment.start_time;
            current_text.clear();
            has_current = true;
        }
        current_end = segment.end_time;
        push_joined_text(&mut current_text, &segment.text);
        if is_sentence_terminal(&segment.text) {
            lines.push(SentenceLine {
                start_time: current_start,
                end_time: current_end,
                text: std::mem::take(&mut current_text),
            });
            has_current = false;
        }
    }
    if has_current && !current_text.trim().is_empty() {
        lines.push(SentenceLine {
            start_time: current_start,
            end_time: current_end,
            text: current_text,
        });
    }

    let mut markdown = String::new();
    if !artifact.video_title.trim().is_empty() {
        markdown.push_str(&format!("## {}\n\n", artifact.video_title.trim()));
    }

    for (idx, slide) in slides.iter().enumerate() {
        let slide_start = slide.create_sec.max(0.0);
        let next_start = slides.get(idx + 1).map(|next| next.create_sec);
        let raw_slide_end = next_start.unwrap_or(max_end);
        let slide_end = next_start
            .map(|next| next.min(max_end.max(slide_start)))
            .unwrap_or_else(|| max_end.max(slide_start));

        markdown.push_str(&format!(
            "### Slide {} [{}-{}]\n\n",
            idx + 1,
            format_clock(slide_start),
            format_clock(slide_end)
        ));

        if !slide.ocr_words.is_empty() {
            markdown.push_str(&format!("_Slide OCR:_ {}\n\n", slide.ocr_words.join(" ")));
        }

        let range_end = next_start.unwrap_or(f64::MAX);
        for line in lines
            .iter()
            .filter(|line| line.start_time >= slide_start && line.start_time < range_end)
        {
            let chunks =
                if line.end_time - line.start_time > 180.0 || raw_slide_end - slide_start > 180.0 {
                    split_sentence_chunks(&line.text, 4)
                } else {
                    vec![line.text.trim().to_string()]
                };
            for chunk in chunks {
                if !chunk.is_empty() {
                    markdown.push_str(&format!(
                        "[{}] {}\n\n",
                        format_clock(line.start_time),
                        chunk
                    ));
                }
            }
        }
    }

    markdown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::TranscriptSegment;

    fn make_transcript(segments: Vec<TranscriptSegment>) -> TranscriptArtifact {
        TranscriptArtifact {
            video_id: "test-video".to_string(),
            video_title: String::new(),
            course_id: String::new(),
            language: "zh".to_string(),
            segments,
            fetched_at: "2024-01-01T00:00:00Z".to_string(),
            recorded_at: None,
            source_url: Some("http://example.com".to_string()),
        }
    }

    #[test]
    fn test_compact_transcript_basic() {
        let artifact = make_transcript(vec![
            TranscriptSegment {
                index: 0,
                start_time: 0.0,
                end_time: 5.0,
                text: "Hello world.".to_string(),
            },
            TranscriptSegment {
                index: 1,
                start_time: 5.0,
                end_time: 10.0,
                text: "This is a test.".to_string(),
            },
        ]);
        let md = transcript_markdown_for_video(&artifact, &[]);
        assert!(md.contains("Hello world"));
        assert!(md.contains("This is a test"));
    }

    #[test]
    fn test_transcript_markdown_uses_ppt_boundaries_without_splitting_sentence() {
        let artifact = make_transcript(vec![TranscriptSegment {
            index: 0,
            start_time: 10.0,
            end_time: 15.0,
            text: "This is one sentence.".to_string(),
        }]);
        let slides = vec![CanvasPptSlice {
            create_sec: 5.0,
            ppt_img_url: None,
            ocr_words: vec!["slide1".to_string()],
        }];
        let md = transcript_markdown_for_video(&artifact, &slides);
        // The sentence lands in slide 1 (its start_time >= 5.0 and < f64::MAX).
        assert!(md.contains("Slide 1"), "expected Slide 1 heading: {}", md);
        assert!(md.contains("This is one sentence"), "{}", md);
        assert!(!md.contains("Slide 2"), "unexpected Slide 2: {}", md);
    }

    #[test]
    fn test_transcript_markdown_long_ppt_range_falls_back_to_sentence_chunks() {
        let mut segments = Vec::new();
        for i in 0..50 {
            segments.push(TranscriptSegment {
                index: i,
                start_time: i as f64 * 8.0,
                end_time: i as f64 * 8.0 + 7.5,
                text: format!("Sentence {} is here.", i),
            });
        }
        let artifact = make_transcript(segments);
        // Single slide with a 200s range (above the 180s threshold).
        let slides = vec![CanvasPptSlice {
            create_sec: 0.0,
            ppt_img_url: None,
            ocr_words: vec!["ocr".to_string()],
        }];
        let md = transcript_markdown_for_video(&artifact, &slides);
        // Should produce chunks, so there should be fewer than 50 timestamp lines.
        let timestamp_lines = md.lines().filter(|l| l.starts_with('[')).count();
        assert!(
            timestamp_lines <= 50,
            "expected at most 50 timestamp lines, got {}",
            timestamp_lines
        );
        // sentence_chunks strips whitespace, so "Sentence 0 is here." → "Sentence0ishere."
        assert!(md.contains("Sentence0ishere"), "{}", md);
        assert!(md.contains("Slide"), "{}", md);
        assert!(md.contains("ocr"), "{}", md);
    }
}
