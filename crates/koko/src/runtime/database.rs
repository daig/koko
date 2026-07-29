//! Database-wide graph registry, resources, and connection creation.

use super::connection::{ConnId, Connection};
use super::graph::{GraphId, GraphRegistry, GraphState, MAIN_GRAPH_ID};
use crate::{DatabaseConfig, Value};
use koko_catalog::Catalog;
use koko_common::{MemoryTracker, MemoryUsage, START_TX_ID, Ts};
use koko_storage::CommitClock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Clone)]
pub(super) struct WriterLease {
    pub(super) writer_id: Ts,
    pub(super) graph: Arc<GraphState>,
}

/// Database-wide graph registry, identity/timestamp allocation, writer admission, and resources.
/// Connection/session state and graph-local catalog/storage state never live here.
pub(super) struct DatabaseState {
    pub(super) graphs: Arc<GraphRegistry>,
    pub(super) active_writers: HashMap<ConnId, WriterLease>,
    pub(super) next_writer_id: Ts,
    pub(super) multi_writes: Arc<AtomicBool>,
    pub(super) config: DatabaseConfig,
    pub(super) memory: MemoryTracker,
    pub(super) next_graph_id: u64,
    pub(super) table_ids: Arc<AtomicU64>,
    pub(super) commit_clock: CommitClock,
}

impl DatabaseState {
    fn new(config: DatabaseConfig, memory: MemoryTracker) -> Self {
        let mut database = Self {
            graphs: Arc::new(GraphRegistry::default()),
            active_writers: HashMap::new(),
            next_writer_id: START_TX_ID,
            multi_writes: Arc::new(AtomicBool::new(false)),
            config,
            memory,
            next_graph_id: 0,
            table_ids: Arc::new(AtomicU64::new(0)),
            commit_clock: CommitClock::default(),
        };
        let main = database.allocate_graph("main", koko_parser::ast::GraphKind::Typed);
        debug_assert_eq!(main.id, MAIN_GRAPH_ID);
        Arc::make_mut(&mut database.graphs).insert(main);
        database
    }

    pub(super) fn allocate_graph(
        &mut self,
        name: &str,
        kind: koko_parser::ast::GraphKind,
    ) -> Arc<GraphState> {
        let id = GraphId(self.next_graph_id);
        self.next_graph_id = self.next_graph_id.saturating_add(1);
        Arc::new(GraphState::new(
            id,
            name.to_string(),
            kind,
            Arc::clone(&self.table_ids),
            self.commit_clock.clone(),
            self.config.clone(),
            self.memory.clone(),
            Arc::clone(&self.multi_writes),
        ))
    }

    pub(super) fn show_table_rows(
        &self,
        catalog_override: Option<(GraphId, &Catalog)>,
    ) -> Vec<Vec<Value>> {
        let mut graphs: Vec<_> = self.graphs.by_id.values().collect();
        graphs.sort_by_key(|graph| graph.id.0);
        let mut rows = Vec::new();
        for graph in graphs {
            let graph_data = graph.snapshot();
            let catalog = match catalog_override {
                Some((graph_id, catalog)) if graph_id == graph.id => catalog,
                _ => graph_data.catalog.as_ref(),
            };
            let database_name = format!("{}(graph)", graph.name);
            for id in catalog.node_table_ids() {
                let table = catalog.node_table(id).expect("listed node table");
                rows.push(vec![
                    Value::Int64(id.0 as i64),
                    Value::String(table.name().to_string()),
                    Value::String("NODE".to_string()),
                    Value::String(database_name.clone()),
                    Value::String(catalog.table_comment(id).to_string()),
                ]);
            }
            for id in catalog.rel_table_ids() {
                let table = catalog.rel_table(id).expect("listed rel table");
                rows.push(vec![
                    Value::Int64((id.0 + table.pairs().len() as u64) as i64),
                    Value::String(table.name().to_string()),
                    Value::String("REL".to_string()),
                    Value::String(database_name.clone()),
                    Value::String(catalog.table_comment(id).to_string()),
                ]);
            }
        }
        rows
    }

    pub(super) fn alloc_writer_id(&mut self) -> Ts {
        let id = self.next_writer_id;
        self.next_writer_id = self.next_writer_id.saturating_add(1);
        id
    }

    pub(super) fn release_writer(&mut self, connection: ConnId) {
        self.active_writers.remove(&connection);
    }
}

/// An embedded Koko database.
///
/// `Database` is cheaply clonable and shareable (`Arc`-backed). The product is intentionally
/// in-memory; native durability is permanently deferred unless explicitly restored to scope.
#[derive(Clone)]
pub struct Database {
    inner: Arc<Mutex<DatabaseState>>,
    /// DDL/legacy coordination. Ordinary regular queries take a shared guard;
    /// catalog-changing and compatibility paths take an exclusive guard.
    schema_gate: Arc<RwLock<()>>,
    /// Lock-free source of per-connection ids (shared across clones).
    next_conn_id: Arc<AtomicU64>,
    memory: MemoryTracker,
}

impl Database {
    fn from_config(config: DatabaseConfig) -> Database {
        let memory = MemoryTracker::with_resource(config.memory_limit(), config.memory_resource());
        let schema_gate = Arc::new(RwLock::new(()));
        Database {
            inner: Arc::new(Mutex::new(DatabaseState::new(config, memory.clone()))),
            schema_gate,
            next_conn_id: Arc::new(AtomicU64::new(0)),
            memory,
        }
    }

    /// Open a fresh in-memory database with unrestricted resource defaults.
    pub fn new() -> Self {
        Self::with_config(DatabaseConfig::default())
    }

    /// Open an in-memory database with validated resource limits.
    pub fn with_config(config: DatabaseConfig) -> Self {
        Self::from_config(config)
    }

    /// Return this database's current, peak, and configured tracked-memory counters.
    pub fn memory_usage(&self) -> MemoryUsage {
        self.memory.usage()
    }

    /// Open a connection. Connections are cheap; one per thread is the idiom.
    pub fn connect(&self) -> Connection {
        let max_threads = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .config
            .max_threads();
        Connection::new(
            ConnId(self.next_conn_id.fetch_add(1, Ordering::Relaxed)),
            Arc::clone(&self.inner),
            Arc::clone(&self.schema_gate),
            max_threads,
        )
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}
