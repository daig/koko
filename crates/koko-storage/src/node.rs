//! Node-row storage accounting.

use crate::index::PkKey;
use koko_common::{MemoryReservation, Ts};

pub(crate) fn row_bytes(key: &PkKey) -> u64 {
    (usize_bytes(std::mem::size_of::<Ts>()) * 2)
        .saturating_add(usize_bytes(std::mem::size_of::<MemoryReservation>()))
        .saturating_add(match key {
            PkKey::Str(value) => usize_bytes(value.capacity()),
            PkKey::Bytes(value) => usize_bytes(value.capacity()),
            _ => 0,
        })
}

fn usize_bytes(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
