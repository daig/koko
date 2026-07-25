//! Connection identity, session state, serialization, and public connection API.

use super::context::{
    ConcurrentQuerySnapshot, QueryContext, READ_ONLY_WRITE_MSG, StatementControl,
    binder_config_from_settings, mvcc_view, mvcc_write,
};
use super::database::DatabaseState;
use super::graph::{GraphId, GraphRegistry, GraphState, MAIN_GRAPH_ID};
use crate::copy::{CopyOperationContext, run_copy};
use crate::interchange;
use crate::result;
use crate::{
    Error, FailureKind, InterruptReason, LogicalType, QueryResult, Result, ScalarUdfNullPolicy,
    StatementOutcome, Value, analyze_cypher,
};
use koko_binder::{BoundStatement, bind_statement};
use koko_common::ReadView;
use koko_parser::parse_statement;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const SINGLE_WRITER_MSG: &str = "Cannot start a new write transaction in the system. Only one \
                                 write transaction at a time is allowed in the system.";
pub(crate) const ACTIVE_TRANSACTION_MSG: &str = "Connection already has an active transaction. Cannot start \
                                      a transaction within another one. For concurrent multiple \
                                      transactions, please open other connections.";

fn scalar_function_name_is_reserved(name: &str) -> bool {
    koko_function::is_scalar(name)
        || koko_function::AggOp::from_name(name).is_some()
        || matches!(
            name.to_ascii_lowercase().as_str(),
            "cast" | "nextval" | "currval" | "cost"
        )
}

mod execution;
mod observation;

use execution::{
    apply_index_statement, apply_interchange_image, attach_query_summary,
    normalize_query_parameters, refresh_execution_time, run_regular_on_snapshot,
    statement_touches_catalog, statement_writes,
};

mod prepared;

mod transaction;

pub use transaction::Transaction;
use transaction::TxnState;

pub use prepared::{
    ParameterMetadata, PreparedStatement, PreparedStatementType, PreparedWriteMetadata,
    QueryParameter,
};

/// A per-`Database` connection identifier (used for the default single-writer slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ConnId(pub(super) u64);
/// Cloneable, lock-free cancellation handle for one connection.
///
/// Interrupts advance an epoch. A statement captures the epoch at start, so an
/// interrupt affects only statements already running and never poisons the next.
#[derive(Debug, Clone)]
pub struct InterruptHandle {
    epoch: Arc<AtomicU64>,
}

