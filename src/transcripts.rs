//! SRT subtitle parsing and writing.
//!
//! Handles SRT and VTT-style timestamp formats, normalises Windows line
//! endings, and provides round-trip serialisation through
//! [`TranscriptSegment`] / [`TranscriptArtifact`].

use regex::Regex;
use std::sync::OnceLock;

use crate::artifacts::{TranscriptArtifact, TranscriptSegment};

// ---------------------------------------------------------------------------
// Regex singleton - compiled once and reused
// ---------------------------------------------------------------------------

fn timestamp_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:(\d+):)?(\d{1,2}):(\d{2})[,.](\d{3})$").unwrap())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse SRT (or VTT) text into a list of [`TranscriptSegment`]s.
///
/// * Normalises `\r\n` to `\n`, then splits blocks on `\n\n`.
/// * Each block: line 0 = integer index, line 1 = timestamp range,
///   remaining lines = subtitle text.
/// * Blocks with fewer than 3 lines, a non-integer index, or empty text are
///   silently skipped.
/// * Timestamps are parsed via [`parse_timestamp_line`] which supports
///   `HH:MM:SS,mmm`, `HH:MM:SS.mmm`, `MM:SS.mmm`, and raw-second floats.
pub fn parse_srt(srt_text: &str) -> Vec<TranscriptSegment> {
    // Normalise Windows line endings.
    let normalized = srt_text.replace("\r\n", "\n");
    let blocks: Vec<&str> = normalized.split("\n\n").collect();

    let mut segments: Vec<TranscriptSegment> = Vec::new();

    for block in blocks {
        let lines: Vec<&str> = block.trim().lines().collect();
        if lines.len() < 3 {
            continue;
        }

        // Line 0 - segment index (1-based convention).
        let index: usize = match lines[0].trim().parse() {
            Ok(i) => i,
            Err(_) => continue,
        };

        // Line 1 - timestamp range.
        let (start_opt, end_opt) = parse_timestamp_line(lines[1].trim());
        let start_time = match start_opt {
            Some(t) => t,
            None => continue,
        };
        let end_time = end_opt.unwrap_or(start_time + 2.0);

        // Remaining lines - subtitle text.
        let text: String = lines[2..].join("\n").trim().to_string();
        if text.is_empty() {
            continue;
        }

        segments.push(TranscriptSegment {
            index,
            start_time,
            end_time,
            text,
        });
    }

    segments
}

/// Parse a timestamp range line such as `"00:01:23,456 --> 00:01:25,789"`.
///
/// Returns `(start_seconds, end_seconds)`.  Either value may be `None` if
/// that side could not be parsed.
pub fn parse_timestamp_line(line: &str) -> (Option<f64>, Option<f64>) {
    let parts: Vec<&str> = line.splitn(2, "-->").collect();
    let start = parts
        .first()
        .map(|s| parse_single_timestamp(s.trim()))
        .flatten();
    let end = parts
        .get(1)
        .map(|s| parse_single_timestamp(s.trim()))
        .flatten();
    (start, end)
}

/// Parse a single timestamp string to seconds as `f64`.
///
/// Supported formats (case-insensitive leading whitespace is stripped):
///
/// | Format             | Example            |
/// |--------------------|--------------------|
/// | `HH:MM:SS,mmm`    | `00:01:23,456`     |
/// | `HH:MM:SS.mmm`    | `00:01:23.456`     |
/// | `MM:SS.mmm`       | `01:23.456`        |
/// | raw seconds       | `83.456`           |
///
/// Returns `None` when the string cannot be matched by any format.
pub fn parse_single_timestamp(ts: &str) -> Option<f64> {
    let ts = ts.trim();

    // Fast path - raw float (no colons).
    if !ts.contains(':') {
        return ts.parse::<f64>().ok();
    }

    // Regex path - HH:MM:SS,mmm  /  HH:MM:SS.mmm  /  MM:SS.mmm
    if let Some(caps) = timestamp_regex().captures(ts) {
        let hours: f64 = caps
            .get(1)
            .map(|m| m.as_str().parse::<f64>().unwrap_or(0.0))
            .unwrap_or(0.0);
        let minutes: f64 = caps[2].parse::<f64>().unwrap_or(0.0);
        let seconds: f64 = caps[3].parse::<f64>().unwrap_or(0.0);
        let millis: f64 = caps[4].parse::<f64>().unwrap_or(0.0);
        return Some(hours * 3600.0 + minutes * 60.0 + seconds + millis / 1000.0);
    }

    None
}

/// Serialise segments into a standard SRT string.
pub fn segments_to_srt(segments: &[TranscriptSegment]) -> String {
    let mut out = String::new();
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n",
            seg.index,
            format_timestamp(seg.start_time),
            format_timestamp(seg.end_time),
            seg.text,
        ));
    }
    out
}

/// Format a duration in seconds as `HH:MM:SS,mmm`.
///
/// Returns `"00:00:00,000"` for zero, `"01:01:01,500"` for 3661.5 s, etc.
pub fn format_timestamp(seconds: f64) -> String {
    // Work in integer milliseconds to avoid floating-point wobble.
    let total_ms = (seconds.abs() * 1000.0).round() as u64;
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms % 3_600_000) / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let millis = total_ms % 1000;
    format!("{:02}:{:02}:{:02},{:03}", hours, minutes, secs, millis)
}

