//! JSON-backed project state for the Web GUI.
//!
//! Rules:
//! - Never persist JAAuthCookie, OPENAI_API_KEY, or other secrets to disk.
//! - Project state is stored as a JSON file in the project directory.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Forbidden / allowed key lists
// ---------------------------------------------------------------------------

/// Keys that must never be persisted to disk.
const FORBIDDEN_STATE_KEYS: &[&str] = &[
    "jaauth",
    "jaauthcookie",
    "cookie",
    "openaiapikey",
    "apikey",
    "apikeyopenai",
    "openai",
    "secret",
    "password",
    "token",
];

/// Allowed config keys (whitelist for loading).
const ALLOWED_CONFIG_KEYS: &[&str] = &[
    "course_id",
    "transcripts_dir",
    "notes_path",
    "output_notes",
    "output_patches",
    "output_distilled",
    "output_pdf",
    "template_path",
    "typst_path",
    "max_pages",
    "video_id",
    "llm_max_concurrency",
];

/// Check if a key name is forbidden from persistence.
///
/// Normalizes to lowercase, removes underscores and dashes before comparing
/// against the blacklist.
pub fn is_forbidden(key: &str) -> bool {
    let key_lower = key.to_lowercase().replace('_', "").replace('-', "");
    FORBIDDEN_STATE_KEYS.iter().any(|k| *k == key_lower)
}

// ---------------------------------------------------------------------------
// ProjectState
// ---------------------------------------------------------------------------

/// Mutable project configuration for the Web GUI.
///
/// Persisted fields (safe to write to disk):
/// - course_id, transcripts_dir, notes_path, output_notes,
///   output_patches, output_distilled, output_pdf, template_path,
///   max_pages, video_id
///
/// Runtime-only fields (never persisted):
/// - cookie
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectState {
    #[serde(default)]
    pub course_id: String,
    #[serde(default = "default_transcripts_dir")]
    pub transcripts_dir: String,
    #[serde(default = "default_notes_path")]
    pub notes_path: String,
    #[serde(default = "default_output_notes")]
    pub output_notes: String,
    #[serde(default = "default_output_patches")]
    pub output_patches: String,
    #[serde(default = "default_output_distilled")]
    pub output_distilled: String,
    #[serde(default = "default_output_pdf")]
    pub output_pdf: String,
    #[serde(default)]
    pub template_path: String,
    #[serde(default)]
    pub typst_path: String,
    #[serde(default = "default_max_pages")]
    pub max_pages: usize,
    #[serde(default)]
    pub video_id: String,
    #[serde(default = "default_llm_max_concurrency")]
    pub llm_max_concurrency: usize,
    /// Cookie is runtime-only, never persisted.
    #[serde(skip)]
    pub cookie: String,
}

fn default_transcripts_dir() -> String {
    "artifacts/transcripts".into()
}
fn default_notes_path() -> String {
    "notes.md".into()
}
fn default_output_notes() -> String {
    "artifacts/notes/notes.patched.md".into()
}
fn default_output_patches() -> String {
    "artifacts/notes/patches.json".into()
}
fn default_output_distilled() -> String {
    "artifacts/distill/distilled.md".into()
}
fn default_output_pdf() -> String {
    "artifacts/outputs/cheatsheet.pdf".into()
}
fn default_max_pages() -> usize {
    2
}
fn default_llm_max_concurrency() -> usize {
    2
}

impl Default for ProjectState {
    fn default() -> Self {
        Self {
            course_id: String::new(),
            transcripts_dir: default_transcripts_dir(),
            notes_path: default_notes_path(),
            output_notes: default_output_notes(),
            output_patches: default_output_patches(),
            output_distilled: default_output_distilled(),
            output_pdf: default_output_pdf(),
            template_path: String::new(),
            typst_path: String::new(),
            max_pages: default_max_pages(),
            video_id: String::new(),
            llm_max_concurrency: default_llm_max_concurrency(),
            cookie: String::new(),
        }
    }
}

