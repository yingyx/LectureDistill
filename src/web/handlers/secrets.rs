//! Secrets handlers: GET/PATCH /api/secrets.

use axum::extract::State;
use axum::Json;
use std::collections::HashMap;

use crate::web::app::AppState;

// ---------------------------------------------------------------------------
// PatchSecretsBody
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PatchSecretsBody {
    #[serde(default)]
    pub(crate) fields: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub(crate) clear: Vec<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/secrets` returns only redacted credential status.
pub(crate) async fn api_get_secrets(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "secrets": state.secrets.load().status_value(),
    }))
}

/// `PATCH /api/secrets` stores local credentials in `secrets.local.json`.
pub(crate) async fn api_patch_secrets(
    State(state): State<AppState>,
    Json(body): Json<PatchSecretsBody>,
) -> Json<serde_json::Value> {
    match state.secrets.update(&body.fields, &body.clear) {
        Ok(secrets) => Json(serde_json::json!({
            "status": "ok",
            "secrets": secrets.status_value(),
        })),
        Err(e) => Json(serde_json::json!({
            "status": "error",
            "error": e.to_string(),
        })),
    }
}
