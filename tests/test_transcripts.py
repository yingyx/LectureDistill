"""Tests for SRT parsing and serialization."""

import pytest

from lecture_distill.transcripts import parse_srt, segments_to_srt, transcript_to_srt
from lecture_distill.artifacts import TranscriptArtifact, TranscriptSegment


class TestParseSRT:
    def test_parse_basic_srt(self):
        srt = """1
00:00:01,000 --> 00:00:04,000
Hello world

2
00:00:05,500 --> 00:00:10,200
Second subtitle
with two lines
"""
        segments = parse_srt(srt)
        assert len(segments) == 2
        assert segments[0].index == 1
        assert segments[0].start_time == 1.0
        assert segments[0].end_time == 4.0
        assert segments[0].text == "Hello world"
        assert segments[1].index == 2
        assert segments[1].start_time == 5.5
        assert segments[1].end_time == 10.2
        assert segments[1].text == "Second subtitle\nwith two lines"

    def test_parse_vtt_style_timestamps(self):
        srt = """1
00:00:01.000 --> 00:00:04.000
VTT style periods
"""
        segments = parse_srt(srt)
        assert len(segments) == 1
        assert segments[0].start_time == 1.0
        assert segments[0].end_time == 4.0

    def test_parse_empty(self):
        assert parse_srt("") == []
        assert parse_srt("\n\n") == []

    def test_parse_no_timestamps(self):
        srt = """1
Some text without proper format"""
        segments = parse_srt(srt)
        assert len(segments) == 0

    def test_parse_windows_line_endings(self):
        srt = "1\r\n00:00:01,000 --> 00:00:05,000\r\nText\r\n"
        segments = parse_srt(srt)
        assert len(segments) == 1
        assert segments[0].text == "Text"


class TestSegmentsToSRT:
    def test_roundtrip(self):
        segments = [
            TranscriptSegment(index=1, start_time=1.0, end_time=4.0, text="Hello"),
            TranscriptSegment(index=2, start_time=5.5, end_time=10.2, text="World"),
        ]
        srt = segments_to_srt(segments)
        parsed = parse_srt(srt)
        assert len(parsed) == 2
        assert parsed[0].text == "Hello"
        assert parsed[1].text == "World"

    def test_format_timestamp(self):
        from lecture_distill.transcripts import _format_timestamp
        assert _format_timestamp(0) == "00:00:00,000"
        assert _format_timestamp(61.5) == "00:01:01,500"
        assert _format_timestamp(3661.123) == "01:01:01,123"


class TestTranscriptToSRT:
    def test_conversion(self):
        artifact = TranscriptArtifact(
            video_id="test123",
            video_title="Test Video",
            course_id="42",
            segments=[
                TranscriptSegment(index=1, start_time=0.0, end_time=2.0, text="Test"),
            ],
        )
        srt = transcript_to_srt(artifact)
        assert "00:00:00,000 --> 00:00:02,000" in srt
        assert "Test" in srt
