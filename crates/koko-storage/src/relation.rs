//! Relationship rows and dense adjacency.

use koko_common::{InternalId, MemoryReservation, TableId, Ts};

/// One adjacency result produced for an input node in a batched extend.
#[derive(Debug, Clone, Copy)]
pub struct BatchNeighbor {
    pub input_pos: usize,
    pub nbr: InternalId,
    pub rel: InternalId,
}

/// The direction in which a node participates in a relationship table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDir {
    Fwd,
    Bwd,
}

impl EdgeDir {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fwd => "fwd",
            Self::Bwd => "bwd",
        }
    }
}

pub(crate) fn row_bytes() -> u64 {
    (usize_bytes(std::mem::size_of::<InternalId>()) * 2)
        .saturating_add(usize_bytes(std::mem::size_of::<Ts>()) * 2)
        .saturating_add(usize_bytes(std::mem::size_of::<u64>()) * 2)
        .saturating_add(usize_bytes(std::mem::size_of::<MemoryReservation>()))
}

pub(crate) fn adjacency(entries: &[Vec<u64>], node: InternalId, keyed_table: TableId) -> &[u64] {
    if node.table_id != keyed_table {
        return &[];
    }
    entries
        .get(node.offset.0 as usize)
        .map_or(&[][..], Vec::as_slice)
}

pub(crate) fn push_adjacency(entries: &mut Vec<Vec<u64>>, node: InternalId, rel: u64) {
    let offset = node.offset.0 as usize;
    if offset >= entries.len() {
        entries.resize_with(offset + 1, Vec::new);
    }
    entries[offset].push(rel);
}

pub(crate) fn remove_adjacency(entries: &mut [Vec<u64>], node: InternalId, rel: u64) {
    if let Some((entries, position)) = entries.get_mut(node.offset.0 as usize).and_then(|entries| {
        entries
            .iter()
            .rposition(|&entry| entry == rel)
            .map(|position| (entries, position))
    }) {
        entries.remove(position);
    }
}

fn usize_bytes(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
