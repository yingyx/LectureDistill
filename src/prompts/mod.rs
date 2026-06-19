//! LLM prompt builders extracted from `crate::web::app`.
//!
//! Each sub-module contains pure functions that return system/user prompt
//! strings. They have zero dependencies on `AppState` or axum HTTP types.

pub mod cheat_sheet;
pub mod note_patch;
pub mod ref_digest;
pub mod unified_pipeline;
