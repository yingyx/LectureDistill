"""Tests for LaTeX rendering helpers (no actual compilation needed)."""

import os
import tempfile
from pathlib import Path

import pytest

from lecture_distill.latex import (
    _markdown_to_latex,
    _escape_latex,
    _compress_content,
    count_pdf_pages,
)


class TestEscapeLatex:
    def test_escape_special_chars(self):
        assert "\\&" in _escape_latex("A & B")
        assert "\\%" in _escape_latex("100%")
        assert "\\#" in _escape_latex("#tag")
        assert "\\_" in _escape_latex("file_name")

    def test_preserves_math_mode(self):
        text = "$x^2 + y^2$"
        assert _escape_latex(text) == text

    def test_preserves_latex_commands(self):
        text = r"\textbf{bold} and \textit{italic}"
        result = _escape_latex(text)
        assert r"\textbf" in result


class TestMarkdownToLatex:
    def test_converts_headings(self):
        md = "# Main\n## Section\n### Sub\n#### Minor"
        result = _markdown_to_latex(md)
        assert r"\section*{Main}" in result
        assert r"\subsection*{Section}" in result
        assert r"\subsubsection*{Sub}" in result
        assert r"\paragraph*{Minor}" in result

    def test_converts_lists(self):
        md = "- Item 1\n- Item 2\n\n1. First\n2. Second"
        result = _markdown_to_latex(md)
        assert r"\begin{itemize}" in result
        assert r"\item Item 1" in result
        assert r"\item Item 2" in result
        assert r"\end{itemize}" in result
        assert r"\begin{enumerate}" in result

    def test_converts_bold_and_italic(self):
        md = "This is **bold** and *italic* text."
        result = _markdown_to_latex(md)
        assert r"\textbf{bold}" in result
        assert r"\textit{italic}" in result

    def test_skips_html_comments(self):
        md = "<!-- comment -->\n> quote\nReal content"
        result = _markdown_to_latex(md)
        assert "comment" not in result


class TestCompressContent:
    def test_compress_attempt_1_removes_blank_lines(self):
        content = "Line 1\n\n\n\nLine 2\n\n\nLine 3"
        result = _compress_content(content, 1)
        assert result.count("\n\n") <= 2  # Should reduce excessive blank lines

    def test_compress_attempt_2_removes_supporting_section(self):
        content = """## Key Concepts
Important stuff

## Supporting Concepts
Less important

## Content Summary
Summary here"""
        result = _compress_content(content, 2)
        assert "Supporting Concepts" not in result
        assert "Key Concepts" in result

    def test_compress_attempt_3_keeps_only_key(self):
        content = """## Key Concepts
Important

## Supporting Concepts
Less important

## Content Summary
Summary"""
        result = _compress_content(content, 3)
        assert "Key Concepts" in result
        assert "Supporting Concepts" not in result