impl InterruptHandle {
    pub fn interrupt(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct ScalarUdfRegistry {
    entries: Arc<HashMap<String, Arc<koko_common::ScalarUdf>>>,
    generation: u64,
}

/// Mutable semantic state owned by one connection.
struct ConnectionState {
    txn: Option<TxnState>,
    revision: u64,
    selected_graph: GraphId,
    selected_graph_name: String,
    settings: koko_common::settings::SessionSettings,
    warnings: koko_common::warnings::WarningRegistry,
    random: koko_function::oracle_hash::RandomState,
    next_query_id: u64,
}

impl ConnectionState {
    fn new(max_workers: Option<usize>) -> Self {
        let mut settings = koko_common::settings::SessionSettings::with_environment_defaults();
        let constrained_workers = max_workers.filter(|max_workers| {
            settings
                .current("threads")
                .as_int128()
                .is_some_and(|workers| workers > *max_workers as i128)
        });
        if let Some(max_workers) = constrained_workers {
            settings.set("threads", Value::Int64(max_workers as i64));
        }
        Self {
            txn: None,
            revision: 0,
            selected_graph: MAIN_GRAPH_ID,
            selected_graph_name: "main".to_string(),
            settings,
            warnings: koko_common::warnings::WarningRegistry::default(),
            random: koko_function::oracle_hash::RandomState::default(),
            next_query_id: 0,
        }
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }

    fn query_context(
        &mut self,
        parameters: HashMap<String, Value>,
        view: ReadView,
        scalar_udfs: Arc<HashMap<String, Arc<koko_common::ScalarUdf>>>,
        control: &StatementControl,
    ) -> QueryContext {
        let query_id = self.next_query_id;
        self.next_query_id = self.next_query_id.saturating_add(1);
        let warning_limit = self
            .settings
            .get("warning_limit")
            .and_then(Value::as_int128)
            .unwrap_or(u64::MAX as i128)
            .clamp(0, u64::MAX as i128) as u64;
        let deadline = self
            .settings
            .current("timeout")
            .as_int128()
            .filter(|&milliseconds| milliseconds > 0)
            .and_then(|milliseconds| {
                control.started.checked_add(Duration::from_millis(
                    milliseconds.min(u64::MAX as i128) as u64,
                ))
            });
        QueryContext {
            parameters,
            view,
            settings: self.settings.clone(),
            base_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            warnings: self.warnings.sink(query_id, warning_limit),
            scalar_udfs,
            random: self.random.clone(),
            interrupt_epoch: Arc::clone(&control.interrupt_epoch),
            captured_epoch: control.captured_epoch,
            deadline,
            setting_update: None,
            compilation_time: Duration::ZERO,
            show_table_rows: None,
        }
    }
}

/// A connection through which Cypher statements are issued. Each connection holds
/// its own transaction and session state over shared committed database state.
pub struct Connection {
    id: ConnId,
    inner: Arc<Mutex<DatabaseState>>,
    schema_gate: Arc<RwLock<()>>,
    interrupt_epoch: Arc<AtomicU64>,
    /// Serializes calls issued through one connection. Different connections do
    /// not share this mutex and can execute concurrently.
    execution: Mutex<()>,
    scalar_udfs: RwLock<ScalarUdfRegistry>,
    state: Mutex<ConnectionState>,
}

impl Connection {
    pub(super) fn new(
        id: ConnId,
        inner: Arc<Mutex<DatabaseState>>,
        schema_gate: Arc<RwLock<()>>,
        max_workers: Option<usize>,
    ) -> Self {
        Self {
            id,
            inner,
            schema_gate,
            interrupt_epoch: Arc::new(AtomicU64::new(0)),
            execution: Mutex::new(()),
            scalar_udfs: RwLock::new(ScalarUdfRegistry::default()),
            state: Mutex::new(ConnectionState::new(max_workers)),
        }
    }

    /// Parse, bind, plan, and execute a single Cypher statement.
    pub fn query(&self, cypher: &str) -> Result<QueryResult> {
        self.query_with_params(cypher, &[])
    }
    /// Register one safe Rust scalar function on this connection.
    ///
    /// Names are case-insensitive. Registration never replaces a builtin,
    /// macro, or existing connection-local function. Queries and prepared
    /// statements bind an immutable `Arc` snapshot of the resolved callback.
    pub fn register_scalar_function<F>(
        &self,
        name: impl Into<String>,
        parameter_types: Vec<LogicalType>,
        result_type: LogicalType,
        null_policy: ScalarUdfNullPolicy,
        callback: F,
    ) -> Result<()>
    where
        F: Fn(&[Value]) -> Result<Value> + Send + Sync + 'static,
    {
        let name = name.into();
        let mut chars = name.chars();
        if !chars
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            return Err(Error::configuration(format!(
                "Invalid scalar function name: {name}."
            )));
        }
        if scalar_function_name_is_reserved(&name) {
            return Err(Error::catalog(format!(
                "Scalar function {name} collides with a built-in function."
            )));
        }

        let _schema = self
            .schema_gate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if database.graphs.by_id.values().any(|graph| {
            graph
                .snapshot()
                .macros
                .contains_key(&name.to_ascii_uppercase())
        }) {
            return Err(Error::catalog(format!(
                "Scalar function {name} collides with an existing macro."
            )));
        }
        drop(database);
        let key = name.to_ascii_lowercase();
        let mut registry = self
            .scalar_udfs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if registry.entries.contains_key(&key) {
            return Err(Error::catalog(format!(
                "Scalar function {name} is already registered on this connection."
            )));
        }
        Arc::make_mut(&mut registry.entries).insert(
            key,
            Arc::new(koko_common::ScalarUdf::new(
                name,
                parameter_types,
                result_type,
                null_policy,
                callback,
            )),
        );
        registry.generation = registry.generation.saturating_add(1);
        Ok(())
    }

    /// Remove a connection-local scalar function. Returns whether one existed.
    pub fn remove_scalar_function(&self, name: &str) -> Result<bool> {
        let key = name.to_ascii_lowercase();
        let mut registry = self
            .scalar_udfs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removed = Arc::make_mut(&mut registry.entries).remove(&key).is_some();
        if removed {
            registry.generation = registry.generation.saturating_add(1);
        }
        Ok(removed)
    }

    fn scalar_udf_snapshot(&self) -> (Arc<HashMap<String, Arc<koko_common::ScalarUdf>>>, u64) {
        let registry = self
            .scalar_udfs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (Arc::clone(&registry.entries), registry.generation)
    }

    /// Obtain a cloneable handle that can interrupt this connection's currently
    /// running statement without waiting for the connection execution mutex.
    pub fn interrupt_handle(&self) -> InterruptHandle {
        InterruptHandle {
            epoch: Arc::clone(&self.interrupt_epoch),
        }
    }

    /// Interrupt the statement currently running on this connection.
    ///
    /// The request is lock-free and does not affect a statement started after
    /// this call. Use [`Self::interrupt_handle`] when another thread does not own
    /// the connection itself.
    pub fn interrupt(&self) {
        self.interrupt_epoch.fetch_add(1, Ordering::AcqRel);
    }

    /// Set this connection's per-statement timeout in milliseconds.
    ///
    /// Zero disables the deadline. Positive values apply independently to each
    /// subsequent statement and match the `CALL timeout=...` session setting.
    pub fn set_query_timeout_ms(&self, timeout_ms: u64) -> Result<()> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = i64::try_from(timeout_ms)
            .map(Value::Int64)
            .unwrap_or_else(|_| Value::UInt128(timeout_ms as u128));
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.settings.set("timeout", value);
        state.bump_revision();
        Ok(())
    }

    /// Set this connection's per-statement timeout.
    ///
    /// Timeout resolution is one millisecond; a positive sub-millisecond
    /// duration rounds up. Use [`Self::clear_query_timeout`] to disable it.
    pub fn set_query_timeout(&self, timeout: Duration) -> Result<()> {
        if timeout.is_zero() {
            return Err(Error::configuration(
                "Connection query timeout must be greater than zero.",
            ));
        }
        let milliseconds = timeout.as_millis().max(1).min(u64::MAX as u128) as u64;
        self.set_query_timeout_ms(milliseconds)
    }

