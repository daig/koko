//! Prepared statement handles and metadata.

use crate::{Connection, LogicalType};

/// The syntactic kind of a prepared statement.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Query,
    CreateGraph,
    UseGraph,
    DropGraph,
    CreateIndex,
    DropIndex,
    CreateNodeTable,
    CreateRelTable,
    CreateTableAs,
    DropTable,
    AlterTable,
    CreateSequence,
    DropSequence,
    Comment,
    CreateType,
    Copy,
    CopyTo,
    ExportDatabase,
    ImportDatabase,
    CreateMacro,
    DropMacro,
    Call,
    Transaction,
    Explain,
    Profile,
}

/// One named query parameter and the type supplied when metadata was last bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterInfo {
    pub(crate) name: String,
    pub(crate) logical_type: LogicalType,
}

impl ParameterInfo {
    #[cfg(test)]
    pub(crate) fn new(name: String, logical_type: LogicalType) -> Self {
        Self { name, logical_type }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn logical_type(&self) -> &LogicalType {
        &self.logical_type
    }
}

/// A parsed and initially bound Cypher statement, ready for repeated execution.
///
/// Execution re-binds against current parameter values. If catalog state changed
/// since the previous bind, metadata is rebuilt against the current catalog before
/// execution, so a dropped or reshaped dependency fails safely in the binder.
pub struct PreparedStatement<'conn> {
    pub(crate) conn: &'conn Connection,
    pub(crate) stmt: koko_parser::ast::Statement,
    pub(crate) parameter_names: Vec<String>,
    pub(crate) metadata: crate::runtime::PreparedStatementMetadata,
}
