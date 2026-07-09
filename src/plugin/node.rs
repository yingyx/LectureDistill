//! Node-based plugin contracts and dependency expansion.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use crate::web::processes::{ProcessOutput, ProcessOutputKind, ProcessStore};
use crate::web::sources::SourceStore;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Input,
    Output,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginDescriptor {
    pub id: String,
    pub version: String,
    pub display_name: String,
    pub kind: PluginKind,
    #[serde(default)]
    pub nodes: Vec<OutputNodeDescriptor>,
    #[serde(default)]
    pub config_schema: serde_json::Value,
    #[serde(default)]
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct OutputNodeDescriptor {
    pub key: String,
    pub plugin_id: String,
    pub node_id: String,
    pub display_name: String,
    pub legacy_kind: String,
    pub artifact_ext: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

pub struct NodeExecutionContext<'a> {
    pub process_id: &'a str,
    pub source_ids: &'a [String],
    pub process_store: &'a ProcessStore,
    pub source_store: &'a SourceStore,
    pub job_id: &'a str,
}

#[async_trait]
pub trait OutputPlugin: Send + Sync {
    fn descriptor(&self) -> PluginDescriptor;

    async fn execute_nodes(&self, outputs: &[ProcessOutput], ctx: &NodeExecutionContext<'_>);
}

#[derive(Debug, thiserror::Error)]
pub enum PluginGraphError {
    #[error("unknown output node: {0}")]
    UnknownNode(String),
    #[error("cyclic output node dependency involving: {0}")]
    CyclicDependency(String),
}

pub struct PluginRegistry {
    output_plugins: Vec<Box<dyn OutputPlugin>>,
    nodes: HashMap<String, OutputNodeDescriptor>,
    owner_by_node: HashMap<String, usize>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            output_plugins: Vec::new(),
            nodes: HashMap::new(),
            owner_by_node: HashMap::new(),
        }
    }

    pub fn register_output(&mut self, plugin: Box<dyn OutputPlugin>) {
        let idx = self.output_plugins.len();
        let descriptor = plugin.descriptor();
        for node in descriptor.nodes {
            self.owner_by_node.insert(node.key.clone(), idx);
            self.nodes.insert(node.key.clone(), node);
        }
        self.output_plugins.push(plugin);
    }

    pub fn descriptors(&self) -> Vec<PluginDescriptor> {
        self.output_plugins.iter().map(|p| p.descriptor()).collect()
    }

    pub fn node(&self, key: &str) -> Option<&OutputNodeDescriptor> {
        self.nodes.get(key)
    }

    pub fn key_for_legacy_kind(&self, kind: &str) -> Option<String> {
        let normalized = match kind {
            "note_patch" => ProcessOutputKind::NotePatch.node_key(),
            "reference_digest" => ProcessOutputKind::ReferenceDigest.node_key(),
            "cheating_sheet" => ProcessOutputKind::CheatingSheet.node_key(),
            other if other.contains('.') => other.to_string(),
            _ => return None,
        };
        self.nodes.contains_key(&normalized).then_some(normalized)
    }

    pub fn expand_requested_nodes(
        &self,
        requested: &[String],
    ) -> Result<Vec<String>, PluginGraphError> {
        let mut ordered = Vec::new();
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();

        for key in requested {
            self.visit_node(key, &mut visiting, &mut visited, &mut ordered)?;
        }

        Ok(ordered)
    }

    fn visit_node(
        &self,
        key: &str,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        ordered: &mut Vec<String>,
    ) -> Result<(), PluginGraphError> {
        if visited.contains(key) {
            return Ok(());
        }
        let node = self
            .nodes
            .get(key)
            .ok_or_else(|| PluginGraphError::UnknownNode(key.to_string()))?;
        if !visiting.insert(key.to_string()) {
            return Err(PluginGraphError::CyclicDependency(key.to_string()));
        }
        for dep in &node.depends_on {
            self.visit_node(dep, visiting, visited, ordered)?;
        }
        visiting.remove(key);
        visited.insert(key.to_string());
        ordered.push(key.to_string());
        Ok(())
    }

    pub async fn execute_outputs(&self, outputs: &[ProcessOutput], ctx: &NodeExecutionContext<'_>) {
        let mut grouped: HashMap<usize, Vec<ProcessOutput>> = HashMap::new();
        for output in outputs {
            let key = output.node_key();
            if let Some(owner_idx) = self.owner_by_node.get(&key) {
                grouped.entry(*owner_idx).or_default().push(output.clone());
            } else {
                mark_unknown_node(output, ctx.process_id, ctx.process_store);
            }
        }

        let mut owners: Vec<usize> = grouped.keys().copied().collect();
        owners.sort_unstable();
        for owner_idx in owners {
            if let Some(plugin) = self.output_plugins.get(owner_idx) {
                if let Some(nodes) = grouped.get(&owner_idx) {
                    plugin.execute_nodes(nodes, ctx).await;
                }
            }
        }
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn mark_unknown_node(output: &ProcessOutput, process_id: &str, process_store: &ProcessStore) {
    let key = output.node_key();
    let _ = process_store.update(process_id, |record| {
        if let Some(existing) = record.outputs.iter_mut().find(|o| o.id == output.id) {
            existing.status = crate::web::processes::ProcessStatus::Failed;
            existing.last_error = Some(format!("No output plugin registered for node {key}"));
        }
    });
}

pub fn output_node(
    plugin_id: &str,
    node_id: &str,
    display_name: &str,
    legacy_kind: ProcessOutputKind,
    artifact_ext: &str,
    depends_on: Vec<String>,
) -> OutputNodeDescriptor {
    OutputNodeDescriptor {
        key: format!("{plugin_id}.{node_id}"),
        plugin_id: plugin_id.to_string(),
        node_id: node_id.to_string(),
        display_name: display_name.to_string(),
        legacy_kind: legacy_kind.legacy_id().to_string(),
        artifact_ext: artifact_ext.to_string(),
        depends_on,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EmptyPlugin {
        descriptor: PluginDescriptor,
    }

    #[async_trait]
    impl OutputPlugin for EmptyPlugin {
        fn descriptor(&self) -> PluginDescriptor {
            self.descriptor.clone()
        }

        async fn execute_nodes(&self, _outputs: &[ProcessOutput], _ctx: &NodeExecutionContext<'_>) {
        }
    }

    fn registry_with_ref_cheat() -> PluginRegistry {
        let mut registry = PluginRegistry::new();
        registry.register_output(Box::new(EmptyPlugin {
            descriptor: PluginDescriptor {
                id: "builtin.ref_cheat".to_string(),
                version: "0.1.0".to_string(),
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
                config_schema: serde_json::json!({}),
                actions: vec![],
            },
        }));
        registry
    }

    #[test]
    fn expands_node_dependencies_in_order() {
        let registry = registry_with_ref_cheat();
        let expanded = registry
            .expand_requested_nodes(&["builtin.ref_cheat.cheat".to_string()])
            .unwrap();
        assert_eq!(
            expanded,
            vec![
                "builtin.ref_cheat.ref".to_string(),
                "builtin.ref_cheat.cheat".to_string()
            ]
        );
    }

    #[test]
    fn maps_legacy_kinds_to_node_keys() {
        let registry = registry_with_ref_cheat();
        assert_eq!(
            registry.key_for_legacy_kind("reference_digest").unwrap(),
            "builtin.ref_cheat.ref"
        );
        assert_eq!(
            registry.key_for_legacy_kind("cheating_sheet").unwrap(),
            "builtin.ref_cheat.cheat"
        );
    }

    #[test]
    fn detects_unknown_nodes() {
        let registry = registry_with_ref_cheat();
        let err = registry
            .expand_requested_nodes(&["missing.node".to_string()])
            .unwrap_err();
        assert!(matches!(err, PluginGraphError::UnknownNode(_)));
    }
}
