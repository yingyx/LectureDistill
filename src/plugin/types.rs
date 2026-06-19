//! Shared types for the plugin system.
//!
//! These types provide a common vocabulary for passing context between
//! pipeline strategies and processor plugins.

use std::sync::Arc;

use crate::web::processes::ProcessStore;
use crate::web::sources::SourceStore;

/// Shared context passed to all processor and pipeline executions.
///
/// Aggregates the identifiers and store references needed by every
/// processor to read sources, write outputs, and report progress.
#[derive(Clone)]
pub struct PipelineContext {
    /// The ID of the process record being executed.
    pub process_id: String,
    /// Source record IDs attached to this process.
    pub source_ids: Vec<String>,
    /// Persistent process store for updating output status/metadata.
    pub process_store: Arc<ProcessStore>,
    /// Persistent source store for reading source content.
    pub source_store: Arc<SourceStore>,
    /// Background job ID for logging and status updates.
    pub job_id: String,
}

impl PipelineContext {
    /// Create a new pipeline context.
    pub fn new(
        process_id: impl Into<String>,
        source_ids: Vec<String>,
        process_store: Arc<ProcessStore>,
        source_store: Arc<SourceStore>,
        job_id: impl Into<String>,
    ) -> Self {
        Self {
            process_id: process_id.into(),
            source_ids,
            process_store,
            source_store,
            job_id: job_id.into(),
        }
    }
}
