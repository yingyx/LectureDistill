//! Plugin discovery and project-level plugin configuration APIs.

use std::fs;
use std::path::{Path as FsPath, PathBuf};

use axum::{
    extract::{Json as ExtractJson, Path, State},
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};

use crate::plugin::builtins::builtin_registry;
use crate::plugin::node::{PluginDescriptor, PluginKind};
use crate::utils::calibration::ensure_calibration;
use crate::web::app::AppState;
use crate::web::plugins::{plugin_data_dir, PluginConfigStore};

pub(crate) async fn api_get_plugins(State(state): State<AppState>) -> Json<serde_json::Value> {
    let registry = builtin_registry();
    let mut descriptors = input_plugin_descriptors();
    descriptors.extend(registry.descriptors());
    let config_store = PluginConfigStore::new(&state.project_dir);
    Json(serde_json::json!({
        "plugins": descriptors,
        "config": config_store.load_all(),
    }))
}

pub(crate) async fn api_get_plugin_config(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
) -> Json<serde_json::Value> {
    let config_store = PluginConfigStore::new(&state.project_dir);
    Json(serde_json::json!({
        "plugin_id": plugin_id,
        "config": config_store.get(&plugin_id),
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PatchPluginConfigBody {
    #[serde(default)]
    fields: serde_json::Value,
}

pub(crate) async fn api_patch_plugin_config(
    State(state): State<AppState>,
    Path(plugin_id): Path<String>,
    Json(body): Json<PatchPluginConfigBody>,
) -> Response {
    let config_store = PluginConfigStore::new(&state.project_dir);
    match config_store.patch(&plugin_id, body.fields) {
        Ok(config) => Json(serde_json::json!({
            "status": "ok",
            "plugin_id": plugin_id,
            "config": config,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"status": "failed", "error": e.to_string()})),
        )
            .into_response(),
    }
}

pub(crate) async fn api_plugin_action(
    State(state): State<AppState>,
    Path((plugin_id, action)): Path<(String, String)>,
    body: Option<ExtractJson<serde_json::Value>>,
) -> Response {
    if plugin_id != "builtin.ref_cheat" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "status": "failed",
                "error": format!("plugin action is not implemented for {}", plugin_id),
            })),
        )
            .into_response();
    }

    let body = body
        .map(|ExtractJson(value)| value)
        .unwrap_or_else(|| serde_json::json!({}));
    match handle_ref_cheat_action(&state.project_dir, &action, body) {
        Ok(value) => Json(serde_json::json!({
            "status": "ok",
            "plugin_id": plugin_id,
            "action": action,
            "result": value,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"status": "failed", "error": e.to_string()})),
        )
            .into_response(),
    }
}

fn input_plugin_descriptors() -> Vec<PluginDescriptor> {
    vec![
        PluginDescriptor {
            id: "builtin.canvas.transcript_day".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: "Canvas Transcript Day".to_string(),
            kind: PluginKind::Input,
            nodes: vec![],
            config_schema: serde_json::json!({}),
            actions: vec!["sync".to_string(), "ask".to_string()],
        },
        PluginDescriptor {
            id: "builtin.canvas.transcript_course".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: "Canvas Course Transcript".to_string(),
            kind: PluginKind::Input,
            nodes: vec![],
            config_schema: serde_json::json!({}),
            actions: vec!["sync".to_string(), "reindex".to_string(), "ask".to_string()],
        },
        PluginDescriptor {
            id: "builtin.canvas.pdf_file".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: "Canvas PDF File".to_string(),
            kind: PluginKind::Input,
            nodes: vec![],
            config_schema: serde_json::json!({}),
            actions: vec!["sync".to_string()],
        },
        PluginDescriptor {
            id: "builtin.note".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: "Markdown Note".to_string(),
            kind: PluginKind::Input,
            nodes: vec![],
            config_schema: serde_json::json!({}),
            actions: vec!["import".to_string(), "update".to_string()],
        },
    ]
}

