use crate::bound::VarId;
use koko_common::{LogicalType, TableId};
use std::collections::HashMap;

/// Maps a table column id to its position in a runtime chunk.
#[derive(Debug, Clone)]
pub struct PropCol {
    pub column_id: u32,
    pub col_index: usize,
}

/// A property's location and type in a runtime chunk.
#[derive(Debug, Clone)]
pub struct LayoutProp {
    pub name: String,
    pub col_index: usize,
    pub ty: LogicalType,
}

/// Per-variable column bookkeeping in a [`RowLayout`].
#[derive(Debug, Clone)]
pub enum VarColKind {
    Node {
        table: Option<TableId>,
        label: String,
    },
    Rel {
        table: Option<TableId>,
        label: String,
        src_id_col: usize,
        dst_id_col: usize,
    },
    Scalar,
}

impl VarColKind {
    pub fn graph_value_type(&self) -> Option<(LogicalType, bool)> {
        match self {
            Self::Node { table, .. } => {
                Some((LogicalType::Node(table.unwrap_or(TableId(u64::MAX))), true))
            }
            Self::Rel { table, .. } => {
                Some((LogicalType::Rel(table.unwrap_or(TableId(u64::MAX))), false))
            }
            Self::Scalar => None,
        }
    }
}
impl VarColKind {
    pub fn node(table: Option<TableId>, label: String) -> Self {
        Self::Node { table, label }
    }

    pub fn relation(
        table: Option<TableId>,
        label: String,
        src_id_col: usize,
        dst_id_col: usize,
    ) -> Self {
        Self::Rel {
            table,
            label,
            src_id_col,
            dst_id_col,
        }
    }

    pub fn node_table(&self) -> Option<TableId> {
        match self {
            Self::Node { table, .. } => *table,
            _ => None,
        }
    }

    pub fn relation_columns(&self) -> Option<(usize, usize)> {
        match self {
            Self::Rel {
                src_id_col,
                dst_id_col,
                ..
            } => Some((*src_id_col, *dst_id_col)),
            _ => None,
        }
    }
}

/// Where a variable's columns live in a runtime chunk.
#[derive(Debug, Clone)]
pub struct VarColumns {
    pub var: VarId,
    pub kind: VarColKind,
    pub id_col: usize,
    pub props: Vec<LayoutProp>,
    pub value_col: Option<usize>,
}

/// Runtime column layout and variable/property locations.
#[derive(Debug, Clone, Default)]
pub struct RowLayout {
    pub col_types: Vec<LogicalType>,
    vars: Vec<VarColumns>,
    index: HashMap<VarId, usize>,
    subquery_cols: Vec<usize>,
    sequence_cols: Vec<usize>,
    pub table_names: HashMap<TableId, String>,
}

impl RowLayout {
    pub fn width(&self) -> usize {
        self.col_types.len()
    }

    pub fn subquery_column(&self, id: usize) -> Option<usize> {
        self.subquery_cols.get(id).copied()
    }

    pub fn sequence_column(&self, id: usize) -> Option<usize> {
        self.sequence_cols.get(id).copied()
    }

    pub fn allocate(&mut self, ty: LogicalType) -> usize {
        let index = self.col_types.len();
        self.col_types.push(ty);
        index
    }

    pub fn add_variable(&mut self, columns: VarColumns) {
        self.index.insert(columns.var, self.vars.len());
        self.vars.push(columns);
    }

    pub fn add_scalar(&mut self, var: VarId, ty: LogicalType) -> usize {
        let col = self.allocate(ty);
        self.add_variable(VarColumns {
            var,
            kind: VarColKind::Scalar,
            id_col: col,
            props: Vec::new(),
            value_col: None,
        });
        col
    }

    pub fn var(&self, var: VarId) -> &VarColumns {
        &self.vars[self.index[&var]]
    }

    pub fn try_var(&self, var: VarId) -> Option<&VarColumns> {
        self.index.get(&var).map(|&index| &self.vars[index])
    }

    pub fn var_ids(&self) -> impl Iterator<Item = VarId> + '_ {
        self.index.keys().copied()
    }

    pub fn capture_var_slot(&self, var: VarId) -> Option<usize> {
        self.index.get(&var).copied()
    }

    pub fn restore_var_slot(&mut self, var: VarId, slot: usize) {
        self.index.insert(var, slot);
    }

    pub fn add_subquery_column(&mut self, id: usize, ty: LogicalType) -> usize {
        let col = self.allocate(ty);
        self.ensure_subquery_slots(id + 1);
        self.subquery_cols[id] = col;
        col
    }

    pub fn ensure_subquery_slots(&mut self, len: usize) {
        if self.subquery_cols.len() < len {
            self.subquery_cols.resize(len, usize::MAX);
        }
    }

    pub fn add_sequence_column(&mut self, ty: LogicalType) -> usize {
        let col = self.allocate(ty);
        self.sequence_cols.push(col);
        col
    }

    pub fn add_value_column(&mut self, var: VarId, ty: LogicalType) -> Option<(usize, usize)> {
        let slot = *self.index.get(&var)?;
        if self.vars[slot].value_col.is_some() {
            return None;
        }
        let id_col = self.vars[slot].id_col;
        let value_col = self.allocate(ty);
        self.vars[slot].value_col = Some(value_col);
        Some((id_col, value_col))
    }

    pub fn column(&self, var: VarId, property: Option<&str>) -> Option<usize> {
        let columns = self.try_var(var)?;
        match property {
            None => Some(columns.id_col),
            Some(name) => columns
                .props
                .iter()
                .find(|property| property.name.eq_ignore_ascii_case(name))
                .map(|property| property.col_index),
        }
    }
}
