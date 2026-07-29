use crate::{Column, ColumnDefinition};
use koko_common::{RelStorageDirection, TableId};
use std::collections::HashMap;

/// One declared endpoint pair and its physical relationship-table member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelTablePair {
    pub from: TableId,
    pub to: TableId,
    pub member: TableId,
}

/// Input for atomically defining a relationship table.
#[derive(Debug, Clone, PartialEq)]
pub struct RelTableDefinition {
    pub name: String,
    pub endpoint_pairs: Vec<(TableId, TableId)>,
    pub columns: Vec<ColumnDefinition>,
    pub storage_direction: RelStorageDirection,
}

/// A catalog-owned relationship table.
#[derive(Debug, Clone)]
pub struct RelTable {
    pub(crate) id: TableId,
    pub(crate) name: String,
    pub(crate) pairs: Vec<RelTablePair>,
    pub(crate) columns: Vec<Column>,
    pub(crate) storage_direction: RelStorageDirection,
    pub(crate) comment: Option<String>,
    pub(crate) name_to_idx: HashMap<String, usize>,
}

impl RelTable {
    pub const fn id(&self) -> TableId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn pairs(&self) -> &[RelTablePair] {
        &self.pairs
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub const fn storage_direction(&self) -> RelStorageDirection {
        self.storage_direction
    }

    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// Representative FROM table (the first pair's FROM).
    pub fn from(&self) -> TableId {
        self.pairs[0].from
    }

    /// Representative TO table (the first pair's TO).
    pub fn to(&self) -> TableId {
        self.pairs[0].to
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.name_to_idx
            .get(&name.to_ascii_lowercase())
            .map(|&index| &self.columns[index])
    }

    pub fn has_column(&self, name: &str) -> bool {
        self.name_to_idx.contains_key(&name.to_ascii_lowercase())
    }
}