#[derive(Debug, Deserialize)]
struct RefCheatActionBody {
    #[serde(alias = "source_path")]
    path: Option<String>,
    name: Option<String>,
    template: Option<String>,
    #[serde(default)]
    make_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TemplateEntry {
    name: String,
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    calibrated_at: Option<String>,
}

fn handle_ref_cheat_action(
    project_dir: &str,
    action: &str,
    body: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let action_body: RefCheatActionBody =
        serde_json::from_value(body).unwrap_or(RefCheatActionBody {
            path: None,
            name: None,
            template: None,
            make_default: false,
        });
    let project_path = FsPath::new(project_dir);
    let store = PluginConfigStore::new(project_dir);
    let mut config = store.get("builtin.ref_cheat");
    let mut templates = read_template_entries(&config);
    let mut default_template = config
        .get("default_template")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let result = match action {
        "import_template" => {
            let src = action_body
                .path
                .as_deref()
                .map(FsPath::new)
                .ok_or_else(|| anyhow::anyhow!("import_template requires path"))?;
            if !src.is_file() {
                anyhow::bail!("template file does not exist: {}", src.display());
            }
            let ext = src
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ext != "typ" && ext != "tex" {
                anyhow::bail!("template must be a .typ or .tex file");
            }
            let name = action_body
                .name
                .as_deref()
                .map(validate_template_name)
                .transpose()?
                .unwrap_or_else(|| {
                    src.file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or_else(|| format!("template.{}", ext))
                });
            validate_template_name(&name)?;
            let template_dir = plugin_data_dir(project_path, "builtin.ref_cheat").join("templates");
            fs::create_dir_all(&template_dir)?;
            let dst = template_dir.join(&name);
            fs::copy(src, &dst)?;
            upsert_template(&mut templates, &name, &dst);
            if action_body.make_default || default_template.is_empty() {
                default_template = dst.to_string_lossy().to_string();
            }
            serde_json::json!({"template": template_entry_json(&name, &dst)})
        }
        "delete_template" => {
            let template = action_body
                .template
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("delete_template requires template"))?;
            let removed = remove_template(&mut templates, template);
            if removed.is_none() {
                anyhow::bail!("template not found: {}", template);
            }
            let removed = removed.expect("checked above");
            let _ = fs::remove_file(&removed.path);
            if default_template == removed.path {
                default_template.clear();
            }
            serde_json::json!({"removed": removed})
        }
        "set_default_template" => {
            let entry = find_template(
                &templates,
                action_body
                    .template
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("set_default_template requires template"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("template not found"))?;
            default_template = entry.path.clone();
            serde_json::json!({"default_template": default_template})
        }
        "calibrate_template" => {
            let entry = find_template(
                &templates,
                action_body
                    .template
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("calibrate_template requires template"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("template not found"))?;
            let entry_path = entry.path.clone();
            let template_path = PathBuf::from(&entry_path);
            let calibration = ensure_calibration(Some(&template_path), project_path);
            let calibrated_at = calibration.calibrated_at.clone();
            for item in &mut templates {
                if item.path == entry_path {
                    item.calibrated_at = Some(calibrated_at.clone());
                }
            }
            serde_json::json!({"calibration": calibration})
        }
        _ => anyhow::bail!("unknown builtin.ref_cheat action: {}", action),
    };

    config = serde_json::json!({
        "default_template": default_template,
        "templates": templates,
    });
    store.set("builtin.ref_cheat", config)?;
    Ok(result)
}

fn read_template_entries(config: &serde_json::Value) -> Vec<TemplateEntry> {
    config
        .get("templates")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    if let Some(path) = item.as_str() {
                        let path_buf = PathBuf::from(path);
                        let name = path_buf
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.to_string());
                        return Some(TemplateEntry {
                            name,
                            path: path.to_string(),
                            calibrated_at: None,
                        });
                    }
                    serde_json::from_value(item.clone()).ok()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn validate_template_name(name: &str) -> anyhow::Result<String> {
    let path = FsPath::new(name);
    if path.components().count() != 1 {
        anyhow::bail!("template name must be a file name, not a path");
    }
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext != "typ" && ext != "tex" {
        anyhow::bail!("template name must end with .typ or .tex");
    }
    Ok(name.to_string())
}

fn upsert_template(templates: &mut Vec<TemplateEntry>, name: &str, path: &FsPath) {
    let path = path.to_string_lossy().to_string();
    if let Some(existing) = templates.iter_mut().find(|entry| entry.path == path) {
        existing.name = name.to_string();
        return;
    }
    templates.push(TemplateEntry {
        name: name.to_string(),
        path,
        calibrated_at: None,
    });
}

fn remove_template(templates: &mut Vec<TemplateEntry>, key: &str) -> Option<TemplateEntry> {
    templates
        .iter()
        .position(|entry| entry.name == key || entry.path == key)
        .map(|idx| templates.remove(idx))
}

fn find_template<'a>(templates: &'a [TemplateEntry], key: &str) -> Option<&'a TemplateEntry> {
    templates
        .iter()
        .find(|entry| entry.name == key || entry.path == key)
}

fn template_entry_json(name: &str, path: &FsPath) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "path": path.to_string_lossy(),
    })
}
