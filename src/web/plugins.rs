//! Project-level plugin configuration storage.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Clone)]
pub struct PluginConfigStore {
    path: PathBuf,
}

impl PluginConfigStore {
    pub fn new(project_dir: &str) -> Self {
        Self {
            path: Path::new(project_dir).join("plugins.json"),
        }
    }

    pub fn load_all(&self) -> HashMap<String, serde_json::Value> {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|content| serde_json::from_str(&content).ok())
            .unwrap_or_default()
    }

    pub fn get(&self, plugin_id: &str) -> serde_json::Value {
        self.load_all()
            .remove(plugin_id)
            .unwrap_or_else(|| serde_json::json!({}))
    }

    pub fn patch(&self, plugin_id: &str, fields: serde_json::Value) -> Result<serde_json::Value> {
        let mut all = self.load_all();
        let mut current = all
            .remove(plugin_id)
            .unwrap_or_else(|| serde_json::json!({}));
        if !current.is_object() {
            current = serde_json::json!({});
        }
        if let Some(fields) = fields.as_object() {
            let obj = current.as_object_mut().expect("object checked above");
            for (key, value) in fields {
                if is_secret_like_key(key) {
                    continue;
                }
                obj.insert(key.clone(), value.clone());
            }
        }
        all.insert(plugin_id.to_string(), current.clone());
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, serde_json::to_string_pretty(&all)?)?;
        Ok(current)
    }

    pub fn set(&self, plugin_id: &str, config: serde_json::Value) -> Result<serde_json::Value> {
        let mut all = self.load_all();
        all.insert(plugin_id.to_string(), config.clone());
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, serde_json::to_string_pretty(&all)?)?;
        Ok(config)
    }
}

pub fn plugin_data_dir(project_dir: &Path, plugin_id: &str) -> PathBuf {
    project_dir.join("plugins").join(plugin_id)
}

pub fn ref_cheat_default_template_path(project_dir: &Path) -> Option<PathBuf> {
    let project = project_dir.to_string_lossy();
    let store = PluginConfigStore::new(&project);
    let config = store.get("builtin.ref_cheat");
    let path = config.get("default_template")?.as_str()?.trim();
    if path.is_empty() {
        return None;
    }
    let template_path = PathBuf::from(path);
    if template_path.exists() {
        Some(template_path)
    } else {
        None
    }
}

fn is_secret_like_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['_', '-'], "");
    ["key", "apikey", "token", "secret", "password", "cookie"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_secret_like_plugin_config_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = PluginConfigStore::new(dir.path().to_str().unwrap());
        let patched = store
            .patch(
                "p",
                serde_json::json!({"template": "a.typ", "api_key": "secret"}),
            )
            .unwrap();
        assert_eq!(patched["template"], "a.typ");
        assert!(patched.get("api_key").is_none());
    }
}
