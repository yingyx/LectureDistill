"""Smoke tests for CLI commands (non-network only)."""

import subprocess
import sys


def run_cli(args: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "-m", "lecture_distill.cli"] + args,
        capture_output=True,
        text=True,
        timeout=10,
    )


class TestCLIHelp:
    def test_main_help(self):
        result = run_cli(["--help"])
        assert result.returncode == 0
        assert "SJTU Canvas lecture video subtitle ingestion" in result.stdout
        assert "patch-notes" in result.stdout

    def test_canvas_help(self):
        result = run_cli(["canvas", "--help"])
        assert result.returncode == 0
        assert "Canvas video operations" in result.stdout

    def test_canvas_list_videos_help(self):
        result = run_cli(["canvas", "list-videos", "--help"])
        assert result.returncode == 0
        assert "--course-id" in result.stdout
        assert "--cookie" in result.stdout

    def test_patch_notes_help(self):
        result = run_cli(["patch-notes", "--help"])
        assert result.returncode == 0
        assert "--notes" in result.stdout
        assert "--transcripts" in result.stdout

    def test_distill_help(self):
        result = run_cli(["distill", "--help"])
        assert result.returncode == 0

    def test_render_cheatsheet_help(self):
        result = run_cli(["render-cheatsheet", "--help"])
        assert result.returncode == 0
        assert "--max-pages" in result.stdout

    def test_run_help(self):
        result = run_cli(["run", "--help"])
        assert result.returncode == 0

    def test_gui_help(self):
        result = run_cli(["gui", "--help"])
        assert result.returncode == 0
        assert "--host" in result.stdout
        assert "--port" in result.stdout
        assert "--project-dir" in result.stdout

    def test_version(self):
        result = run_cli(["version"])
        assert result.returncode == 0
        assert "0.1.0" in result.stdout


class TestCLIErrors:
    def test_no_args_shows_help(self):
        result = run_cli([])
        # Should exit with non-zero or show help
        assert result.returncode != 0 or "Usage" in result.stdout or "Commands" in result.stdout

    def test_canvas_no_args_shows_help(self):
        result = run_cli(["canvas"])
        assert result.returncode != 0
