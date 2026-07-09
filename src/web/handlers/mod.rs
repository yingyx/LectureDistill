//! Thin HTTP handler functions extracted from `crate::web::app`.
//!
//! Each handler takes axum `State`, `Path`, `Query`, `Json` extractors and
//! delegates to the processor / pipeline / source / course layers.

pub mod canvas;
pub mod courses;
pub mod jobs;
pub mod llm_logs;
pub mod notes;
pub mod plugins;
pub mod processes;
pub mod secrets;
pub mod sources;
pub mod spa;
pub mod state_config;
pub mod transcripts;
