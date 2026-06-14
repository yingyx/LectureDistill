"""Tests for the web GUI: FastAPI TestClient smoke tests."""

import sys
from pathlib import Path

import pytest
from fastapi.testclient import TestClient


@pytest.fixture
def client(tmp_path):
    """Create a TestClient with a temporary project directory."""
    from lecture_distill.web.app import create_app

    app = create_app(project_dir=str(tmp_path))
    return TestClient(app)


class TestDashboard:
    def test_dashboard_returns_html(self, client):
        response = client.get("/")
        assert response.status_code == 200
        assert "text/html" in response.headers["content-type"]
        assert "lecture-distill" in response.text

    def test_dashboard_shows_version(self, client):
        from lecture_distill import __version__

        response = client.get("/")
        assert __version__ in response.text


class TestCanvasPage:
    def test_canvas_page_returns_html(self, client):
        response = client.get("/canvas")
        assert response.status_code == 200
        assert "text/html" in response.headers["content-type"]

    def test_list_videos_requires_course_id_and_cookie(self, client):
        response = client.post("/canvas/list-videos", data={})
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "failed"
        assert "Course ID" in str(data["errors"])

    def test_list_videos_with_args_returns_job(self, client):
        response = client.post(
            "/canvas/list-videos",
            data={"course_id": "12345", "cookie": "fake-cookie"},
        )
        assert response.status_code == 200
        data = response.json()
        assert "job_id" in data
        assert data["status"] == "running"


class TestNotesPage:
    def test_notes_page_returns_html(self, client):
        response = client.get("/notes")
        assert response.status_code == 200

    def test_patch_requires_notes_path(self, client):
        response = client.post("/notes/patch", data={})
        assert response.status_code == 200
        data = response.json()
        assert data["status"] == "failed"


class TestDistillPage:
    def test_distill_page_returns_html(self, client):
        response = client.get("/distill")
        assert response.status_code == 200


class TestCheatsheetPage:
    def test_cheatsheet_page_returns_html(self, client):
        response = client.get("/cheatsheet")
        assert response.status_code == 200

    def test_render_accepts_form(self, client):
        response = client.post(
            "/cheatsheet/render",
            data={"input_md": "distilled.md", "max_pages": "2"},
        )
        assert response.status_code == 200
        data = response.json()
        assert "job_id" in data


class TestLogsPage:
    def test_logs_page_returns_html(self, client):
        response = client.get("/logs")
        assert response.status_code == 200


class TestJobAPI:
    def test_unknown_job_returns_404(self, client):
        response = client.get("/jobs/nonexistent")
        assert response.status_code == 404

    def test_job_created_by_list_videos_is_pollable(self, client):
        # Create a job
        r = client.post(
            "/canvas/list-videos",
            data={"course_id": "12345", "cookie": "fake-cookie"},
        )
        job_id = r.json()["job_id"]

        # Poll it
        r2 = client.get(f"/jobs/{job_id}")
        assert r2.status_code == 200
        data = r2.json()
        assert data["job_id"] == job_id
        assert data["name"] == "list-videos"
        # Job will fail (fake cookie), but it should exist
        assert data["status"] in ("running", "pending", "succeeded", "failed")