    /// Disable this connection's per-statement timeout.
    pub fn clear_query_timeout(&self) -> Result<()> {
        self.set_query_timeout_ms(0)
    }

    /// Set this connection's maximum execution worker count without issuing a
    /// Cypher statement. This mirrors the native client configuration path.
    pub fn set_max_num_threads(&self, threads: usize) -> Result<()> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if threads == 0 {
            return Err(Error::configuration(
                "Connection thread count must be greater than zero.",
            ));
        }
        let max_workers = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .config
            .max_workers();
        if max_workers.is_some_and(|limit| threads > limit) {
            return Err(Error::configuration(format!(
                "Connection thread count {threads} exceeds database max_workers {}.",
                max_workers.expect("checked above")
            )));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.settings.set("threads", Value::Int64(threads as i64));
        state.bump_revision();
        Ok(())
    }
    /// Like [`query`](Self::query), with values for the statement's `$name`
    /// parameters. An unbound parameter follows C++ semantics: its containing expression becomes
    /// `NULL`, and a predicate containing it is omitted.
    pub fn query_with_params(&self, cypher: &str, params: &[(&str, Value)]) -> Result<QueryResult> {
        let parse_started = Instant::now();
        let stmt = parse_statement(cypher)?;
        self.execute_parsed(&stmt, params, parse_started.elapsed())
    }

