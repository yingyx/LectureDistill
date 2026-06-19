//! Job handlers: GET /api/jobs, GET /api/jobs/{job_id}.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::web::app::AppState;

// ---------------------------------------------------------------------------
// ListJobsQuery
// ---------------------------------------------------------------------------

/// `GET /api/jobs` -- list recent jobs.
#[derive(Debug, Deserialize)]
pub(crate) struct ListJobsQuery {
    #[serde(default = "default_limit")]
    pub(crate) limit: usize,
}

fn default_limit() -> usize {
    30
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub(crate) async fn job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Response {
    match state.registry.get(&job_id) {
        Some(job) => Json(serde_json::to_value(&job).unwrap_or_default()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Job not found"})),
        )
            .into_response(),
    }
}

/// `GET /api/jobs` -- list recent jobs.
pub(crate) async fn api_list_jobs(
    State(state): State<AppState>,
    Query(query): Query<ListJobsQuery>,
) -> Json<serde_json::Value> {
    let jobs = state.registry.list_jobs(query.limit);
    Json(serde_json::json!({
        "jobs": jobs,
    }))
}

/// `GET /api/jobs/{job_id}` -- get individual job status.
pub(crate) async fn api_job_status(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Response {
    match state.registry.get(&job_id) {
        Some(job) => Json(serde_json::to_value(&job).unwrap_or_default()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Job not found"})),
        )
            .into_response(),
    }
}
