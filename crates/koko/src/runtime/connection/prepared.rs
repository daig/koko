//! Prepared-statement metadata, invalidation, and execution delegation.

use super::execution::statement_writes;
use super::{Connection, ConnectionState};
use crate::macros::{self, MacroRegistry};
use crate::runtime::context::binder_config_from_settings;
use crate::runtime::graph::{GraphData, GraphId};
use crate::{ColumnSchema, Error, LogicalType, QueryResult, Result, Value};
use koko_binder::{BoundStatement, SessionConfig};
use koko_catalog::Catalog;
use koko_common::Ts;
use koko_parser::parse_statement;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// The syntactic kind of a prepared statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedStatementType {
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

/// One borrowed named query parameter with an optional declared type.
#[derive(Debug, Clone, Copy)]
pub struct QueryParameter<'a> {
    name: &'a str,
    value: &'a Value,
    declared_type: Option<&'a LogicalType>,
}

impl<'a> QueryParameter<'a> {
    pub const fn new(name: &'a str, value: &'a Value) -> Self {
        Self {
            name,
            value,
            declared_type: None,
        }
    }

    pub const fn typed(name: &'a str, value: &'a Value, declared_type: &'a LogicalType) -> Self {
        Self {
            name,
            value,
            declared_type: Some(declared_type),
        }
    }

    pub const fn name(&self) -> &'a str {
        self.name
    }

    pub const fn value(&self) -> &'a Value {
        self.value
    }

    pub const fn declared_type(&self) -> Option<&'a LogicalType> {
        self.declared_type
    }
}

/// One named query parameter and the type supplied when metadata was last bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterMetadata {
    name: String,
    logical_type: LogicalType,
}

impl ParameterMetadata {
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

/// Whether and how a prepared statement mutates database state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedWriteMetadata {
    statement_type: PreparedStatementType,
    read_only: bool,
}

impl PreparedWriteMetadata {
    #[cfg(test)]
    pub(crate) const fn new(statement_type: PreparedStatementType, read_only: bool) -> Self {
        Self {
            statement_type,
            read_only,
        }
    }

    pub fn statement_type(self) -> PreparedStatementType {
        self.statement_type
    }

    pub fn is_read_only(self) -> bool {
        self.read_only
    }

    pub fn writes(self) -> bool {
        !self.read_only
    }
}

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
struct PreparedMetadata {
    catalog_token: PreparedCatalogToken,
    parameters: Vec<ParameterMetadata>,
    result_schema: Vec<ColumnSchema>,
}

/// A parsed and initially bound Cypher statement, ready for repeated execution.
///
/// Execution re-binds against current parameter values. If catalog state changed
/// since the previous bind, metadata is rebuilt against the current catalog before
/// execution, so a dropped or reshaped dependency fails safely in the binder.
pub struct PreparedStatement<'conn> {
    conn: &'conn Connection,
    stmt: koko_parser::ast::Statement,
    parameter_names: Vec<String>,
    metadata: RefCell<PreparedMetadata>,
}

