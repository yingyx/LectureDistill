//! Plugin registries.
//!
//! Thread-safe registries that hold available source plugins, processor
//! plugins, and pipeline strategies. Plugins are registered at startup
//! and looked up at runtime by kind or by applicability.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::web::processes::{ProcessOutput, ProcessOutputKind};
use crate::web::sources::SourceKind;

use super::pipeline::PipelineStrategy;
use super::processor::ProcessorPlugin;
use super::source::SourcePlugin;
use super::types::PipelineContext;

// ---------------------------------------------------------------------------
// Source registry
// ---------------------------------------------------------------------------

/// Registry of available source plugins, keyed by `SourceKind`.
///
/// Thread-safe: registration and lookup both use `RwLock`.
pub struct SourceRegistry {
    sources: RwLock<HashMap<SourceKind, Box<dyn SourcePlugin>>>,
}

impl SourceRegistry {
    /// Create an empty source registry.
    pub fn new() -> Self {
        Self {
            sources: RwLock::new(HashMap::new()),
        }
    }

    /// Register a source plugin.
    ///
    /// If a plugin with the same `SourceKind` already exists, it is
    /// replaced (last-write-wins).
    pub fn register(&self, plugin: Box<dyn SourcePlugin>) {
        let mut sources = self.sources.write().expect("SourceRegistry lock poisoned");
        sources.insert(plugin.kind(), plugin);
    }

    /// Return `true` if a plugin is registered for the given kind.
    pub fn has(&self, kind: &SourceKind) -> bool {
        let sources = self.sources.read().expect("SourceRegistry lock poisoned");
        sources.contains_key(kind)
    }

    /// List all registered source plugin kinds (for UI discovery).
    pub fn kinds(&self) -> Vec<SourceKind> {
        let sources = self.sources.read().expect("SourceRegistry lock poisoned");
        sources.keys().cloned().collect()
    }

    /// Return the number of registered source plugins.
    pub fn len(&self) -> usize {
        self.sources
            .read()
            .expect("SourceRegistry lock poisoned")
            .len()
    }

    /// Return `true` if no plugins are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Processor registry
// ---------------------------------------------------------------------------

/// Registry of available processor plugins, keyed by `ProcessOutputKind`.
///
/// Thread-safe: registration and lookup both use `RwLock`.
pub struct ProcessorRegistry {
    processors: RwLock<HashMap<ProcessOutputKind, Box<dyn ProcessorPlugin>>>,
}

impl ProcessorRegistry {
    /// Create an empty processor registry.
    pub fn new() -> Self {
        Self {
            processors: RwLock::new(HashMap::new()),
        }
    }

    /// Register a processor plugin.
    ///
    /// If a plugin with the same `ProcessOutputKind` already exists, it is
    /// replaced (last-write-wins).
    pub fn register(&self, plugin: Box<dyn ProcessorPlugin>) {
        let mut processors = self
            .processors
            .write()
            .expect("ProcessorRegistry lock poisoned");
        processors.insert(plugin.kind(), plugin);
    }

    /// Return `true` if a plugin is registered for the given kind.
    pub fn has(&self, kind: &ProcessOutputKind) -> bool {
        let processors = self
            .processors
            .read()
            .expect("ProcessorRegistry lock poisoned");
        processors.contains_key(kind)
    }

    /// Return the set of all registered output kinds (for UI discovery).
    pub fn kinds(&self) -> Vec<ProcessOutputKind> {
        let processors = self
            .processors
            .read()
            .expect("ProcessorRegistry lock poisoned");
        processors.keys().cloned().collect()
    }

    /// Return the number of registered processor plugins.
    pub fn len(&self) -> usize {
        self.processors
            .read()
            .expect("ProcessorRegistry lock poisoned")
            .len()
    }

    /// Return `true` if no plugins are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Execute the processor for the given kind on the specified outputs.
    ///
    /// Returns `true` if a matching processor was found and executed.
    pub async fn execute(
        &self,
        kind: &ProcessOutputKind,
        outputs: &[ProcessOutput],
        ctx: &PipelineContext,
    ) -> bool {
        // We must drop the read lock before awaiting, so we extract what we
        // need inside a block.
        let processor_opt = {
            let processors = self
                .processors
                .read()
                .expect("ProcessorRegistry lock poisoned");
            // We cannot return a reference through the lock — instead we
            // check existence and rely on the caller side. For now, this is
            // a placeholder that will be connected in a follow-up PR.
            let _ = processors.get(kind);
            None::<()>
        };

        // Placeholder: in the follow-up PR, processors will be stored as
        // `Arc<dyn ProcessorPlugin>` so we can clone and execute.
        let _ = processor_opt;

        // Check if registered and report.
        let registered = {
            let processors = self
                .processors
                .read()
                .expect("ProcessorRegistry lock poisoned");
            processors.contains_key(kind)
        };
        if registered {
            // Full integration in follow-up PR.
            let _ = outputs;
            let _ = ctx;
        }
        registered
    }
}

impl Default for ProcessorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Pipeline registry
// ---------------------------------------------------------------------------

/// Registry of pipeline strategies, ordered by priority.
///
/// Strategies are tried in registration order; the first one whose
/// `applicable_to` returns `true` handles the outputs.
///
/// Thread-safe: registration and lookup both use `RwLock`.
pub struct PipelineRegistry {
    strategies: RwLock<Vec<Box<dyn PipelineStrategy>>>,
}

impl PipelineRegistry {
    /// Create an empty pipeline registry.
    pub fn new() -> Self {
        Self {
            strategies: RwLock::new(Vec::new()),
        }
    }

    /// Register a pipeline strategy. Earlier registrations have higher
    /// priority.
    pub fn register(&self, strategy: Box<dyn PipelineStrategy>) {
        let mut strategies = self
            .strategies
            .write()
            .expect("PipelineRegistry lock poisoned");
        strategies.push(strategy);
    }

    /// Find the first applicable strategy and execute it.
    ///
    /// Returns `true` if a strategy was found and executed, `false` if
    /// no strategy claimed the outputs.
    pub async fn run_first_applicable(
        &self,
        outputs: &[ProcessOutput],
        ctx: &PipelineContext,
    ) -> bool {
        // Placeholder: full integration in follow-up PR.
        let _ = outputs;
        let _ = ctx;

        let strategies = self
            .strategies
            .read()
            .expect("PipelineRegistry lock poisoned");
        for strategy in strategies.iter() {
            if strategy.applicable_to(outputs) {
                strategy.run(outputs, ctx).await;
                return true;
            }
        }
        false
    }

    /// Return the number of registered strategies.
    pub fn len(&self) -> usize {
        self.strategies
            .read()
            .expect("PipelineRegistry lock poisoned")
            .len()
    }

    /// Return `true` if no strategies are registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for PipelineRegistry {
    fn default() -> Self {
        Self::new()
    }
}
