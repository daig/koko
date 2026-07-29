//! `koko-catalog` — the schema catalog: node/rel table entries, their columns,
//! primary keys, and FROM/TO wiring.
//!
//! Established invariants: table/property lookup is case-insensitive, column IDs
//! are assigned monotonically at table creation, and a node identity is
//! `(table_id, offset)`. The facade snapshots this catalog for transaction-local
//! DDL; MVCC data lives in `koko-storage`. Native persistence is out of scope.

use crate::column::StoredColumnGeneration;
use crate::{
    AnyTables, Column, ColumnDefault, ColumnDefinition, ColumnGeneration, CreateIndexOutcome,
    IcebugTable, IcebugTableSource, IndexEntry, IndexType, NodeTable, NodeTableDefinition,
    RelTable, RelTableDefinition, RelTablePair, Sequence, serial_sequence_name,
};
use koko_common::{ColumnId, Error, LogicalType, Result, TableId};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// Whether a catalog entry is a node table or a relationship table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Node,
    Rel,
}

fn build_name_index(columns: &[Column]) -> Result<HashMap<String, usize>> {
    let mut index = HashMap::with_capacity(columns.len());
    for (position, column) in columns.iter().enumerate() {
        if index
            .insert(column.name.to_ascii_lowercase(), position)
            .is_some()
        {
            return Err(Error::binder(format!(
                "Duplicated column name: {}, column name must be unique.",
                column.name
            )));
        }
    }
    Ok(index)
}

fn add_column_to(
    columns: &mut Vec<Column>,
    name_to_idx: &mut HashMap<String, usize>,
    mut column: Column,
) {
    let index = columns.len();
    column.column_id = ColumnId(index as u32);
    name_to_idx.insert(column.name.to_ascii_lowercase(), index);
    columns.push(column);
}

fn drop_column_at(
    columns: &mut Vec<Column>,
    name_to_idx: &mut HashMap<String, usize>,
    index: usize,
) {
    columns.remove(index);
    for (position, column) in columns.iter_mut().enumerate() {
        column.column_id = ColumnId(position as u32);
    }
    *name_to_idx = build_name_index(columns).expect("remaining column names stay unique");
}

fn materialize_columns(
    table_name: &str,
    definitions: Vec<ColumnDefinition>,
) -> (Vec<Column>, Vec<String>) {
    let mut serial_sequences = Vec::new();
    let columns = definitions
        .into_iter()
        .enumerate()
        .map(|(index, definition)| {
            let (generation, default_text) = match definition.generation {
                ColumnGeneration::None => (StoredColumnGeneration::None, "NULL".to_string()),
                ColumnGeneration::Default { value, source_text } => {
                    (StoredColumnGeneration::Default(value), source_text)
                }
                ColumnGeneration::Serial => {
                    let sequence = serial_sequence_name(table_name, &definition.name);
                    serial_sequences.push(sequence.clone());
                    (StoredColumnGeneration::Serial { sequence }, String::new())
                }
            };
            Column {
                name: definition.name,
                logical_type: definition.logical_type,
                type_text: definition.type_text,
                column_id: ColumnId(index as u32),
                generation,
                default_text,
            }
        })
        .collect();
    (columns, serial_sequences)
}

fn ensure_serial_names_available(catalog: &Catalog, sequences: &[String]) -> Result<()> {
    for sequence in sequences {
        if catalog.contains_sequence(sequence) {
            return Err(Error::catalog(format!(
                "{sequence} already exists in catalog."
            )));
        }
    }
    Ok(())
}

fn create_serial_sequences(catalog: &mut Catalog, sequences: Vec<String>) -> Result<()> {
    for sequence in sequences {
        catalog.create_sequence(Sequence::new(sequence, 0, 1, 0, i64::MAX, false))?;
    }
    Ok(())
}

