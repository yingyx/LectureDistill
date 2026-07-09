//! Node graph output dispatch.

pub mod separate;
pub mod unified_ref_cheat;

use std::collections::HashMap;

use crate::plugin::builtins::builtin_registry;
use crate::plugin::node::NodeExecutionContext;
use crate::web::processes::{ProcessOutput, ProcessStore};
use crate::web::sources::SourceStore;

/// Execute process outputs through the built-in node plugin registry.
///
/// The public signature is intentionally kept compatible with the previous
/// dispatcher used by CLI and Web handlers.
pub async fn run_process_outputs(
    process_id: &str,
    source_ids: &[String],
    outputs: &[ProcessOutput],
    process_store: &ProcessStore,
    source_store: &SourceStore,
    job_id: &str,
) {
    let registry = builtin_registry();
    let output_by_key: HashMap<String, Vec<ProcessOutput>> =
        outputs
            .iter()
            .cloned()
            .fold(HashMap::new(), |mut acc, output| {
                acc.entry(output.node_key()).or_default().push(output);
                acc
            });
    let requested_keys: Vec<String> = output_by_key.keys().cloned().collect();
    let ordered_keys = match registry.expand_requested_nodes(&requested_keys) {
        Ok(keys) => keys,
        Err(e) => {
            for output in outputs {
                let msg = e.to_string();
                let _ = process_store.update(process_id, |record| {
                    if let Some(existing) = record.outputs.iter_mut().find(|o| o.id == output.id) {
                        existing.status = crate::web::processes::ProcessStatus::Failed;
                        existing.last_error = Some(msg.clone());
                    }
                });
            }
            return;
        }
    };

    let mut expanded_outputs = Vec::new();
    for key in ordered_keys {
        if let Some(existing) = output_by_key.get(&key) {
            expanded_outputs.extend(existing.clone());
        }
    }

    let ctx = NodeExecutionContext {
        process_id,
        source_ids,
        process_store,
        source_store,
        job_id,
    };
    registry.execute_outputs(&expanded_outputs, &ctx).await;
}
