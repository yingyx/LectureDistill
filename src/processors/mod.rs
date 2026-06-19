//! Processor execution engines extracted from `crate::web::app`.
//!
//! Each sub-module contains an async `run_*` function that executes one
//! kind of process output (Note Patch, Reference Digest, Cheating Sheet, etc.).

pub mod cheating_sheet;
pub mod course_note;
pub mod course_note_patch;
pub mod course_reference_digest;
pub mod note_patch;
pub mod reference_digest;