impl PreparedStatement<'_> {
    pub fn parameters(&self) -> Vec<ParameterMetadata> {
        self.metadata.borrow().parameters.clone()
    }

    pub fn result_schema(&self) -> Vec<ColumnSchema> {
        self.metadata.borrow().result_schema.clone()
    }

    pub fn statement_type(&self) -> PreparedStatementType {
        prepared_statement_type(&self.stmt)
    }

    pub fn is_read_only(&self) -> bool {
        !statement_writes(&self.stmt)
    }

    pub fn write_metadata(&self) -> PreparedWriteMetadata {
        PreparedWriteMetadata {
            statement_type: self.statement_type(),
            read_only: self.is_read_only(),
        }
    }

    /// Execute with the given `$name` parameter values. Metadata is rebound first,
    /// including after transactional or committed catalog changes.
    pub fn execute(&self, params: &[(&str, Value)]) -> Result<QueryResult> {
        let metadata_started = Instant::now();
        let (catalog_token, parameters, result_schema) =
            self.conn
                .prepared_metadata(&self.stmt, &self.parameter_names, params)?;
        {
            let mut metadata = self.metadata.borrow_mut();
            let _catalog_changed = metadata.catalog_token != catalog_token;
            metadata.catalog_token = catalog_token;
            metadata.parameters = parameters;
            metadata.result_schema = result_schema;
        }
        self.conn
            .execute_parsed(&self.stmt, params, metadata_started.elapsed())
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

fn prepared_statement_type(stmt: &koko_parser::ast::Statement) -> PreparedStatementType {
    use koko_parser::ast::Statement;
    match stmt {
        Statement::Query(_) => PreparedStatementType::Query,
        Statement::CreateGraph(_) => PreparedStatementType::CreateGraph,
        Statement::UseGraph { .. } => PreparedStatementType::UseGraph,
        Statement::DropGraph { .. } => PreparedStatementType::DropGraph,
        Statement::CreateIndex(_) => PreparedStatementType::CreateIndex,
        Statement::DropIndex(_) => PreparedStatementType::DropIndex,
        Statement::CreateNodeTable(_) => PreparedStatementType::CreateNodeTable,
        Statement::CreateRelTable(_) => PreparedStatementType::CreateRelTable,
        Statement::CreateTableAs(_) => PreparedStatementType::CreateTableAs,
        Statement::DropTable(_) => PreparedStatementType::DropTable,
        Statement::Alter(_) => PreparedStatementType::AlterTable,
        Statement::CreateSequence(_) => PreparedStatementType::CreateSequence,
        Statement::DropSequence(_) => PreparedStatementType::DropSequence,
        Statement::Comment(_) => PreparedStatementType::Comment,
        Statement::CreateType(_) => PreparedStatementType::CreateType,
        Statement::Copy(_) => PreparedStatementType::Copy,
        Statement::CopyTo(_) => PreparedStatementType::CopyTo,
        Statement::ExportDatabase(_) => PreparedStatementType::ExportDatabase,
        Statement::ImportDatabase(_) => PreparedStatementType::ImportDatabase,
        Statement::CreateMacro(_) => PreparedStatementType::CreateMacro,
        Statement::DropMacro { .. } => PreparedStatementType::DropMacro,
        Statement::Call(_) => PreparedStatementType::Call,
        Statement::Transaction(_) => PreparedStatementType::Transaction,
        Statement::Explain { profile: true, .. } => PreparedStatementType::Profile,
        Statement::Explain { profile: false, .. } => PreparedStatementType::Explain,
    }
}

fn prepared_bind_metadata(
    catalog: &Catalog,
    macros: &MacroRegistry,
    stmt: &koko_parser::ast::Statement,
    parameter_names: &[String],
    initial_types: &HashMap<String, LogicalType>,
    config: &SessionConfig,
) -> Result<(HashMap<String, LogicalType>, Vec<ColumnSchema>)> {
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
                koko_binder::BoundTableFunc::from(*func),
                arg.as_deref(),
                extra_args,
            )?
            .into_iter()
            .map(|(name, logical_type)| ColumnSchema::new(name, logical_type))
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
                vec![ColumnSchema::new("result".to_string(), LogicalType::String)],
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
        BoundStatement::Query(query) => query
            .operands
            .first()
            .map(koko_binder::BoundQuery::result_columns)
            .unwrap_or_default(),
        BoundStatement::CopyTo(copy) => copy.columns,
        _ => vec![("result".to_string(), LogicalType::String)],
    }
    .into_iter()
    .map(|(name, logical_type)| ColumnSchema::new(name, logical_type))
    .collect();
    Ok((prepared.parameter_types, columns))
}

impl Connection {
    /// Parse and bind a statement for repeated execution, publishing its parameter,
    /// result-schema, statement-kind, and read/write metadata.
    pub fn prepare(&self, cypher: &str) -> Result<PreparedStatement<'_>> {
        self.prepare_with_params(cypher, &[])
    }

    /// Prepare with initial parameter values so parameter-dependent result types are
    /// known immediately. Every execution still re-binds against its supplied values.
    pub fn prepare_with_params(
        &self,
        cypher: &str,
        params: &[(&str, Value)],
    ) -> Result<PreparedStatement<'_>> {
        let stmt = parse_statement(cypher)?;
        let parameter_names = statement_parameter_names(cypher)?;
        let (catalog_token, parameters, result_schema) =
            self.prepared_metadata(&stmt, &parameter_names, params)?;
        Ok(PreparedStatement {
            conn: self,
            stmt,
            parameter_names,
            metadata: RefCell::new(PreparedMetadata {
                catalog_token,
                parameters,
                result_schema,
            }),
        })
    }

    fn prepared_metadata(
        &self,
        stmt: &koko_parser::ast::Statement,
        parameter_names: &[String],
        params: &[(&str, Value)],
    ) -> Result<(
        PreparedCatalogToken,
        Vec<ParameterMetadata>,
        Vec<ColumnSchema>,
    )> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (scalar_udfs, scalar_udf_generation) = self.scalar_udf_snapshot();
        if let Some((name, _)) = params
            .iter()
            .find(|(name, _)| !parameter_names.iter().any(|expected| expected == name))
        {
            return Err(Error::binder(format!(
                "Unexpected prepared-statement parameter ${name}."
            )));
        }
        let values: HashMap<String, Value> = params
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
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
        let (inferred_parameters, result_schema) = prepared_bind_metadata(
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
                ParameterMetadata {
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
            result_schema,
        ))
    }
}
