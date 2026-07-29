//! Typed, chunked in-memory MVCC storage.
//!
//! The crate exposes one concrete [`InMemStorage`] engine plus explicit shared
//! read/write guards. Physical persistence remains outside the product scope.

mod engine;
mod handle;
mod index;
mod node;
mod relation;
mod shared;
mod undo;
mod version;

pub use engine::InMemStorage;
pub use handle::{StorageReadHandle, StorageWriteHandle};
pub use relation::{BatchNeighbor, EdgeDir};
pub use shared::{CommitClock, SharedStorage};
