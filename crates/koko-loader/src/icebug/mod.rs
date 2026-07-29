//! Validated descriptors and pinned query-time readers for local read-only
//! `icebug-disk` tables.

mod descriptor;
mod source;

pub use descriptor::{
    deferred_node_error, deferred_rel_error, inspect_node_table, inspect_rel_table, is_remote,
    validate_schema, validate_version,
};
pub use source::{IcebugNodeScan, IcebugQuerySources};
