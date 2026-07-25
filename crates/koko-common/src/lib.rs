//! `koko-common` — the universal substrate for the Koko Rust engine.
//!
//! Everything else in the workspace speaks the types defined here: the
//! identity/[type system](types), the materialized [`Value`](value::Value), the
//! vectorized [data layer](vector) (`ValueVector`/`DataChunk`), and the unified
//! [error model](error). It depends on nothing internal — it is the leaf of the
//! dependency DAG.

pub mod csv_dialect;
pub mod decimal;
pub mod error;
pub mod file_resolver;
pub mod json;
pub mod literal;
pub mod memory;
pub mod mvcc;
pub mod scalar;
pub mod settings;
pub mod stats;
pub mod temporal;
pub mod types;
pub mod udf;
pub mod value;
pub mod vector;
pub mod warnings;

pub use error::{Error, Result};
pub use json::JsonValue;
pub use memory::{MemoryReservation, MemoryResource, MemoryTracker, MemoryUsage};
pub use mvcc::{ReadView, START_TX_ID, TS_INF, Ts, UNCOMMITTED};
pub use stats::{ColumnStats, HyperLogLog, TableStats};
pub use temporal::Interval;
pub use types::{
    ColumnId, ExtendDir, IntKind, InternalId, LogicalType, Offset, PhysicalType, RelMultiplicity,
    TableId,
};
pub use udf::{ScalarUdf, ScalarUdfCallback, ScalarUdfNullPolicy};
pub use value::{NodeValue, RecursiveRelValue, RelValue, Value};
pub use vector::{
    ColumnData, DataChunk, NullMask, Selection, VECTOR_CAPACITY, ValueVector, value_payload_bytes,
};
