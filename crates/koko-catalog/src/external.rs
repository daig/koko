use koko_common::TableId;
use std::path::{Path, PathBuf};

/// Validated physical shape of a local read-only `icebug-disk` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IcebugTableSource {
    Node {
        path: PathBuf,
        num_rows: u64,
    },
    RelCsr {
        indices_path: PathBuf,
        indptr_path: PathBuf,
        target_column: String,
        num_rows: u64,
        num_bound_nodes: u64,
    },
    RelFlat {
        path: PathBuf,
        source_column: String,
        target_column: String,
        num_rows: u64,
    },
}

impl IcebugTableSource {
    pub const fn num_rows(&self) -> u64 {
        match self {
            Self::Node { num_rows, .. }
            | Self::RelCsr { num_rows, .. }
            | Self::RelFlat { num_rows, .. } => *num_rows,
        }
    }

    pub fn node_path(&self) -> Option<&Path> {
        match self {
            Self::Node { path, .. } => Some(path),
            Self::RelCsr { .. } | Self::RelFlat { .. } => None,
        }
    }
}

/// Catalog ownership for one local read-only `icebug-disk` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcebugTable {
    pub(crate) storage: String,
    pub(crate) source: Option<IcebugTableSource>,
    pub(crate) load_error: Option<String>,
}

impl IcebugTable {
    pub fn storage(&self) -> &str {
        &self.storage
    }

    pub fn source(&self) -> Option<&IcebugTableSource> {
        self.source.as_ref()
    }

    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }
}

/// Hidden physical tables that back one schemaless graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnyTables {
    pub(crate) nodes: TableId,
    pub(crate) edges: TableId,
}

impl AnyTables {
    pub const fn nodes(self) -> TableId {
        self.nodes
    }

    pub const fn edges(self) -> TableId {
        self.edges
    }
}
