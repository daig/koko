use crate::{binder, config::SessionConfig};
use koko_catalog::Catalog;
use koko_common::{LogicalType, Result, Value};
use koko_ir::bound::{BoundExpr, BoundStatement};
use koko_parser::ast;
use std::collections::HashMap;

pub use crate::binder::PreparedBinding;

/// Bind a parsed statement against the catalog and supplied parameter values.
pub fn bind_statement(
    catalog: &Catalog,
    statement: &ast::Statement,
    parameters: &HashMap<String, Value>,
    config: &SessionConfig,
) -> Result<BoundStatement> {
    binder::bind_statement(catalog, statement, parameters, config)
}

/// Bind a statement with symbolic parameters and return inferred parameter types.
pub fn bind_statement_for_prepare(
    catalog: &Catalog,
    statement: &ast::Statement,
    parameters: &[String],
    initial_types: &HashMap<String, LogicalType>,
    config: &SessionConfig,
) -> Result<PreparedBinding> {
    binder::bind_statement_for_prepare(catalog, statement, parameters, initial_types, config)
}

/// Bind and coerce one runtime-setting value to its declared type.
pub fn bind_config_value(
    catalog: &Catalog,
    expression: &ast::Expr,
    config: &SessionConfig,
    destination: &LogicalType,
) -> Result<BoundExpr> {
    binder::bind_config_value(catalog, expression, config, destination)
}
