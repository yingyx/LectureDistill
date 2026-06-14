"""Tests for project state - must never persist secrets."""

import json
import os
import tempfile
from pathlib import Path

from lecture_distill.web.state import ProjectState, ProjectStateStore, _is_forbidden


class TestIsForbidden:
    def test_cookie_is_forbidden(self):
        assert _is_forbidden("JAAuthCookie") is True
        assert _is_forbidden("jaauthcookie") is True
        assert _is_forbidden("ja_auth_cookie") is True
        assert _is_forbidden("cookie") is True

    def test_api_key_is_forbidden(self):
        assert _is_forbidden("OPENAI_API_KEY") is True
        assert _is_forbidden("openai_api_key") is True
        assert _is_forbidden("api_key") is True

    def test_token_is_forbidden(self):
        assert _is_forbidden("token") is True

    def test_normal_keys_not_forbidden(self):
        assert _is_forbidden("course_id") is False
        assert _is_forbidden("notes_path") is False
        assert _is_forbidden("transcripts_dir") is False


class TestProjectState:
    def test_cookie_not_in_config_dict(self):
        state = ProjectState(course_id="12345", cookie="secret-cookie-value")
        data = state.to_config_dict()
        assert "cookie" not in data
        assert "course_id" in data

    def test_cookie_not_loaded_from_config(self):
        data = {"course_id": "12345", "cookie": "should-be-ignored"}
        state = ProjectState.from_config_dict(data)
        assert state.course_id == "12345"
        assert state.cookie == ""  # Not loaded

    def test_default_values(self):
        state = ProjectState()
        assert state.course_id == ""
        assert state.max_pages == 2
        assert state.transcripts_dir == "transcripts"

    def test_roundtrip_safe_fields(self):
        state = ProjectState(
            course_id="test123",
            notes_path="my_notes.md",
            max_pages=3,
            cookie="secret",  # should not survive
        )
        data = state.to_config_dict()
        restored = ProjectState.from_config_dict(data)
        assert restored.course_id == "test123"
        assert restored.notes_path == "my_notes.md"
        assert restored.max_pages == 3
        assert restored.cookie == ""  # Not persisted


class TestProjectStateStore:
    def test_load_returns_defaults_for_new_dir(self):
        with tempfile.TemporaryDirectory() as d:
            store = ProjectStateStore(d)
            state = store.load()
            assert state.course_id == ""

    def test_save_and_load(self):
        with tempfile.TemporaryDirectory() as d:
            store = ProjectStateStore(d)
            state = ProjectState(course_id="test", max_pages=5)
            store.save(state)

            loaded = store.load()
            assert loaded.course_id == "test"
            assert loaded.max_pages == 5

    def test_save_does_not_persist_cookie(self):
        with tempfile.TemporaryDirectory() as d:
            store = ProjectStateStore(d)
            state = ProjectState(course_id="test", cookie="super-secret")
            store.save(state)

            # Read raw config.json
            config_path = os.path.join(d, "config.json")
            raw = json.loads(Path(config_path).read_text(encoding="utf-8"))
            assert "cookie" not in raw
            assert "JAAuthCookie" not in raw

    def test_load_refuses_forbidden_keys(self):
        with tempfile.TemporaryDirectory() as d:
            config_path = os.path.join(d, "config.json")
            # Write a config with a forbidden key
            Path(config_path).write_text(
                json.dumps({"course_id": "test", "ja_auth_cookie": "bad"})
            )
            store = ProjectStateStore(d)
            # Should still load defaults because config is rejected
            state = store.load()
            assert state.course_id == "test"  # from load, not from_config_dict

    def test_update_and_save(self):
        with tempfile.TemporaryDirectory() as d:
            store = ProjectStateStore(d)
            state = store.update_and_save(course_id="updated", max_pages=10)
            assert state.course_id == "updated"
            assert state.max_pages == 10

            # Reload to verify persistence
            state2 = store.load()
            assert state2.course_id == "updated"
            assert state2.max_pages == 10
