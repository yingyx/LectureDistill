//! Node-based plugin infrastructure.
//!
//! A plugin can expose one or more output nodes. Nodes, not whole plugins, are
//! the dependency unit: for example `builtin.ref_cheat` exposes both
//! `builtin.ref_cheat.ref` and `builtin.ref_cheat.cheat`, and other plugins can
//! depend on either node.

pub mod builtins;
pub mod node;
