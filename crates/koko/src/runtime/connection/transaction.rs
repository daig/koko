//! Explicit transaction lifecycle, writer admission, rollback, and panic recovery.

use super::{
    ACTIVE_TRANSACTION_MSG, Connection, PreparedStatement, READ_ONLY_WRITE_MSG, SINGLE_WRITER_MSG,
};
use crate::runtime::context::mvcc_write;
use crate::runtime::database::{DatabaseState, WriterLease};
use crate::runtime::graph::{GraphData, GraphState};
use crate::{Error, MemoryTracker, QueryResult, Result, TableId, Value};
use koko_catalog::Catalog;
use koko_common::Ts;
use koko_storage::{SharedStorage, StorageWriteHandle};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// A connection's active explicit transaction (`BEGIN` … `COMMIT`/`ROLLBACK`).
pub(super) struct TxnState {
    pub(super) graph: Arc<GraphState>,
    pub(super) snapshot: GraphData,
    pub(super) read_ts: Ts,
    pub(super) mark: usize,
    pub(super) writer_id: Option<Ts>,
    pub(super) catalog_version: u64,
    pub(super) catalog_epoch: u64,
    pub(super) catalog_dirty: bool,
    pub(super) rel_base: HashMap<TableId, u64>,
}

/// Narrow graph resources used by the runtime's adapter-write protocol.
struct WriteOperationContext<'a> {
    graph: &'a mut GraphData,
}

impl WriteOperationContext<'_> {
    fn catalog(&self) -> &Catalog {
        &self.graph.catalog
    }

    fn storage(&self) -> &SharedStorage {
        &self.graph.storage
    }

    fn memory(&self) -> &MemoryTracker {
        &self.graph.memory
    }
}

/// An RAII handle to an explicit transaction, returned by
/// [`Connection::transaction`] / [`Connection::transaction_read_only`].
///
/// Statements run through the guard ([`query`](Self::query) /
/// [`prepare`](Self::prepare)) or through the underlying [`Connection`] both see
/// the transaction. Call [`commit`](Self::commit) to publish the writes;
/// **dropping the guard without committing rolls the transaction back** (the safe
/// `rusqlite` idiom), as does [`rollback`](Self::rollback).
///
/// ```
/// # use koko::Database;
/// # fn main() -> koko::Result<()> {
/// let db = Database::in_memory();
/// let conn = db.connect();
/// conn.query("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")?;
/// let tx = conn.transaction()?;
/// tx.query("CREATE (:P {id: 1})")?;
/// tx.commit()?; // without this, the drop would roll back
/// # Ok(())
/// # }
/// ```
pub struct Transaction<'conn> {
    conn: &'conn Connection,
    /// Set once `commit`/`rollback` has run, so `Drop` doesn't roll back again.
    finished: bool,
}

impl<'conn> Transaction<'conn> {
    /// Run a statement inside this transaction (delegates to the connection).
    pub fn query(&self, cypher: &str) -> Result<QueryResult> {
        self.conn.query(cypher)
    }

    /// Run a parameterized statement inside this transaction.
    pub fn query_with_params(&self, cypher: &str, params: &[(&str, Value)]) -> Result<QueryResult> {
        self.conn.query_with_params(cypher, params)
    }

    /// Prepare a statement on this transaction's connection. The returned handle is
    /// tied to the connection (not this guard), and runs within the transaction
    /// while it stays active.
    pub fn prepare(&self, cypher: &str) -> Result<PreparedStatement<'conn>> {
        self.conn.prepare(cypher)
    }

    /// Commit the transaction, publishing its writes to all connections.
    pub fn commit(mut self) -> Result<()> {
        self.conn
            .handle_transaction_op(koko_parser::ast::TxnOp::Commit)?;
        self.finished = true;
        Ok(())
    }

    /// Roll the transaction back, discarding its writes. Equivalent to dropping the
    /// guard, but lets you observe any error.
    pub fn rollback(mut self) -> Result<()> {
        self.conn
            .handle_transaction_op(koko_parser::ast::TxnOp::Rollback)?;
        self.finished = true;
        Ok(())
    }
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            // Best-effort rollback. If a statement error already aborted the
            // transaction, this ROLLBACK is a no-op that errors with "No active
            // transaction" — deliberately ignored here.
            let _ = self
                .conn
                .handle_transaction_op(koko_parser::ast::TxnOp::Rollback);
        }
    }
}

