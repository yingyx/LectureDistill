"""Tests for Canvas response parsing using mocked fixture data."""

import pytest

from lecture_distill.canvas_sjtu import (
    _extract_form_action_and_inputs,
    CanvasSJTUVideoClient,
)


class TestFormParsing:
    def test_extract_form_action_and_inputs(self):
        html = """<html>
<body>
<form action="https://example.com/login" method="POST">
<input type="hidden" name="csrf" value="abc123" />
<input type="hidden" name="state" value="xyz">
</form>
</body>
</html>"""
        action, inputs = _extract_form_action_and_inputs(html)
        assert action == "https://example.com/login"
        assert inputs["csrf"] == "abc123"
        assert inputs["state"] == "xyz"

    def test_extract_with_self_closing_inputs(self):
        html = """<form action="/auth">
<input type="hidden" name="token" value="tok123"/>
<input name="user" value="alice"/>
</form>"""
        action, inputs = _extract_form_action_and_inputs(html)
        assert action == "/auth"
        assert inputs.get("token") == "tok123" or inputs.get("user") == "alice"

    def test_extract_form_no_action_raises(self):
        html = "<html><body>No form here</body></html>"
        with pytest.raises(ValueError, match="Could not find form action"):
            _extract_form_action_and_inputs(html)


class TestTokenIdExtraction:
    def test_extract_token_id_from_query(self):
        location = "https://example.com/callback?tokenId=abc123def456&other=param"
        result = CanvasSJTUVideoClient._extract_token_id(location)
        assert result == "abc123def456"

    def test_extract_token_id_from_path(self):
        location = "https://example.com/auth/tokenId=xyz789"
        result = CanvasSJTUVideoClient._extract_token_id(location)
        assert result == "xyz789"

    def test_extract_token_id_no_match_raises(self):
        location = "https://example.com/no-token-here"
        with pytest.raises(RuntimeError, match="Could not extract tokenId"):
            CanvasSJTUVideoClient._extract_token_id(location)


class TestSubtitleExtraction:
    def test_extract_subtitle_text_string(self):
        result = CanvasSJTUVideoClient._extract_subtitle_text("plain text content")
        assert result == "plain text content"

    def test_extract_subtitle_text_from_dict(self):
        data = {"subtitles": "SRT content here"}
        result = CanvasSJTUVideoClient._extract_subtitle_text(data)
        assert result == "SRT content here"

    def test_extract_subtitle_text_fallback(self):
        data = {"unknown_key": "some value", "nested": {"x": 1}}
        result = CanvasSJTUVideoClient._extract_subtitle_text(data)
        # Should return a JSON string since no known keys match
        assert isinstance(result, str)
        assert "unknown_key" in result

    def test_extract_subtitle_text_from_records(self):
        data = {
            "records": [
                {
                    "index": 1,
                    "startTime": 0,
                    "endTime": 5,
                    "text": "First subtitle",
                },
                {
                    "index": 2,
                    "startTime": 5,
                    "endTime": 10,
                    "text": "Second subtitle",
                },
            ]
        }
        result = CanvasSJTUVideoClient._extract_subtitle_text(data)
        assert "First subtitle" in result
        assert "Second subtitle" in result

    def test_extract_sjtu_translate_detail_segments(self):
        data = {
            "beforeAssemblyList": [
                {"bg": 1000, "ed": 2500, "res": "First subtitle", "videoId": 10},
                {"bg": 3000, "ed": 4500, "res": "Second subtitle", "videoId": 10},
            ]
        }
        segments = CanvasSJTUVideoClient._extract_subtitle_segments(data)
        assert len(segments) == 2
        assert segments[0].start_time == 1.0
        assert segments[0].end_time == 2.5
        assert segments[0].text == "First subtitle"


class TestCanvasVideoInfo:
    def test_video_info_creation(self):
        from lecture_distill.canvas_sjtu import CanvasVideoInfo
        info = CanvasVideoInfo(
            video_id="v123",
            title="Lecture 1",
            duration=3600,
        )
        assert info.video_id == "v123"
        assert info.title == "Lecture 1"
        assert info.duration == 3600
