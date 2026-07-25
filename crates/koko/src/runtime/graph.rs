//! Graph identity, published graph state, and registry ownership.

use super::context::mvcc_write;
use crate::macros::MacroRegistry;
use crate::{DatabaseConfig, MemoryTracker};
use koko_catalog::Catalog;
use koko_common::{START_TX_ID, Ts};
use koko_storage::{CommitClock, InMemStorage, SharedStorage};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

/// Stable database-local identity of a graph registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct GraphId(pub(super) u64);

pub(super) const MAIN_GRAPH_ID: GraphId = GraphId(0);

/// One graph's catalog generations, concrete storage, macros, and shared resource policy.
///
/// Cloning is deliberately shallow: regular statements capture immutable catalog/macro
/// generations and the concrete shared storage. A read-write transaction replaces those two
/// metadata arcs with private copy-on-write values while retaining the same storage domain.
#[derive(Clone)]
pub(super) struct GraphData {
    pub(super) catalog: Arc<Catalog>,
    pub(super) storage: Arc<SharedStorage>,
    pub(super) macros: Arc<MacroRegistry>,
    pub(super) catalog_version: u64,
    pub(super) catalog_writers: Arc<Mutex<HashMap<String, Ts>>>,
    pub(super) config: DatabaseConfig,
    pub(super) memory: MemoryTracker,
    pub(super) multi_writes: Arc<AtomicBool>,
}

/// Immutable graph-registry entry. Dropping a graph removes discoverability, while an already
/// captured `Arc<GraphState>` keeps its query snapshot and storage alive until execution ends.
pub(super) struct GraphState {
    pub(super) id: GraphId,
    pub(super) name: String,
    pub(super) kind: koko_parser::ast::GraphKind,
    pub(super) data: Mutex<GraphData>,
}

impl GraphState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        id: GraphId,
        name: String,
        kind: koko_parser::ast::GraphKind,
        table_ids: Arc<AtomicU64>,
        commit_clock: CommitClock,
        config: DatabaseConfig,
        memory: MemoryTracker,
        multi_writes: Arc<AtomicBool>,
    ) -> Self {
        let mut catalog = Catalog::with_table_id_allocator(table_ids);
        let mut storage = InMemStorage::with_memory_tracker_and_clock(memory.clone(), commit_clock);
        if matches!(kind, koko_parser::ast::GraphKind::Any) {
            let tables = catalog
                .initialize_any_graph()
                .expect("fresh ANY catalog initialization cannot fail");
            let read_ts = storage.current_commit_ts();
            let write = mvcc_write(read_ts, START_TX_ID);
            let node = catalog
                .node_table(tables.nodes)
                .expect("ANY node table was just created");
            let node_types = node
                .columns
                .iter()
                .map(|column| column.ty.clone())
                .collect::<Vec<_>>();
            storage.create_node_table(write, tables.nodes, &node_types, node.primary_key);
            let rel = catalog
                .rel_table(tables.edges)
                .expect("ANY relationship table was just created");
            let rel_types = rel
                .columns
                .iter()
                .map(|column| column.ty.clone())
                .collect::<Vec<_>>();
            storage.create_rel_table(
                write,
                tables.edges,
                tables.nodes,
                tables.nodes,
                &rel_types,
                "_edges",
                koko_common::RelMultiplicity::default(),
            );
            storage.commit_to(write, 0);
        }
        Self {
            id,
            name,
            kind,
            data: Mutex::new(GraphData {
                catalog: Arc::new(catalog),
                storage: Arc::new(SharedStorage::new(storage)),
                macros: Arc::new(HashMap::new()),
                catalog_version: 0,
                catalog_writers: Arc::new(Mutex::new(HashMap::new())),
                config,
                memory,
                multi_writes,
            }),
        }
    }

    pub(super) fn snapshot(&self) -> GraphData {
        self.data
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// Immutable, generation-published graph name/id registry.
#[derive(Clone, Default)]
pub(super) struct GraphRegistry {
    pub(super) generation: u64,
    pub(super) by_id: HashMap<GraphId, Arc<GraphState>>,
    pub(super) by_name: HashMap<String, GraphId>,
}

impl GraphRegistry {
    pub(super) fn graph(&self, id: GraphId) -> Option<Arc<GraphState>> {
        self.by_id.get(&id).cloned()
    }

    pub(super) fn id(&self, name: &str) -> Option<GraphId> {
        self.by_name.get(&name.to_ascii_lowercase()).copied()
    }

    pub(super) fn insert(&mut self, graph: Arc<GraphState>) {
        self.by_name
            .insert(graph.name.to_ascii_lowercase(), graph.id);
        self.by_id.insert(graph.id, graph);
        self.generation = self.generation.saturating_add(1);
    }

    pub(super) fn remove(&mut self, id: GraphId) -> Option<Arc<GraphState>> {
        let graph = self.by_id.remove(&id)?;
        self.by_name.remove(&graph.name.to_ascii_lowercase());
        self.generation = self.generation.saturating_add(1);
        Some(graph)
    }
}