impl ProjectState {
    /// Convert to a config dict, filtering out forbidden keys and cookie.
    pub fn to_config_value(&self) -> serde_json::Value {
        let raw = serde_json::to_value(self).unwrap_or_default();
        match raw {
            serde_json::Value::Object(map) => {
                let filtered: serde_json::Map<String, serde_json::Value> = map
                    .into_iter()
                    .filter(|(k, _)| !is_forbidden(k) && k != "cookie")
                    .collect();
                serde_json::Value::Object(filtered)
            }
            _ => serde_json::Value::Object(Default::default()),
        }
    }

    /// Load from a config dict, only accepting keys from ALLOWED_CONFIG_KEYS.
    pub fn from_config_value(value: &serde_json::Value) -> Self {
        let mut state = ProjectState::default();
        if let Some(obj) = value.as_object() {
            for allowed in ALLOWED_CONFIG_KEYS {
                if let Some(v) = obj.get(*allowed) {
                    match *allowed {
                        "course_id" => {
                            if let Some(s) = v.as_str() {
                                state.course_id = s.to_string();
                            }
                        }
                        "transcripts_dir" => {
                            if let Some(s) = v.as_str() {
                                state.transcripts_dir = s.to_string();
                            }
                        }
                        "notes_path" => {
                            if let Some(s) = v.as_str() {
                                state.notes_path = s.to_string();
                            }
                        }
                        "output_notes" => {
                            if let Some(s) = v.as_str() {
                                state.output_notes = s.to_string();
                            }
                        }
                        "output_patches" => {
                            if let Some(s) = v.as_str() {
                                state.output_patches = s.to_string();
                            }
                        }
                        "output_distilled" => {
                            if let Some(s) = v.as_str() {
                                state.output_distilled = s.to_string();
                            }
                        }
                        "output_pdf" => {
                            if let Some(s) = v.as_str() {
                                state.output_pdf = s.to_string();
                            }
                        }
                        "template_path" => {
                            if let Some(s) = v.as_str() {
                                state.template_path = s.to_string();
                            }
                        }
                        "typst_path" => {
                            if let Some(s) = v.as_str() {
                                state.typst_path = s.to_string();
                            }
                        }
                        "max_pages" => {
                            if let Some(n) = v.as_u64() {
                                state.max_pages = n as usize;
                            }
                        }
                        "video_id" => {
                            if let Some(s) = v.as_str() {
                                state.video_id = s.to_string();
                            }
                        }
                        "llm_max_concurrency" => {
                            if let Some(n) = v.as_u64() {
                                state.llm_max_concurrency = (n as usize).clamp(1, 32);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        state
    }
}

// ---------------------------------------------------------------------------
// ProjectStateStore
// ---------------------------------------------------------------------------

/// Load and save ProjectState to/from a project directory.
pub struct ProjectStateStore {
    config_path: String,
}

impl ProjectStateStore {
    const CONFIG_FILENAME: &'static str = "config.json";

    pub fn new(project_dir: &str) -> Self {
        let config_path = Path::new(project_dir)
            .join(Self::CONFIG_FILENAME)
            .to_string_lossy()
            .to_string();
        Self { config_path }
    }

    /// Load state from config file, or return defaults if file doesn't exist
    /// or is corrupted.
    pub fn load(&self) -> ProjectState {
        match fs::read_to_string(&self.config_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(data) => ProjectState::from_config_value(&data),
                Err(_) => ProjectState::default(),
            },
            Err(_) => ProjectState::default(),
        }
    }

    /// Save state to config file (skipping forbidden/cookie fields).
    pub fn save(&self, state: &ProjectState) -> Result<()> {
        // Ensure parent directory exists.
        if let Some(parent) = Path::new(&self.config_path).parent() {
            fs::create_dir_all(parent)?;
        }
        let data = state.to_config_value();
        let json = serde_json::to_string_pretty(&data)?;
        fs::write(&self.config_path, json)?;
        Ok(())
    }

    /// Load current state, update with provided fields (skipping forbidden
    /// keys and cookie), save, return new state.
    ///
    /// The `fields` parameter is a JSON object with key-value pairs.
    pub fn update_and_save(&self, fields: &serde_json::Value) -> Result<ProjectState> {
        let mut state = self.load();
        if let Some(obj) = fields.as_object() {
            if let Some(v) = obj.get("course_id").and_then(|v| v.as_str()) {
                state.course_id = v.to_string();
            }
            if let Some(v) = obj.get("transcripts_dir").and_then(|v| v.as_str()) {
                state.transcripts_dir = v.to_string();
            }
            if let Some(v) = obj.get("notes_path").and_then(|v| v.as_str()) {
                state.notes_path = v.to_string();
            }
            if let Some(v) = obj.get("output_notes").and_then(|v| v.as_str()) {
                state.output_notes = v.to_string();
            }
            if let Some(v) = obj.get("output_patches").and_then(|v| v.as_str()) {
                state.output_patches = v.to_string();
            }
            if let Some(v) = obj.get("output_distilled").and_then(|v| v.as_str()) {
                state.output_distilled = v.to_string();
            }
            if let Some(v) = obj.get("output_pdf").and_then(|v| v.as_str()) {
                state.output_pdf = v.to_string();
            }
            if let Some(v) = obj.get("template_path").and_then(|v| v.as_str()) {
                state.template_path = v.to_string();
            }
            if let Some(v) = obj.get("typst_path").and_then(|v| v.as_str()) {
                state.typst_path = v.to_string();
            }
            if let Some(v) = obj.get("max_pages").and_then(|v| v.as_u64()) {
                state.max_pages = v as usize;
            }
            if let Some(v) = obj.get("video_id").and_then(|v| v.as_str()) {
                state.video_id = v.to_string();
            }
            if let Some(v) = obj.get("llm_max_concurrency").and_then(|v| v.as_u64()) {
                state.llm_max_concurrency = (v as usize).clamp(1, 32);
            }
        }
        self.save(&state)?;
        Ok(state)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_forbidden_rejects_secret_keys() {
        assert!(is_forbidden("cookie"));
        assert!(is_forbidden("JAAuthCookie"));
        assert!(is_forbidden("api_key"));
        assert!(is_forbidden("PASSWORD"));
        assert!(is_forbidden("token"));
        assert!(is_forbidden("secret"));
    }

    #[test]
    fn test_is_forbidden_allows_safe_keys() {
        assert!(!is_forbidden("course_id"));
        assert!(!is_forbidden("notes_path"));
        assert!(!is_forbidden("max_pages"));
        assert!(!is_forbidden("video_id"));
        assert!(!is_forbidden("transcripts_dir"));
    }

    #[test]
    fn test_state_cookie_never_serialized() {
        let mut state = ProjectState::default();
        state.course_id = "12345".into();
        state.cookie = "super-secret-cookie".into();
        let json = state.to_config_value();
        let json_str = serde_json::to_string(&json).unwrap();
        assert!(!json_str.contains("cookie"));
        assert!(!json_str.contains("super-secret-cookie"));
        assert!(json_str.contains("12345"));
    }

    #[test]
    fn test_state_cookie_not_loaded_from_disk() {
        let data = serde_json::json!({
            "course_id": "12345",
            "cookie": "leaked-cookie-value"
        });
        let state = ProjectState::from_config_value(&data);
        assert_eq!(state.course_id, "12345");
        // Cookie should NOT be loaded.
        assert!(state.cookie.is_empty());
    }

    #[test]
    fn test_state_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStateStore::new(dir.path().to_str().unwrap());

        let fields = serde_json::json!({
            "course_id": "12345",
            "transcripts_dir": "tx",
            "max_pages": 4
        });
        let updated = store.update_and_save(&fields).unwrap();
        assert_eq!(updated.course_id, "12345");
        assert_eq!(updated.max_pages, 4);

        let loaded = store.load();
        assert_eq!(loaded.course_id, "12345");
        assert_eq!(loaded.max_pages, 4);
    }

    #[test]
    fn test_state_store_defaults_when_no_config() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProjectStateStore::new(dir.path().to_str().unwrap());
        let state = store.load();
        assert!(state.course_id.is_empty());
        assert_eq!(state.max_pages, 2);
    }
}