/// The in-memory schema catalog.
///
/// `Clone` is load-bearing for transactions: a read-write transaction takes a
/// copy-on-write snapshot of the catalog (alongside the storage snapshot) at
/// `BEGIN`, binds/plans/executes against it, and either swaps it into the
/// committed catalog at `COMMIT` or drops it at `ROLLBACK` — so DDL inside a
/// transaction is isolated and fully rolled back. Sequence state is part of the
/// same snapshot.
#[derive(Debug, Default, Clone)]
pub struct Catalog {
    node_tables: HashMap<TableId, NodeTable>,
    rel_tables: HashMap<TableId, RelTable>,
    /// Case-insensitive table-name → id index.
    name_to_id: HashMap<String, TableId>,
    /// Sequences, keyed by lowercase name (a namespace separate from tables).
    sequences: HashMap<String, Sequence>,
    /// User-defined type aliases (`CREATE TYPE`), keyed by lowercase name.
    user_types: HashMap<String, LogicalType>,
    /// Maps every rel-group member (per-pair) id — including the primary — to its
    /// group's primary id, so a member id resolves to the group's catalog entry.
    member_to_primary: HashMap<TableId, TableId>,
    /// Explicitly named primary-key indexes, keyed by lowercase index name.
    indexes: HashMap<String, IndexEntry>,
    /// Read-only external table metadata keyed by catalog table id.
    icebug_tables: HashMap<TableId, IcebugTable>,
    /// Present only for a schemaless graph. The entries remain ordinary catalog tables so every
    /// query uses the same binder, planner, MVCC storage, and processor paths as typed graphs.
    any_tables: Option<AnyTables>,
    next_table_id: Arc<AtomicU64>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc_table_id(&self) -> TableId {
        TableId(self.next_table_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Construct a catalog whose table identities come from a database-wide allocator.
    ///
    /// Catalog clones and independent named-graph catalogs share this allocator, so an
    /// [`InternalId`](koko_common::InternalId) never needs hidden graph context.
    pub fn with_table_id_allocator(next_table_id: Arc<AtomicU64>) -> Self {
        Self {
            next_table_id,
            ..Self::default()
        }
    }

    /// Mark this catalog as schemaless and create its hidden physical tables.
    pub fn initialize_any_graph(&mut self) -> Result<AnyTables> {
        if let Some(tables) = self.any_tables {
            return Ok(tables);
        }
        let nodes = self.create_node_table(NodeTableDefinition {
            name: "_nodes".to_string(),
            columns: vec![
                ColumnDefinition::serial("id"),
                ColumnDefinition::plain("label", LogicalType::List(Box::new(LogicalType::String))),
                ColumnDefinition::plain("data", LogicalType::Json),
            ],
            primary_key: "id".to_string(),
        })?;
        let edges = self.create_rel_table(RelTableDefinition {
            name: "_edges".to_string(),
            endpoint_pairs: vec![(nodes, nodes)],
            columns: vec![
                ColumnDefinition::plain("_id", LogicalType::InternalId),
                ColumnDefinition::plain("label", LogicalType::String),
                ColumnDefinition::plain("data", LogicalType::Json),
            ],
            storage_direction: koko_common::RelStorageDirection::Both,
        })?;
        let tables = AnyTables { nodes, edges };
        self.any_tables = Some(tables);
        Ok(tables)
    }

    /// Hidden schemaless-table identities, if this is an `ANY` graph catalog.
    pub const fn any_tables(&self) -> Option<AnyTables> {
        self.any_tables
    }

    pub fn is_any_node_table(&self, table: TableId) -> bool {
        self.any_tables
            .is_some_and(|tables| tables.nodes() == table)
    }

    pub fn is_any_rel_table(&self, table: TableId) -> bool {
        self.any_tables
            .is_some_and(|tables| tables.edges() == table)
    }

    pub fn contains_table(&self, name: &str) -> bool {
        self.name_to_id.contains_key(&name.to_ascii_lowercase())
    }

    /// Resolve a table name to its id (case-insensitive).
    pub fn table_id(&self, name: &str) -> Option<TableId> {
        self.name_to_id.get(&name.to_ascii_lowercase()).copied()
    }

    /// Register one explicit primary-key index. The storage layer already owns the
    /// MVCC-aware key map; this metadata selects HASH/ART DDL semantics and
    /// introspection without duplicating key storage.
    pub fn create_primary_key_index(
        &mut self,
        name: &str,
        table_name: &str,
        properties: &[String],
        index_type: IndexType,
        if_not_exists: bool,
    ) -> Result<CreateIndexOutcome> {
        let table_id = self
            .table_id(table_name)
            .ok_or_else(|| Error::binder(format!("Table {table_name} does not exist.")))?;
        let table = self.node_table(table_id).ok_or_else(|| {
            Error::binder(format!(
                "{} indexes are currently supported only on node primary keys.",
                index_type.name()
            ))
        })?;
        let primary_key = table.primary_key_column().name.clone();
        if properties.len() != 1 || !properties[0].eq_ignore_ascii_case(&primary_key) {
            return Err(Error::binder(format!(
                "{} indexes are currently supported only on node primary keys.",
                index_type.name()
            )));
        }
        if let Some(existing) = self
            .indexes
            .values()
            .find(|entry| entry.table_id == table_id)
        {
            if if_not_exists {
                return Ok(CreateIndexOutcome::Existing(existing.name.clone()));
            }
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                existing.name
            )));
        }
        let key = name.to_ascii_lowercase();
        if let Some(existing) = self.indexes.get(&key) {
            if if_not_exists {
                return Ok(CreateIndexOutcome::Existing(existing.name.clone()));
            }
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                existing.name
            )));
        }
        self.indexes.insert(
            key,
            IndexEntry {
                name: name.to_string(),
                table_id,
                index_type,
                property_names: vec![primary_key],
            },
        );
        Ok(CreateIndexOutcome::Created)
    }

    /// Drop a named index, returning whether it existed.
    pub fn drop_index(&mut self, name: &str) -> bool {
        self.indexes.remove(&name.to_ascii_lowercase()).is_some()
    }

    /// Explicit indexes in stable table/id/name order.
    pub fn indexes(&self) -> Vec<&IndexEntry> {
        let mut indexes: Vec<_> = self.indexes.values().collect();
        indexes.sort_by(|left, right| {
            left.table_id
                .0
                .cmp(&right.table_id.0)
                .then_with(|| left.name.cmp(&right.name))
        });
        indexes
    }

    pub fn mark_icebug_table(
        &mut self,
        table_id: TableId,
        storage: String,
        source: Option<IcebugTableSource>,
        load_error: Option<String>,
    ) {
        self.icebug_tables.insert(
            table_id,
            IcebugTable {
                storage,
                source,
                load_error,
            },
        );
    }

    pub fn is_icebug_table(&self, table_id: TableId) -> bool {
        self.icebug_tables.contains_key(&table_id)
    }

    pub fn icebug_table(&self, table_id: TableId) -> Option<&IcebugTable> {
        self.icebug_tables.get(&table_id)
    }

    pub fn table_kind(&self, id: TableId) -> Option<TableKind> {
        if self.node_tables.contains_key(&id) {
            Some(TableKind::Node)
        } else if self.rel_tables.contains_key(&id) || self.member_to_primary.contains_key(&id) {
            Some(TableKind::Rel)
        } else {
            None
        }
    }

    pub fn node_table(&self, id: TableId) -> Option<&NodeTable> {
        self.node_tables.get(&id)
    }
    pub fn rel_table(&self, id: TableId) -> Option<&RelTable> {
        // Resolve a per-pair member id to its group's primary entry (a non-member id
        // falls through unchanged — `get` then returns `None` for a non-rel id).
        let primary = self.member_to_primary.get(&id).copied().unwrap_or(id);
        self.rel_tables.get(&primary)
    }

    /// The per-pair members of a rel group as `(member_id, from, to)`, index-aligned
    /// with `pairs`. For a single-pair rel this is one entry whose member id is the
    /// primary id. `id` may be the primary or any member. Used to create and route
    /// per-pair storage.
    pub fn rel_members(&self, id: TableId) -> Vec<(TableId, TableId, TableId)> {
        self.rel_table(id)
            .map(|table| {
                table
                    .pairs
                    .iter()
                    .map(|pair| (pair.member, pair.from, pair.to))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The per-pair member id a directed edge `(from)->(to)` belongs to within a rel
    /// group, or `None` if no declared pair matches. The match is **exact** on
    /// orientation: a `CREATE (b)-[:R]->(a)` edge belongs to the `(B,A)` pair, never a
    /// reverse-declared `(A,B)` one — mis-routing there files the edge under the wrong
    /// FROM/TO, which the per-pair adjacency (keyed by the pair's FROM/TO table) then
    /// can't find. `id` may be the primary or any member.
    pub fn rel_member_for(&self, id: TableId, from: TableId, to: TableId) -> Option<TableId> {
        self.rel_table(id)?
            .pairs
            .iter()
            .find(|pair| pair.from == from && pair.to == to)
            .map(|pair| pair.member)
    }

    pub fn node_table_by_name(&self, name: &str) -> Option<&NodeTable> {
        self.table_id(name).and_then(|id| self.node_table(id))
    }

    /// All node-table ids, in creation order (ascending id). Used to scan an
    /// unlabeled / multi-label node pattern across every candidate table.
    pub fn node_table_ids(&self) -> Vec<TableId> {
        let mut ids: Vec<TableId> = self.node_tables.keys().copied().collect();
        ids.sort_unstable_by_key(|t| t.0);
        ids
    }

    /// All relationship-table ids, in creation order. Used to extend an
    /// unlabeled / multi-label relationship pattern across every candidate table.
    pub fn rel_table_ids(&self) -> Vec<TableId> {
        let mut ids: Vec<TableId> = self.rel_tables.keys().copied().collect();
        ids.sort_unstable_by_key(|t| t.0);
        ids
    }
    pub fn rel_table_by_name(&self, name: &str) -> Option<&RelTable> {
        self.table_id(name).and_then(|id| self.rel_table(id))
    }

    /// Atomically create a node table from one structural definition.
    pub fn create_node_table(&mut self, definition: NodeTableDefinition) -> Result<TableId> {
        if self.contains_table(&definition.name) {
            return Err(Error::catalog(format!(
                "{} already exists in catalog.",
                definition.name
            )));
        }
        if definition.columns.is_empty() {
            return Err(Error::binder(format!(
                "Cannot create node table {} with no columns.",
                definition.name
            )));
        }
        let (columns, serial_sequences) = materialize_columns(&definition.name, definition.columns);
        let name_to_idx = build_name_index(&columns)?;
        let primary_key = *name_to_idx
            .get(&definition.primary_key.to_ascii_lowercase())
            .ok_or_else(|| {
                Error::binder(format!(
                    "Primary key {} does not match any of the predefined node properties.",
                    definition.primary_key
                ))
            })?;
        ensure_serial_names_available(self, &serial_sequences)?;

        let id = self.alloc_table_id();
        self.node_tables.insert(
            id,
            NodeTable {
                id,
                name: definition.name.clone(),
                columns,
                primary_key,
                comment: None,
                name_to_idx,
            },
        );
        self.name_to_id
            .insert(definition.name.to_ascii_lowercase(), id);
        create_serial_sequences(self, serial_sequences)?;
        Ok(id)
    }

    /// `COMMENT ON TABLE`: set a table's comment (node or rel).
    pub fn set_comment(&mut self, id: TableId, comment: String) {
        if let Some(t) = self.node_tables.get_mut(&id) {
            t.comment = Some(comment);
        } else if let Some(t) = self.rel_tables.get_mut(&id) {
            t.comment = Some(comment);
        }
    }

    /// A table's comment, or `""` if none (for `SHOW_TABLES`).
    pub fn table_comment(&self, id: TableId) -> &str {
        self.node_tables
            .get(&id)
            .map(|t| &t.comment)
            .or_else(|| self.rel_tables.get(&id).map(|t| &t.comment))
            .and_then(|c| c.as_deref())
            .unwrap_or("")
    }

    /// The canonical name of any table (node or rel) by id.
    pub fn table_name(&self, id: TableId) -> Option<&str> {
        self.node_tables
            .get(&id)
            .map(|table| table.name.as_str())
            .or_else(|| self.rel_table(id).map(|table| table.name.as_str()))
    }

    /// Whether a rel table already has the `(from, to)` endpoint pair.
    pub fn rel_has_pair(&self, id: TableId, from: TableId, to: TableId) -> bool {
        self.rel_tables.get(&id).is_some_and(|table| {
            table
                .pairs
                .iter()
                .any(|pair| pair.from == from && pair.to == to)
        })
    }

    /// `ALTER … ADD FROM x TO y`: append an endpoint pair (caller verified absence).
    /// A pair added after creation gets a fresh per-pair member id (the original
    /// reservation only covers the initial pairs + group id). Returns that member id so
    /// the caller can create its storage store; `None` if `id` is not a rel table.
    pub fn add_rel_pair(&mut self, id: TableId, from: TableId, to: TableId) -> Option<TableId> {
        if !self.rel_tables.contains_key(&id) {
            return None;
        }
        let member = self.alloc_table_id();
        self.member_to_primary.insert(member, id);
        let table = self.rel_tables.get_mut(&id).expect("checked above");
        table.pairs.push(RelTablePair { from, to, member });
        Some(member)
    }

    /// `ALTER … DROP FROM x TO y`: remove an endpoint pair (caller verified presence).
    /// Dropping the last pair leaves an empty rel group. Returns the retired per-pair
    /// member id (so the caller drops its storage store); the id is not reused.
    pub fn drop_rel_pair(&mut self, id: TableId, from: TableId, to: TableId) -> Option<TableId> {
        let dropped = self.rel_tables.get_mut(&id).and_then(|table| {
            table
                .pairs
                .iter()
                .position(|pair| pair.from == from && pair.to == to)
                .map(|position| table.pairs.remove(position).member)
        });
        if let Some(member) = dropped {
            self.member_to_primary.remove(&member);
        }
        dropped
    }

    /// The name of the first relationship table that references `node` as a FROM
    /// or TO endpoint, if any. Used to reject `DROP TABLE` on a node table still
    /// wired into a rel table.
    pub fn rel_table_referencing(&self, node: TableId) -> Option<&str> {
        // Deterministic order (ascending rel-table id) so the reported rel name
        // is stable across runs.
        self.rel_table_ids()
            .into_iter()
            .filter_map(|id| self.rel_tables.get(&id))
            .find(|table| {
                table
                    .pairs
                    .iter()
                    .any(|pair| pair.from == node || pair.to == node)
            })
            .map(|table| table.name.as_str())
    }

    /// Drop a node or rel table by id, removing it from the catalog and freeing
    /// its name. The table id is *not* reused. (Catalog rollback within a
    /// transaction is a later-phase concern; auto-committed DDL is final.)
    pub fn drop_table(&mut self, id: TableId) {
        if let Some(t) = self.node_tables.remove(&id) {
            self.name_to_id.remove(&t.name.to_ascii_lowercase());
            for column in &t.columns {
                if let Some(sequence) = column.serial_sequence() {
                    self.drop_sequence(sequence);
                }
            }
        } else if let Some(t) = self.rel_tables.remove(&id) {
            self.name_to_id.remove(&t.name.to_ascii_lowercase());
            for pair in &t.pairs {
                self.member_to_primary.remove(&pair.member);
            }
            for column in &t.columns {
                if let Some(sequence) = column.serial_sequence() {
                    self.drop_sequence(sequence);
                }
            }
        }
        self.indexes.retain(|_, index| index.table_id != id);
        self.icebug_tables.remove(&id);
    }

    // ---- user-defined types (`CREATE TYPE`) ----

    /// Resolve a user-defined type alias to its underlying type (case-insensitive).
    pub fn user_type(&self, name: &str) -> Option<LogicalType> {
        self.user_types.get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn contains_user_type(&self, name: &str) -> bool {
        self.user_types.contains_key(&name.to_ascii_lowercase())
    }

    /// Register a user-defined type alias (the caller has checked for a duplicate).
    pub fn create_user_type(&mut self, name: &str, ty: LogicalType) {
        self.user_types.insert(name.to_ascii_lowercase(), ty);
    }

    /// All user-defined type aliases in deterministic name order.
    ///
    /// Logical database export uses this rather than reaching into catalog
    /// internals so the generated schema script is stable across runs.
    pub fn user_types_sorted(&self) -> Vec<(&str, &LogicalType)> {
        let mut types: Vec<_> = self
            .user_types
            .iter()
            .map(|(name, ty)| (name.as_str(), ty))
            .collect();
        types.sort_by(|(left, _), (right, _)| left.cmp(right));
        types
    }

    // ---- sequences (a namespace separate from tables) ----

    pub fn contains_sequence(&self, name: &str) -> bool {
        self.sequences.contains_key(&name.to_ascii_lowercase())
    }

    /// Register a sequence. Errors (with the C++ wording) if the name is taken.
    pub fn create_sequence(&mut self, seq: Sequence) -> Result<()> {
        let key = seq.name.to_ascii_lowercase();
        if self.sequences.contains_key(&key) {
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                seq.name
            )));
        }
        self.sequences.insert(key, seq);
        Ok(())
    }

    /// Drop a sequence; returns whether one existed (for the `IF EXISTS` message).
    pub fn drop_sequence(&mut self, name: &str) -> bool {
        self.sequences.remove(&name.to_ascii_lowercase()).is_some()
    }

    /// `nextval(name)`: advance and return the sequence's next value.
    pub fn sequence_next_val(&self, name: &str) -> Result<i64> {
        self.sequence(name)?.next_val()
    }

    /// `currval(name)`: the sequence's current value (errors if never advanced).
    pub fn sequence_curr_val(&self, name: &str) -> Result<i64> {
        self.sequence(name)?.curr_val()
    }

    fn sequence(&self, name: &str) -> Result<&Sequence> {
        // `currval`/`nextval` on a missing sequence: the C++ engine reports the
        // generic catalog-entry-not-found wording (`getSequenceEntry` → `getEntry`),
        // NOT the `DROP SEQUENCE` "Sequence X does not exist." form.
        self.sequences
            .get(&name.to_ascii_lowercase())
            .ok_or_else(|| Error::catalog(format!("{name} does not exist in catalog.")))
    }

    /// All sequences in ascending name order (for `SHOW_SEQUENCES`).
    pub fn sequences_sorted(&self) -> Vec<&Sequence> {
        let mut v: Vec<&Sequence> = self.sequences.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Snapshot the mutable sequence counters for transaction/catalog conflict
    /// detection. Definition changes are tracked by the catalog version; this
    /// catches `nextval` state changes, including implicit SERIAL defaults.
    pub fn sequence_state(&self) -> Vec<(String, i64, u64)> {
        let mut v: Vec<_> = self
            .sequences
            .values()
            .map(|s| {
                (
                    s.name.clone(),
                    s.curr.load(Ordering::Relaxed),
                    s.usage.load(Ordering::Relaxed),
                )
            })
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Whether a table (node or rel) has a property by name (case-insensitive).
    /// Whether any node or rel table has a property with this (case-insensitive)
    /// name — the `properties(list, key)` bind check consults it when the list's
    /// element tables are not statically known.
    pub fn any_table_has_property(&self, name: &str) -> bool {
        self.node_table_ids()
            .into_iter()
            .chain(self.rel_table_ids())
            .any(|id| self.table_has_column(id, name))
    }

    pub fn table_has_column(&self, id: TableId, name: &str) -> bool {
        self.node_tables
            .get(&id)
            .map(|t| t.has_column(name))
            .or_else(|| self.rel_tables.get(&id).map(|t| t.has_column(name)))
            .unwrap_or(false)
    }

    /// The physical column index (== `column_id`) of a property, if present.
    pub fn column_index(&self, id: TableId, name: &str) -> Option<usize> {
        self.node_tables
            .get(&id)
            .and_then(|t| t.column(name))
            .or_else(|| self.rel_tables.get(&id).and_then(|t| t.column(name)))
            .map(|column| column.column_id.0 as usize)
    }

    /// Append one structurally valid property definition.
    pub fn add_column(&mut self, id: TableId, definition: ColumnDefinition) -> Result<()> {
        let Some(table_name) = self.table_name(id).map(str::to_string) else {
            return Ok(());
        };
        let (mut columns, serial_sequences) = materialize_columns(&table_name, vec![definition]);
        ensure_serial_names_available(self, &serial_sequences)?;
        let column = columns.pop().expect("one definition produces one column");
        if let Some(table) = self.node_tables.get_mut(&id) {
            add_column_to(&mut table.columns, &mut table.name_to_idx, column);
        } else if let Some(table) = self.rel_tables.get_mut(&id) {
            add_column_to(&mut table.columns, &mut table.name_to_idx, column);
        }
        create_serial_sequences(self, serial_sequences)
    }

    /// The effective default of a physical column, if one exists.
    pub fn column_default(&self, id: TableId, column: usize) -> Option<ColumnDefault> {
        self.node_tables
            .get(&id)
            .map(|table| &table.columns)
            .or_else(|| self.rel_tables.get(&id).map(|table| &table.columns))
            .and_then(|columns| columns.get(column))
            .and_then(Column::default)
    }

    /// `ALTER TABLE … DROP`: remove the property at `idx`, re-indexing the rest.
    /// (The primary-key column is rejected before this is called.)
    pub fn drop_column(&mut self, id: TableId, idx: usize) {
        if let Some(t) = self.node_tables.get_mut(&id) {
            drop_column_at(&mut t.columns, &mut t.name_to_idx, idx);
            // The pk column is never dropped, but a column *before* it shifts left.
            if t.primary_key > idx {
                t.primary_key -= 1;
            }
        } else if let Some(t) = self.rel_tables.get_mut(&id) {
            drop_column_at(&mut t.columns, &mut t.name_to_idx, idx);
        }
    }

    /// `ALTER TABLE … RENAME <old> TO <new>`: rename a property in place.
    pub fn rename_column(&mut self, id: TableId, idx: usize, new: &str) {
        let rename = |columns: &mut [Column], name_to_idx: &mut HashMap<String, usize>| {
            let old_key = columns[idx].name.to_ascii_lowercase();
            columns[idx].name = new.to_string();
            name_to_idx.remove(&old_key);
            name_to_idx.insert(new.to_ascii_lowercase(), idx);
        };
        if let Some(t) = self.node_tables.get_mut(&id) {
            rename(&mut t.columns, &mut t.name_to_idx);
        } else if let Some(t) = self.rel_tables.get_mut(&id) {
            rename(&mut t.columns, &mut t.name_to_idx);
        }
    }

    /// `ALTER TABLE … RENAME TO <new>`: rename the table and re-key its name index.
    pub fn rename_table(&mut self, id: TableId, new: &str) {
        let old = if let Some(t) = self.node_tables.get_mut(&id) {
            let old = std::mem::replace(&mut t.name, new.to_string());
            Some(old)
        } else if let Some(t) = self.rel_tables.get_mut(&id) {
            let old = std::mem::replace(&mut t.name, new.to_string());
            Some(old)
        } else {
            None
        };
        if let Some(old) = old {
            self.name_to_id.remove(&old.to_ascii_lowercase());
            self.name_to_id.insert(new.to_ascii_lowercase(), id);
        }
    }

    /// Atomically create a relationship table from one structural definition.
    pub fn create_rel_table(&mut self, definition: RelTableDefinition) -> Result<TableId> {
        if self.contains_table(&definition.name) {
            return Err(Error::catalog(format!(
                "{} already exists in catalog.",
                definition.name
            )));
        }
        let mut seen = HashSet::new();
        for &(from, to) in &definition.endpoint_pairs {
            if self.node_table(from).is_none() || self.node_table(to).is_none() {
                return Err(Error::binder(
                    "REL TABLE endpoints must be node tables.".to_string(),
                ));
            }
            if !seen.insert((from, to)) {
                return Err(Error::binder("Found duplicate FROM-TO pairs.".to_string()));
            }
        }

        let (columns, serial_sequences) = materialize_columns(&definition.name, definition.columns);
        ensure_serial_names_available(self, &serial_sequences)?;
        let name_to_idx = build_name_index(&columns)?;

        let id = self.alloc_table_id();
        self.next_table_id
            .fetch_add(definition.endpoint_pairs.len() as u64, Ordering::Relaxed);
        let pairs = definition
            .endpoint_pairs
            .into_iter()
            .enumerate()
            .map(|(index, (from, to))| RelTablePair {
                from,
                to,
                member: TableId(id.0 + index as u64),
            })
            .collect::<Vec<_>>();
        for pair in &pairs {
            self.member_to_primary.insert(pair.member, id);
        }
        self.rel_tables.insert(
            id,
            RelTable {
                id,
                name: definition.name.clone(),
                pairs,
                columns,
                storage_direction: definition.storage_direction,
                comment: None,
                name_to_idx,
            },
        );
        self.name_to_id
            .insert(definition.name.to_ascii_lowercase(), id);
        create_serial_sequences(self, serial_sequences)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koko_common::RelStorageDirection;

    fn node_definition(
        name: &str,
        columns: Vec<ColumnDefinition>,
        primary_key: &str,
    ) -> NodeTableDefinition {
        NodeTableDefinition {
            name: name.to_string(),
            columns,
            primary_key: primary_key.to_string(),
        }
    }

    fn plain(name: &str, logical_type: LogicalType) -> ColumnDefinition {
        ColumnDefinition::plain(name, logical_type)
    }

    #[test]
    fn create_and_resolve() {
        let mut cat = Catalog::new();
        let p = cat
            .create_node_table(node_definition(
                "Person",
                vec![
                    plain("name", LogicalType::String),
                    plain("age", LogicalType::Int64),
                ],
                "name",
            ))
            .unwrap();
        // Case-insensitive table + column lookup.
        assert_eq!(cat.table_id("person"), Some(p));
        let t = cat.node_table(p).unwrap();
        assert_eq!(t.primary_key_column().name(), "name");
        assert!(t.column("AGE").is_some());

        let k = cat
            .create_rel_table(RelTableDefinition {
                name: "Knows".to_string(),
                endpoint_pairs: vec![(p, p)],
                columns: vec![plain("since", LogicalType::Int64)],
                storage_direction: RelStorageDirection::default(),
            })
            .unwrap();
        let r = cat.rel_table(k).unwrap();
        assert_eq!(r.from(), p);
        assert_eq!(r.to(), p);
        assert_eq!(cat.table_kind(k), Some(TableKind::Rel));
    }

    #[test]
    fn serial_column_registers_implicit_sequence() {
        let mut cat = Catalog::new();
        // A SERIAL column (index 0) gets an implicit sequence + a NextVal default.
        let id = cat
            .create_node_table(node_definition(
                "test",
                vec![ColumnDefinition::serial("id")],
                "id",
            ))
            .unwrap();
        let seq = serial_sequence_name("test", "id");
        assert_eq!(seq, "test_id_serial");
        assert!(cat.contains_sequence(&seq));
        assert_eq!(
            cat.node_table(id).unwrap().columns()[0].default(),
            Some(ColumnDefault::NextVal(seq.clone()))
        );
        // A SERIAL sequence starts at 0: the first nextval yields 0 (no increment).
        assert_eq!(cat.sequence_next_val(&seq).unwrap(), 0);
        assert_eq!(cat.sequence_next_val(&seq).unwrap(), 1);
        // DROP TABLE drops the implicit sequence, so the name is free again.
        cat.drop_table(id);
        assert!(!cat.contains_sequence(&seq));
        assert!(
            cat.create_node_table(node_definition(
                "test",
                vec![ColumnDefinition::serial("id")],
                "id",
            ))
            .is_ok()
        );
    }

    #[test]
    fn serial_name_collision_is_atomic() {
        let mut cat = Catalog::new();
        cat.create_sequence(Sequence::new(
            "t_id_serial".into(),
            1,
            1,
            1,
            i64::MAX,
            false,
        ))
        .unwrap();
        // The pre-existing sequence collides; the table must NOT be half-created.
        assert!(
            cat.create_node_table(node_definition(
                "t",
                vec![ColumnDefinition::serial("id")],
                "id",
            ))
            .is_err()
        );
        assert!(!cat.contains_table("t"));
    }

    #[test]
    fn duplicate_and_missing_pk() {
        let mut cat = Catalog::new();
        cat.create_node_table(node_definition(
            "T",
            vec![plain("id", LogicalType::Int64)],
            "id",
        ))
        .unwrap();
        assert!(
            cat.create_node_table(node_definition(
                "t",
                vec![plain("id", LogicalType::Int64)],
                "id",
            ))
            .is_err()
        );
        assert!(
            cat.create_node_table(node_definition(
                "U",
                vec![plain("id", LogicalType::Int64)],
                "missing",
            ))
            .is_err()
        );
    }
}
