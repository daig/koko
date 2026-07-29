//! Explicit storage read and write capabilities.

use koko_common::{ReadView, Ts};

/// An immutable storage snapshot carried by every visibility-sensitive read.
#[derive(Debug, Clone, Copy)]
pub struct StorageReadHandle {
    view: ReadView,
}

impl StorageReadHandle {
    #[inline]
    pub const fn new(view: ReadView) -> Self {
        Self { view }
    }

    #[inline]
    pub const fn view(self) -> ReadView {
        self.view
    }
}

impl From<ReadView> for StorageReadHandle {
    fn from(view: ReadView) -> Self {
        Self::new(view)
    }
}

/// The writer context for a mutation and its matching snapshot reads.
#[derive(Debug, Clone, Copy)]
pub struct StorageWriteHandle {
    read: StorageReadHandle,
    writer_id: Ts,
}

impl StorageWriteHandle {
    #[inline]
    pub fn new(view: ReadView, writer_id: Ts) -> Self {
        debug_assert_eq!(view.writer_id, Some(writer_id));
        Self {
            read: StorageReadHandle::new(view),
            writer_id,
        }
    }

    #[inline]
    pub const fn read(self) -> StorageReadHandle {
        self.read
    }

    #[inline]
    pub const fn writer_id(self) -> Ts {
        self.writer_id
    }
}