/// Convenience: convert an entire [`TranscriptArtifact`] to its SRT
/// representation.
pub fn transcript_to_srt(artifact: &TranscriptArtifact) -> String {
    segments_to_srt(&artifact.segments)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // parse_srt
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_srt_basic() {
        let srt = "\
1
00:00:01,000 --> 00:00:03,000
Hello world

2
00:00:05,000 --> 00:00:08,500
Second subtitle
";

        let segments = parse_srt(srt);
        assert_eq!(segments.len(), 2);

        assert_eq!(segments[0].index, 1);
        assert!((segments[0].start_time - 1.0).abs() < 0.001);
        assert!((segments[0].end_time - 3.0).abs() < 0.001);
        assert_eq!(segments[0].text, "Hello world");

        assert_eq!(segments[1].index, 2);
        assert!((segments[1].start_time - 5.0).abs() < 0.001);
        assert!((segments[1].end_time - 8.5).abs() < 0.001);
        assert_eq!(segments[1].text, "Second subtitle");
    }

    #[test]
    fn test_parse_srt_with_vtt_periods() {
        // VTT uses . instead of , as millisecond separator.
        let srt = "\
1
00:00:01.000 --> 00:00:03.500
VTT-style
";
        let segments = parse_srt(srt);
        assert_eq!(segments.len(), 1);
        assert!((segments[0].start_time - 1.0).abs() < 0.001);
        assert!((segments[0].end_time - 3.5).abs() < 0.001);
        assert_eq!(segments[0].text, "VTT-style");
    }

    #[test]
    fn test_parse_srt_empty_input() {
        assert!(parse_srt("").is_empty());
        assert!(parse_srt("\n\n\n").is_empty());
    }

    #[test]
    fn test_parse_srt_no_timestamps() {
        // Blocks without valid timestamps should be skipped.
        let srt = "\
1
not a timestamp
some text

2
00:00:01,000 --> 00:00:02,000
good block
";
        let segments = parse_srt(srt);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].index, 2);
    }

    #[test]
    fn test_parse_srt_windows_line_endings() {
        let srt = "1\r\n00:00:01,000 --> 00:00:03,000\r\nHello\r\n";
        let segments = parse_srt(srt);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "Hello");
    }

    // -----------------------------------------------------------------------
    // parse_single_timestamp
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_single_timestamp_hhmmss() {
        let ts = parse_single_timestamp("01:02:03,456").unwrap();
        assert!((ts - 3723.456).abs() < 0.001);
    }

    #[test]
    fn test_parse_single_timestamp_mmss() {
        let ts = parse_single_timestamp("02:03,456").unwrap();
        assert!((ts - 123.456).abs() < 0.001);
    }

    #[test]
    fn test_parse_single_timestamp_raw_seconds() {
        let ts = parse_single_timestamp("83.456").unwrap();
        assert!((ts - 83.456).abs() < 0.001);
    }

    #[test]
    fn test_parse_single_timestamp_vtt_period() {
        let ts = parse_single_timestamp("00:01:23.456").unwrap();
        assert!((ts - 83.456).abs() < 0.001);
    }

    #[test]
    fn test_parse_single_timestamp_invalid() {
        assert!(parse_single_timestamp("not a timestamp").is_none());
        assert!(parse_single_timestamp("").is_none());
    }

    // -----------------------------------------------------------------------
    // parse_timestamp_line
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_timestamp_line_arrow() {
        let (start, end) = parse_timestamp_line("00:00:01,000 --> 00:00:05,500");
        assert!((start.unwrap() - 1.0).abs() < 0.001);
        assert!((end.unwrap() - 5.5).abs() < 0.001);
    }

    #[test]
    fn test_parse_timestamp_line_single() {
        let (start, end) = parse_timestamp_line("00:00:01,000");
        assert!(start.is_some());
        assert!(end.is_none());
    }

    // -----------------------------------------------------------------------
    // format_timestamp
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_timestamp_zero() {
        assert_eq!(format_timestamp(0.0), "00:00:00,000");
    }

    #[test]
    fn test_format_timestamp_typical() {
        assert_eq!(format_timestamp(3661.5), "01:01:01,500");
    }

    #[test]
    fn test_format_timestamp_sub_second() {
        assert_eq!(format_timestamp(0.001), "00:00:00,001");
    }

    #[test]
    fn test_format_timestamp_near_boundary() {
        // 999 ms - should not round up to next second.
        assert_eq!(format_timestamp(0.999), "00:00:00,999");
        assert_eq!(format_timestamp(0.9999), "00:00:01,000");
    }

    // -----------------------------------------------------------------------
    // segments_to_srt   (round-trip)
    // -----------------------------------------------------------------------

    #[test]
    fn test_segments_to_srt_roundtrip() {
        let original = vec![
            TranscriptSegment {
                index: 1,
                start_time: 0.0,
                end_time: 2.5,
                text: "First".into(),
            },
            TranscriptSegment {
                index: 2,
                start_time: 3.0,
                end_time: 5.0,
                text: "Second".into(),
            },
        ];

        let srt = segments_to_srt(&original);
        let parsed = parse_srt(&srt);

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].text, "First");
        assert_eq!(parsed[1].text, "Second");
        assert!((parsed[0].start_time - 0.0).abs() < 0.001);
        assert!((parsed[0].end_time - 2.5).abs() < 0.001);
    }

    // -----------------------------------------------------------------------
    // transcript_to_srt
    // -----------------------------------------------------------------------

    #[test]
    fn test_transcript_to_srt() {
        let artifact = TranscriptArtifact {
            video_id: "v1".into(),
            video_title: "Test".into(),
            course_id: "c1".into(),
            language: "zh".into(),
            segments: vec![TranscriptSegment {
                index: 1,
                start_time: 1.0,
                end_time: 2.0,
                text: "Hi".into(),
            }],
            fetched_at: "2025-01-01T00:00:00Z".into(),
            recorded_at: None,
            source_url: None,
        };

        let srt = transcript_to_srt(&artifact);
        assert!(srt.contains("Hi"));
        assert!(srt.contains("00:00:01,000"));
    }
}
