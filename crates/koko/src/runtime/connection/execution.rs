//! Canonical bind-plan-execute funnel and statement-scoped execution state.

use super::QueryParameter;
use crate::copy::{CopyOperationContext, run_copy, run_copy_from_rows};
use crate::interchange;
use crate::macros::{self, MacroDef, MacroRegistry};
use crate::result;
use crate::runtime::context::{QueryContext, READ_ONLY_WRITE_MSG, RUNTIME_SETTING_SPECS};
use crate::runtime::graph::GraphData;
use crate::{
    DataChunk, DatabaseConfig, Error, InternalId, LogicalType, MemoryTracker, MemoryUsage,
    QueryResult, QueryResultKind, Result, TableId, Value,
};
use koko_binder::{BoundAlterOp, BoundColumnDefault, BoundStatement, bind_statement};
use koko_catalog::{Catalog, ColumnDefault};
use koko_common::{ReadView, Ts, VECTOR_CAPACITY};
use koko_storage::{SharedStorage, StorageReadHandle, StorageWriteHandle};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn option_input_type(key: &str) -> Option<LogicalType> {
    RUNTIME_SETTING_SPECS
        .iter()
        .find_map(|(name, logical_type)| (*name == key).then(|| logical_type.clone()))
}

/// Enforce an exhaustive contract for every accepted `CALL k=v` option.
///
/// Operational settings are consumed by binding or execution (`threads`, warning/depth/map/
/// optimizer controls, multi-writer admission, and IM3 path settings). The remaining accepted
/// names are explicit compatibility metadata for subsystems outside the current phase; they
/// round-trip through `current_setting` but must not become hidden controls. Unknown names are
/// rejected by [`option_input_type`].
fn validate_runtime_setting(key: &str, _value: &Value) -> Result<()> {
    match key {
        "threads"
        | "warning_limit"
        | "var_length_extend_max_depth"
        | "disable_map_key_check"
        | "enable_plan_optimizer"
        | "debug_enable_multi_writes"
        | "timeout"
        | "home_directory"
        | "file_search_path"
        | "recursive_pattern_semantic" => Ok(()),
        "progress_bar"
        | "sparse_frontier_threshold"
        | "recursive_pattern_factor"
        | "checkpoint_threshold"
        | "enable_semi_mask"
        | "enable_zone_map"
        | "auto_checkpoint"
        | "force_checkpoint_on_close"
        | "enable_default_hash_index"
        | "spill_to_disk"
        | "enable_internal_catalog" => Ok(()),
        _ => Err(Error::binder(format!("Invalid option name: {key}."))),
    }
}

/// Snapshot per-table statistics for the cost-based optimizer (P3 step 8): the
/// committed [`TableStats`](koko_common::stats::TableStats) of every node and rel
/// table. Cheap (a few tables) and rebuilt per query so plans see current cardinalities.
fn collect_stats(
    catalog: &Catalog,
    storage: &SharedStorage,
    read: StorageReadHandle,
) -> koko_planner::StatsMap {
    let guard = storage.read();
    let mut map = koko_planner::StatsMap::new();
    for id in catalog
        .node_table_ids()
        .into_iter()
        .chain(catalog.rel_table_ids())
    {
        if let Some(source) = catalog
            .icebug_table(id)
            .and_then(|table| table.source.as_ref())
        {
            let num_columns = catalog
                .node_table(id)
                .map(|table| table.columns.len())
                .or_else(|| catalog.rel_table(id).map(|table| table.columns.len()))
                .unwrap_or(0);
            map.insert(
                id,
                koko_common::stats::TableStats::with_row_count(num_columns, source.num_rows()),
            );
            continue;
        }
        if let Some(s) = guard.table_stats(read, id) {
            map.insert(id, s);
        }
    }
    map
}

struct TableFunctionContext<'a> {
    query: &'a QueryContext,
    macros: &'a MacroRegistry,
    memory: &'a MemoryTracker,
    stats: koko_planner::StatsMap,
}

impl koko_binder::TableFuncRuntime for TableFunctionContext<'_> {
    fn current_setting(&self, key: &str) -> Value {
        self.query.settings.current(key)
    }

    fn warning_rows(&self) -> Vec<Vec<Value>> {
        self.query
            .warnings
            .all()
            .into_iter()
            .map(|w| {
                vec![
                    Value::IntX {
                        value: w.query_id as i128,
                        kind: koko_common::IntKind::U64,
                    },
                    Value::String(w.message),
                    Value::String(w.file_path),
                    Value::IntX {
                        value: w.line_number as i128,
                        kind: koko_common::IntKind::U64,
                    },
                    Value::String(w.skipped_line_or_record),
                ]
            })
            .collect()
    }

    fn show_table_rows(&self) -> Option<Vec<Vec<Value>>> {
        self.query.show_table_rows.clone()
    }

    fn clear_warnings(&self) {
        self.query.warnings.clear();
    }

    fn macro_rows(&self) -> Vec<Vec<Value>> {
        self.macros
            .iter()
            .map(|(upper_name, def)| {
                let mut params = def.positional.clone();
                for (pname, dexpr) in &def.defaults {
                    params.push(format!("{pname}:={}", koko_parser::expr_to_cypher(dexpr)));
                }
                let definition = format!(
                    "CREATE MACRO `{upper_name}` ({}) AS {};",
                    params.join(","),
                    koko_parser::expr_to_cypher(&def.body)
                );
                vec![Value::String(upper_name.clone()), Value::String(definition)]
            })
            .collect()
    }

    fn memory_usage(&self) -> MemoryUsage {
        self.memory.usage()
    }
    fn table_stats(&self, table_id: TableId) -> Option<koko_common::TableStats> {
        self.stats.get(&table_id).cloned()
    }
}

impl GraphData {
    /// Each rel table's current size — the base for the 2^62 created-rel display:
    /// a rel whose dense offset is at or past its table's base was created since.
    pub(super) fn rel_table_bases(&self) -> HashMap<TableId, u64> {
        self.catalog
            .rel_table_ids()
            .into_iter()
            .map(|id| (id, self.storage.rel_count(id)))
            .collect()
    }

    fn reserve_catalog_name(&self, name: &str, view: ReadView) -> Result<()> {
        let Some(writer) = view.writer_id else {
            return Ok(());
        };
        let key = name.to_ascii_lowercase();
        let mut reservations = self
            .catalog_writers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if reservations.get(&key).is_some_and(|owner| *owner != writer) {
            return Err(Error::catalog(format!(
                "Write-write conflict on creating catalog entry with name {name}."
            )));
        }
        reservations.insert(key, writer);
        Ok(())
    }

