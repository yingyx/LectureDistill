//! Pure utility functions extracted from `crate::web::app`.
//!
//! These functions have no dependency on `AppState` or axum HTTP types.
//! They take plain Rust/domain types and return computed values.

pub mod budget;
pub mod markdown;
pub mod outline;
pub mod output;
pub mod retrieval;
pub mod transcript_md;
pub mod video;
