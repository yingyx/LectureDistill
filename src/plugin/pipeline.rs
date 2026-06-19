//! Pipeline strategy trait.
//!
//! A **Pipeline Strategy** orchestrates multiple processors — for example,
//! the unified pipeline runs Reference Digest and Cheat Sheet in a single
//! multi-turn LLM conversation for DeepSeek prefix caching.

use async_trait::async_trait;

use crate::web::processes::ProcessOutput;

use super::types::PipelineContext;

/// Trait for pipeline strategies that orchestrate multiple processors.
///
/// A pipeline strategy examines the set of requested outputs and either
/// claims them (returning `true` from `applicable_to`) or leaves them for
/// the next strategy (or the default separate execution).
///
/// # Example
///
/// ```rust,ignore
/// struct UnifiedRefCheatPipeline;
///
/// #[async_trait]
/// impl PipelineStrategy for UnifiedRefCheatPipeline {
///     fn name(&self) -> &'static str { "Unified Ref Digest + Cheat Sheet" }
///
///     fn applicable_to(&self, outputs: &[ProcessOutput]) -> bool {
///         let has_rd = outputs.iter().any(|o| o.kind == ProcessOutputKind::ReferenceDigest);
///         let has_cs = outputs.iter().any(|o| o.kind == ProcessOutputKind::CheatingSheet);
///         has_rd && has_cs
///     }
///
///     async fn run(&self, outputs: &[ProcessOutput], ctx: &PipelineContext) {
///         // ... run unified pipeline ...
///     }
/// }
/// ```
#[async_trait]
pub trait PipelineStrategy: Send + Sync {
    /// Human-readable name for logging and debugging.
    fn name(&self) -> &'static str;

    /// Return `true` if this strategy should handle the given outputs.
    ///
    /// Strategies are tried in registration order; the first match wins.
    fn applicable_to(&self, outputs: &[ProcessOutput]) -> bool;

    /// Execute this pipeline strategy on the given outputs.
    ///
    /// The strategy owns the execution of ALL outputs it claims — it
    /// must update each output's status via `ctx.process_store`.
    async fn run(&self, outputs: &[ProcessOutput], ctx: &PipelineContext);
}
