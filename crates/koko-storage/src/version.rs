//! MVCC visibility and version-chain operations.

use koko_common::{MemoryReservation, Ts, Value};

/// A superseded cell value retained until every older snapshot is gone.
pub(crate) struct PriorVersion {
    pub(crate) value: Value,
    pub(crate) replaced_at: Ts,
    pub(crate) _memory: MemoryReservation,
}

pub(crate) fn upgrade(stamps: Option<&mut Vec<Ts>>, offset: u64, writer: Ts, ts: Ts) {
    if let Some(stamp) = stamps.and_then(|stamps| stamps.get_mut(offset as usize)) {
        if *stamp == writer {
            *stamp = ts;
        }
    }
}

pub(crate) fn upgrade_chain(chain: Option<&mut Vec<PriorVersion>>, writer: Ts, timestamp: Ts) {
    if let Some(chain) = chain {
        for prior in chain {
            if prior.replaced_at == writer {
                prior.replaced_at = timestamp;
            }
        }
    }
}

pub(crate) fn pop_prior(chain: Option<&mut Vec<PriorVersion>>, writer: Ts) -> Option<PriorVersion> {
    let chain = chain?;
    let position = chain
        .iter()
        .rposition(|prior| prior.replaced_at == writer)?;
    Some(chain.remove(position))
}
