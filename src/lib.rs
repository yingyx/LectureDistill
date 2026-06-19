//! lecture-distill - SJTU Canvas video subtitle ingestion, Markdown notes
//! patching, exam-focused distillation, and cheat sheet PDF rendering.
//!
//! Library crate, re-exporting all modules for use by both the binary
//! and integration tests.

pub mod artifacts;
pub mod canvas_sjtu;
pub mod diff;
pub mod distill;
pub mod latex;
pub mod llm;
pub mod llm_log;
pub mod notes;
pub mod pipeline;
pub mod pipelines;
pub mod plugin;
pub mod processors;
pub mod prompts;
pub mod ranking;
pub mod transcripts;
pub mod utils;
pub mod web;
