//! Source plugin trait.
//!
//! A **Source** is a data input — lecture transcripts, Markdown notes,
//! Canvas videos, PDF slides, etc. Each source plugin knows how to create
//! and describe one kind of source record.

use crate::web::sources::{SourceKind, SourceRecord};

/// Trait for source data providers.
///
/// Implementations describe how to create and manage one kind of source
/// (e.g. Canvas video transcripts, user-uploaded Markdown notes).
///
/// # Example
///
/// ```rust,ignore
/// struct NoteSourcePlugin;
///
/// impl SourcePlugin for NoteSourcePlugin {
///     fn kind(&self) -> SourceKind { SourceKind::Note }
///     fn name(&self) -> &'static str { "Markdown Note" }
///     fn description(&self) -> &'static str {
///         "A user-authored or uploaded Markdown note"
///     }
///     fn create_record(&self, id: &str, title: &str, path: &str) -> SourceRecord {
///         SourceRecord::new(id, SourceKind::Note, title, path)
///     }
/// }
/// ```
pub trait SourcePlugin: Send + Sync {
    /// The `SourceKind` this plugin handles.
    fn kind(&self) -> SourceKind;

    /// Human-readable name for UI display.
    fn name(&self) -> &'static str;

    /// One-line description for tooltips and help text.
    fn description(&self) -> &'static str;

    /// Create a new `SourceRecord` with the given identity.
    ///
    /// The record is not persisted — the caller is responsible for saving
    /// it to the source store.
    fn create_record(&self, id: &str, title: &str, path: &str) -> SourceRecord;
}
