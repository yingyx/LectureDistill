//! In-memory background job registry for the Web GUI.
//!
//! Jobs track pipeline stage execution with running/succeeded/failed states.
//! Thread-safe, using `std::sync::Mutex` for internal state.

use serde::Serialize;
use std::backtrace::Backtrace;
use std::collections::HashMap;
use std::panic;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// JobStatus
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Succeeded => write!(f, "succeeded"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Job
// ---------------------------------------------------------------------------

/// A background job tracking pipeline execution.
#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub job_id: String,
    pub name: String,
    pub status: JobStatus,
    #[serde(default)]
    pub logs: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Unix timestamp when the job was created.
    pub created_at: f64,
    /// Unix timestamp when the job finished (None if still running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<f64>,
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

impl Job {
    pub fn new(job_id: String, name: String) -> Self {
        Self {
            job_id,
            name,
            status: JobStatus::Pending,
            logs: Vec::new(),
            errors: Vec::new(),
            artifact_paths: Vec::new(),
            result: None,
            created_at: now_secs(),
            finished_at: None,
        }
    }
}

// ---------------------------------------------------------------------------
// JobRegistry
// ---------------------------------------------------------------------------

/// Thread-safe in-memory job registry.
#[derive(Clone)]
pub struct JobRegistry {
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    max_jobs: usize,
}

impl JobRegistry {
    pub fn new(max_jobs: usize) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(HashMap::new())),
            max_jobs,
        }
    }

    /// Create a new job and return it. Evicts oldest jobs if over max_jobs.
    pub fn create(&self, name: &str) -> Job {
        let job_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let job = Job::new(job_id, name.to_string());
        let mut jobs = self.jobs.lock().unwrap();
        jobs.insert(job.job_id.clone(), job.clone());

        // Evict oldest jobs if over limit.
        if jobs.len() > self.max_jobs {
            let mut sorted: Vec<(String, f64)> = jobs
                .iter()
                .map(|(id, j)| (id.clone(), j.created_at))
                .collect();
            sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
            let to_remove = sorted.len() - self.max_jobs;
            for (id, _) in sorted.iter().take(to_remove) {
                jobs.remove(id);
            }
        }

        job
    }

    /// Get a job by ID.
    pub fn get(&self, job_id: &str) -> Option<Job> {
        let jobs = self.jobs.lock().unwrap();
        jobs.get(job_id).cloned()
    }

    /// Update a job's fields. Returns the updated job or None if not found.
    pub fn update(
        &self,
        job_id: &str,
        status: Option<JobStatus>,
        log: Option<&str>,
        error: Option<&str>,
        artifact_path: Option<&str>,
        result: Option<serde_json::Value>,
    ) -> Option<Job> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs.get_mut(job_id)?;
        if let Some(s) = status {
            job.status = s;
            if matches!(s, JobStatus::Succeeded | JobStatus::Failed) {
                job.finished_at = Some(now_secs());
            }
        }
        if let Some(l) = log {
            job.logs.push(l.to_string());
        }
        if let Some(e) = error {
            job.errors.push(e.to_string());
        }
        if let Some(a) = artifact_path {
            job.artifact_paths.push(a.to_string());
        }
        if let Some(r) = result {
            job.result = Some(r);
        }
        Some(job.clone())
    }

    /// List jobs, newest first, limited to `limit`.
    pub fn list_jobs(&self, limit: usize) -> Vec<Job> {
        let jobs = self.jobs.lock().unwrap();
        let mut jobs: Vec<Job> = jobs.values().cloned().collect();
        jobs.sort_by(|a, b| {
            b.created_at
                .partial_cmp(&a.created_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        jobs.truncate(limit);
        jobs
    }

    /// Run a closure in a background thread, updating the job status.
    ///
    /// Creates the job, sets it to Running, spawns a thread.  The closure
    /// receives the Job and can update it.  Returns the created job (with
    /// Running status).
    ///
    /// If the thread panics the job is automatically marked as Failed with
    /// the panic message.
    pub fn run_in_background<F>(&self, name: &str, target: F) -> Job
    where
        F: FnOnce(Job) + Send + 'static,
    {
        let mut job = self.create(name);
        job.status = JobStatus::Running;

        // Update the created job to Running status.
        {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(existing) = jobs.get_mut(&job.job_id) {
                existing.status = JobStatus::Running;
            }
        }

        let registry = self.clone();
        let job_id = job.job_id.clone();

        thread::spawn(move || {
            let job_snapshot = {
                let jobs = registry.jobs.lock().unwrap();
                jobs.get(&job_id).cloned()
            };

            if let Some(j) = job_snapshot {
                let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                    target(j);
                }));

                if let Err(panic_err) = result {
                    let msg = if let Some(s) = panic_err.downcast_ref::<String>() {
                        s.clone()
                    } else if let Some(s) = panic_err.downcast_ref::<&str>() {
                        s.to_string()
                    } else {
                        "Unknown panic in background job".to_string()
                    };
                    eprintln!(
                        "[lecture-distill][web][panic] job {} panicked: {}",
                        job_id, msg
                    );
                    eprintln!(
                        "[lecture-distill][web][panic][backtrace]\n{}",
                        Backtrace::force_capture()
                    );
                    registry.update(
                        &job_id,
                        Some(JobStatus::Failed),
                        None,
                        Some(&format!("Panic: {}", msg)),
                        None,
                        None,
                    );
                }
            }
        });

        job
    }
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new(100)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_creation() {
        let job = Job::new("abc123".into(), "test-job".into());
        assert_eq!(job.job_id, "abc123");
        assert_eq!(job.name, "test-job");
        assert_eq!(job.status, JobStatus::Pending);
        assert!(job.errors.is_empty());
        assert!(job.logs.is_empty());
        assert!(job.result.is_none());
        assert!(job.finished_at.is_none());
    }

    #[test]
    fn test_job_status_display() {
        assert_eq!(JobStatus::Pending.to_string(), "pending");
        assert_eq!(JobStatus::Running.to_string(), "running");
        assert_eq!(JobStatus::Succeeded.to_string(), "succeeded");
        assert_eq!(JobStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn test_registry_create_and_get() {
        let registry = JobRegistry::new(10);
        let job = registry.create("my-job");
        let retrieved = registry.get(&job.job_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "my-job");
    }

    #[test]
    fn test_registry_get_missing() {
        let registry = JobRegistry::new(10);
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_update() {
        let registry = JobRegistry::new(10);
        let job = registry.create("update-test");

        registry.update(
            &job.job_id,
            Some(JobStatus::Succeeded),
            Some("all done"),
            None,
            Some("output.pdf"),
            None,
        );

        let updated = registry.get(&job.job_id).unwrap();
        assert_eq!(updated.status, JobStatus::Succeeded);
        assert_eq!(updated.logs, vec!["all done"]);
        assert_eq!(updated.artifact_paths, vec!["output.pdf"]);
        assert!(updated.finished_at.is_some());
    }

    #[test]
    fn test_registry_list_jobs_sorted() {
        let registry = JobRegistry::new(10);
        registry.create("job-a");
        std::thread::sleep(std::time::Duration::from_millis(10));
        registry.create("job-b");

        let jobs = registry.list_jobs(10);
        assert_eq!(jobs.len(), 2);
        // Most recently created first.
        assert!(jobs[0].created_at >= jobs[1].created_at);
    }

    #[test]
    fn test_registry_evicts_oldest() {
        let registry = JobRegistry::new(3);
        for i in 0..5 {
            registry.create(&format!("job-{}", i));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let jobs = registry.list_jobs(10);
        assert!(jobs.len() <= 3);
    }

    #[test]
    fn test_job_name_success_and_failure() {
        let registry = JobRegistry::new(10);
        let job = registry.create("success-job");

        registry.update(
            &job.job_id,
            Some(JobStatus::Succeeded),
            None,
            None,
            None,
            None,
        );
        let j = registry.get(&job.job_id).unwrap();
        assert_eq!(j.status, JobStatus::Succeeded);
        assert!(j.finished_at.is_some());

        let job2 = registry.create("fail-job");
        registry.update(
            &job2.job_id,
            Some(JobStatus::Failed),
            None,
            Some("an error occurred"),
            None,
            None,
        );
        let j2 = registry.get(&job2.job_id).unwrap();
        assert_eq!(j2.status, JobStatus::Failed);
        assert_eq!(j2.errors, vec!["an error occurred"]);
    }

    #[test]
    fn test_job_result_can_store_json() {
        let registry = JobRegistry::new(10);
        let job = registry.create("result-test");

        let result = serde_json::json!({
            "videos": [{"id": "v1", "title": "Test"}],
            "count": 1
        });

        registry.update(
            &job.job_id,
            Some(JobStatus::Succeeded),
            None,
            None,
            None,
            Some(result.clone()),
        );

        let updated = registry.get(&job.job_id).unwrap();
        assert_eq!(updated.result, Some(result));
    }

    #[test]
    fn test_run_in_background_panic_marks_failed() {
        let registry = JobRegistry::new(10);
        let job = registry.run_in_background("panic-test", |_job| {
            panic!("intentional test panic");
        });

        // Give the thread a moment to panic and record the failure. Capturing a
        // backtrace can take longer than a fixed short sleep on Windows.
        for _ in 0..20 {
            if registry
                .get(&job.job_id)
                .is_some_and(|job| job.status == JobStatus::Failed)
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let updated = registry.get(&job.job_id).unwrap();
        assert_eq!(updated.status, JobStatus::Failed);
        assert!(!updated.errors.is_empty());
        assert!(updated.errors[0].contains("Panic"));
    }
}