impl Connection {
    /// Begin an explicit **read-write** transaction, returning an RAII
    /// [`Transaction`] guard. Statements run via the guard (or via this connection
    /// directly) see their own uncommitted writes in isolation; `tx.commit()`
    /// publishes them and **dropping the guard without committing rolls back** (the
    /// safe `rusqlite` idiom). Errors if another connection already holds the
    /// single write-transaction slot.
    pub fn transaction(&self) -> Result<Transaction<'_>> {
        self.handle_transaction_op(koko_parser::ast::TxnOp::Begin { read_only: false })?;
        Ok(Transaction {
            conn: self,
            finished: false,
        })
    }

    /// Begin an explicit **read-only** transaction (an RAII [`Transaction`] guard).
    /// Reads run against committed state; an attempted write is rejected. Dropping
    /// the guard ends the transaction (a read-only transaction has nothing to roll
    /// back). Read-only transactions don't take the single-writer slot.
    pub fn transaction_read_only(&self) -> Result<Transaction<'_>> {
        self.handle_transaction_op(koko_parser::ast::TxnOp::Begin { read_only: true })?;
        Ok(Transaction {
            conn: self,
            finished: false,
        })
    }

    /// Run a transaction-control op (`BEGIN`/`COMMIT`/`ROLLBACK`), taking the lock
    /// the same way a query would. The RAII [`Transaction`] handle drives `BEGIN`
    /// and `COMMIT`/`ROLLBACK` through here.
    pub(super) fn handle_transaction_op(&self, op: koko_parser::ast::TxnOp) -> Result<QueryResult> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _schema = self
                .schema_gate
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut guard = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            self.handle_transaction(op, &mut guard)
        })) {
            Ok(result) => result,
            Err(payload) => {
                self.recover_after_panic();
                let detail = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("unknown panic");
                Err(Error::runtime(format!(
                    "Query execution panicked: {detail}"
                )))
            }
        }
    }

    pub(super) fn recover_after_panic(&self) {
        let transaction = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .txn
            .take();
        let lease = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active_writers
            .remove(&self.id);
        if let Some(transaction) = transaction.filter(|transaction| transaction.writer_id.is_some())
        {
            let writer_id = transaction.writer_id.expect("filtered above");
            transaction
                .snapshot
                .storage
                .rollback_to(mvcc_write(transaction.read_ts, writer_id), transaction.mark);
            transaction.snapshot.release_catalog_writes(writer_id);
        } else if let Some(lease) = lease {
            let graph_data = lease.graph.snapshot();
            graph_data.storage.rollback_to(
                mvcc_write(graph_data.storage.current_commit_ts(), lease.writer_id),
                0,
            );
            graph_data.release_catalog_writes(lease.writer_id);
        }
    }

    /// Abort this connection's active explicit transaction: drop its snapshot
    /// (discarding its writes), release the writer slot, and clear the state. Used
    /// when a statement errors inside a txn and on the nested-`BEGIN` /
    /// in-txn-`CHECKPOINT` errors.
    fn abort_transaction(&self, database: &mut DatabaseState) {
        let transaction = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let transaction = state.txn.take();
            if transaction.is_some() {
                state.bump_revision();
            }
            transaction
        };
        if let Some(transaction) = transaction.filter(|transaction| transaction.writer_id.is_some())
        {
            let writer_id = transaction.writer_id.expect("filtered above");
            transaction
                .snapshot
                .storage
                .rollback_to(mvcc_write(transaction.read_ts, writer_id), transaction.mark);
            transaction.snapshot.release_catalog_writes(writer_id);
            database.active_writers.remove(&self.id);
        }
    }

    /// Acquire the single write-transaction slot for this connection, or error if
    /// another connection holds it (unless `multi_writes`).
    pub(super) fn acquire_writer(
        &self,
        database: &mut DatabaseState,
        graph: &Arc<GraphState>,
    ) -> Result<Ts> {
        let conflicting_writer = (!database.multi_writes.load(Ordering::Acquire))
            .then(|| database.active_writers.keys().next().copied())
            .flatten()
            .filter(|other| *other != self.id);
        if conflicting_writer.is_some() {
            return Err(Error::transaction(SINGLE_WRITER_MSG));
        }
        let writer_id = database.alloc_writer_id();
        database.active_writers.insert(
            self.id,
            WriterLease {
                writer_id,
                graph: Arc::clone(graph),
            },
        );
        Ok(writer_id)
    }

    /// Run an adapter write with the same transaction, savepoint, and writer
    /// lifecycle as a normal statement. `prepare` runs before auto-commit writer
    /// admission so a no-op retains its existing contention behavior.
    fn with_prepared_write<P, T>(
        &self,
        prepare: impl FnOnce(&WriteOperationContext<'_>) -> Result<Option<P>>,
        apply: impl FnOnce(&mut WriteOperationContext<'_>, StorageWriteHandle, P) -> Result<T>,
    ) -> Result<Option<T>> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _schema = self
            .schema_gate
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if connection
            .txn
            .as_ref()
            .is_some_and(|transaction| transaction.writer_id.is_none())
        {
            return Err(Error::transaction(READ_ONLY_WRITE_MSG));
        }

        if let Some(transaction) = connection.txn.as_mut() {
            let prepared = {
                let context = WriteOperationContext {
                    graph: &mut transaction.snapshot,
                };
                prepare(&context)?
            };
            let Some(prepared) = prepared else {
                return Ok(None);
            };
            let writer = transaction.writer_id.expect("read-only rejected above");
            let write = mvcc_write(transaction.read_ts, writer);
            let mark = transaction.snapshot.storage.undo_mark();
            let result = {
                let mut context = WriteOperationContext {
                    graph: &mut transaction.snapshot,
                };
                apply(&mut context, write, prepared)
            };
            if result.is_err() {
                transaction.snapshot.storage.rollback_to(write, mark);
            }
            return result.map(Some);
        }

        let mut database = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let graph = Self::selected_graph(&database, &mut connection)?;
        let mut graph_data = graph.snapshot();
        let prepared = {
            let context = WriteOperationContext {
                graph: &mut graph_data,
            };
            prepare(&context)?
        };
        let Some(prepared) = prepared else {
            return Ok(None);
        };
        let writer = self.acquire_writer(&mut database, &graph)?;
        let read_ts = graph_data.storage.current_commit_ts();
        let write = mvcc_write(read_ts, writer);
        let mark = graph_data.storage.undo_mark();
        drop(database);
        let result = {
            let mut context = WriteOperationContext {
                graph: &mut graph_data,
            };
            apply(&mut context, write, prepared)
        };
        if result.is_ok() {
            graph_data.storage.commit_to(write, mark);
        } else {
            graph_data.storage.rollback_to(write, mark);
        }
        graph_data.release_catalog_writes(writer);
        self.release_writer();
        result.map(Some)
    }

    /// Atomically import native Rust Arrow batches into an existing node table,
    /// relationship table, or relationship group.
    ///
    /// Field names are matched case-insensitively and duplicate folded names are
    /// rejected. Node schemas contain exactly the stored properties. Relationship
    /// schemas contain `from`, `to`, then exactly the stored properties; endpoints
    /// are primary-key values and group rows route to their resolved physical pair.
    /// Missing/default columns are never implicit. Every batch is preflighted before
    /// mutation, conversion memory is charged to the database tracker, and a call
    /// savepoint restores all earlier batches if any row fails.
    pub fn import_arrow(&self, table: &str, batches: &[arrow_array::RecordBatch]) -> Result<u64> {
        self.with_prepared_write(
            |runtime| {
                let context = crate::arrow::ArrowImportContext::new(
                    runtime.catalog(),
                    runtime.storage(),
                    runtime.memory(),
                );
                crate::arrow::prepare_import(&context, table, batches)
            },
            |runtime, write, plan| {
                let context = crate::arrow::ArrowImportContext::new(
                    runtime.catalog(),
                    runtime.storage(),
                    runtime.memory(),
                );
                crate::arrow::apply_import(&context, write, plan, batches)
            },
        )
        .map(|result| result.unwrap_or(0))
    }

    /// Drive `BEGIN`/`COMMIT`/`ROLLBACK`/`CHECKPOINT` against this connection's
    /// transaction state and the shared storage undo log.
    pub(super) fn handle_transaction(
        &self,
        op: koko_parser::ast::TxnOp,
        database: &mut DatabaseState,
    ) -> Result<QueryResult> {
        match op {
            koko_parser::ast::TxnOp::Begin { read_only } => {
                let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if connection.txn.is_some() {
                    drop(connection);
                    self.abort_transaction(database);
                    return Err(Error::transaction(ACTIVE_TRANSACTION_MSG));
                }
                let graph = Self::selected_graph(database, &mut connection)?;
                let mut snapshot = graph.snapshot();
                let storage = Arc::clone(&snapshot.storage);
                let read_ts = storage.current_commit_ts();
                let writer_id = if read_only {
                    None
                } else {
                    Some(self.acquire_writer(database, &graph)?)
                };
                if writer_id.is_some() {
                    snapshot.catalog = Arc::new((*snapshot.catalog).clone());
                    snapshot.macros = Arc::new((*snapshot.macros).clone());
                }
                let mark = writer_id.map_or(0, |_| storage.undo_mark());
                let rel_base = snapshot.rel_table_bases();
                let catalog_version = snapshot.catalog_version;
                connection.txn = Some(TxnState {
                    graph,
                    snapshot,
                    read_ts,
                    mark,
                    writer_id,
                    catalog_version,
                    catalog_epoch: 0,
                    catalog_dirty: false,
                    rel_base,
                });
                connection.bump_revision();
            }
            koko_parser::ast::TxnOp::Commit => {
                let transaction = {
                    let mut connection =
                        self.state.lock().unwrap_or_else(|error| error.into_inner());
                    let transaction = connection
                        .txn
                        .take()
                        .ok_or_else(|| Error::transaction("No active transaction for COMMIT."))?;
                    connection.bump_revision();
                    transaction
                };
                if let Some(writer_id) = transaction.writer_id {
                    let mut committed = transaction
                        .graph
                        .data
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if transaction.catalog_dirty
                        && committed.catalog_version != transaction.catalog_version
                    {
                        transaction.snapshot.storage.rollback_to(
                            mvcc_write(transaction.read_ts, writer_id),
                            transaction.mark,
                        );
                        transaction.snapshot.release_catalog_writes(writer_id);
                        database.active_writers.remove(&self.id);
                        return Err(Error::transaction(
                            "Write-write conflict on catalog changes.",
                        ));
                    }
                    transaction
                        .snapshot
                        .storage
                        .commit_to(mvcc_write(transaction.read_ts, writer_id), transaction.mark);
                    if transaction.catalog_dirty {
                        committed.catalog = Arc::clone(&transaction.snapshot.catalog);
                        committed.macros = Arc::clone(&transaction.snapshot.macros);
                        committed.catalog_version = committed.catalog_version.saturating_add(1);
                    }
                    committed.release_catalog_writes(writer_id);
                    database.active_writers.remove(&self.id);
                }
            }
            koko_parser::ast::TxnOp::Rollback => {
                let transaction = {
                    let mut connection =
                        self.state.lock().unwrap_or_else(|error| error.into_inner());
                    let transaction = connection
                        .txn
                        .take()
                        .ok_or_else(|| Error::transaction("No active transaction for ROLLBACK."))?;
                    connection.bump_revision();
                    transaction
                };
                if let Some(writer_id) = transaction.writer_id {
                    transaction
                        .snapshot
                        .storage
                        .rollback_to(mvcc_write(transaction.read_ts, writer_id), transaction.mark);
                    transaction.snapshot.release_catalog_writes(writer_id);
                    database.active_writers.remove(&self.id);
                }
            }
            koko_parser::ast::TxnOp::Checkpoint => {
                let mut connection = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if connection.txn.is_some() {
                    drop(connection);
                    self.abort_transaction(database);
                    return Err(Error::transaction(
                        "Found active transaction for CHECKPOINT.",
                    ));
                }
                let graph = Self::selected_graph(database, &mut connection)?;
                let _ = graph;
            }
        }
        Ok(QueryResult::default())
    }
}
