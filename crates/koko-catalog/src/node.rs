use crate::Column;
use koko_common::{LogicalType, TableId};
use std::collections::HashMap;

/// Input for atomically defining a node table.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeTableDefinition {
    pub name: String,
    pub columns: Vec<crate::ColumnDefinition>,
    pub primary_key: String,
}

/// A catalog-owned node table.
#[derive(Debug, Clone)]
pub struct NodeTable {
    pub(crate) id: TableId,
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) primary_key: usize,
    pub(crate) comment: Option<String>,
    pub(crate) name_to_idx: HashMap<String, usize>,
}

impl NodeTable {
    pub const fn id(&self) -> TableId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub const fn primary_key_index(&self) -> usize {
        self.primary_key
    }

    pub fn primary_key_column(&self) -> &Column {
        &self.columns[self.primary_key]
    }

    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.name_to_idx
            .get(&name.to_ascii_lowercase())
            .map(|&index| &self.columns[index])
    }

    pub fn has_column(&self, name: &str) -> bool {
        self.name_to_idx.contains_key(&name.to_ascii_lowercase())
    }

    pub fn column_types(&self) -> impl ExactSizeIterator<Item = &LogicalType> {
        self.columns.iter().map(Column::logical_type)
    }
}
