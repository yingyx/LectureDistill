//! Local credential persistence for the Web GUI.
//!
//! Secrets are stored separately from `config.json` so normal project state
//! and API responses can stay redacted. The file is intended to be local-only
//! and is ignored by git as `secrets.local.json`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectSecrets {
    #[serde(default)]
    pub llm_api_key: String,
    #[serde(default)]
    pub llm_base_url: String,
    #[serde(default)]
    pub llm_model: String,
    #[serde(default)]
    pub canvas_token: String,
    #[serde(default)]
    pub canvas_cookie: String,
    #[serde(default)]
    pub jaccount_cookie: String,
}

impl ProjectSecrets {
    pub fn apply_to_env(&self) {
        if !self.llm_api_key.is_empty() {
            std::env::set_var("OPENAI_API_KEY", &self.llm_api_key);
        }
        if !self.llm_base_url.is_empty() {
            std::env::set_var("OPENAI_BASE_URL", &self.llm_base_url);
        }
        if !self.llm_model.is_empty() {
            std::env::set_var("OPENAI_MODEL", &self.llm_model);
        }
    }

    pub fn canvas_auth_cookie(&self) -> Option<String> {
        if !self.canvas_cookie.is_empty() {
            Some(self.canvas_cookie.clone())
        } else if !self.jaccount_cookie.is_empty() {
            Some(self.jaccount_cookie.clone())
        } else {
            None
        }
    }

    pub fn status_value(&self) -> Value {
        serde_json::json!({
            "llm": {
                "api_key_set": !self.llm_api_key.is_empty(),
                "api_key_masked": mask_secret(&self.llm_api_key),
                "base_url": self.llm_base_url,
                "model": self.llm_model,
            },
            "canvas": {
                "token_set": !self.canvas_token.is_empty(),
                "token_masked": mask_secret(&self.canvas_token),
                "cookie_set": !self.canvas_cookie.is_empty(),
                "cookie_masked": mask_secret(&self.canvas_cookie),
            },
            "jaccount": {
                "cookie_set": !self.jaccount_cookie.is_empty(),
                "cookie_masked": mask_secret(&self.jaccount_cookie),
            }
        })
    }
}

pub struct SecretStore {
    path: String,
}

impl SecretStore {
    const FILENAME: &'static str = "secrets.local.json";

    pub fn new(project_dir: &str) -> Self {
        let path = Path::new(project_dir)
            .join(Self::FILENAME)
            .to_string_lossy()
            .to_string();
        Self { path }
    }

    pub fn load(&self) -> ProjectSecrets {
        match fs::read_to_string(&self.path) {
            Ok(content) => serde_json::from_str::<ProjectSecrets>(&content).unwrap_or_default(),
            Err(_) => ProjectSecrets::default(),
        }
    }

    pub fn save(&self, secrets: &ProjectSecrets) -> Result<()> {
        if let Some(parent) = Path::new(&self.path).parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(secrets)?;
        fs::write(&self.path, json)?;
        Ok(())
    }

    pub fn update(
        &self,
        fields: &HashMap<String, Value>,
        clear: &[String],
    ) -> Result<ProjectSecrets> {
        let mut secrets = self.load();

        for field in clear {
            set_secret_field(&mut secrets, field, "");
        }

        for (key, value) in fields {
            if let Some(raw) = value.as_str() {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    set_secret_field(&mut secrets, key, trimmed);
                }
            }
        }

        self.save(&secrets)?;
        Ok(secrets)
    }
}

fn set_secret_field(secrets: &mut ProjectSecrets, key: &str, value: &str) {
    match key {
        "llm_api_key" => secrets.llm_api_key = value.to_string(),
        "llm_base_url" => secrets.llm_base_url = value.to_string(),
        "llm_model" => secrets.llm_model = value.to_string(),
        "canvas_token" => secrets.canvas_token = value.to_string(),
        "canvas_cookie" => secrets.canvas_cookie = value.to_string(),
        "jaccount_cookie" => secrets.jaccount_cookie = value.to_string(),
        _ => {}
    }
}

pub fn mask_secret(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 8 {
        return "********".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("********{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_empty_and_short_values() {
        assert_eq!(mask_secret(""), "");
        assert_eq!(mask_secret("abc"), "********");
    }

    #[test]
    fn masks_with_tail_for_long_values() {
        assert_eq!(mask_secret("sk-1234567890"), "********7890");
    }

    #[test]
    fn store_roundtrip_and_status_are_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path().to_str().unwrap());
        let fields = HashMap::from([
            (
                "llm_api_key".to_string(),
                Value::String("sk-test-secret".to_string()),
            ),
            (
                "canvas_cookie".to_string(),
                Value::String("cookie-secret".to_string()),
            ),
        ]);

        let saved = store.update(&fields, &[]).unwrap();
        assert_eq!(saved.llm_api_key, "sk-test-secret");
        assert_eq!(store.load().canvas_cookie, "cookie-secret");

        let status = store.load().status_value().to_string();
        assert!(status.contains("api_key_set"));
        assert!(!status.contains("sk-test-secret"));
        assert!(!status.contains("cookie-secret"));
    }

    #[test]
    fn empty_updates_do_not_clear_existing_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path().to_str().unwrap());
        let fields = HashMap::from([(
            "llm_api_key".to_string(),
            Value::String("sk-test-secret".to_string()),
        )]);
        store.update(&fields, &[]).unwrap();

        let empty = HashMap::from([("llm_api_key".to_string(), Value::String(String::new()))]);
        store.update(&empty, &[]).unwrap();
        assert_eq!(store.load().llm_api_key, "sk-test-secret");
    }
}