    /// Execute with typed parameters without interpolating values into Cypher source.
    pub fn query_with_typed_params(
        &self,
        cypher: &str,
        params: &[QueryParameter<'_>],
    ) -> Result<QueryResult> {
        let parse_started = Instant::now();
        let stmt = parse_statement(cypher)?;
        let normalized = normalize_query_parameters(params)?;
        let borrowed = normalized
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect::<Vec<_>>();
        self.execute_parsed(&stmt, &borrowed, parse_started.elapsed())
    }

    /// Execute one statement while preserving structural result, diagnostic,
    /// interruption, session, and internal-panic metadata.
    pub fn execute_with_metadata(
        &self,
        cypher: &str,
        params: &[QueryParameter<'_>],
    ) -> StatementOutcome {
        let session_before = self.session_snapshot().ok();
        let parse_started = Instant::now();
        let statement = match parse_statement(cypher) {
            Ok(statement) => statement,
            Err(error) => {
                let diagnostic = analyze_cypher(cypher, None).diagnostic().cloned();
                return result::failure_outcome(
                    error,
                    FailureKind::Parser,
                    None,
                    diagnostic,
                    session_before,
                    self.session_snapshot().ok(),
                );
            }
        };
        let normalized = match normalize_query_parameters(params) {
            Ok(normalized) => normalized,
            Err(error) => {
                let kind = result::failure_kind(&error);
                return result::failure_outcome(
                    error,
                    kind,
                    None,
                    None,
                    session_before,
                    self.session_snapshot().ok(),
                );
            }
        };
        let borrowed = normalized
            .iter()
            .map(|(name, value)| (name.as_str(), value.clone()))
            .collect::<Vec<_>>();
        let interrupt_epoch = self.interrupt_epoch.load(Ordering::Acquire);
        let timeout_enabled = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .settings
            .get("timeout")
            .and_then(Value::as_i64)
            .is_some_and(|timeout| timeout > 0);
        match self.execute_parsed_catching(&statement, &borrowed, parse_started.elapsed()) {
            Ok(Ok(result)) => {
                result::success_outcome(result, session_before, self.session_snapshot().ok())
            }
            Ok(Err(error)) => {
                let kind = result::failure_kind(&error);
                let interrupt_reason = (kind == FailureKind::Interrupt).then(|| {
                    if self.interrupt_epoch.load(Ordering::Acquire) != interrupt_epoch {
                        InterruptReason::Explicit
                    } else if timeout_enabled {
                        InterruptReason::Deadline
                    } else {
                        InterruptReason::Explicit
                    }
                });
                result::failure_outcome(
                    error,
                    kind,
                    interrupt_reason,
                    None,
                    session_before,
                    self.session_snapshot().ok(),
                )
            }
            Err(detail) => result::failure_outcome(
                Error::runtime(format!("Query execution panicked: {detail}")),
                FailureKind::InternalPanic,
                None,
                None,
                session_before,
                self.session_snapshot().ok(),
            ),
        }
    }

    fn selected_graph(
        database: &DatabaseState,
        connection: &mut ConnectionState,
    ) -> Result<Arc<GraphState>> {
        if let Some(transaction) = &connection.txn {
            return Ok(Arc::clone(&transaction.graph));
        }
        if let Some(graph) = database.graphs.graph(connection.selected_graph) {
            return Ok(graph);
        }
        let missing = std::mem::replace(&mut connection.selected_graph_name, "main".to_string());
        connection.selected_graph = MAIN_GRAPH_ID;
        connection.bump_revision();
        Err(Error::binder(format!("No graph named {missing}.")))
    }

    fn release_writer(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .release_writer(self.id);
    }

    /// Auto-commit regular-query path. The global coordinator is held only to
    /// capture snapshots and acquire/release a writer lease; bind, plan, scan,
    /// mutation, and materialization run after it is released.
    fn execute_regular_autocommit(
        &self,
        stmt: &koko_parser::ast::Statement,
        parameters: HashMap<String, Value>,
        initial_compilation_time: Duration,
        execution_started: Instant,
        connection: &mut ConnectionState,
        control: &StatementControl,
    ) -> Result<QueryResult> {
        let _schema = self
            .schema_gate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let writes = statement_writes(stmt);
        let snapshot = {
            let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let graph = Self::selected_graph(&database, connection)?;
            let show_table_rows = database.show_table_rows(None);
            let graph_data = graph.snapshot();
            let storage = Arc::clone(&graph_data.storage);
            let read_ts = storage.current_commit_ts();
            if writes {
                let writer_id = self.acquire_writer(&mut database, &graph)?;
                ConcurrentQuerySnapshot {
                    graph,
                    catalog: Arc::clone(&graph_data.catalog),
                    catalog_version: graph_data.catalog_version,
                    macros: Arc::clone(&graph_data.macros),
                    storage,
                    config: graph_data.config.clone(),
                    memory: graph_data.memory.clone(),
                    read_ts,
                    writer_id: Some(writer_id),
                    mark: graph_data.storage.undo_mark(),
                    rel_base: graph_data.rel_table_bases(),
                    sequence_before: Some(graph_data.catalog.sequence_state()),
                    show_table_rows,
                }
            } else {
                ConcurrentQuerySnapshot {
                    graph,
                    catalog: Arc::clone(&graph_data.catalog),
                    catalog_version: graph_data.catalog_version,
                    macros: Arc::clone(&graph_data.macros),
                    storage,
                    config: graph_data.config.clone(),
                    memory: graph_data.memory.clone(),
                    read_ts,
                    writer_id: None,
                    mark: 0,
                    rel_base: HashMap::new(),
                    sequence_before: None,
                    show_table_rows,
                }
            }
        };

        let mut query = connection.query_context(
            parameters,
            mvcc_view(snapshot.read_ts, snapshot.writer_id),
            self.scalar_udf_snapshot().0,
            control,
        );
        query.compilation_time = initial_compilation_time;
        query.show_table_rows = Some(snapshot.show_table_rows.clone());
        let result = run_regular_on_snapshot(
            stmt,
            &snapshot.catalog,
            snapshot.catalog_version,
            &snapshot.macros,
            &snapshot.storage,
            &snapshot.memory,
            &snapshot.config,
            &snapshot.rel_base,
            &mut query,
        );
        let mut result = attach_query_summary(
            result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
            &snapshot.memory,
        );

        if let Some(writer_id) = snapshot.writer_id {
            let write = mvcc_write(snapshot.read_ts, writer_id);
            if result.is_ok() {
                snapshot.storage.commit_to(write, snapshot.mark);
            } else {
                snapshot.storage.rollback_to(write, snapshot.mark);
            }
            let sequence_changed = snapshot
                .sequence_before
                .as_ref()
                .is_some_and(|before| *before != snapshot.catalog.sequence_state());
            let mut graph_data = snapshot
                .graph
                .data
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if result.is_ok() && sequence_changed {
                graph_data.catalog_version = graph_data.catalog_version.saturating_add(1);
            }
            graph_data.release_catalog_writes(writer_id);
            drop(graph_data);
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active_writers
                .remove(&self.id);
        }
        if let Some((key, value)) = result
            .is_ok()
            .then(|| query.setting_update.take())
            .flatten()
        {
            connection.settings.set(&key, value);
            connection.bump_revision();
        }
        refresh_execution_time(
            &mut result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
        );
        result
    }

    fn statement_control(&self) -> StatementControl {
        StatementControl {
            interrupt_epoch: Arc::clone(&self.interrupt_epoch),
            captured_epoch: self.interrupt_epoch.load(Ordering::Acquire),
            started: Instant::now(),
        }
    }

    fn execute_parsed(
        &self,
        stmt: &koko_parser::ast::Statement,
        params: &[(&str, Value)],
        initial_compilation_time: Duration,
    ) -> Result<QueryResult> {
        match self.execute_parsed_catching(stmt, params, initial_compilation_time) {
            Ok(result) => result,
            Err(detail) => Err(Error::runtime(format!(
                "Query execution panicked: {detail}"
            ))),
        }
    }

    fn execute_parsed_catching(
        &self,
        stmt: &koko_parser::ast::Statement,
        params: &[(&str, Value)],
        initial_compilation_time: Duration,
    ) -> std::result::Result<Result<QueryResult>, String> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let control = self.statement_control();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.execute_parsed_inner(stmt, params, None, initial_compilation_time, &control)
        })) {
            Ok(result) => Ok(result),
            Err(payload) => {
                self.recover_after_panic();
                let detail = payload
                    .downcast_ref::<&str>()
                    .map(|detail| (*detail).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                Err(detail)
            }
        }
    }

    fn execute_graph_statement(&self, stmt: &koko_parser::ast::Statement) -> Result<QueryResult> {
        use koko_parser::ast::Statement;

        let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match stmt {
            Statement::CreateGraph(create) => {
                if database.graphs.id(&create.name).is_some() {
                    if create.if_not_exists {
                        return Ok(QueryResult::message(
                            "Created graph successfully.".to_string(),
                        ));
                    }
                    return Err(Error::runtime(format!(
                        "Graph {} already exists.",
                        create.name
                    )));
                }
                let graph = database.allocate_graph(&create.name, create.kind);
                Arc::make_mut(&mut database.graphs).insert(graph);
                Ok(QueryResult::message(
                    "Created graph successfully.".to_string(),
                ))
            }
            Statement::UseGraph { name } => {
                if connection.txn.is_some() {
                    return Err(Error::transaction(
                        "Cannot switch graphs while a transaction is active.",
                    ));
                }
                let Some(id) = database.graphs.id(name) else {
                    return Err(Error::binder(format!("No graph named {name}.")));
                };
                connection.selected_graph = id;
                connection.selected_graph_name.clone_from(name);
                connection.bump_revision();
                Ok(QueryResult::message("Used graph successfully.".to_string()))
            }
            Statement::DropGraph { name, if_exists } => {
                let Some(id) = database.graphs.id(name) else {
                    if *if_exists {
                        return Ok(QueryResult::default());
                    }
                    return Err(Error::binder(format!("Graph {name} does not exist.")));
                };
                if id == MAIN_GRAPH_ID {
                    return Err(Error::binder("Cannot drop the main graph."));
                }
                Arc::make_mut(&mut database.graphs).remove(id);
                if connection.selected_graph == id {
                    connection.selected_graph = MAIN_GRAPH_ID;
                    connection.selected_graph_name = "main".to_string();
                    connection.bump_revision();
                }
                Ok(QueryResult::message(format!(
                    "Graph {name} has been dropped."
                )))
            }
            _ => unreachable!("graph statement dispatcher"),
        }
    }

    fn execute_index_statement(&self, stmt: &koko_parser::ast::Statement) -> Result<QueryResult> {
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if connection.txn.is_some() {
            let writer_id = connection
                .txn
                .as_ref()
                .and_then(|transaction| transaction.writer_id)
                .ok_or_else(|| Error::transaction(READ_ONLY_WRITE_MSG))?;
            let result = {
                let transaction = connection.txn.as_mut().expect("checked above");
                apply_index_statement(Arc::make_mut(&mut transaction.snapshot.catalog), stmt)
            };
            match result {
                Ok(message) => {
                    let transaction = connection.txn.as_mut().expect("active transaction");
                    transaction.catalog_dirty = true;
                    transaction.catalog_epoch = transaction.catalog_epoch.saturating_add(1);
                    connection.bump_revision();
                    return Ok(QueryResult::message(message));
                }
                Err(error) => {
                    let transaction = connection.txn.take().expect("active transaction");
                    connection.bump_revision();
                    transaction
                        .snapshot
                        .storage
                        .rollback_to(mvcc_write(transaction.read_ts, writer_id), transaction.mark);
                    transaction.snapshot.release_catalog_writes(writer_id);
                    database.active_writers.remove(&self.id);
                    return Err(error);
                }
            }
        }

        let graph = Self::selected_graph(&database, &mut connection)?;
        let writer_id = self.acquire_writer(&mut database, &graph)?;
        let mut graph_data = graph.snapshot();
        let result = apply_index_statement(Arc::make_mut(&mut graph_data.catalog), stmt);
        if result.is_ok() {
            let mut committed = graph.data.lock().unwrap_or_else(|error| error.into_inner());
            committed.catalog = Arc::clone(&graph_data.catalog);
            committed.catalog_version = committed.catalog_version.saturating_add(1);
        }
        graph_data.release_catalog_writes(writer_id);
        database.active_writers.remove(&self.id);
        result.map(QueryResult::message)
    }

    fn execute_database_export(
        &self,
        stmt: &koko_parser::ast::Statement,
        parameters: HashMap<String, Value>,
        initial_compilation_time: Duration,
        execution_started: Instant,
        control: &StatementControl,
    ) -> Result<QueryResult> {
        let _schema = self
            .schema_gate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if connection.txn.is_some() {
            return Err(Error::transaction(
                "EXPORT DATABASE is not allowed while a transaction is active.",
            ));
        }
        let (selected, graphs, read_ts) = {
            let database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let selected = Self::selected_graph(&database, &mut connection)?.snapshot();
            let captured_generation = database.graphs.generation;
            let read_ts = database.commit_clock.current();
            let graphs = database
                .graphs
                .by_id
                .values()
                .map(|graph| {
                    let data = graph.snapshot();
                    interchange::ExportGraph {
                        name: graph.name.clone(),
                        data: interchange::InterchangeSnapshot::new(
                            data.catalog,
                            data.storage,
                            data.macros,
                            data.memory,
                        ),
                    }
                })
                .collect::<Vec<_>>();
            debug_assert_eq!(captured_generation, database.graphs.generation);
            (selected, graphs, read_ts)
        };
        let binder_config = binder_config_from_settings(&connection.settings);
        let compilation_started = Instant::now();
        let bound = bind_statement(&selected.catalog, stmt, &parameters, &binder_config)?;
        let BoundStatement::ExportDatabase(export) = bound else {
            unreachable!("database export dispatcher received another statement");
        };
        let mut query = connection.query_context(
            parameters,
            mvcc_view(read_ts, None),
            self.scalar_udf_snapshot().0,
            control,
        );
        query.compilation_time = initial_compilation_time + compilation_started.elapsed();
        let export_context =
            interchange::InterchangeExportContext::new(query.storage_read(), query.query_control());
        let result = interchange::export_database_image(&graphs, &export, &export_context)
            .map(|()| QueryResult::message("Exported database successfully.".to_string()));
        let memory = selected.memory.clone();
        let mut result = attach_query_summary(
            result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
            &memory,
        );
        refresh_execution_time(
            &mut result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
        );
        result
    }

    fn execute_database_import(
        &self,
        stmt: &koko_parser::ast::Statement,
        parameters: HashMap<String, Value>,
        initial_compilation_time: Duration,
        execution_started: Instant,
        control: &StatementControl,
    ) -> Result<QueryResult> {
        let _schema = self
            .schema_gate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if connection.txn.is_some() {
            return Err(Error::transaction(
                "IMPORT DATABASE is not allowed while a transaction is active.",
            ));
        }
        let selected = {
            let database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            Self::selected_graph(&database, &mut connection)?.snapshot()
        };
        let binder_config = binder_config_from_settings(&connection.settings);
        let compilation_started = Instant::now();
        let bound = bind_statement(&selected.catalog, stmt, &parameters, &binder_config)?;
        let BoundStatement::ImportDatabase(import) = bound else {
            unreachable!("database import dispatcher received another statement");
        };
        let image = interchange::preflight_database_image(Path::new(&import.path), &binder_config)?;

        let (
            captured_generation,
            table_ids,
            commit_clock,
            config,
            memory,
            multi_writes,
            plans,
            next_graph_id,
        ) = {
            let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            let captured_generation = database.graphs.generation;
            let mut next_graph_id = database.next_graph_id;
            let mut used_ids = HashSet::new();
            let plans = image
                .graphs
                .iter()
                .map(|graph| {
                    let id = if graph.name.eq_ignore_ascii_case("main") {
                        MAIN_GRAPH_ID
                    } else if let Some(id) = database.graphs.id(&graph.name) {
                        id
                    } else {
                        let id = GraphId(next_graph_id);
                        next_graph_id = next_graph_id.saturating_add(1);
                        id
                    };
                    if !used_ids.insert(id) {
                        return Err(Error::runtime(
                            "Import database failed: graph identities collide.",
                        ));
                    }
                    Ok((id, database.alloc_writer_id()))
                })
                .collect::<Result<Vec<_>>>()?;
            (
                captured_generation,
                Arc::clone(&database.table_ids),
                database.commit_clock.clone(),
                database.config.clone(),
                database.memory.clone(),
                Arc::clone(&database.multi_writes),
                plans,
                next_graph_id,
            )
        };

        let scalar_udfs = self.scalar_udf_snapshot().0;
        let mut staged = Vec::with_capacity(image.graphs.len());
        for (graph_image, (id, writer_id)) in image.graphs.iter().zip(plans) {
            let graph = Arc::new(GraphState::new(
                id,
                graph_image.name.clone(),
                graph_image.kind,
                Arc::clone(&table_ids),
                commit_clock.clone(),
                config.clone(),
                memory.clone(),
                Arc::clone(&multi_writes),
            ));
            let mut data = graph.snapshot();
            let read_ts = data.storage.current_commit_ts();
            let mark = data.storage.undo_mark();
            let mut query = connection.query_context(
                HashMap::new(),
                mvcc_view(read_ts, Some(writer_id)),
                Arc::clone(&scalar_udfs),
                control,
            );
            query.compilation_time = initial_compilation_time + compilation_started.elapsed();
            let imported = apply_interchange_image(&mut data, graph_image, &mut query);
            if let Err(error) = imported {
                data.storage
                    .rollback_to(mvcc_write(read_ts, writer_id), mark);
                data.release_catalog_writes(writer_id);
                return Err(error);
            }
            data.storage.commit_to(mvcc_write(read_ts, writer_id), mark);
            data.release_catalog_writes(writer_id);
            *graph.data.lock().unwrap_or_else(|error| error.into_inner()) = data;
            staged.push(graph);
        }

        {
            let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            if database.graphs.generation != captured_generation {
                return Err(Error::transaction(
                    "Graph registry changed while importing database.",
                ));
            }
            let mut registry = GraphRegistry {
                generation: captured_generation,
                ..GraphRegistry::default()
            };
            for graph in staged {
                registry.insert(graph);
            }
            let selected_name = connection.selected_graph_name.clone();
            connection.selected_graph = registry.id(&selected_name).unwrap_or(MAIN_GRAPH_ID);
            if connection.selected_graph == MAIN_GRAPH_ID
                && !selected_name.eq_ignore_ascii_case("main")
            {
                connection.selected_graph_name = "main".to_string();
            }
            connection.bump_revision();
            database.graphs = Arc::new(registry);
            database.next_graph_id = next_graph_id;
        }

        let read_ts = commit_clock.current();
        let query = connection.query_context(
            HashMap::new(),
            mvcc_view(read_ts, None),
            scalar_udfs,
            control,
        );
        let result = Ok(QueryResult::message(
            "Imported database successfully.".to_string(),
        ));
        let mut result = attach_query_summary(
            result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
            &memory,
        );
        refresh_execution_time(
            &mut result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
        );
        result
    }

    /// Bind, plan, and execute an already-parsed statement with its parameter
    /// values, routing it through this connection's transaction state. Shared by
    /// [`query_with_params`](Self::query_with_params) and
    /// [`PreparedStatement::execute`] (which reuses an earlier parse).
    fn execute_parsed_inner(
        &self,
        stmt: &koko_parser::ast::Statement,
        params: &[(&str, Value)],
        parameter_names: Option<&[String]>,
        initial_compilation_time: Duration,
        control: &StatementControl,
    ) -> Result<QueryResult> {
        let unexpected_parameter = parameter_names.and_then(|expected_params| {
            params
                .iter()
                .find(|(name, _)| !expected_params.iter().any(|expected| expected == name))
        });
        if let Some((name, _)) = unexpected_parameter {
            return Err(Error::binder(format!(
                "Unexpected query parameter ${name}."
            )));
        }
        let parameters: HashMap<String, Value> = params
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        let execution_started = Instant::now();
        if matches!(stmt, koko_parser::ast::Statement::ExportDatabase(_)) {
            return self.execute_database_export(
                stmt,
                parameters,
                initial_compilation_time,
                execution_started,
                control,
            );
        }
        if matches!(stmt, koko_parser::ast::Statement::ImportDatabase(_)) {
            return self.execute_database_import(
                stmt,
                parameters,
                initial_compilation_time,
                execution_started,
                control,
            );
        }

        if matches!(
            stmt,
            koko_parser::ast::Statement::CreateGraph(_)
                | koko_parser::ast::Statement::UseGraph { .. }
                | koko_parser::ast::Statement::DropGraph { .. }
        ) {
            return self.execute_graph_statement(stmt);
        }

        if matches!(
            stmt,
            koko_parser::ast::Statement::CreateIndex(_) | koko_parser::ast::Statement::DropIndex(_)
        ) {
            let _schema = self
                .schema_gate
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            return self.execute_index_statement(stmt);
        }

        if let koko_parser::ast::Statement::Transaction(op) = stmt {
            let _schema = self
                .schema_gate
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            return self.handle_transaction(*op, &mut database);
        }

        let writes = statement_writes(stmt);
        if let koko_parser::ast::Statement::Query(_) = stmt {
            let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if connection.txn.is_none() {
                return self.execute_regular_autocommit(
                    stmt,
                    parameters,
                    initial_compilation_time,
                    execution_started,
                    &mut connection,
                    control,
                );
            }
        }

        let _schema = self
            .schema_gate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());

        if connection.txn.is_some() {
            let show_table_rows = {
                let transaction = connection.txn.as_ref().expect("checked above");
                database.show_table_rows(Some((
                    transaction.graph.id,
                    transaction.snapshot.catalog.as_ref(),
                )))
            };
            let (read_ts, writer_id, rel_base) = {
                let transaction = connection.txn.as_ref().expect("checked above");
                (
                    transaction.read_ts,
                    transaction.writer_id,
                    transaction.rel_base.clone(),
                )
            };
            if writes && writer_id.is_none() {
                return Err(Error::transaction(READ_ONLY_WRITE_MSG));
            }
            let mut query = connection.query_context(
                parameters,
                mvcc_view(read_ts, writer_id),
                self.scalar_udf_snapshot().0,
                control,
            );
            query.compilation_time = initial_compilation_time;
            query.show_table_rows = Some(show_table_rows);
            let transaction = connection.txn.as_mut().expect("checked above");
            let sequence_before = transaction.snapshot.catalog.sequence_state();
            let result = transaction
                .snapshot
                .run_statement(stmt, &rel_base, &mut query);
            let memory = transaction.snapshot.memory.clone();
            let mut result = attach_query_summary(
                result,
                &query,
                initial_compilation_time + execution_started.elapsed(),
                &memory,
            );
            if result.is_ok() {
                if statement_touches_catalog(stmt)
                    || transaction.snapshot.catalog.sequence_state() != sequence_before
                {
                    transaction.catalog_dirty = true;
                    transaction.catalog_epoch = transaction.catalog_epoch.saturating_add(1);
                    connection.bump_revision();
                }
                if let Some((key, value)) = query.setting_update.take() {
                    connection.settings.set(&key, value);
                    connection.bump_revision();
                }
            } else {
                let transaction = connection.txn.take().expect("active transaction");
                connection.bump_revision();
                if let Some(writer_id) = transaction.writer_id {
                    transaction
                        .snapshot
                        .storage
                        .rollback_to(mvcc_write(transaction.read_ts, writer_id), transaction.mark);
                    transaction.snapshot.release_catalog_writes(writer_id);
                    database.active_writers.remove(&self.id);
                }
            }
            refresh_execution_time(
                &mut result,
                &query,
                initial_compilation_time + execution_started.elapsed(),
            );
            return result;
        }

        let graph = Self::selected_graph(&database, &mut connection)?;
        let show_table_rows = database.show_table_rows(None);
        let mut graph_data = graph.snapshot();
        let read_ts = graph_data.storage.current_commit_ts();
        let writer_id = if writes {
            Some(self.acquire_writer(&mut database, &graph)?)
        } else {
            None
        };
        let mark = writer_id.map_or(0, |_| graph_data.storage.undo_mark());
        let rel_base = if writes {
            graph_data.rel_table_bases()
        } else {
            HashMap::new()
        };
        let sequence_before = graph_data.catalog.sequence_state();
        drop(database);

        let mut query = connection.query_context(
            parameters,
            mvcc_view(read_ts, writer_id),
            self.scalar_udf_snapshot().0,
            control,
        );
        query.compilation_time = initial_compilation_time;
        query.show_table_rows = Some(show_table_rows);
        let result = graph_data.run_statement(stmt, &rel_base, &mut query);
        let memory = graph_data.memory.clone();
        let mut result = attach_query_summary(
            result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
            &memory,
        );
        if let Some(writer_id) = writer_id {
            let write = mvcc_write(read_ts, writer_id);
            if result.is_ok() {
                graph_data.storage.commit_to(write, mark);
            } else {
                graph_data.storage.rollback_to(write, mark);
            }
            let catalog_changed = statement_touches_catalog(stmt)
                || graph_data.catalog.sequence_state() != sequence_before;
            if result.is_ok() && catalog_changed {
                let mut committed = graph.data.lock().unwrap_or_else(|error| error.into_inner());
                committed.catalog = Arc::clone(&graph_data.catalog);
                committed.macros = Arc::clone(&graph_data.macros);
                committed.catalog_version = committed.catalog_version.saturating_add(1);
            }
            graph_data.release_catalog_writes(writer_id);
            self.inner
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .active_writers
                .remove(&self.id);
        }
        if let Some((key, value)) = result
            .is_ok()
            .then(|| query.setting_update.take())
            .flatten()
        {
            connection.settings.set(&key, value);
            connection.bump_revision();
        }
        refresh_execution_time(
            &mut result,
            &query,
            initial_compilation_time + execution_started.elapsed(),
        );
        result
    }

    /// Load a CSV dataset directory (`schema.cypher` + `copy.cypher`), resolving
    /// `COPY` file paths relative to `dir`. This is how the `.test` corpus's
    /// `-DATASET CSV <name>` directive populates a fresh database.
    pub fn load_csv_dataset(&self, dir: &Path) -> Result<()> {
        let schema = std::fs::read_to_string(dir.join("schema.cypher"))?;
        let storage_root = dir.to_string_lossy().replace('\'', "''");
        for stmt in interchange::split_statements(&schema) {
            let stmt = stmt
                .replace("storage = '.'", &format!("storage = '{storage_root}'"))
                .replace("storage='.'", &format!("storage='{storage_root}'"));
            self.query(&stmt)?;
        }
        let copy_path = dir.join("copy.cypher");
        if !copy_path.exists() {
            return Ok(());
        }
        let copy = std::fs::read_to_string(copy_path)?;
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _schema = self
            .schema_gate
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let control = self.statement_control();
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let graph = Self::selected_graph(&database, &mut connection)?;
        let graph_data = graph.snapshot();
        let writer_id = self.acquire_writer(&mut database, &graph)?;
        let mark = graph_data.storage.undo_mark();
        let read_ts = graph_data.storage.current_commit_ts();
        let sequence_before = graph_data.catalog.sequence_state();
        drop(database);
        let result: Result<()> = (|| {
            for stmt in interchange::split_statements(&copy) {
                let parsed = parse_statement(&stmt)?;
                match parsed {
                    koko_parser::ast::Statement::Copy(_) => {
                        let query = connection.query_context(
                            HashMap::new(),
                            mvcc_view(read_ts, Some(writer_id)),
                            self.scalar_udf_snapshot().0,
                            &control,
                        );
                        let mut config = query.binder_config();
                        config.base_dir = dir.to_path_buf();
                        let bound = bind_statement(
                            &graph_data.catalog,
                            &parsed,
                            &query.parameters,
                            &config,
                        )?;
                        if let BoundStatement::Copy(copy) = bound {
                            let mut adapter = CopyOperationContext::new(
                                &graph_data.catalog,
                                &graph_data.storage,
                                &graph_data.memory,
                                query.storage_read(),
                                query.storage_write()?,
                                &query.warnings,
                                query.worker_count(),
                                query.query_control(),
                            );
                            run_copy(&mut adapter, &copy)?;
                        }
                    }
                    koko_parser::ast::Statement::Transaction(_)
                    | koko_parser::ast::Statement::Call(_)
                    | koko_parser::ast::Statement::Comment(_) => {}
                    _ => {
                        return Err(Error::runtime(format!(
                            "copy.cypher may only contain COPY statements, found: {stmt}"
                        )));
                    }
                }
            }
            Ok(())
        })();
        let write = mvcc_write(read_ts, writer_id);
        if result.is_ok() {
            graph_data.storage.commit_to(write, mark);
            if sequence_before != graph_data.catalog.sequence_state() {
                graph
                    .data
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .catalog_version += 1;
            }
        } else {
            graph_data.storage.rollback_to(write, mark);
        }
        graph_data.release_catalog_writes(writer_id);
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active_writers
            .remove(&self.id);
        result
    }
}
