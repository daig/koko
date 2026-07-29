//! Reversible transaction mutation records.

use crate::{
    engine::{NodeStore, PropertyColumn, RelStore},
    index::PkKey,
};
use koko_common::{TableId, Ts};

/// The inverse of one storage mutation.
pub(crate) enum UndoOp {
    UninsertNode {
        table: TableId,
        offset: u64,
        pk: PkKey,
        prev_owner: Option<u64>,
    },
    UninsertRel {
        table: TableId,
        offset: u64,
    },
    RestoreNodeProp {
        table: TableId,
        offset: u64,
        col: usize,
    },
    RestoreRelProp {
        table: TableId,
        offset: u64,
        col: usize,
    },
    UndeleteNode {
        table: TableId,
        offset: u64,
    },
    UndeleteRel {
        table: TableId,
        offset: u64,
    },
    UncreateTable {
        table: TableId,
    },
    RestoreNodeTable {
        table: TableId,
        store: NodeStore,
    },
    RestoreRelTable {
        table: TableId,
        store: RelStore,
    },
    UnaddColumn {
        table: TableId,
    },
    RestoreNodeColumn {
        table: TableId,
        idx: usize,
        column: PropertyColumn,
        pk_column: usize,
    },
    RestoreRelColumn {
        table: TableId,
        idx: usize,
        column: PropertyColumn,
    },
}

pub(crate) struct UndoEntry {
    pub(crate) writer: Ts,
    pub(crate) seq: usize,
    pub(crate) op: UndoOp,
}
