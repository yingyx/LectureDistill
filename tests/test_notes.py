"""Tests for Markdown notes patching."""

import os
import tempfile
from pathlib import Path
import json

import pytest

from lecture_distill.notes import (
    load_notes,
    load_transcripts,
    patch_notes,
    _extract_headings,
    _apply_patches,
    _deterministic_patch,
)
from lecture_distill.artifacts import (
    NoteArtifact,
    PatchArtifact,
    PatchEntry,
    KeepLevel,
    TranscriptArtifact,
    TranscriptSegment,
)


class TestExtractHeadings:
    def test_extract_headings(self):
        content = """# Main Title
Some text
## Section 1
More text
### Subsection 1.1
#### Minor heading
"""
        headings = _extract_headings(content)
        assert "Main Title" in headings
        assert "Section 1" in headings
        assert "Subsection 1.1" in headings
        assert "Minor heading" in headings

    def test_no_headings(self):
        assert _extract_headings("Just plain text, no headings.") == []


class TestApplyPatches:
    def test_apply_patches_appends_sections(self):
        original = "# Original Notes\n\nSome content."
        artifact = PatchArtifact(
            source_notes_path="notes.md",
            patches=[
                PatchEntry(
                    location="New Section",
                    new_text="Added from transcript",
                    source_video_id="vid1",
                    source_timestamp=42.0,
                ),
            ],
            conflicts=["Ambiguity in Section 1"],
        )
        result = _apply_patches(original, artifact)
        assert "# Original Notes" in result
        assert "## Transcript Additions" in result
        assert "### New Section" in result
        assert "Added from transcript" in result
        assert "## Conflicts / Needs Review" in result
        assert "Ambiguity in Section 1" in result

    def test_apply_patches_no_conflicts(self):
        original = "# Notes"
        artifact = PatchArtifact(
            source_notes_path="notes.md",
            patches=[],
            conflicts=[],
        )
        result = _apply_patches(original, artifact)
        assert "## Conflicts / Needs Review" not in result


class TestDeterministicPatch:
    def test_extracts_quoted_terms(self):
        note = NoteArtifact(path="notes.md", content="# Notes\n\nSome text.", headings=["Notes"])
        transcripts = [
            TranscriptArtifact(
                video_id="v1",
                video_title="Lecture 1",
                course_id="42",
                segments=[
                    TranscriptSegment(index=1, start_time=0, end_time=5, text='The "Riemann Hypothesis" is important.'),
                ],
            )
        ]
        artifact = _deterministic_patch(note, transcripts)
        # Should find "Riemann Hypothesis" as a quoted term
        riemann_patches = [p for p in artifact.patches if "riemann" in p.new_text.lower()]
        assert len(riemann_patches) > 0


class TestLoadNotes:
    def test_load_notes(self):
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md", delete=False, encoding="utf-8") as f:
            f.write("# Test\n\nContent here.\n## Section A\n\nMore.")
            path = f.name
        try:
            note = load_notes(path)
            assert note.path == path
            assert "Test" in note.headings
            assert "Section A" in note.headings
        finally:
            os.unlink(path)


class TestLoadTranscripts:
    def test_load_transcripts_from_dir(self):
        with tempfile.TemporaryDirectory() as d:
            art = TranscriptArtifact(
                video_id="v1",
                video_title="Test",
                course_id="42",
                segments=[TranscriptSegment(index=1, start_time=0, end_time=1, text="Hi")],
            )
            Path(d, "v1.json").write_text(art.model_dump_json(indent=2), encoding="utf-8")
            Path(d, "bad.json").write_text("not valid json", encoding="utf-8")

            artifacts = load_transcripts(d)
            assert len(artifacts) == 1
            assert artifacts[0].video_id == "v1"

    def test_load_transcripts_missing_dir(self):
        artifacts = load_transcripts("/nonexistent/path/12345")
        assert artifacts == []


class TestPatchNotesIntegration:
    def test_patch_notes_no_transcripts(self):
        with tempfile.TemporaryDirectory() as d:
            notes_path = os.path.join(d, "notes.md")
            Path(notes_path).write_text("# Test\n\nContent.", encoding="utf-8")
            out_path = os.path.join(d, "patched.md")
            patches_path = os.path.join(d, "patches.json")
            trans_dir = os.path.join(d, "transcripts")
            os.makedirs(trans_dir)

            artifact = patch_notes(
                notes_path=notes_path,
                transcripts_dir=trans_dir,
                output_notes_path=out_path,
                output_patches_path=patches_path,
            )
            assert os.path.exists(out_path)
            assert os.path.exists(patches_path)
            assert "No transcript files found" in artifact.conflicts[0]

    def test_patch_notes_never_overwrites_source(self):
        with tempfile.TemporaryDirectory() as d:
            notes_path = os.path.join(d, "notes.md")
            original = "# Original\n\nThis is the original content."
            Path(notes_path).write_text(original, encoding="utf-8")
            out_path = os.path.join(d, "patched.md")
            patches_path = os.path.join(d, "patches.json")
            trans_dir = os.path.join(d, "transcripts")
            os.makedirs(trans_dir)

            patch_notes(
                notes_path=notes_path,
                transcripts_dir=trans_dir,
                output_notes_path=out_path,
                output_patches_path=patches_path,
            )
            # Source file must be unchanged
            assert Path(notes_path).read_text(encoding="utf-8") == original