    pub(super) fn release_catalog_writes(&self, writer: Ts) {
        self.catalog_writers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|_, owner| *owner != writer);
    }

    /// Bind and execute one non-control statement (DDL / `COPY` / query).
    ///
    /// `rel_base` maps each rel table to the size below which rels are "already
    /// there" for 2^62-offset purposes (the transaction's — or auto-commit
    /// statement's — start size); a query result's rels at/above it were created in
    /// this transaction and render with the transaction-local 2^62 offset. Empty
    /// when no created rel can appear (a read with no active write transaction).
    pub(super) fn run_statement(
        &mut self,
        stmt: &koko_parser::ast::Statement,
        rel_base: &HashMap<TableId, u64>,
        query: &mut QueryContext,
    ) -> Result<QueryResult> {
        // `CALL` is not bound like a query: a config knob mutates committed session
        // state (never snapshotted), while an introspection table function reads
        // the *selected* catalog (so it observes this transaction's own DDL). Both
        // run against `self`, which the connection has pointed at the right state.
        if let koko_parser::ast::Statement::Call(call) = stmt {
            return self.handle_call(call, query);
        }
        // CREATE/DROP MACRO mutate the (snapshotted) macro registry directly — there
        // is no bound form and the binder/catalog never see macros. Handled here, like
        // `Call`, so they run against the registry the connection has selected (the
        // transaction's when inside one — `self.macros` was swapped in by the caller).
        // Regular queries retain a structural plan. For retained shell compatibility,
        // EXPLAIN only validates other statement classes while PROFILE executes them.
        if let koko_parser::ast::Statement::Explain { inner, profile } = stmt {
            let compilation_started = Instant::now();
            let expanded;
            let inner = if self.macros.is_empty() {
                inner.as_ref()
            } else {
                expanded = macros::expand_statement(inner, &self.macros)?;
                &expanded
            };
            if let koko_parser::ast::Statement::Call(call) = inner {
                if *profile {
                    return self.run_statement(inner, rel_base, query);
                }
                self.validate_call(call, query)?;
                return Ok(QueryResult::default());
            }
            let binder_config = query.binder_config();
            let bound = bind_statement(&self.catalog, inner, &query.parameters, &binder_config)?;
            let BoundStatement::Query(regular_query) = bound else {
                query.compilation_time += compilation_started.elapsed();
                if *profile {
                    return self.run_statement(inner, rel_base, query);
                }
                return Ok(QueryResult::default());
            };
            query.compilation_time += compilation_started.elapsed();
            let plan = self.prepare_regular_plan(&regular_query, query)?;
            let presentation = result::plan_presentation(&plan, *profile);
            let mut result = if *profile {
                let mut execution =
                    execute_regular_with_context(self, &regular_query, &plan, query)?;
                if !rel_base.is_empty() {
                    remap_created_rel_ids(&mut execution, rel_base);
                }
                QueryResult::from_exec(execution)
            } else {
                QueryResult::default()
            };
            let kind = if *profile {
                QueryResultKind::Profile
            } else {
                QueryResultKind::Explain
            };
            result.configure_explain(
                kind,
                result::capture_result_type_context(&self.catalog, self.catalog_version),
                presentation,
            );
            return Ok(result);
        }
        match stmt {
            koko_parser::ast::Statement::CreateMacro(m) => {
                if query.scalar_udfs.contains_key(&m.name.to_ascii_lowercase()) {
                    return Err(Error::catalog(format!(
                        "Macro {} collides with a connection-local scalar function.",
                        m.name
                    )));
                }
                self.reserve_catalog_name(&m.name, query.view)?;
                return self.register_macro(m);
            }
            koko_parser::ast::Statement::DropMacro { name, if_exists } => {
                self.reserve_catalog_name(name, query.view)?;
                return self.remove_macro(name, *if_exists);
            }
            _ => {}
        }
        let compilation_started = Instant::now();
        // Expand any macro calls to plain expressions before binding. Skipped when no
        // macros are defined (the common case) so an ordinary query isn't cloned.
        let expanded;
        let stmt = if self.macros.is_empty() {
            stmt
        } else {
            expanded = macros::expand_statement(stmt, &self.macros)?;
            &expanded
        };
        let binder_config = query.binder_config();
        let bound = bind_statement(&self.catalog, stmt, &query.parameters, &binder_config)?;
        query.compilation_time += compilation_started.elapsed();
        match bound {
            BoundStatement::CreateNodeTable {
                name,
                columns,
                defaults,
                metadata,
                primary_key,
                if_not_exists,
                serial_columns,
                icebug_storage,
            } => {
                if if_not_exists && self.catalog.contains_table(&name) {
                    return Ok(QueryResult::message(format!(
                        "Table {name} already exists."
                    )));
                }
                self.reserve_catalog_name(&name, query.view)?;
                let column_types: Vec<_> = columns.iter().map(|(_, ty)| ty.clone()).collect();
                let pk_col = columns
                    .iter()
                    .position(|(n, _)| n.eq_ignore_ascii_case(&primary_key))
                    .expect("binder validated the primary key column");
                // Fold constant `DEFAULT`s to values before they reach the catalog.
                let defaults = resolve_column_defaults(defaults)?;
                let GraphData {
                    catalog, storage, ..
                } = self;
                let type_texts: Vec<_> = metadata.iter().map(|m| m.type_text.clone()).collect();
                let default_texts: Vec<_> =
                    metadata.iter().map(|m| m.default_text.clone()).collect();
                let icebug = match icebug_storage.as_deref() {
                    Some(storage_root) if koko_loader::icebug::is_remote(storage_root) => Some((
                        None,
                        Some(koko_loader::icebug::deferred_node_error(
                            storage_root,
                            &name,
                        )),
                    )),
                    Some(storage_root) => Some((
                        Some(koko_loader::icebug::inspect_node_table(
                            &name,
                            &columns,
                            storage_root,
                        )?),
                        None,
                    )),
                    None => None,
                };
                let table_id = Arc::make_mut(catalog).create_node_table_with_metadata(
                    &name,
                    columns,
                    &defaults,
                    &type_texts,
                    &default_texts,
                    &serial_columns,
                    &primary_key,
                )?;
                let write = query.storage_write()?;
                storage.create_node_table(write, table_id, &column_types, pk_col);
                if let (Some(storage_root), Some((source, load_error))) = (icebug_storage, icebug) {
                    storage.mark_icebug_node_table(table_id);
                    Arc::make_mut(catalog).mark_icebug_table(
                        table_id,
                        storage_root,
                        source,
                        load_error,
                    );
                }
                Ok(QueryResult::message(format!(
                    "Table {name} has been created."
                )))
            }
            BoundStatement::CreateRelTable {
                name,
                pairs,
                columns,
                defaults,
                metadata,
                if_not_exists,
                multiplicity,
                storage_direction,
                icebug_storage,
            } => {
                if if_not_exists && self.catalog.contains_table(&name) {
                    return Ok(QueryResult::message(format!(
                        "Table {name} already exists."
                    )));
                }
                self.reserve_catalog_name(&name, query.view)?;
                let column_types: Vec<_> = columns.iter().map(|(_, ty)| ty.clone()).collect();
                let defaults = resolve_column_defaults(defaults)?;
                let GraphData {
                    catalog, storage, ..
                } = self;
                let type_texts: Vec<_> = metadata.iter().map(|m| m.type_text.clone()).collect();
                let default_texts: Vec<_> =
                    metadata.iter().map(|m| m.default_text.clone()).collect();
                let serial_columns: Vec<usize> = columns
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, ty))| *ty == LogicalType::Serial)
                    .map(|(i, _)| i)
                    .collect();
                let icebug = match icebug_storage.as_deref() {
                    Some(storage_root) if koko_loader::icebug::is_remote(storage_root) => Some((
                        None,
                        Some(koko_loader::icebug::deferred_rel_error(storage_root, &name)),
                    )),
                    Some(storage_root) => {
                        let from = pairs
                            .first()
                            .map(|pair| pair.0)
                            .expect("relationship has one endpoint pair");
                        let from_count = catalog
                            .icebug_table(from)
                            .and_then(|table| table.source.as_ref())
                            .map(koko_catalog::IcebugTableSource::num_rows)
                            .unwrap_or_else(|| storage.node_count(from));
                        Some((
                            Some(koko_loader::icebug::inspect_rel_table(
                                &name,
                                &columns,
                                storage_root,
                                from_count,
                            )?),
                            None,
                        ))
                    }
                    None => None,
                };
                let table_id = Arc::make_mut(catalog).create_rel_table_with_serials(
                    &name,
                    &pairs,
                    columns,
                    &defaults,
                    &type_texts,
                    &default_texts,
                    &serial_columns,
                    storage_direction,
                )?;
                // One physical store per (FROM,TO) pair (matching C++/Kùzu): a rel's
                // `_ID` carries its pair's member id. A single-pair rel is exactly one
                // store at the primary id — unchanged. Each store also holds the name +
                // multiplicity to enforce the constraint on insert.
                let write = query.storage_write()?;
                let members = catalog.rel_members(table_id);
                for &(member, from, to) in &members {
                    storage.create_rel_table(
                        write,
                        member,
                        from,
                        to,
                        &column_types,
                        &name,
                        multiplicity,
                    );
                }
                if let (Some(storage_root), Some((source, load_error))) = (icebug_storage, icebug) {
                    let [(member, _, _)] = members.as_slice() else {
                        return Err(Error::binder(
                            "An icebug-disk relationship table requires exactly one FROM-TO pair.",
                        ));
                    };
                    storage.mark_icebug_rel_table(*member);
                    let catalog = Arc::make_mut(catalog);
                    catalog.mark_icebug_table(
                        table_id,
                        storage_root.clone(),
                        source.clone(),
                        load_error.clone(),
                    );
                    if *member != table_id {
                        catalog.mark_icebug_table(*member, storage_root, source, load_error);
                    }
                }
                Ok(QueryResult::message(format!(
                    "Table {name} has been created."
                )))
            }
            BoundStatement::DropTable { name, table } => {
                let Some(id) = table else {
                    // Absent table under `IF EXISTS`: a skip, reported as a message.
                    return Ok(QueryResult::message(format!(
                        "Table {name} does not exist."
                    )));
                };
                self.reserve_catalog_name(&name, query.view)?;
                // A rel group is several per-pair stores; capture them before the catalog
                // forgets the group, then drop each. A node table is a single store.
                let rel_members: Vec<_> = self
                    .catalog
                    .rel_members(id)
                    .into_iter()
                    .map(|(m, _, _)| m)
                    .collect();
                Arc::make_mut(&mut self.catalog).drop_table(id);
                if rel_members.is_empty() {
                    self.storage.drop_table(query.storage_write()?, id);
                } else {
                    for m in rel_members {
                        self.storage.drop_table(query.storage_write()?, m);
                    }
                }
                Ok(QueryResult::message(format!(
                    "Table {name} has been dropped."
                )))
            }
            BoundStatement::Alter {
                table,
                table_name,
                op,
            } => self.run_alter(table, &table_name, op, query.view),
            BoundStatement::CreateSequence {
                name,
                if_not_exists,
                start,
                increment,
                min,
                max,
                cycle,
            } => {
                if if_not_exists && self.catalog.contains_sequence(&name) {
                    return Ok(QueryResult::message(format!(
                        "Sequence {name} already exists."
                    )));
                }
                self.reserve_catalog_name(&name, query.view)?;
                let seq =
                    koko_catalog::Sequence::new(name.clone(), start, increment, min, max, cycle);
                // Errors with "{name} already exists in catalog." on a duplicate
                // (the binder already rejected that path when not IF NOT EXISTS).
                Arc::make_mut(&mut self.catalog).create_sequence(seq)?;
                Ok(QueryResult::message(format!(
                    "Sequence {name} has been created."
                )))
            }
            BoundStatement::DropSequence { name, if_exists } => {
                self.reserve_catalog_name(&name, query.view)?;
                if Arc::make_mut(&mut self.catalog).drop_sequence(&name) {
                    Ok(QueryResult::message(format!(
                        "Sequence {name} has been dropped."
                    )))
                } else {
                    // Reachable only via IF EXISTS (the binder errors otherwise).
                    debug_assert!(if_exists);
                    Ok(QueryResult::message(format!(
                        "Sequence {name} does not exist."
                    )))
                }
            }
            BoundStatement::Comment {
                table,
                table_name,
                comment,
            } => {
                self.reserve_catalog_name(&table_name, query.view)?;
                Arc::make_mut(&mut self.catalog).set_comment(table, comment);
                Ok(QueryResult::message(format!(
                    "Comment added to table {table_name}."
                )))
            }
            BoundStatement::CreateType { name, ty } => {
                self.reserve_catalog_name(&name, query.view)?;
                Arc::make_mut(&mut self.catalog).create_user_type(&name, ty.clone());
                Ok(QueryResult::message(format!(
                    "Type {name}({ty}) has been created."
                )))
            }
            BoundStatement::CreateTableAs {
                name,
                is_node,
                pairs,
                storage_direction,
                if_not_exists,
                columns,
                query: source_query,
            } => {
                if if_not_exists && self.catalog.contains_table(&name) {
                    return Ok(QueryResult::message(format!(
                        "Table {name} already exists."
                    )));
                }
                self.reserve_catalog_name(&name, query.view)?;
                // Run the query against the current catalog *before* the new (empty)
                // table exists, so an unlabeled scan in the query doesn't see it.
                // Cost-based join order reads the executing connection's immutable
                // settings snapshot.
                let stats = if query.optimizer_enabled() {
                    collect_stats(&self.catalog, &self.storage, query.storage_read())
                } else {
                    koko_planner::StatsMap::new()
                };
                let mut plan = koko_planner::plan_regular(&source_query, &self.catalog, &stats)?;
                if query.optimizer_enabled() {
                    koko_planner::optimize_regular(&source_query, &mut plan, &self.catalog, &stats);
                }
                let rows =
                    execute_regular_with_context(self, &source_query, &plan, query)?.into_rows();
                if is_node {
                    // The first result column becomes the primary key.
                    let pk_name = columns[0].0.clone();
                    let column_types: Vec<_> = columns.iter().map(|(_, ty)| ty.clone()).collect();
                    let GraphData {
                        catalog, storage, ..
                    } = self;
                    let table_id = Arc::make_mut(catalog).create_node_table(
                        &name,
                        columns,
                        &[],
                        &[],
                        &pk_name,
                    )?;
                    storage.create_node_table(query.storage_write()?, table_id, &column_types, 0);
                    // CTAS ingests through the bulk-copy path: constraint
                    // violations (duplicate PK etc.) are Copy exceptions.
                    for rows in rows.chunks(VECTOR_CAPACITY) {
                        let mut batch = DataChunk::new(&column_types);
                        for (position, row) in rows.iter().enumerate() {
                            for (column, value) in batch.columns.iter_mut().zip(row) {
                                column.set_value(position, value);
                            }
                        }
                        batch.set_flat(rows.len());
                        for result in storage.insert_node_batch(
                            query.storage_write()?,
                            table_id,
                            &batch,
                            false,
                        ) {
                            result.map_err(|error| match error {
                                Error::Runtime(message) => Error::Copy(message),
                                other => other,
                            })?;
                        }
                    }
                } else {
                    // Rel CTAS: the first two result columns are the FROM/TO endpoint
                    // *keys* (matched against each endpoint table's primary key). C++
                    // still keeps every query column, including those keys, as rel
                    // properties, so the inserted property row is the full result row.
                    if columns.len() < 2 {
                        return Err(Error::binder(
                            "Binder exception: CREATE REL TABLE ... AS requires the query to \
                             return a FROM column and a TO column."
                                .to_string(),
                        ));
                    }
                    let (from0, to0) = pairs[0];
                    let props = columns;
                    let column_types: Vec<_> = props.iter().map(|(_, ty)| ty.clone()).collect();
                    let table_id = Arc::make_mut(&mut self.catalog)
                        .create_rel_table_with_metadata(
                            &name,
                            &pairs,
                            props,
                            &[],
                            &[],
                            &[],
                            storage_direction,
                        )?;
                    // Rel CTAS has no multiplicity keyword → unconstrained. One store per
                    // pair (single-pair CTAS = one store at the primary id, unchanged).
                    for (member, from, to) in self.catalog.rel_members(table_id) {
                        self.storage.create_rel_table(
                            query.storage_write()?,
                            member,
                            from,
                            to,
                            &column_types,
                            &name,
                            koko_common::RelMultiplicity::default(),
                        );
                    }
                    let mut batch_types = vec![LogicalType::InternalId, LogicalType::InternalId];
                    batch_types.extend(column_types);
                    for rows in rows.chunks(VECTOR_CAPACITY) {
                        let mut batch = DataChunk::new(&batch_types);
                        for (position, row) in rows.iter().enumerate() {
                            let src =
                                self.find_ctas_endpoint(from0, &row[0], query.storage_read())?;
                            let dst =
                                self.find_ctas_endpoint(to0, &row[1], query.storage_read())?;
                            batch.columns[0].set_internal_id(position, src);
                            batch.columns[1].set_internal_id(position, dst);
                            for (column, value) in batch.columns[2..].iter_mut().zip(row) {
                                column.set_value(position, value);
                            }
                        }
                        batch.set_flat(rows.len());
                        for result in self.storage.insert_rel_batch(
                            query.storage_write()?,
                            table_id,
                            &batch,
                            false,
                        ) {
                            result?;
                        }
                    }
                }
                Ok(QueryResult::message(format!(
                    "Table {name} has been created."
                )))
            }
            BoundStatement::Copy(copy) => {
                // File-backed COPY paths were canonicalized by the binder.
                // The done-message names the table like C++ (audit R7).
                let name = self
                    .catalog
                    .node_table(copy.table)
                    .map(|t| t.name.clone())
                    .or_else(|| self.catalog.rel_table(copy.table).map(|t| t.name.clone()))
                    .unwrap_or_default();
                let qid = query.warnings.query_id();
                let rows = if let Some(source) = copy.source_query.as_deref() {
                    Some(execute_copy_source(self, source, query)?)
                } else {
                    None
                };
                let mut adapter = CopyOperationContext::new(
                    &self.catalog,
                    &self.storage,
                    &self.memory,
                    query.storage_read(),
                    query.storage_write()?,
                    &query.warnings,
                    query.worker_count(),
                    query.query_control(),
                );
                let n = if let Some(rows) = rows {
                    run_copy_from_rows(&mut adapter, &copy, rows)?
                } else {
                    run_copy(&mut adapter, &copy)?
                };
                let warned = query.warnings.count();
                if warned > 0 {
                    let copied = format!("{n} tuples have been copied to the {name} table.");
                    let warning = format!(
                        "{warned} warnings encountered during copy. Use 'CALL \
                         show_warnings() RETURN *' to view the actual warnings. Query \
                         ID: {qid}"
                    );
                    let mut result = QueryResult::from_typed_rows(
                        col_names(&["result"]),
                        vec![LogicalType::String],
                        vec![
                            vec![Value::String(copied.clone())],
                            vec![Value::String(warning.clone())],
                        ],
                    );
                    result.configure_status(format!("{copied}\n{warning}"));
                    return Ok(result);
                }
                Ok(QueryResult::message(format!(
                    "{n} tuples have been copied to the {name} table."
                )))
            }
            BoundStatement::CopyTo(copy) => {
                let mut result = self.run_regular_query(&copy.query, rel_base, query)?;
                let memory = self.memory.clone();
                result.track_memory(&memory)?;
                let rows = interchange::write_query_result(
                    Path::new(&copy.path),
                    &copy.options,
                    &result,
                    &memory,
                    query.query_control(),
                )?;
                Ok(QueryResult::message(format!(
                    "{rows} tuples have been exported to {}.",
                    copy.path
                )))
            }
            BoundStatement::ExportDatabase(_) | BoundStatement::ImportDatabase(_) => Err(
                Error::runtime("Database interchange was not dispatched at the database layer."),
            ),
            BoundStatement::Query(rq) => self.run_regular_query(&rq, rel_base, query),
        }
    }

    fn run_regular_query(
        &mut self,
        query: &koko_binder::BoundRegularQuery,
        rel_base: &HashMap<TableId, u64>,
        context: &mut QueryContext,
    ) -> Result<QueryResult> {
        let plan = self.prepare_regular_plan(query, context)?;
        let mut result = execute_regular_with_context(self, query, &plan, context)?;
        if !rel_base.is_empty() {
            remap_created_rel_ids(&mut result, rel_base);
        }
        let mut result = QueryResult::from_exec(result);
        result.set_type_context(result::capture_result_type_context(
            &self.catalog,
            self.catalog_version,
        ));
        Ok(result)
    }

    fn prepare_regular_plan(
        &self,
        query: &koko_binder::BoundRegularQuery,
        context: &mut QueryContext,
    ) -> Result<koko_planner::RegularPlan> {
        let compilation_started = Instant::now();
        let stats = if context.optimizer_enabled() {
            collect_stats(&self.catalog, &self.storage, context.storage_read())
        } else {
            koko_planner::StatsMap::new()
        };
        let mut plan = koko_planner::plan_regular(query, &self.catalog, &stats)?;
        if context.optimizer_enabled() {
            koko_planner::optimize_regular(query, &mut plan, &self.catalog, &stats);
        }
        context.compilation_time += compilation_started.elapsed();
        Ok(plan)
    }

    /// Apply a bound `ALTER TABLE` op, performing the existence/conflict checks
    /// (which produce `Runtime exception`s or `IF`-guarded skip messages) and the
    /// catalog+storage mutation. The primary-key-drop check ran at bind time.
    fn run_alter(
        &mut self,
        table: TableId,
        table_name: &str,
        op: BoundAlterOp,
        view: ReadView,
    ) -> Result<QueryResult> {
        self.reserve_catalog_name(table_name, view)?;
        let read = StorageReadHandle::new(view);
        let writer_id = view
            .writer_id
            .ok_or_else(|| Error::transaction(READ_ONLY_WRITE_MSG))?;
        let write = StorageWriteHandle::new(view, writer_id);
        match op {
            BoundAlterOp::AddProperty {
                name,
                ty,
                default,
                metadata,
                if_not_exists,
            } => {
                if self.catalog.table_has_column(table, &name) {
                    let msg = format!("{table_name} table already has property {name}.");
                    return if if_not_exists {
                        Ok(QueryResult::message(msg))
                    } else {
                        Err(Error::runtime(msg))
                    };
                }
                let physical_tables: Vec<TableId> = if self.catalog.node_table(table).is_some() {
                    vec![table]
                } else {
                    self.catalog
                        .rel_members(table)
                        .into_iter()
                        .map(|(member, _, _)| member)
                        .collect()
                };
                let storage_type = ty.clone();
                match default {
                    BoundColumnDefault::None => {
                        Arc::make_mut(&mut self.catalog).add_column_with_metadata(
                            table,
                            &name,
                            ty,
                            ColumnDefault::None,
                            metadata.type_text,
                            metadata.default_text,
                        );
                        for physical_table in &physical_tables {
                            self.storage.add_column(
                                write,
                                *physical_table,
                                storage_type.clone(),
                                Value::Null,
                            )?;
                        }
                    }
                    BoundColumnDefault::Const(expr) => {
                        // Backfill every existing row with the one folded value.
                        let v = koko_processor::eval_constant(&expr)?;
                        Arc::make_mut(&mut self.catalog).add_column_with_metadata(
                            table,
                            &name,
                            ty,
                            ColumnDefault::Const(v.clone()),
                            metadata.type_text,
                            metadata.default_text,
                        );
                        for physical_table in &physical_tables {
                            self.storage.add_column(
                                write,
                                *physical_table,
                                storage_type.clone(),
                                v.clone(),
                            )?;
                        }
                    }
                    BoundColumnDefault::NextVal(seq) => {
                        // A sequence default backfills each existing row with its own
                        // `nextval`; the C++ engine forbids this on REL tables.
                        if self.catalog.node_table(table).is_none() {
                            return Err(Error::runtime(
                                "Cannot set a non-constant default value when adding columns on \
                                 REL tables."
                                    .to_string(),
                            ));
                        }
                        Arc::make_mut(&mut self.catalog).add_column_with_metadata(
                            table,
                            &name,
                            ty,
                            ColumnDefault::NextVal(seq.clone()),
                            metadata.type_text,
                            metadata.default_text,
                        );
                        let col_idx = self
                            .catalog
                            .column_index(table, &name)
                            .expect("just added the column");
                        self.storage
                            .add_column(write, table, storage_type, Value::Null)?;
                        let column_type = self
                            .catalog
                            .node_table(table)
                            .expect("node table checked above")
                            .columns[col_idx]
                            .ty
                            .clone();
                        let n = self.storage.node_count(table);
                        for start in (0..n).step_by(VECTOR_CAPACITY) {
                            let mut batch =
                                DataChunk::new(&[LogicalType::InternalId, column_type.clone()]);
                            let mut len = 0;
                            for off in start..(start + VECTOR_CAPACITY as u64).min(n) {
                                if !self.storage.node_is_deleted(read, table, off) {
                                    let value = self.catalog.sequence_next_val(&seq)?;
                                    batch.columns[0]
                                        .set_internal_id(len, InternalId::new(table, off));
                                    batch.columns[1].set_value_owned(len, Value::Int64(value));
                                    len += 1;
                                }
                            }
                            batch.set_flat(len);
                            self.storage
                                .set_node_property_batch(write, table, col_idx, &batch)?;
                        }
                    }
                }
                Ok(QueryResult::message(format!(
                    "Property {name} added to table {table_name}."
                )))
            }
            BoundAlterOp::DropProperty { name, if_exists } => {
                let Some(idx) = self.catalog.column_index(table, &name) else {
                    let msg = format!("{table_name} table does not have property {name}.");
                    return if if_exists {
                        Ok(QueryResult::message(msg))
                    } else {
                        Err(Error::runtime(msg))
                    };
                };
                let physical_tables: Vec<TableId> = if self.catalog.node_table(table).is_some() {
                    vec![table]
                } else {
                    self.catalog
                        .rel_members(table)
                        .into_iter()
                        .map(|(member, _, _)| member)
                        .collect()
                };
                Arc::make_mut(&mut self.catalog).drop_column(table, idx);
                for physical_table in physical_tables {
                    self.storage.drop_column(write, physical_table, idx);
                }
                Ok(QueryResult::message(format!(
                    "Property {name} has been dropped from table {table_name}."
                )))
            }
            BoundAlterOp::RenameProperty { old, new } => {
                let Some(idx) = self.catalog.column_index(table, &old) else {
                    return Err(Error::runtime(format!(
                        "{table_name} table does not have property {old}."
                    )));
                };
                if self.catalog.table_has_column(table, &new) {
                    return Err(Error::runtime(format!(
                        "{table_name} table already has property {new}."
                    )));
                }
                Arc::make_mut(&mut self.catalog).rename_column(table, idx, &new);
                Ok(QueryResult::message(format!(
                    "Property {old} renamed to {new}."
                )))
            }
            BoundAlterOp::RenameTable { new } => {
                self.reserve_catalog_name(&new, view)?;
                if self.catalog.contains_table(&new) {
                    return Err(Error::binder(format!("Table {new} already exists.")));
                }
                Arc::make_mut(&mut self.catalog).rename_table(table, &new);
                Ok(QueryResult::message(format!(
                    "Table {table_name} renamed to {new}."
                )))
            }
            BoundAlterOp::AddFromTo {
                from,
                to,
                if_not_exists,
            } => {
                let (from_name, to_name) = self.pair_names(from, to);
                if self.catalog.rel_has_pair(table, from, to) {
                    let msg =
                        format!("{from_name}->{to_name} already exists in {table_name} table.");
                    return if if_not_exists {
                        Ok(QueryResult::message(msg))
                    } else {
                        Err(Error::binder(msg))
                    };
                }
                // Inherit the group's multiplicity (all pairs share it), then create the
                // new pair's per-pair storage store so inserts can route to it.
                let existing_member = self.catalog.rel_members(table).first().map(|&(m, _, _)| m);
                let mult = existing_member
                    .map(|m| self.storage.rel_multiplicity(m))
                    .unwrap_or_default();
                if let Some(member) = Arc::make_mut(&mut self.catalog).add_rel_pair(table, from, to)
                {
                    let (column_types, rel_name) = {
                        let rt = self.catalog.rel_table(table).expect("rel table exists");
                        (
                            rt.columns
                                .iter()
                                .map(|column| column.ty.clone())
                                .collect::<Vec<_>>(),
                            rt.name.clone(),
                        )
                    };
                    self.storage.create_rel_table(
                        write,
                        member,
                        from,
                        to,
                        &column_types,
                        &rel_name,
                        mult,
                    );
                }
                Ok(QueryResult::message(format!(
                    "{from_name}->{to_name} added to table {table_name}."
                )))
            }
            BoundAlterOp::DropFromTo {
                from,
                to,
                if_exists,
            } => {
                let (from_name, to_name) = self.pair_names(from, to);
                if !self.catalog.rel_has_pair(table, from, to) {
                    let msg =
                        format!("{from_name}->{to_name} does not exist in {table_name} table.");
                    return if if_exists {
                        Ok(QueryResult::message(msg))
                    } else {
                        Err(Error::binder(msg))
                    };
                }
                if let Some(member) =
                    Arc::make_mut(&mut self.catalog).drop_rel_pair(table, from, to)
                {
                    self.storage.drop_table(write, member);
                }
                Ok(QueryResult::message(format!(
                    "{from_name}->{to_name} has been dropped from table {table_name}."
                )))
            }
        }
    }

    /// The canonical names of a FROM-TO node-table pair (for rel-group messages).
    fn pair_names(&self, from: TableId, to: TableId) -> (String, String) {
        let name = |id| self.catalog.table_name(id).unwrap_or_default().to_string();
        (name(from), name(to))
    }

    /// Resolve a rel-CTAS endpoint key (a value the query projected for the FROM/TO
    /// column, e.g. `a.id`) back to its node `InternalId` by matching the endpoint
    /// table's primary key.
    fn find_ctas_endpoint(
        &self,
        table: TableId,
        key: &Value,
        read: StorageReadHandle,
    ) -> Result<InternalId> {
        let lookup_key = if let Some(t) = self.catalog.node_table(table) {
            koko_function::cast_value(key, &t.primary_key_column().ty)?
        } else {
            key.clone()
        };
        self.storage
            .find_node_by_pk(read, table, &lookup_key)
            .ok_or_else(|| {
                // The bulk-copy wording/class (rel CTAS ingests like COPY).
                Error::copy(format!(
                    "Unable to find primary key value {}.",
                    lookup_key.to_result_string()
                ))
            })
    }

    /// Register a scalar macro. The name is stored uppercased (case-insensitive);
    /// a duplicate is a binder error matching the C++ wording.
    fn register_macro(&mut self, m: &koko_parser::ast::CreateMacro) -> Result<QueryResult> {
        let upper = m.name.to_uppercase();
        if self.macros.contains_key(&upper) {
            return Err(Error::binder(format!("Macro {upper} already exists.")));
        }
        Arc::make_mut(&mut self.macros).insert(
            upper.clone(),
            MacroDef {
                positional: m.positional.clone(),
                defaults: m.defaults.clone(),
                body: (*m.body).clone(),
            },
        );
        Ok(QueryResult::message(format!(
            "Macro: {upper} has been created."
        )))
    }

    /// Remove a scalar macro. The lookup is case-insensitive, but the error/skip
    /// message echoes the name **as typed** (matching the C++ oracle, e.g.
    /// `Macro add2 does not exist.`). A missing macro under `IF EXISTS` errors in
    /// C++ too — with the upstream "Marco" typo. The corpus is the contract, typo
    /// and all (ledger: docs/DIVERGENCES.md "drop-macro-if-exists").
    fn remove_macro(&mut self, name: &str, if_exists: bool) -> Result<QueryResult> {
        if Arc::make_mut(&mut self.macros)
            .remove(&name.to_uppercase())
            .is_none()
        {
            if if_exists {
                return Err(Error::catalog(format!("Marco {name} doesn't exist.")));
            }
            return Err(Error::binder(format!("Macro {name} does not exist.")));
        }
        Ok(QueryResult::message(format!(
            "Macro {name} has been dropped."
        )))
    }

    /// Handle a standalone `CALL`: set a session/config option, or read one with
    /// `current_setting`.
    fn handle_call(
        &mut self,
        call: &koko_parser::ast::CallStmt,
        query: &mut QueryContext,
    ) -> Result<QueryResult> {
        use koko_parser::ast::CallStmt;
        match call {
            CallStmt::SetConfig { key, value } => {
                let (key, value) = self.bind_config_update(key, value, query)?;
                // This compatibility test control changes database-wide writer
                // admission; every graph observes the same atomic policy.
                if let Some(enabled) = (key == "debug_enable_multi_writes")
                    .then(|| value.as_bool())
                    .flatten()
                {
                    self.multi_writes.store(enabled, Ordering::Release);
                }
                query.setting_update = Some((key, value));
                Ok(QueryResult::default())
            }
            CallStmt::TableFunc {
                func,
                arg,
                extra_args,
                has_return,
            } => {
                Self::validate_table_func_form(*func, *has_return)?;
                self.run_table_func(*func, arg.as_deref(), extra_args, query)
            }
        }
    }

    /// Validate a standalone `CALL` without applying settings or running a table function.
    fn validate_call(
        &self,
        call: &koko_parser::ast::CallStmt,
        query: &mut QueryContext,
    ) -> Result<()> {
        use koko_parser::ast::CallStmt;
        match call {
            CallStmt::SetConfig { key, value } => {
                self.bind_config_update(key, value, query)?;
            }
            CallStmt::TableFunc {
                func,
                arg,
                extra_args,
                has_return,
            } => {
                Self::validate_table_func_form(*func, *has_return)?;
                let compilation_started = Instant::now();
                koko_binder::table_func_schema(
                    &self.catalog,
                    koko_binder::BoundTableFunc::from(*func),
                    arg.as_deref(),
                    extra_args,
                )?;
                query.compilation_time += compilation_started.elapsed();
            }
        }
        Ok(())
    }

    fn bind_config_update(
        &self,
        key: &str,
        value: &koko_parser::ast::Expr,
        query: &mut QueryContext,
    ) -> Result<(String, Value)> {
        let key = key.to_ascii_lowercase();
        let Some(destination) = option_input_type(&key) else {
            return Err(Error::binder(format!("Invalid option name: {key}.")));
        };
        let compilation_started = Instant::now();
        let config = query.binder_config();
        let bound = koko_binder::bind_config_value(&self.catalog, value, &config, &destination)?;
        query.compilation_time += compilation_started.elapsed();
        let value = koko_processor::eval_constant(&bound)?;
        let invalid_semantic = (key == "recursive_pattern_semantic")
            .then(|| value.as_str())
            .flatten()
            .filter(|semantic| {
                !matches!(
                    semantic.to_ascii_uppercase().as_str(),
                    "WALK" | "TRAIL" | "ACYCLIC"
                )
            });
        if let Some(semantic) = invalid_semantic {
            return Err(Error::binder(format!(
                "Cannot parse {semantic} as a path semantic. Supported inputs are \
                 [WALK, TRAIL, ACYCLIC]"
            )));
        }
        validate_runtime_setting(&key, &value)?;
        let exceeded_worker_limit = self.config.max_workers().filter(|max_workers| {
            key == "threads"
                && value
                    .as_int128()
                    .is_some_and(|requested| requested > *max_workers as i128)
        });
        if let Some(max_workers) = exceeded_worker_limit {
            return Err(Error::configuration(format!(
                "Requested threads value {} exceeds the database max_workers limit of \
                 {max_workers}.",
                value.to_result_string()
            )));
        }
        Ok((key, value))
    }

    fn validate_table_func_form(func: koko_parser::ast::TableFunc, has_return: bool) -> Result<()> {
        if !has_return
            && !matches!(
                func,
                koko_parser::ast::TableFunc::CacheArrayColumn
                    | koko_parser::ast::TableFunc::ClearWarnings
            )
        {
            return Err(Error::binder(
                "Only standalone table functions can be called without return statement."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Run a catalog-introspection table function (the standalone `CALL` form),
    /// producing a row-set from catalog metadata. (`RETURN *` / `ORDER BY` are
    /// handled by the runner, which sorts rows.) The catalog-backed functions
    /// delegate their column schema and rows to the shared `koko-binder` helpers,
    /// so the standalone form and the in-query scan stay byte-exact;
    /// `show_macros` is produced inline (its rows live in the macro registry).
    fn run_table_func(
        &self,
        func: koko_parser::ast::TableFunc,
        arg: Option<&str>,
        extra_args: &[String],
        query: &mut QueryContext,
    ) -> Result<QueryResult> {
        let bound_func = koko_binder::BoundTableFunc::from(func);
        let compilation_started = Instant::now();
        let schema = koko_binder::table_func_schema(&self.catalog, bound_func, arg, extra_args)?;
        query.compilation_time += compilation_started.elapsed();
        let (names, types): (Vec<_>, Vec<_>) = schema.into_iter().unzip();
        let runtime = TableFunctionContext {
            query,
            macros: &self.macros,
            memory: &self.memory,
            stats: collect_stats(&self.catalog, &self.storage, query.storage_read()),
        };
        let rows = koko_binder::table_func_rows(&self.catalog, bound_func, arg, &runtime)?;
        Ok(QueryResult::from_typed_rows(names, types, rows))
    }
}

fn execute_copy_source(
    graph: &mut GraphData,
    source: &koko_binder::BoundRegularQuery,
    context: &QueryContext,
) -> Result<Vec<Vec<Value>>> {
    let stats = if context.optimizer_enabled() {
        collect_stats(&graph.catalog, &graph.storage, context.storage_read())
    } else {
        koko_planner::StatsMap::new()
    };
    let mut plan = koko_planner::plan_regular(source, &graph.catalog, &stats)?;
    if context.optimizer_enabled() {
        koko_planner::optimize_regular(source, &mut plan, &graph.catalog, &stats);
    }
    Ok(execute_regular_with_context(graph, source, &plan, context)?.into_rows())
}

pub(super) fn apply_interchange_image(
    graph: &mut GraphData,
    image: &crate::interchange::GraphImage,
    query: &mut QueryContext,
) -> Result<()> {
    let previous_base = query.base_dir.clone();
    query.base_dir = image.root().to_path_buf();
    let result: Result<()> = (|| {
        for statement in image.data_statements() {
            query.query_control().check()?;
            graph.run_statement(statement, &HashMap::new(), query)?;
        }
        for statement in image.index_statements() {
            query.query_control().check()?;
            apply_index_statement(Arc::make_mut(&mut graph.catalog), statement)?;
        }
        Ok(())
    })();
    query.base_dir = previous_base;
    result.map_err(|error| image.apply_error(error))
}

fn execute_regular_with_context(
    database: &mut GraphData,
    query: &koko_binder::BoundRegularQuery,
    plan: &koko_planner::RegularPlan,
    context: &QueryContext,
) -> Result<koko_processor::ExecResult> {
    let GraphData {
        catalog,
        storage,
        macros,
        memory,
        config,
        ..
    } = database;
    let table_functions = TableFunctionContext {
        query: context,
        macros,
        memory,
        stats: collect_stats(catalog, storage, context.storage_read()),
    };
    let operator_memory = koko_processor::QueryMemory::new(memory)?;
    let sources = koko_processor::QuerySourceState::capture(
        catalog,
        context.query_control(),
        &operator_memory,
    )?;
    let execution = koko_processor::ExecutionContext {
        storage_read: context.storage_read(),
        storage_write: context
            .view
            .writer_id
            .map(|writer_id| StorageWriteHandle::new(context.view, writer_id)),
        table_functions: &table_functions,
        random: &context.random,
        worker_count: context
            .worker_count()
            .min(config.max_workers().unwrap_or(usize::MAX)),
        warnings: &context.warnings,
        control: context.query_control(),
        memory: &operator_memory,
        sources: &sources,
    };
    koko_processor::execute_regular_synchronized(query, plan, catalog, storage, &execution)
}

/// Bind, plan, and execute one regular query against immutable catalog/macro
/// snapshots and shared versioned storage. No database-coordinator guard is held.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_regular_on_snapshot(
    stmt: &koko_parser::ast::Statement,
    catalog: &Catalog,
    catalog_version: u64,
    macros: &MacroRegistry,
    storage: &SharedStorage,
    memory: &MemoryTracker,
    config: &DatabaseConfig,
    rel_base: &HashMap<TableId, u64>,
    context: &mut QueryContext,
) -> Result<QueryResult> {
    let compilation_started = Instant::now();
    let expanded;
    let stmt = if macros.is_empty() {
        stmt
    } else {
        expanded = macros::expand_statement(stmt, macros)?;
        &expanded
    };
    let bound = bind_statement(catalog, stmt, &context.parameters, &context.binder_config())?;
    let BoundStatement::Query(query) = bound else {
        unreachable!("concurrent path accepts only regular queries");
    };
    let stats = if context.optimizer_enabled() {
        collect_stats(catalog, storage, context.storage_read())
    } else {
        koko_planner::StatsMap::new()
    };
    let mut plan = koko_planner::plan_regular(&query, catalog, &stats)?;
    if context.optimizer_enabled() {
        koko_planner::optimize_regular(&query, &mut plan, catalog, &stats);
    }
    context.compilation_time += compilation_started.elapsed();

    let table_functions = TableFunctionContext {
        query: context,
        macros,
        memory,
        stats,
    };
    let operator_memory = koko_processor::QueryMemory::new(memory)?;
    let sources = koko_processor::QuerySourceState::capture(
        catalog,
        context.query_control(),
        &operator_memory,
    )?;
    let execution = koko_processor::ExecutionContext {
        storage_read: context.storage_read(),
        storage_write: context
            .view
            .writer_id
            .map(|writer_id| StorageWriteHandle::new(context.view, writer_id)),
        table_functions: &table_functions,
        random: &context.random,
        worker_count: context
            .worker_count()
            .min(config.max_workers().unwrap_or(usize::MAX)),
        warnings: &context.warnings,
        control: context.query_control(),
        memory: &operator_memory,
        sources: &sources,
    };
    let mut result =
        koko_processor::execute_regular_synchronized(&query, &plan, catalog, storage, &execution)?;
    if !rel_base.is_empty() {
        remap_created_rel_ids(&mut result, rel_base);
    }
    let mut result = QueryResult::from_exec(result);
    result.set_type_context(result::capture_result_type_context(
        catalog,
        catalog_version,
    ));
    Ok(result)
}

fn col_names(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

/// Kùzu gives a rel created inside a transaction a temporary offset starting at
/// 2^62 (its `StorageConstants` transaction-local base), visible in that
/// transaction's own results until commit rewrites it to a dense offset. Our
/// in-memory store assigns the dense offset immediately, so we reproduce the
/// temporary id only at the display boundary (see [`remap_created_rel_ids`]).
const TXN_LOCAL_REL_OFFSET_BASE: u64 = 1 << 62;

/// Rewrite, in a statement's result rows, the internal id of every rel created
/// since `base` to its transaction-local 2^62-based offset. `base` maps each rel
/// table to its size at the start of the active write transaction (or, in
/// auto-commit, of this statement), so a rel whose dense offset is at or past that
/// base is uncommitted and renders with the 2^62 id; committed rels (offset below
/// base) and all nodes are left untouched — only rel-table ids appear in `base`.
pub(super) fn remap_created_rel_ids(
    result: &mut koko_processor::ExecResult,
    base: &HashMap<TableId, u64>,
) {
    result.map_values_mut(|value| remap_value(value, base));
}

fn remap_value(v: &mut Value, base: &HashMap<TableId, u64>) {
    match v {
        Value::InternalId(id) => remap_rel_id(id, base),
        Value::Rel(r) => remap_rel_id(&mut r.id, base),
        Value::RecursiveRel(rr) => {
            for rel in &mut rr.rels {
                remap_rel_id(&mut rel.id, base);
            }
        }
        Value::List(items) => items.iter_mut().for_each(|x| remap_value(x, base)),
        Value::Struct(fields) => fields.iter_mut().for_each(|(_, x)| remap_value(x, base)),
        Value::Map(entries) => entries.iter_mut().for_each(|(k, val)| {
            remap_value(k, base);
            remap_value(val, base);
        }),
        _ => {}
    }
}

fn remap_rel_id(id: &mut InternalId, base: &HashMap<TableId, u64>) {
    if let Some(&b) = base.get(&id.table_id) {
        if id.offset.0 >= b {
            *id = InternalId::new(id.table_id, TXN_LOCAL_REL_OFFSET_BASE + (id.offset.0 - b));
        }
    }
}

pub(super) fn apply_index_statement(
    catalog: &mut Catalog,
    statement: &koko_parser::ast::Statement,
) -> Result<String> {
    use koko_catalog::{CreateIndexOutcome, IndexType};
    use koko_parser::ast::Statement;

    match statement {
        Statement::CreateIndex(create) => {
            let index_type = match create.index_type {
                koko_parser::ast::IndexType::Hash => IndexType::Hash,
                koko_parser::ast::IndexType::Art => IndexType::Art,
            };
            if !create.options.is_empty() {
                return Err(Error::binder(format!(
                    "CREATE {} INDEX does not support OPTIONS.",
                    index_type.name()
                )));
            }
            match catalog.create_primary_key_index(
                &create.name,
                &create.table,
                &create.properties,
                index_type,
                create.if_not_exists,
            )? {
                CreateIndexOutcome::Created => {
                    Ok(format!("Index {} has been created.", create.name))
                }
                CreateIndexOutcome::Existing(name) => Ok(format!("Index {name} already exists.")),
            }
        }
        Statement::DropIndex(drop) => {
            if catalog.drop_index(&drop.name) {
                Ok(format!("Index {} has been dropped.", drop.name))
            } else if drop.if_exists {
                Ok(format!("Index {} does not exist.", drop.name))
            } else {
                Err(Error::binder(format!(
                    "Index {} does not exist in catalog.",
                    drop.name
                )))
            }
        }
        _ => unreachable!("index statement dispatcher"),
    }
}

pub(super) fn normalize_query_parameters(
    params: &[QueryParameter<'_>],
) -> Result<Vec<(String, Value)>> {
    params
        .iter()
        .map(|parameter| {
            if parameter.name().is_empty() {
                return Err(Error::binder("Parameter name cannot be empty."));
            }
            let value = match parameter.declared_type() {
                Some(declared_type) => koko_function::cast_value(parameter.value(), declared_type)?,
                None => parameter.value().clone(),
            };
            Ok((parameter.name().to_string(), value))
        })
        .collect()
}

/// Whether a statement mutates the database (so it needs the single-writer slot
/// when run auto-commit). DDL/`COPY`/CTAS are writes; a query is a write iff it has
/// an updating clause or calls `nextval`. `currval` is a pure read.
pub(super) fn statement_writes(stmt: &koko_parser::ast::Statement) -> bool {
    use koko_parser::ast::{CallStmt, Statement};
    match stmt {
        Statement::Explain { inner, profile } => *profile && statement_writes(inner),
        Statement::CreateGraph(_) | Statement::DropGraph { .. } => true,
        Statement::CreateIndex(_) | Statement::DropIndex(_) => true,
        Statement::CreateNodeTable(_)
        | Statement::CreateRelTable(_)
        | Statement::DropTable(_)
        | Statement::Alter(_)
        | Statement::CreateSequence(_)
        | Statement::DropSequence(_)
        | Statement::Comment(_)
        | Statement::CreateType(_)
        | Statement::CreateTableAs(_)
        | Statement::CreateMacro(_)
        | Statement::DropMacro { .. }
        | Statement::Copy(_) => true,
        Statement::ImportDatabase(_) => true,
        Statement::Query(rq) => query_has_updating(rq) || query_calls_nextval(rq),
        Statement::Call(CallStmt::SetConfig { value, .. }) => expr_calls_nextval(value),
        Statement::Call(CallStmt::TableFunc { .. })
        | Statement::Transaction(_)
        | Statement::UseGraph { .. } => false,
        Statement::CopyTo(_) | Statement::ExportDatabase(_) => false,
    }
}

/// Whether a successful statement changed catalog/macros/sequence state. Explicit
/// read-write transactions only publish their catalog snapshot when this is true;
/// pure DML commits must leave the shared catalog untouched.
pub(super) fn statement_touches_catalog(stmt: &koko_parser::ast::Statement) -> bool {
    use koko_parser::ast::{CallStmt, Statement};
    match stmt {
        Statement::Explain { inner, profile } => *profile && statement_touches_catalog(inner),
        Statement::CreateGraph(_) | Statement::DropGraph { .. } => true,
        Statement::CreateIndex(_) | Statement::DropIndex(_) => true,
        Statement::CreateNodeTable(_)
        | Statement::CreateRelTable(_)
        | Statement::DropTable(_)
        | Statement::Alter(_)
        | Statement::CreateSequence(_)
        | Statement::DropSequence(_)
        | Statement::Comment(_)
        | Statement::CreateType(_)
        | Statement::CreateTableAs(_)
        | Statement::CreateMacro(_)
        | Statement::DropMacro { .. } => true,
        Statement::ImportDatabase(_) => true,
        Statement::Query(rq) => query_calls_nextval(rq),
        Statement::Call(CallStmt::SetConfig { value, .. }) => expr_calls_nextval(value),
        Statement::Copy(_)
        | Statement::CopyTo(_)
        | Statement::ExportDatabase(_)
        | Statement::Call(CallStmt::TableFunc { .. })
        | Statement::Transaction(_)
        | Statement::UseGraph { .. } => false,
    }
}

fn query_has_updating(rq: &koko_parser::ast::RegularQuery) -> bool {
    rq.singles
        .iter()
        .any(|sq| !sq.updating.is_empty() || sq.parts.iter().any(|p| !p.updating.is_empty()))
}

/// Whether any expression reachable from the query AST calls `nextval`.
///
/// This deliberately mirrors the parser's owned AST instead of relying on planner
/// shape: all represented expression positions are walked (reading-clause filters,
/// pattern properties and recursive lambdas, `WITH`/`RETURN` projections plus
/// `ORDER BY`/`SKIP`/`LIMIT`, updating-clause expressions, and subquery bodies).
fn query_calls_nextval(rq: &koko_parser::ast::RegularQuery) -> bool {
    rq.singles.iter().any(single_query_calls_nextval)
}

fn single_query_calls_nextval(sq: &koko_parser::ast::SingleQuery) -> bool {
    sq.parts.iter().any(|part| {
        part.reading.iter().any(reading_calls_nextval)
            || part.updating.iter().any(updating_calls_nextval)
            || with_calls_nextval(&part.with)
    }) || sq.reading.iter().any(reading_calls_nextval)
        || sq.updating.iter().any(updating_calls_nextval)
        || sq.ret.as_ref().is_some_and(return_calls_nextval)
}

fn reading_calls_nextval(c: &koko_parser::ast::ReadingClause) -> bool {
    use koko_parser::ast::ReadingClause;
    match c {
        ReadingClause::Match(m) => {
            m.patterns.iter().any(pattern_calls_nextval)
                || m.where_clause.as_ref().is_some_and(expr_calls_nextval)
        }
        ReadingClause::Unwind(u) => expr_calls_nextval(&u.expr),
        ReadingClause::TableFuncScan(t) => t.where_clause.as_ref().is_some_and(expr_calls_nextval),
        ReadingClause::LoadFrom(l) => l.where_clause.as_ref().is_some_and(expr_calls_nextval),
    }
}

fn updating_calls_nextval(c: &koko_parser::ast::UpdatingClause) -> bool {
    use koko_parser::ast::UpdatingClause;
    match c {
        UpdatingClause::Create(c) => c.patterns.iter().any(pattern_calls_nextval),
        UpdatingClause::Set(s) => s.items.iter().any(|item| expr_calls_nextval(&item.value)),
        UpdatingClause::Delete(d) => d.exprs.iter().any(expr_calls_nextval),
        UpdatingClause::Merge(m) => {
            m.patterns.iter().any(pattern_calls_nextval)
                || m.on_create
                    .iter()
                    .chain(m.on_match.iter())
                    .any(|item| expr_calls_nextval(&item.value))
        }
    }
}

fn with_calls_nextval(w: &koko_parser::ast::WithClause) -> bool {
    return_calls_nextval(&w.projection) || w.where_clause.as_ref().is_some_and(expr_calls_nextval)
}

fn return_calls_nextval(r: &koko_parser::ast::ReturnClause) -> bool {
    r.items.iter().any(projection_item_calls_nextval)
        || r.order_by.iter().any(|(expr, _)| expr_calls_nextval(expr))
        || r.skip.iter().chain(r.limit.iter()).any(expr_calls_nextval)
}

fn projection_item_calls_nextval(item: &koko_parser::ast::ProjectionItem) -> bool {
    match item {
        koko_parser::ast::ProjectionItem::Expr { expr, .. } => expr_calls_nextval(expr),
        // `a.state.*` spreads a struct's stored fields — no sequence call.
        koko_parser::ast::ProjectionItem::AllStructFields(base) => expr_calls_nextval(base),
        koko_parser::ast::ProjectionItem::Star
        | koko_parser::ast::ProjectionItem::AllProperties(_) => false,
    }
}

fn pattern_calls_nextval(p: &koko_parser::ast::PatternElement) -> bool {
    node_calls_nextval(&p.head)
        || p.chains
            .iter()
            .any(|(rel, node)| rel_calls_nextval(rel) || node_calls_nextval(node))
}

fn node_calls_nextval(n: &koko_parser::ast::NodePattern) -> bool {
    n.properties
        .iter()
        .any(|(_, expr)| expr_calls_nextval(expr))
}

fn rel_calls_nextval(r: &koko_parser::ast::RelPattern) -> bool {
    r.properties
        .iter()
        .any(|(_, expr)| expr_calls_nextval(expr))
        || r.recursive
            .as_ref()
            .and_then(|rec| rec.lambda.as_ref())
            .is_some_and(recursive_lambda_calls_nextval)
}

fn recursive_lambda_calls_nextval(lam: &koko_parser::ast::RecursiveLambda) -> bool {
    lam.predicate.as_ref().is_some_and(expr_calls_nextval)
        || lam
            .rel_projection
            .iter()
            .chain(lam.node_projection.iter())
            .any(|projection| projection.iter().any(expr_calls_nextval))
}

fn expr_calls_nextval(e: &koko_parser::ast::Expr) -> bool {
    use koko_parser::ast::Expr;
    match e {
        Expr::Function { name, args, .. } => {
            name.eq_ignore_ascii_case("nextval") || args.iter().any(expr_calls_nextval)
        }
        Expr::Arithmetic { lhs, rhs, .. } | Expr::Comparison { lhs, rhs, .. } => {
            expr_calls_nextval(lhs) || expr_calls_nextval(rhs)
        }
        Expr::And(xs) | Expr::Or(xs) | Expr::List(xs) => xs.iter().any(expr_calls_nextval),
        Expr::Xor(lhs, rhs) => expr_calls_nextval(lhs) || expr_calls_nextval(rhs),
        Expr::Not(x) | Expr::Negate(x) | Expr::IsNull(x) | Expr::IsNotNull(x) => {
            expr_calls_nextval(x)
        }
        Expr::Struct(fields) => fields.iter().any(|(_, expr)| expr_calls_nextval(expr)),
        Expr::Lambda { body, .. } => expr_calls_nextval(body),
        Expr::PatternComprehension { projection, .. } => {
            projection.as_deref().is_some_and(expr_calls_nextval)
        }
        Expr::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            expr_calls_nextval(list)
                || predicate.as_deref().is_some_and(expr_calls_nextval)
                || projection.as_deref().is_some_and(expr_calls_nextval)
        }
        Expr::Case {
            operand,
            when_thens,
            else_,
        } => {
            operand.as_deref().is_some_and(expr_calls_nextval)
                || when_thens
                    .iter()
                    .any(|(when, then)| expr_calls_nextval(when) || expr_calls_nextval(then))
                || else_.as_deref().is_some_and(expr_calls_nextval)
        }
        Expr::Subquery {
            patterns,
            where_clause,
            ..
        } => {
            patterns.iter().any(pattern_calls_nextval)
                || where_clause.as_deref().is_some_and(expr_calls_nextval)
        }
        Expr::Property { base, .. } => expr_calls_nextval(base),
        Expr::Literal(_)
        | Expr::OverflowInt(_)
        | Expr::Variable(_)
        | Expr::Parameter(_)
        | Expr::Star => false,
    }
}

/// Fold each bound column `DEFAULT` into the catalog's resolved form: a constant
/// expression is evaluated to a `Value` (via the real evaluator); a `nextval`
/// default keeps its sequence name for per-row evaluation at insert.
fn resolve_column_defaults(defaults: Vec<BoundColumnDefault>) -> Result<Vec<ColumnDefault>> {
    defaults.into_iter().map(resolve_column_default).collect()
}

fn resolve_column_default(d: BoundColumnDefault) -> Result<ColumnDefault> {
    Ok(match d {
        BoundColumnDefault::None => ColumnDefault::None,
        BoundColumnDefault::Const(expr) => {
            ColumnDefault::Const(koko_processor::eval_constant(&expr)?)
        }
        BoundColumnDefault::NextVal(s) => ColumnDefault::NextVal(s),
    })
}

pub(super) fn attach_query_summary(
    result: Result<QueryResult>,
    query: &QueryContext,
    total_time: Duration,
    tracker: &MemoryTracker,
) -> Result<QueryResult> {
    let result = result.map(|mut result| {
        result.set_diagnostics(result::statement_diagnostics(
            query.warnings.retained(),
            query.warnings.count() as u64,
        ));
        result
    });
    let mut result = attach_summary(result, query.compilation_time, total_time, tracker)?;
    result.attach_plan_execution_time();
    Ok(result)
}

pub(super) fn attach_summary(
    result: Result<QueryResult>,
    compiling_time: Duration,
    total_time: Duration,
    tracker: &MemoryTracker,
) -> Result<QueryResult> {
    result.and_then(|mut result| {
        result.track_memory(tracker)?;
        result.set_summary(compiling_time, total_time.saturating_sub(compiling_time));
        Ok(result)
    })
}

pub(super) fn refresh_execution_time(
    result: &mut Result<QueryResult>,
    query: &QueryContext,
    total_time: Duration,
) {
    if let Ok(result) = result {
        result.set_execution_time(total_time.saturating_sub(query.compilation_time));
    }
}
