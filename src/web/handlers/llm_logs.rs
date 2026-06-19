//! LLM log handlers: GET /api/llm-logs, GET /api/llm-logs/{log_id}.

use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::llm_log;

// ---------------------------------------------------------------------------
// ListLlmLogsQuery
// ---------------------------------------------------------------------------

/// `GET /api/llm-logs` -- list LLM call logs, newest first.
#[derive(Debug, Deserialize)]
pub(crate) struct ListLlmLogsQuery {
    #[serde(default = "default_llm_log_limit")]
    pub(crate) limit: usize,
}

fn default_llm_log_limit() -> usize {
    100
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/llm-logs` -- list LLM call logs, newest first.
pub(crate) async fn api_list_llm_logs(
    Query(query): Query<ListLlmLogsQuery>,
) -> Json<serde_json::Value> {
    let limit = query.limit.min(1000);
    match llm_log::list_logs(limit) {
        Ok(logs) => Json(serde_json::json!({ "logs": logs })),
        Err(e) => Json(serde_json::json!({
            "logs": [],
            "error": e.to_string(),
        })),
    }
}

/// `GET /api/llm-logs/{log_id}` -- return the full JSON content of a single log.
pub(crate) async fn api_get_llm_log(Path(log_id): Path<String>) -> Response {
    match llm_log::read_log(&log_id) {
        Ok(value) => Json(value).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
