//! Plugin infrastructure for extensible Sources and Processors.
//!
//! The plugin system enables adding new data sources (Canvas videos, YouTube, PDFs,
//! etc.) and new output processors (Note Patch, Reference Digest, Cheating Sheet,
//! Flashcards, etc.) without modifying core framework code.
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────┐
//! │  plugin::registry::{SourceRegistry,              │
//! │    ProcessorRegistry, PipelineRegistry}           │
//! └──────────┬──────────────┬────────────────────────┘
//!            │              │
//!   ┌────────▼──────┐  ┌───▼──────────────────┐
//!   │ SourcePlugin  │  │ ProcessorPlugin       │
//!   │ (data input)  │  │ (output generation)   │
//!   └───────────────┘  └───────────────────────┘
//!                              │
//!                     ┌────────▼──────────┐
//!                     │ PipelineStrategy   │
//!                     │ (cross-processor   │
//!                     │  orchestration)    │
//!                     └───────────────────┘
//! ```
//!
//! ## Usage
//!
//! ```rust,ignore
//! use crate::plugin::registry::ProcessorRegistry;
//! use crate::processors::note_patch::NotePatchProcessor;
//!
//! let registry = ProcessorRegistry::new();
//! registry.register(Box::new(NotePatchProcessor));
//! ```

pub mod pipeline;
pub mod processor;
pub mod registry;
pub mod source;
pub mod types;
