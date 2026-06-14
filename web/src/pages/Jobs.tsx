import { useEffect, useState, useCallback } from 'react';
import { api, type JobInfo } from '../api';

function formatTimestamp(ts: number): string {
  const d = new Date(ts * 1000);
  return d.toLocaleString();
}

function JobCard({ job, onRefresh }: { job: JobInfo; onRefresh: () => void }) {
  const [detail, setDetail] = useState<JobInfo>(job);
  const [polling, setPolling] = useState(
    job.status === 'running' || job.status === 'pending',
  );

  useEffect(() => {
    if (!polling) return;
    let cancelled = false;
    const poll = async () => {
      try {
        const j = await api.getJob(detail.job_id);
        if (!cancelled) {
          setDetail(j);
          if (j.status !== 'running' && j.status !== 'pending') {
            setPolling(false);
            onRefresh();
          } else {
            setTimeout(poll, 1500);
          }
        }
      } catch {
        if (!cancelled) {
          // Try legacy
          try {
            const j = await api.getJobLegacy(detail.job_id);
            if (!cancelled) {
              setDetail(j);
              if (j.status !== 'running' && j.status !== 'pending') {
                setPolling(false);
                onRefresh();
              } else {
                setTimeout(poll, 1500);
              }
            }
          } catch {
            if (!cancelled) setTimeout(poll, 1500);
          }
        }
      }
    };
    poll();
    return () => {
      cancelled = true;
    };
  }, [polling, detail.job_id, onRefresh]);

  const statusClass = `status-${detail.status}`;
  const hasOutput =
    detail.logs.length > 0 ||
    detail.errors.length > 0 ||
    detail.artifact_paths.length > 0;

  return (
    <div className="card">
      <div className="flex justify-between items-center mb-8">
        <h2 style={{ margin: 0 }}>
          {detail.name}{' '}
          <span className={statusClass} style={{ fontWeight: 600 }}>
            {detail.status.toUpperCase()}
          </span>
        </h2>
        <div className="flex gap-2 items-center">
          {polling && <span className="badge badge-warn">Polling...</span>}
        </div>
      </div>

      <p className="text-sm text-muted mb-4">
        Job ID: <code>{detail.job_id}</code> &bull; Created:{' '}
        {formatTimestamp(detail.created_at)}
        {detail.finished_at && (
          <>
            {' '}
            &bull; Finished: {formatTimestamp(detail.finished_at)}
          </>
        )}
      </p>

      {detail.artifact_paths.length > 0 && (
        <p className="text-sm mb-4">
          Artifacts:{' '}
          {detail.artifact_paths.map((a, i) => (
            <code key={i} style={{ marginRight: '0.5rem' }}>
              {a}
            </code>
          ))}
        </p>
      )}

      {!hasOutput && <p className="text-muted text-sm">No output yet.</p>}

      {detail.logs.length > 0 && (
        <div className="log-block">{detail.logs.join('\n')}</div>
      )}

      {detail.errors.length > 0 && (
        <div className="error-block">{detail.errors.join('\n')}</div>
      )}
    </div>
  );
}

export default function Jobs() {
  const [jobs, setJobs] = useState<JobInfo[]>([]);
  const [loading, setLoading] = useState(true);

  const loadJobs = useCallback(async () => {
    try {
      const res = await api.getJobs(30);
      setJobs(res.jobs);
    } catch {
      // ignore
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    loadJobs();
  }, [loadJobs]);

  return (
    <div>
      <div className="flex justify-between items-center mb-8">
        <h2 style={{ fontSize: '1rem', fontWeight: 600 }}>Job History</h2>
        <button className="btn-sm btn-outline" onClick={loadJobs} disabled={loading}>
          {loading ? 'Loading...' : 'Refresh'}
        </button>
      </div>

      {jobs.length === 0 && !loading ? (
        <p className="text-muted">
          No jobs yet. Run a pipeline stage to see results here.
        </p>
      ) : (
        jobs.map((job) => <JobCard key={job.job_id} job={job} onRefresh={loadJobs} />)
      )}
    </div>
  );
}
