//! Built-in Rust plugins.

use async_trait::async_trait;

use crate::pipelines::unified_ref_cheat::run_unified_cheat_pipeline;
use crate::plugin::node::{
    output_node, NodeExecutionContext, OutputPlugin, PluginDescriptor, PluginKind, PluginRegistry,
};
use crate::processors::cheating_sheet::run_cheating_sheet_outputs;
use crate::processors::note_patch::run_note_patch;
use crate::processors::reference_digest::run_reference_digest_outputs;
use crate::web::processes::{ProcessOutput, ProcessOutputKind};

pub fn builtin_registry() -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    registry.register_output(Box::new(NotePatchPlugin));
    registry.register_output(Box::new(RefCheatPlugin));
    registry
}

pub struct NotePatchPlugin;

#[async_trait]
impl OutputPlugin for NotePatchPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: "builtin.note_patch".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: "Note Patch".to_string(),
            kind: PluginKind::Output,
            nodes: vec![output_node(
                "builtin.note_patch",
                "note",
                "Note Patch",
                ProcessOutputKind::NotePatch,
                "md",
                vec![],
            )],
            config_schema: serde_json::json!({}),
            actions: vec![],
        }
    }

    async fn execute_nodes(&self, outputs: &[ProcessOutput], ctx: &NodeExecutionContext<'_>) {
        run_note_patch(
            ctx.process_id,
            ctx.source_ids,
            outputs,
            ctx.process_store,
            ctx.source_store,
            ctx.job_id,
        )
        .await;
    }
}

pub struct RefCheatPlugin;

#[async_trait]
impl OutputPlugin for RefCheatPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: "builtin.ref_cheat".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: "Reference Digest + Cheating Sheet".to_string(),
            kind: PluginKind::Output,
            nodes: vec![
                output_node(
                    "builtin.ref_cheat",
                    "ref",
                    "Reference Digest",
                    ProcessOutputKind::ReferenceDigest,
                    "md",
                    vec![],
                ),
                output_node(
                    "builtin.ref_cheat",
                    "cheat",
                    "Cheating Sheet",
                    ProcessOutputKind::CheatingSheet,
                    "pdf",
                    vec!["builtin.ref_cheat.ref".to_string()],
                ),
            ],
            config_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "default_template": {"type": "string"},
                    "templates": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": {"type": "string"},
                                "path": {"type": "string"},
                                "calibrated_at": {"type": "string"}
                            }
                        }
                    }
                }
            }),
            actions: vec![
                "import_template".to_string(),
                "delete_template".to_string(),
                "calibrate_template".to_string(),
                "set_default_template".to_string(),
            ],
        }
    }

    async fn execute_nodes(&self, outputs: &[ProcessOutput], ctx: &NodeExecutionContext<'_>) {
        let ref_outputs: Vec<ProcessOutput> = outputs
            .iter()
            .filter(|o| o.kind == ProcessOutputKind::ReferenceDigest)
            .cloned()
            .collect();
        let cheat_outputs: Vec<ProcessOutput> = outputs
            .iter()
            .filter(|o| o.kind == ProcessOutputKind::CheatingSheet)
            .cloned()
            .collect();

        if !ref_outputs.is_empty() && !cheat_outputs.is_empty() {
            run_unified_cheat_pipeline(
                ctx.process_id,
                ctx.source_ids,
                &ref_outputs,
                &cheat_outputs,
                ctx.process_store,
                ctx.source_store,
                ctx.job_id,
            )
            .await;
        } else if !ref_outputs.is_empty() {
            run_reference_digest_outputs(
                ctx.process_id,
                ctx.source_ids,
                &ref_outputs,
                ctx.process_store,
                ctx.source_store,
                ctx.job_id,
            )
            .await;
        } else if !cheat_outputs.is_empty() {
            run_cheating_sheet_outputs(
                ctx.process_id,
                &cheat_outputs,
                ctx.process_store,
                ctx.job_id,
            )
            .await;
        }
    }
}
