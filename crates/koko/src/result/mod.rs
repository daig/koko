//! Eager materialized query results and borrowed typed traversal.

mod cell;
mod plan;
mod tabular;

pub use cell::{Cell, CellValue, FromValue};
pub use plan::{GraphValueType, PlanNode, PlanPresentation, ResultTypeContext};
pub(crate) use plan::{capture_result_type_context, plan_presentation};
pub use tabular::{Cells, ColumnIndex, ColumnView, QueryResult, Row, Rows, TypedColumnView};

use koko_common::LogicalType;
use std::time::Duration;

/// Timings for one successful statement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuerySummary {
    compilation_time: Duration,
    execution_time: Duration,
}

impl QuerySummary {
    pub const fn compilation_time(&self) -> Duration {
        self.compilation_time
    }

    pub const fn execution_time(&self) -> Duration {
        self.execution_time
    }
}

/// One output column's exact bound name and logical type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    name: String,
    logical_type: LogicalType,
}

impl Column {
    pub fn new(name: impl Into<String>, logical_type: LogicalType) -> Self {
        Self {
            name: name.into(),
            logical_type,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn logical_type(&self) -> &LogicalType {
        &self.logical_type
    }
}

/// Structural result family.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultKind {
    Rows,
    #[default]
    Status,
    Explain,
    Profile,
}
