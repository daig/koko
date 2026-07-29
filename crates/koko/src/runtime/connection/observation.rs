//! Lock-coherent session and catalog snapshot capture.

use super::{Connection, ConnectionState};
use crate::runtime::context::RUNTIME_SETTING_SPECS;
use crate::runtime::database::DatabaseState;
use crate::runtime::graph::{GraphData, GraphState, MAIN_GRAPH_ID};
use crate::tooling::{
    self, CatalogSnapshot, EndpointDescriptor, GraphDescriptor, GraphIdentity, GraphKind,
    IndexDescriptor, MacroDescriptor, NodeTableDescriptor, RelationshipTableDescriptor,
    SessionSnapshot, SettingDescriptor, TransactionMode,
};
use crate::{Error, LogicalType, Result};

impl Connection {
    /// Capture authoritative immutable session state under the connection's
    /// ordinary serialization boundary.
    pub fn session_snapshot(&self) -> Result<SessionSnapshot> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let database = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut connection = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let graph = selected_graph_for_snapshot(&database, &mut connection);
        Ok(make_session_snapshot(&database, &connection, &graph))
    }

    /// Capture one coherent, owned catalog/completion view.
    pub fn catalog_snapshot(&self) -> Result<CatalogSnapshot> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let database = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut connection = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let graph = selected_graph_for_snapshot(&database, &mut connection);
        let owned;
        let view = if let Some(transaction) = &connection.txn {
            &transaction.snapshot
        } else {
            owned = graph.snapshot();
            &owned
        };
        let (_, function_revision) = self.scalar_udf_snapshot();
        let scalar_udfs = self
            .scalar_udfs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(build_catalog_snapshot(
            &database,
            &connection,
            &graph,
            view,
            &scalar_udfs.entries,
            function_revision,
        ))
    }

    /// Capture one graph's coherent owned catalog view without changing the
    /// connection's selected graph or transaction state.
    pub fn catalog_snapshot_for_graph(&self, graph_name: &str) -> Result<CatalogSnapshot> {
        let _execution = self
            .execution
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let database = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let connection = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let graph_id = database
            .graphs
            .id(graph_name)
            .ok_or_else(|| Error::catalog(format!("Graph {graph_name} does not exist.")))?;
        let graph = database
            .graphs
            .graph(graph_id)
            .expect("registered graph identity resolves");
        let owned;
        let view = if let Some(transaction) = connection
            .txn
            .as_ref()
            .filter(|transaction| transaction.graph.id == graph.id)
        {
            &transaction.snapshot
        } else {
            owned = graph.snapshot();
            &owned
        };
        let (_, function_revision) = self.scalar_udf_snapshot();
        let scalar_udfs = self
            .scalar_udfs
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(build_catalog_snapshot(
            &database,
            &connection,
            &graph,
            view,
            &scalar_udfs.entries,
            function_revision,
        ))
    }
}

fn selected_graph_for_snapshot(
    database: &DatabaseState,
    connection: &mut ConnectionState,
) -> std::sync::Arc<GraphState> {
    if let Some(transaction) = &connection.txn {
        return std::sync::Arc::clone(&transaction.graph);
    }
    if let Some(graph) = database.graphs.graph(connection.selected_graph) {
        return graph;
    }
    connection.selected_graph = MAIN_GRAPH_ID;
    connection.selected_graph_name = "main".to_string();
    connection.bump_revision();
    database
        .graphs
        .graph(MAIN_GRAPH_ID)
        .expect("main graph is always present")
}

fn make_session_snapshot(
    database: &DatabaseState,
    connection: &ConnectionState,
    graph: &GraphState,
) -> SessionSnapshot {
    let (transaction, catalog_revision) = connection.txn.as_ref().map_or_else(
        || (TransactionMode::None, graph.snapshot().catalog_version),
        |transaction| {
            (
                if transaction.writer_id.is_some() {
                    TransactionMode::ReadWrite
                } else {
                    TransactionMode::ReadOnly
                },
                transaction.catalog_version + u64::from(transaction.catalog_dirty),
            )
        },
    );
    let timeout_ms = connection
        .settings
        .current("timeout")
        .as_int128()
        .unwrap_or_default()
        .max(0) as u64;
    let workers = connection
        .settings
        .current("threads")
        .as_int128()
        .unwrap_or(1)
        .max(1) as usize;
    SessionSnapshot {
        revision: connection.revision,
        graph: graph_descriptor(graph),
        transaction,
        timeout: (timeout_ms != 0).then(|| std::time::Duration::from_millis(timeout_ms)),
        workers,
        catalog_revision,
        graph_registry_revision: database.graphs.generation,
    }
}

