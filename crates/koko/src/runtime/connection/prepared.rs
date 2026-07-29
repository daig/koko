//! Prepared-statement metadata, invalidation, and execution delegation.

use super::execution::{normalize_query_parameters, statement_writes};
use super::{Connection, ConnectionState};
use crate::execution::Parameter;
use crate::macros::{self, MacroRegistry};
use crate::prepared::{ParameterInfo, PreparedStatement, StatementKind};
use crate::result::Column;
use crate::runtime::context::binder_config_from_settings;
use crate::runtime::graph::{GraphData, GraphId};
use crate::{Error, LogicalType, QueryResult, Result, Value};
use koko_binder::config::SessionConfig;
use koko_catalog::Catalog;
use koko_common::Ts;
use koko_ir::bound::BoundStatement;
use koko_parser::parse_statement;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreparedCatalogToken {
    graph_id: GraphId,
    catalog_version: u64,
    scalar_udf_generation: u64,
    transaction_read_ts: Option<Ts>,
    transaction_writer: Option<Ts>,
    transaction_catalog_epoch: u64,
}

impl PreparedCatalogToken {
    fn current(
        graph_id: GraphId,
        graph_data: &GraphData,
        connection: &ConnectionState,
        scalar_udf_generation: u64,
    ) -> Self {
        let transaction = connection.txn.as_ref();
        Self {
            graph_id,
            catalog_version: transaction
                .map_or(graph_data.catalog_version, |txn| txn.catalog_version),
            scalar_udf_generation,
            transaction_read_ts: transaction.map(|txn| txn.read_ts),
            transaction_writer: transaction.and_then(|txn| txn.writer_id),
            transaction_catalog_epoch: transaction.map_or(0, |txn| txn.catalog_epoch),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Metadata {
    catalog_token: PreparedCatalogToken,
    parameters: Vec<ParameterInfo>,
    columns: Vec<Column>,
}

impl PreparedStatement<'_> {
    pub fn parameters(&self) -> &[ParameterInfo] {
        &self.metadata.parameters
    }

    pub fn columns(&self) -> &[Column] {
        &self.metadata.columns
    }

    pub fn kind(&self) -> StatementKind {
        statement_kind(&self.stmt)
    }

    pub fn is_read_only(&self) -> bool {
        !statement_writes(&self.stmt)
    }

    pub fn execute(&mut self) -> Result<QueryResult> {
        self.execute_with(std::iter::empty())
    }

    /// Execute with one owned parameter set. Metadata is rebound first,
    /// including after transactional or committed catalog changes.
    pub fn execute_with(
        &mut self,
        parameters: impl IntoIterator<Item = Parameter>,
    ) -> Result<QueryResult> {
        let metadata_started = Instant::now();
        let values = normalize_query_parameters(parameters)?;
        let (catalog_token, parameter_info, columns) =
            self.conn
                .prepared_metadata(&self.stmt, &self.parameter_names, &values)?;
        self.metadata.catalog_token = catalog_token;
        self.metadata.parameters = parameter_info;
        self.metadata.columns = columns;
        self.conn
            .execute_parsed(&self.stmt, values, metadata_started.elapsed())
    }
}

impl std::fmt::Debug for PreparedStatement<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedStatement")
            .field("stmt", &self.stmt)
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

/// Collect distinct `$name` parameters in lexical encounter order.
fn statement_parameter_names(cypher: &str) -> Result<Vec<String>> {
    use koko_parser::lexer::Tok;

    let tokens = koko_parser::lexer::tokenize(cypher)?;
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    for pair in tokens.windows(2) {
        let new_name = match pair {
            [Tok::Dollar, Tok::Ident(name)] => Some(name),
            _ => None,
        }
        .filter(|name| seen.insert((*name).clone()));
        if let Some(name) = new_name {
            names.push(name.clone());
        }
    }
    Ok(names)
}

fn statement_kind(stmt: &koko_parser::ast::Statement) -> StatementKind {
    use koko_parser::ast::Statement;
    match stmt {
        Statement::Query(_) => StatementKind::Query,
        Statement::CreateGraph(_) => StatementKind::CreateGraph,
        Statement::UseGraph { .. } => StatementKind::UseGraph,
        Statement::DropGraph { .. } => StatementKind::DropGraph,
        Statement::CreateIndex(_) => StatementKind::CreateIndex,
        Statement::DropIndex(_) => StatementKind::DropIndex,
        Statement::CreateNodeTable(_) => StatementKind::CreateNodeTable,
        Statement::CreateRelTable(_) => StatementKind::CreateRelTable,
        Statement::CreateTableAs(_) => StatementKind::CreateTableAs,
        Statement::DropTable(_) => StatementKind::DropTable,
        Statement::Alter(_) => StatementKind::AlterTable,
        Statement::CreateSequence(_) => StatementKind::CreateSequence,
        Statement::DropSequence(_) => StatementKind::DropSequence,
        Statement::Comment(_) => StatementKind::Comment,
        Statement::CreateType(_) => StatementKind::CreateType,
        Statement::Copy(_) => StatementKind::Copy,
        Statement::CopyTo(_) => StatementKind::CopyTo,
        Statement::ExportDatabase(_) => StatementKind::ExportDatabase,
        Statement::ImportDatabase(_) => StatementKind::ImportDatabase,
        Statement::CreateMacro(_) => StatementKind::CreateMacro,
        Statement::DropMacro { .. } => StatementKind::DropMacro,
        Statement::Call(_) => StatementKind::Call,
        Statement::Transaction(_) => StatementKind::Transaction,
        Statement::Explain { profile: true, .. } => StatementKind::Profile,
        Statement::Explain { profile: false, .. } => StatementKind::Explain,
    }
}

fn prepared_bind_metadata(
    catalog: &Catalog,
    macros: &MacroRegistry,
    stmt: &koko_parser::ast::Statement,
    parameter_names: &[String],
    initial_types: &HashMap<String, LogicalType>,
    config: &SessionConfig,
) -> Result<(HashMap<String, LogicalType>, Vec<Column>)> {
    use koko_parser::ast::{CallStmt, Statement};

    let unconstrained_parameters = || {
        parameter_names
            .iter()
            .map(|name| (name.clone(), LogicalType::Any))
            .collect()
    };
    match stmt {
        Statement::Explain { inner, profile } => {
            let (parameters, mut columns) = prepared_bind_metadata(
                catalog,
                macros,
                inner,
                parameter_names,
                initial_types,
                config,
            )?;
            if !profile {
                columns.clear();
            }
            return Ok((parameters, columns));
        }
        Statement::Transaction(_) | Statement::Call(CallStmt::SetConfig { .. }) => {
            return Ok((unconstrained_parameters(), Vec::new()));
        }
        Statement::Call(CallStmt::TableFunc {
            func,
            arg,
            extra_args,
            ..
        }) => {
            let columns = koko_binder::table_func_schema(
                catalog,
                koko_binder::bound_table_func(*func),
                arg.as_deref(),
                extra_args,
            )?
            .into_iter()
            .map(|(name, logical_type)| Column::new(name, logical_type))
            .collect();
            return Ok((unconstrained_parameters(), columns));
        }
        Statement::CreateMacro(_)
        | Statement::DropMacro { .. }
        | Statement::CreateGraph(_)
        | Statement::UseGraph { .. }
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::DropGraph { .. } => {
            return Ok((
                unconstrained_parameters(),
                vec![Column::new("result".to_string(), LogicalType::String)],
            ));
        }
        _ => {}
    }

    let expanded;
    let stmt = if macros.is_empty() {
        stmt
    } else {
        expanded = macros::expand_statement(stmt, macros)?;
        &expanded
    };
    let prepared = koko_binder::bind_statement_for_prepare(
        catalog,
        stmt,
        parameter_names,
        initial_types,
        config,
    )?;
    let columns = match prepared.statement {
        BoundStatement::Query(query) => query.result_columns().to_vec(),
        BoundStatement::CopyTo(copy) => copy.columns,
        _ => vec![("result".to_string(), LogicalType::String)],
    }
    .into_iter()
    .map(|(name, logical_type)| Column::new(name, logical_type))
    .collect();
    Ok((prepared.parameter_types, columns))
}

impl Connection {
    /// Parse and bind a statement for repeated execution.
    pub fn prepare(&self, cypher: &str) -> Result<PreparedStatement<'_>> {
        self.prepare_with(cypher, std::iter::empty())
    }

    /// Prepare with initial parameter values so parameter-dependent result
    /// types are known immediately. Values are not retained for execution.
    pub fn prepare_with(
        &self,
        cypher: &str,
        parameters: impl IntoIterator<Item = Parameter>,
    ) -> Result<PreparedStatement<'_>> {
        let stmt = parse_statement(cypher)?;
        let parameter_names = statement_parameter_names(cypher)?;
        let values = normalize_query_parameters(parameters)?;
        let (catalog_token, parameters, columns) =
            self.prepared_metadata(&stmt, &parameter_names, &values)?;
        Ok(PreparedStatement {
            conn: self,
            stmt,
            parameter_names,
            metadata: Metadata {
                catalog_token,
                parameters,
                columns,
            },
        })
    }

