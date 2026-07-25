//! `koko` — the public, idiomatic Rust API for the Koko engine.
//!
//! This crate composes the per-layer crates into the `Database` / `Connection`
//! / `QueryResult` surface. The result is **columnar** (the load-bearing model,
//! per the roadmap's no-regression bar): [`QueryResult`] stores column buffers,
//! and the owned-[`Row`] cursor is thin sugar over them. Results are
//! materialized eagerly (matching the C++ engine); lazy streaming is a
//! post-parity enhancement.
//!
//! ```no_run
//! use koko::Database;
//! # fn main() -> koko::Result<()> {
//! let db = Database::in_memory();
//! let conn = db.connect();
//! conn.query("CREATE NODE TABLE Person(name STRING, age INT64, PRIMARY KEY(name))")?;
//! conn.query("CREATE (:Person {name: 'Alice', age: 35})")?;
//! let r = conn.query("MATCH (p:Person) WHERE p.age > 30 RETURN p.name, p.age")?;
//! for row in r.rows() {
//!     println!("{} {}", row.get::<String>(0)?, row.get::<i64>(1)?);
//! }
//! # Ok(())
//! # }
//! ```

mod arrow;
mod config;
mod copy;
mod interchange;
mod result;
mod runtime;
mod tooling;

#[cfg(test)]
mod tests;

pub use config::DatabaseConfig;
pub use result::{
    CellRef, CellValueRef, ColumnSchema, ColumnView, FailureKind, FromValue, GraphValueType,
    InterruptReason, PlanNode, PlanPresentation, QueryResult, QueryResultKind, QuerySummary,
    ResultTypeContext, Row, StatementDiagnostics, StatementFailure, StatementOutcome,
    StatementWarning, TypedColumnView,
};
pub use runtime::{
    Connection, Database, InterruptHandle, ParameterMetadata, PreparedStatement,
    PreparedStatementType, PreparedWriteMetadata, QueryParameter, Transaction,
};
pub use tooling::{
    CatalogSnapshot, CursorContext, CursorContextKind, EndpointDescriptor, FunctionDescriptor,
    FunctionKind, GraphDescriptor, GraphIdentity, GraphKind, IndexDescriptor, MacroDescriptor,
    NodeTableDescriptor, OutputClass, PropertyDescriptor, RelationshipTableDescriptor,
    SessionSnapshot, SettingDescriptor, SourceSpan, StatementAnalysis, StatementClass,
    SyntaxAnalysis, SyntaxDiagnostic, SyntaxStatus, TokenKind, TokenSpan, TransactionMode,
    analyze_cypher, cypher_keywords, version,
};

mod macros;

// Re-export the foundational public types.
pub use koko_common::decimal::{format_decimal, parse_to_unscaled as parse_decimal};
pub use koko_common::scalar::format_uuid;
pub use koko_common::temporal::{
    format_date, format_interval, format_timestamp, parse_date, parse_timestamp,
};
pub use koko_common::{
    ColumnData, DataChunk, Error, IntKind, InternalId, Interval, JsonValue, LogicalType,
    MemoryResource, MemoryTracker, MemoryUsage, NodeValue, Offset, RecursiveRelValue, RelValue,
    Result, ScalarUdfNullPolicy, TableId, Value, ValueVector,
};
