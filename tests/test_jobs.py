"""Tests for the in-memory job registry."""

import time

from lecture_distill.web.jobs import JobRegistry, JobStatus


class TestJobRegistry:
    def test_create_job(self):
        registry = JobRegistry()
        job = registry.create("test-job")
        assert job.job_id is not None
        assert job.name == "test-job"
        assert job.status == JobStatus.PENDING

    def test_get_job(self):
        registry = JobRegistry()
        job = registry.create("test")
        retrieved = registry.get(job.job_id)
        assert retrieved is not None
        assert retrieved.job_id == job.job_id

    def test_get_nonexistent(self):
        registry = JobRegistry()
        assert registry.get("nonexistent") is None

    def test_update_status(self):
        registry = JobRegistry()
        job = registry.create("test")
        registry.update(job.job_id, status=JobStatus.RUNNING)
        assert registry.get(job.job_id).status == JobStatus.RUNNING

    def test_update_logs(self):
        registry = JobRegistry()
        job = registry.create("test")
        registry.update(job.job_id, log="Hello")
        registry.update(job.job_id, log="World")
        retrieved = registry.get(job.job_id)
        assert retrieved.logs == ["Hello", "World"]

    def test_update_errors(self):
        registry = JobRegistry()
        job = registry.create("test")
        registry.update(job.job_id, error="Oops")
        retrieved = registry.get(job.job_id)
        assert retrieved.errors == ["Oops"]

    def test_update_artifact_paths(self):
        registry = JobRegistry()
        job = registry.create("test")
        registry.update(job.job_id, artifact_path="/path/to/file")
        retrieved = registry.get(job.job_id)
        assert retrieved.artifact_paths == ["/path/to/file"]

    def test_finished_at_set_on_terminal_status(self):
        registry = JobRegistry()
        job = registry.create("test")
        assert job.finished_at is None

        registry.update(job.job_id, status=JobStatus.SUCCEEDED)
        retrieved = registry.get(job.job_id)
        assert retrieved.finished_at is not None

    def test_list_jobs_newest_first(self):
        registry = JobRegistry()
        j1 = registry.create("first")
        time.sleep(0.01)
        j2 = registry.create("second")
        jobs = registry.list_jobs()
        assert jobs[0].job_id == j2.job_id

    def test_evict_old_jobs(self):
        registry = JobRegistry(max_jobs=3)
        ids = []
        for i in range(5):
            j = registry.create(f"job-{i}")
            ids.append(j.job_id)
            time.sleep(0.01)
        jobs = registry.list_jobs()
        assert len(jobs) == 3
        # Oldest should be evicted
        assert ids[0] not in {j.job_id for j in jobs}

    def test_run_in_background_sets_running(self):
        registry = JobRegistry()

        def _noop(job):
            pass

        job = registry.run_in_background("bg-test", _noop)
        assert job.status == JobStatus.RUNNING

    def test_run_in_background_captures_exception(self):
        registry = JobRegistry()

        def _failing(job):
            raise ValueError("test error")

        job = registry.run_in_background("failing-test", _failing)
        # Give thread time to complete
        time.sleep(0.1)
        retrieved = registry.get(job.job_id)
        assert retrieved.status == JobStatus.FAILED
        assert any("test error" in e for e in retrieved.errors)

    def test_job_to_dict(self):
        registry = JobRegistry()
        job = registry.create("dict-test")
        registry.update(job.job_id, log="log entry")
        d = job.to_dict()
        assert d["job_id"] == job.job_id
        assert d["name"] == "dict-test"
        assert d["status"] == "pending"
        assert "log entry" in d["logs"]