    fn prepared_metadata(
        &self,
        stmt: &koko_parser::ast::Statement,
        parameter_names: &[String],
        values: &HashMap<String, Value>,
    ) -> Result<(PreparedCatalogToken, Vec<ParameterInfo>, Vec<Column>)> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (scalar_udfs, scalar_udf_generation) = self.scalar_udf_snapshot();
        if let Some(name) = values
            .keys()
            .find(|name| !parameter_names.iter().any(|expected| expected == *name))
        {
            return Err(Error::binder(format!(
                "Unexpected prepared-statement parameter ${name}."
            )));
        }
        let database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let graph = Self::selected_graph(&database, &mut connection)?;
        let committed;
        let graph_data = if let Some(transaction) = &connection.txn {
            &transaction.snapshot
        } else {
            committed = graph.snapshot();
            &committed
        };
        let mut config = binder_config_from_settings(&connection.settings);
        config.scalar_udfs = scalar_udfs;
        let initial_types = values
            .iter()
            .map(|(name, value)| (name.clone(), value.logical_type()))
            .collect();
        let (inferred_parameters, columns) = prepared_bind_metadata(
            &graph_data.catalog,
            &graph_data.macros,
            stmt,
            parameter_names,
            &initial_types,
            &config,
        )?;
        let parameters = parameter_names
            .iter()
            .map(|name| {
                let inferred = inferred_parameters
                    .get(name)
                    .cloned()
                    .unwrap_or(LogicalType::Any);
                ParameterInfo {
                    name: name.clone(),
                    logical_type: if inferred == LogicalType::Any {
                        values
                            .get(name)
                            .map(Value::logical_type)
                            .unwrap_or(LogicalType::Any)
                    } else {
                        inferred
                    },
                }
            })
            .collect();
        Ok((
            PreparedCatalogToken::current(graph.id, graph_data, &connection, scalar_udf_generation),
            parameters,
            columns,
        ))
    }
}
