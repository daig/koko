//! Bound semantic IR and physical plan data contracts.
//!
//! This crate owns only typed data passed between binding, planning, expression
//! compilation, and execution. It performs no parsing, catalog lookup, I/O,
//! optimization, storage access, or execution.

pub mod bound;
pub mod plan;
