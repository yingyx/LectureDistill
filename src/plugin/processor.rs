//! Processor plugin trait.
//!
//! A **Processor** generates one or more output artifacts from source data —
//! Note Patches, Reference Digests, Cheating Sheets, Flashcards, etc.

use async_trait::async_trait;

use crate::web::processes::{ProcessOutput, ProcessOutputKind};

use super::types::PipelineContext;

/// Trait for output processors.
///
/// Each processor generates one kind of output. The trait uses
/// `#[async_trait]` so implementations can be async.
///
/// # Example
///
/// ```rust,ignore
/// struct NotePatchProcessor;
///
/// #[async_trait]
/// impl ProcessorPlugin for NotePatchProcessor {
///     fn kind(&self) -> ProcessOutputKind { ProcessOutputKind::NotePatch }
///     fn name(&self) -> &'static str { "Note Patch" }
///     fn description(&self) -> &'static str {
///         "Patches Markdown notes with transcript context"
///     }
///
///     async fn execute(
///         &self,
///         outputs: &[ProcessOutput],
///         ctx: &PipelineContext,
///     ) {
///         crate::processors::note_patch::run_note_patch(
///             &ctx.process_id,
///             &ctx.source_ids,
///             outputs,
///             &ctx.process_store,
///             &ctx.source_store,
///             &ctx.job_id,
///         ).await;
///     }
/// }
/// ```
#[async_trait]
pub trait ProcessorPlugin: Send + Sync {
    /// The `ProcessOutputKind` this processor generates.
    fn kind(&self) -> ProcessOutputKind;

    /// Human-readable name for UI display.
    fn name(&self) -> &'static str;

    /// One-line description for tooltips and help text.
    fn description(&self) -> &'static str;

    /// Execute this processor on the given outputs within the pipeline context.
    ///
    /// The processor should update each output's status and metadata via
    /// `ctx.process_store` as it progresses.
    async fn execute(&self, outputs: &[ProcessOutput], ctx: &PipelineContext);
}