fn build_catalog_snapshot(
    database: &DatabaseState,
    connection: &ConnectionState,
    graph: &GraphState,
    view: &GraphData,
    scalar_udfs: &std::collections::HashMap<
        String,
        std::sync::Arc<koko_common::RegisteredScalarFunction>,
    >,
    function_revision: u64,
) -> CatalogSnapshot {
    let catalog = &view.catalog;
    let mut graphs: Vec<_> = database
        .graphs
        .by_id
        .values()
        .map(|graph| graph_descriptor(graph))
        .collect();
    graphs.sort_by_key(|graph| graph.identity);

    let node_tables = catalog
        .node_table_ids()
        .into_iter()
        .filter(|identity| !catalog.is_any_node_table(*identity))
        .filter_map(|identity| catalog.node_table(identity))
        .map(|table| NodeTableDescriptor {
            identity: table.id().0,
            name: table.name().to_string(),
            properties: table
                .columns()
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    tooling::property_descriptor(column, index == table.primary_key_index())
                })
                .collect(),
            comment: table.comment().map(str::to_string),
        })
        .collect();
    let relationship_tables = catalog
        .rel_table_ids()
        .into_iter()
        .filter(|identity| !catalog.is_any_rel_table(*identity))
        .filter_map(|identity| catalog.rel_table(identity))
        .map(|table| RelationshipTableDescriptor {
            identity: table.id().0,
            name: table.name().to_string(),
            properties: table
                .columns()
                .iter()
                .map(|column| tooling::property_descriptor(column, false))
                .collect(),
            endpoints: table
                .pairs()
                .iter()
                .map(|pair| EndpointDescriptor {
                    from: catalog.table_name(pair.from).unwrap_or("?").to_string(),
                    to: catalog.table_name(pair.to).unwrap_or("?").to_string(),
                })
                .collect(),
            storage_direction: table.storage_direction().as_str().to_string(),
            comment: table.comment().map(str::to_string),
        })
        .collect();
    let indexes = catalog
        .indexes()
        .into_iter()
        .map(|index| IndexDescriptor {
            name: index.name().to_string(),
            table: catalog
                .table_name(index.table_id())
                .unwrap_or("?")
                .to_string(),
            index_type: index.index_type().name().to_string(),
            properties: index.property_names().to_vec(),
        })
        .collect();
    let mut macros: Vec<_> = view
        .macros
        .iter()
        .map(|(name, definition)| {
            let mut parameters = definition.positional.clone();
            parameters.extend(
                definition
                    .defaults
                    .iter()
                    .map(|(name, _)| format!("{name} := ...")),
            );
            MacroDescriptor {
                name: name.clone(),
                signature: format!("({})", parameters.join(", ")),
                body: koko_parser::expr_to_cypher(&definition.body),
            }
        })
        .collect();
    macros.sort_by(|left, right| left.name.cmp(&right.name));
    let functions = tooling::function_descriptors(&macros, scalar_udfs);
    let settings = RUNTIME_SETTING_SPECS
        .iter()
        .map(|(name, logical_type)| SettingDescriptor {
            name: (*name).to_string(),
            logical_type: logical_type.clone(),
            current_value: connection.settings.current(name),
            accepted_values: match logical_type {
                LogicalType::Bool => vec!["true".to_string(), "false".to_string()],
                _ => vec![logical_type.to_string()],
            },
        })
        .collect();
    CatalogSnapshot {
        graph_registry_revision: database.graphs.generation,
        catalog_revision: connection
            .txn
            .as_ref()
            .filter(|transaction| transaction.graph.id == graph.id)
            .map_or(view.catalog_version, |transaction| {
                transaction.catalog_version + u64::from(transaction.catalog_dirty)
            }),
        function_revision,
        selected_graph: GraphIdentity(graph.id.0),
        graphs,
        node_tables,
        relationship_tables,
        indexes,
        macros,
        functions,
        settings,
        schema_script: crate::interchange::schema_script(
            &crate::interchange::InterchangeReadContext::new(
                &view.catalog,
                &view.storage,
                &view.macros,
                &view.memory,
            ),
        ),
    }
}

fn graph_descriptor(graph: &GraphState) -> GraphDescriptor {
    GraphDescriptor {
        identity: GraphIdentity(graph.id.0),
        name: graph.name.clone(),
        kind: match graph.kind {
            koko_parser::ast::GraphKind::Typed => GraphKind::Typed,
            koko_parser::ast::GraphKind::Any => GraphKind::Any,
        },
    }
}
