//! State and config handlers: GET/PATCH /api/state.

use axum::extract::State;
use axum::Json;
use std::collections::HashMap;

use crate::latex;
use crate::llm;
use crate::web::app::AppState;

// ---------------------------------------------------------------------------
// PatchStateBody
// ---------------------------------------------------------------------------

/// `PATCH /api/state` -- update project state fields.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PatchStateBody {
    #[serde(default)]
    pub(crate) fields: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /api/state` -- return current project state (never exposes secrets).
pub(crate) async fn api_get_state(State(state): State<AppState>) -> Json<serde_json::Value> {
    apply_project_runtime_config(&state);
    let project_state = state.store.load();
    let secrets = state.secrets.load();
    let config = project_state.to_config_value();

    Json(serde_json::json!({
        "version": state.version,
        "project_dir": state.project_dir,
        "config": config,
        "llm_available": llm::is_available() || !secrets.llm_api_key.is_empty(),
        "typst_compiler": latex::find_typst_compiler(),
        "latex_compiler": latex::find_latex_compiler(),
        "pdf_renderer": latex::find_pdf_renderer(),
    }))
}

pub(crate) fn apply_project_runtime_config(state: &AppState) {
    let project_state = state.store.load();
    std::env::set_var(
        "LECTURE_DISTILL_LLM_MAX_CONCURRENCY",
        project_state.llm_max_concurrency.clamp(1, 32).to_string(),
    );
    if project_state.typst_path.trim().is_empty() {
        std::env::remove_var("LECTURE_DISTILL_TYPST_PATH");
    } else {
        std::env::set_var(
            "LECTURE_DISTILL_TYPST_PATH",
            project_state.typst_path.trim(),
        );
    }
}

/// `PATCH /api/state` -- update project state fields.
pub(crate) async fn api_patch_state(
    State(state): State<AppState>,
    Json(body): Json<PatchStateBody>,
) -> Json<serde_json::Value> {
    let fields_value = serde_json::to_value(&body.fields).unwrap_or_default();
    match state.store.update_and_save(&fields_value) {
        Ok(updated) => {
            std::env::set_var(
                "LECTURE_DISTILL_LLM_MAX_CONCURRENCY",
                updated.llm_max_concurrency.clamp(1, 32).to_string(),
            );
            if updated.typst_path.trim().is_empty() {
                std::env::remove_var("LECTURE_DISTILL_TYPST_PATH");
            } else {
                std::env::set_var("LECTURE_DISTILL_TYPST_PATH", updated.typst_path.trim());
            }
            let config = updated.to_config_value();
            Json(serde_json::json!({
                "status": "ok",
                "config": config,
            }))
        }
        Err(e) => Json(serde_json::json!({
            "status": "error",
            "error": e.to_string(),
        })),
    }
}
