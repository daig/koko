//! `koko-catalog` — the schema catalog: node/rel table entries, their columns,
//! primary keys, and FROM/TO wiring.
//!
//! Fidelity points preserved from the C++ catalog: table names and property
//! names are looked up **case-insensitively**; column ids are assigned
//! monotonically at table-creation time; a node's identity is `(table_id,
//! offset)`. MVCC, persistence, and DROP/ALTER arrive in later phases.

use koko_common::{ColumnId, Error, LogicalType, Result, TableId, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicU64, Ordering},
};

/// The `DEFAULT` a column applies to rows that omit it at insert time. Stored as
/// resolved data (never a bound expression) so the catalog keeps depending only
/// on `koko-common`: constant defaults are folded to a `Value` by the exec layer
/// before they reach here; a `nextval` default keeps the sequence name and is
/// evaluated per row at insert.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ColumnDefault {
    /// No default — an omitted column is `NULL`. (A `SERIAL` column instead carries
    /// a [`ColumnDefault::NextVal`] of its implicit sequence; see
    /// [`serial_sequence_name`].)
    #[default]
    None,
    /// A constant value (a literal or a folded computed expression).
    Const(Value),
    /// `nextval('seq')` — advance the named sequence once per inserted row.
    NextVal(String),
}

/// A single column (property) of a table.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: LogicalType,
    /// C++ `TABLE_INFO` renders the catalog's logical type string. Rust resolves
    /// `SERIAL` to physical `INT64`, so keep the display text separately.
    pub type_text: String,
    pub column_id: ColumnId,
    pub default: ColumnDefault,
    /// C++ stores the parsed default expression's raw name; execution also needs
    /// the folded [`ColumnDefault`], so this is display-only catalog metadata.
    pub default_text: String,
}

/// Whether a catalog entry is a node table or a relationship table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Node,
    Rel,
}

/// Relationship-table storage direction metadata (`WITH (storage_direction=...)`).
/// The in-memory storage still materializes both adjacency directions; this mirrors
/// C++ catalog/table-info metadata and validates the DDL option.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RelStorageDirection {
    Fwd,
    Bwd,
    #[default]
    Both,
}

impl RelStorageDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            RelStorageDirection::Fwd => "fwd",
            RelStorageDirection::Bwd => "bwd",
            RelStorageDirection::Both => "both",
        }
    }
}

/// A node table: an ordered set of property columns plus a primary key.
#[derive(Debug, Clone)]
pub struct NodeTable {
    pub id: TableId,
    pub name: String,
    pub columns: Vec<Column>,
    /// Index into `columns` of the primary-key column.
    pub primary_key: usize,
    /// The table comment set by `COMMENT ON` (shown in `SHOW_TABLES`).
    pub comment: Option<String>,
    name_to_idx: HashMap<String, usize>,
}

/// A relationship table: property columns plus one or more resolved FROM-TO
/// node-table pairs (multiple for a multi-pair rel table).
#[derive(Debug, Clone)]
pub struct RelTable {
    pub id: TableId,
    pub name: String,
    pub pairs: Vec<(TableId, TableId)>,
    /// Per-pair physical table ids, index-aligned with `pairs` (one storage table
    /// per FROM-TO pair, matching C++/Kùzu). `member_ids[0]` is the primary `id` at
    /// creation; a relationship's runtime `_ID` carries its pair's member id.
    pub member_ids: Vec<TableId>,
    pub columns: Vec<Column>,
    pub storage_direction: RelStorageDirection,
    /// The table comment set by `COMMENT ON` (shown in `SHOW_TABLES`).
    pub comment: Option<String>,
    name_to_idx: HashMap<String, usize>,
}

/// Case-insensitive column lookup shared by both table kinds.
fn column_by_name<'a>(
    columns: &'a [Column],
    name_to_idx: &HashMap<String, usize>,
    name: &str,
) -> Option<&'a Column> {
    name_to_idx
        .get(&name.to_ascii_lowercase())
        .map(|&i| &columns[i])
}

/// Append a column at the next position, stamping its `column_id` to that index.
fn add_column_to(
    columns: &mut Vec<Column>,
    name_to_idx: &mut HashMap<String, usize>,
    mut col: Column,
) {
    let idx = columns.len();
    col.column_id = ColumnId(idx as u32);
    name_to_idx.insert(col.name.to_ascii_lowercase(), idx);
    columns.push(col);
}

fn catalog_default_text(default: &ColumnDefault) -> String {
    match default {
        ColumnDefault::None => "NULL".to_string(),
        ColumnDefault::Const(v) => v.to_result_string(),
        ColumnDefault::NextVal(s) => format!("nextval('{s}')"),
    }
}

fn column_type_text(ty: &LogicalType, type_texts: &[String], i: usize) -> String {
    type_texts.get(i).cloned().unwrap_or_else(|| ty.to_string())
}

fn column_default_text(default: &ColumnDefault, default_texts: &[String], i: usize) -> String {
    default_texts
        .get(i)
        .cloned()
        .unwrap_or_else(|| catalog_default_text(default))
}

/// Remove the column at `idx`, re-assigning the trailing columns' ids to their
/// new positions (ids stay equal to the physical column index, which the storage
/// layer mirrors) and rebuilding the name index.
fn drop_column_at(columns: &mut Vec<Column>, name_to_idx: &mut HashMap<String, usize>, idx: usize) {
    columns.remove(idx);
    for (i, c) in columns.iter_mut().enumerate() {
        c.column_id = ColumnId(i as u32);
    }
    *name_to_idx = build_name_index(columns).expect("remaining column names stay unique");
}

impl NodeTable {
    pub fn column(&self, name: &str) -> Option<&Column> {
        column_by_name(&self.columns, &self.name_to_idx, name)
    }
    pub fn has_column(&self, name: &str) -> bool {
        self.name_to_idx.contains_key(&name.to_ascii_lowercase())
    }
    pub fn primary_key_column(&self) -> &Column {
        &self.columns[self.primary_key]
    }
}

impl RelTable {
    /// The representative FROM table (the first pair's FROM). Use `pairs` for the
    /// full set of FROM-TO pairs.
    pub fn from(&self) -> TableId {
        self.pairs[0].0
    }
    /// The representative TO table (the first pair's TO).
    pub fn to(&self) -> TableId {
        self.pairs[0].1
    }
    pub fn column(&self, name: &str) -> Option<&Column> {
        column_by_name(&self.columns, &self.name_to_idx, name)
    }
    pub fn has_column(&self, name: &str) -> bool {
        self.name_to_idx.contains_key(&name.to_ascii_lowercase())
    }
}

/// Build a `name -> index` map, erroring on duplicate (case-insensitive) names.
fn build_name_index(columns: &[Column]) -> Result<HashMap<String, usize>> {
    let mut map = HashMap::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        let key = c.name.to_ascii_lowercase();
        if map.insert(key, i).is_some() {
            return Err(Error::binder(format!(
                "Duplicated column name: {}, column name must be unique.",
                c.name
            )));
        }
    }
    Ok(map)
}

/// A `CREATE SEQUENCE` object. The definition fields are immutable; the running
/// value (`curr`/`usage`) lives behind atomics so `nextval`/`currval` can advance
/// it through a shared `&Catalog` during query execution — and so `Catalog` is
/// `Sync` (the prerequisite for P3 step 9's parallel query execution). The engine
/// mutates the counter only under the single connection lock (a `nextval` query is
/// never parallelized — `SequenceCall` is not a parallel spine op), so `Relaxed`
/// suffices; the lock supplies the happens-before edge. (The C++ engine guards the
/// same state with a mutex.)
#[derive(Debug)]
pub struct Sequence {
    pub name: String,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cycle: bool,
    /// The last value `nextval` returned (initialised to `start`).
    curr: AtomicI64,
    /// `nextval` call count (0 ⇒ never called; the first call returns `start`).
    usage: AtomicU64,
}

// Atomics are not `Clone`, but the catalog is cloned for the per-transaction
// copy-on-write snapshot, so preserve the running counter exactly (the old `Cell`
// derive copied the value).
impl Clone for Sequence {
    fn clone(&self) -> Self {
        Sequence {
            name: self.name.clone(),
            start: self.start,
            increment: self.increment,
            min: self.min,
            max: self.max,
            cycle: self.cycle,
            curr: AtomicI64::new(self.curr.load(Ordering::Relaxed)),
            usage: AtomicU64::new(self.usage.load(Ordering::Relaxed)),
        }
    }
}

impl Sequence {
    pub fn new(name: String, start: i64, increment: i64, min: i64, max: i64, cycle: bool) -> Self {
        Sequence {
            name,
            start,
            increment,
            min,
            max,
            cycle,
            curr: AtomicI64::new(start),
            usage: AtomicU64::new(0),
        }
    }

    /// The value `SHOW_SEQUENCES` reports in its "start value" column: the
    /// *defined* start, regardless of use (audit R4 — C++ never shows mutated
    /// state there; `currval` is the way to read the position).
    pub fn display_val(&self) -> i64 {
        self.start
    }

    /// Advance and return the next value (mirrors the C++ `nextValNoLock`): the
    /// first call yields `start`; later calls add `increment`, wrapping on `CYCLE`
    /// and erroring at the bound otherwise.
    fn next_val(&self) -> Result<i64> {
        if self.usage.load(Ordering::Relaxed) == 0 {
            self.usage.store(1, Ordering::Relaxed);
            return Ok(self.curr.load(Ordering::Relaxed)); // == start
        }
        let min_err = || {
            Error::catalog(format!(
                "nextval: reached minimum value of sequence \"{}\" {}",
                self.name, self.min
            ))
        };
        let max_err = || {
            Error::catalog(format!(
                "nextval: reached maximum value of sequence \"{}\" {}",
                self.name, self.max
            ))
        };
        let checked = self
            .curr
            .load(Ordering::Relaxed)
            .checked_add(self.increment);
        let next = if self.cycle {
            match checked {
                // An i64 overflow wraps to the far end (min for ascending, max for
                // descending) just like crossing the configured bound.
                None => {
                    if self.increment < 0 {
                        self.max
                    } else {
                        self.min
                    }
                }
                Some(n) if n < self.min => self.max,
                Some(n) if n > self.max => self.min,
                Some(n) => n,
            }
        } else {
            match checked {
                None if self.increment < 0 => return Err(min_err()),
                None => return Err(max_err()),
                Some(n) if n < self.min => return Err(min_err()),
                Some(n) if n > self.max => return Err(max_err()),
                Some(n) => n,
            }
        };
        self.curr.store(next, Ordering::Relaxed);
        self.usage.fetch_add(1, Ordering::Relaxed);
        Ok(next)
    }

    /// The last value `nextval` returned; errors if `nextval` was never called.
    fn curr_val(&self) -> Result<i64> {
        if self.usage.load(Ordering::Relaxed) == 0 {
            return Err(Error::catalog(format!(
                "currval: sequence \"{}\" is not yet defined. To define the sequence, call nextval \
                 first.",
                self.name
            )));
        }
        Ok(self.curr.load(Ordering::Relaxed))
    }
}

/// The implicit sequence name for a `SERIAL` column, mirroring C++'s
/// `SequenceCatalogEntry::getSerialName`: `<table>_<col>_serial`. The sequence is
/// created alongside the table and backs the column's `nextval` default, so
/// `currval`/`nextval`/`SHOW_SEQUENCES` treat it like any user sequence.
pub fn serial_sequence_name(table: &str, col: &str) -> String {
    format!("{table}_{col}_serial")
}

/// The observable implementation kind of a named primary-key index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    Hash,
    Art,
}

impl IndexType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Hash => "HASH",
            Self::Art => "ART",
        }
    }
}

/// Catalog metadata for one explicitly named in-memory primary-key index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub name: String,
    pub table_id: TableId,
    pub index_type: IndexType,
    pub property_names: Vec<String>,
}

/// Outcome of idempotent index creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateIndexOutcome {
    Created,
    Existing(String),
}

/// Validated physical shape of a local read-only `icebug-disk` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IcebugTableSource {
    Node {
        path: PathBuf,
        num_rows: u64,
    },
    RelCsr {
        indices_path: PathBuf,
        indptr_path: PathBuf,
        target_column: String,
        num_rows: u64,
        num_bound_nodes: u64,
    },
    RelFlat {
        path: PathBuf,
        source_column: String,
        target_column: String,
        num_rows: u64,
    },
}

impl IcebugTableSource {
    pub fn num_rows(&self) -> u64 {
        match self {
            Self::Node { num_rows, .. }
            | Self::RelCsr { num_rows, .. }
            | Self::RelFlat { num_rows, .. } => *num_rows,
        }
    }
}

/// Catalog ownership for a local read-only `icebug-disk` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcebugTable {
    pub storage: String,
    pub source: Option<IcebugTableSource>,
    pub load_error: Option<String>,
}

/// Hidden physical tables that back one schemaless graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnyTables {
    pub nodes: TableId,
    pub edges: TableId,
}

/// The in-memory schema catalog.
///
/// `Clone` is load-bearing for transactions: a read-write transaction takes a
/// copy-on-write snapshot of the catalog (alongside the storage snapshot) at
/// `BEGIN`, binds/plans/executes against it, and either swaps it into the
/// committed catalog at `COMMIT` or drops it at `ROLLBACK` — so DDL inside a
/// transaction is isolated and fully rolled back. (Sequence `nextval` state
/// rides along in the snapshot; see `docs/KNOWN_GAPS.md`.)
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
        let nodes = self.create_node_table(
            "_nodes",
            vec![
                ("id".to_string(), LogicalType::Serial),
                (
                    "label".to_string(),
                    LogicalType::List(Box::new(LogicalType::String)),
                ),
                ("data".to_string(), LogicalType::Json),
            ],
            &[],
            &[0],
            "id",
        )?;
        let edges = self.create_rel_table(
            "_edges",
            &[(nodes, nodes)],
            vec![
                ("_id".to_string(), LogicalType::InternalId),
                ("label".to_string(), LogicalType::String),
                ("data".to_string(), LogicalType::Json),
            ],
            &[],
        )?;
        let tables = AnyTables { nodes, edges };
        self.any_tables = Some(tables);
        Ok(tables)
    }

    /// Hidden schemaless-table identities, if this is an `ANY` graph catalog.
    pub const fn any_tables(&self) -> Option<AnyTables> {
        self.any_tables
    }

    pub fn is_any_node_table(&self, table: TableId) -> bool {
        self.any_tables.is_some_and(|tables| tables.nodes == table)
    }

    pub fn is_any_rel_table(&self, table: TableId) -> bool {
        self.any_tables.is_some_and(|tables| tables.edges == table)
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
        match self.rel_table(id) {
            Some(t) => t
                .member_ids
                .iter()
                .zip(&t.pairs)
                .map(|(&m, &(f, to))| (m, f, to))
                .collect(),
            None => Vec::new(),
        }
    }

    /// The per-pair member id a directed edge `(from)->(to)` belongs to within a rel
    /// group, or `None` if no declared pair matches. The match is **exact** on
    /// orientation: a `CREATE (b)-[:R]->(a)` edge belongs to the `(B,A)` pair, never a
    /// reverse-declared `(A,B)` one — mis-routing there files the edge under the wrong
    /// FROM/TO, which the per-pair adjacency (keyed by the pair's FROM/TO table) then
    /// can't find. `id` may be the primary or any member.
    pub fn rel_member_for(&self, id: TableId, from: TableId, to: TableId) -> Option<TableId> {
        let t = self.rel_table(id)?;
        t.pairs
            .iter()
            .zip(&t.member_ids)
            .find(|((f, tt), _)| *f == from && *tt == to)
            .map(|(_, &m)| m)
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

    /// Create a node table. `columns` are `(name, type)` in declaration order;
    /// `pk_name` must name one of them. Column ids are assigned by position.
    /// `defaults` is aligned to `columns` by index (a shorter/empty slice ⇒ the
    /// missing entries are [`ColumnDefault::None`]). `serial_columns` are the
    /// indices of `SERIAL` columns: each gets an implicit sequence
    /// ([`serial_sequence_name`]) plus a matching `nextval` default — the single
    /// source of truth for its auto-increment (storage holds no counter).
    pub fn create_node_table(
        &mut self,
        name: &str,
        columns: Vec<(String, LogicalType)>,
        defaults: &[ColumnDefault],
        serial_columns: &[usize],
        pk_name: &str,
    ) -> Result<TableId> {
        self.create_node_table_with_metadata(
            name,
            columns,
            defaults,
            &[],
            &[],
            serial_columns,
            pk_name,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_node_table_with_metadata(
        &mut self,
        name: &str,
        columns: Vec<(String, LogicalType)>,
        defaults: &[ColumnDefault],
        type_texts: &[String],
        default_texts: &[String],
        serial_columns: &[usize],
        pk_name: &str,
    ) -> Result<TableId> {
        if self.contains_table(name) {
            return Err(Error::catalog(format!("{name} already exists in catalog.")));
        }
        if columns.is_empty() {
            return Err(Error::binder(format!(
                "Cannot create node table {name} with no columns."
            )));
        }
        let mut columns: Vec<Column> = columns
            .into_iter()
            .enumerate()
            .map(|(i, (cname, ty))| {
                let default = defaults.get(i).cloned().unwrap_or_default();
                Column {
                    type_text: column_type_text(&ty, type_texts, i),
                    default_text: column_default_text(&default, default_texts, i),
                    name: cname,
                    ty,
                    column_id: ColumnId(i as u32),
                    default,
                }
            })
            .collect();
        let name_to_idx = build_name_index(&columns)?;
        let primary_key = *name_to_idx
            .get(&pk_name.to_ascii_lowercase())
            .ok_or_else(|| {
                Error::binder(format!(
                    "Primary key {pk_name} does not match any of the predefined node properties."
                ))
            })?;

        // A `SERIAL` column is modelled exactly as Kùzu does: an implicit sequence
        // named `<table>_<col>_serial` plus a `DEFAULT nextval('<that name>')`.
        // Validate every name is free *before* mutating the catalog, so a collision
        // can't leave a half-created table (auto-commit DDL writes committed state
        // directly — there is no snapshot to drop on error).
        let serial_seqs: Vec<(usize, String)> = serial_columns
            .iter()
            .map(|&i| (i, serial_sequence_name(name, &columns[i].name)))
            .collect();
        for (_, seq) in &serial_seqs {
            if self.contains_sequence(seq) {
                // C++ reports this as a Catalog exception (like every other
                // name-collision in the catalog), not a Binder one.
                return Err(Error::catalog(format!("{seq} already exists in catalog.")));
            }
        }
        // Stamp the per-row `nextval` default on each `SERIAL` column, overriding the
        // `None` the binder produced (a `SERIAL` column never carries an explicit
        // default — the binder rejects that).
        for (i, seq) in &serial_seqs {
            columns[*i].default = ColumnDefault::NextVal(seq.clone());
            if default_texts.get(*i).is_none() {
                columns[*i].default_text.clear();
            }
        }

        let id = self.alloc_table_id();
        self.node_tables.insert(
            id,
            NodeTable {
                id,
                name: name.to_string(),
                columns,
                primary_key,
                comment: None,
                name_to_idx,
            },
        );
        self.name_to_id.insert(name.to_ascii_lowercase(), id);
        // Create the implicit sequence(s): start=0, increment=1, min=0,
        // max=INT64_MAX, cycle=false — so the first `nextval` returns 0 (first id 0,
        // currval-after-one-insert 0). Pre-checked free above, so this can't fail.
        for (_, seq) in serial_seqs {
            self.create_sequence(Sequence::new(seq, 0, 1, 0, i64::MAX, false))?;
        }
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
        self.rel_tables
            .get(&id)
            .is_some_and(|t| t.pairs.contains(&(from, to)))
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
        let t = self.rel_tables.get_mut(&id).unwrap();
        t.pairs.push((from, to));
        t.member_ids.push(member);
        Some(member)
    }

    /// `ALTER … DROP FROM x TO y`: remove an endpoint pair (caller verified presence).
    /// Dropping the last pair leaves an empty rel group. Returns the retired per-pair
    /// member id (so the caller drops its storage store); the id is not reused.
    pub fn drop_rel_pair(&mut self, id: TableId, from: TableId, to: TableId) -> Option<TableId> {
        let dropped = self.rel_tables.get_mut(&id).and_then(|t| {
            t.pairs.iter().position(|&p| p == (from, to)).map(|pos| {
                t.pairs.remove(pos);
                t.member_ids.remove(pos)
            })
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
            .find(|rt| rt.pairs.iter().any(|&(f, t)| f == node || t == node))
            .map(|rt| rt.name.as_str())
    }

    /// Drop a node or rel table by id, removing it from the catalog and freeing
    /// its name. The table id is *not* reused. (Catalog rollback within a
    /// transaction is a later-phase concern; auto-committed DDL is final.)
    pub fn drop_table(&mut self, id: TableId) {
        if let Some(t) = self.node_tables.remove(&id) {
            self.name_to_id.remove(&t.name.to_ascii_lowercase());
            // Drop each implicit `SERIAL` sequence (mirroring C++'s
            // `dropSerialSequence`) so re-CREATE of the same table doesn't collide. A
            // column is implicit-SERIAL iff its default is `nextval` of its own
            // canonical serial name.
            for c in &t.columns {
                let seq = serial_sequence_name(&t.name, &c.name);
                if c.default == ColumnDefault::NextVal(seq.clone()) {
                    self.drop_sequence(&seq);
                }
            }
        } else if let Some(t) = self.rel_tables.remove(&id) {
            self.name_to_id.remove(&t.name.to_ascii_lowercase());
            for m in &t.member_ids {
                self.member_to_primary.remove(m);
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
            .map(|c| c.column_id.0 as usize)
    }

    /// `ALTER TABLE … ADD`: append a property column. The caller has already
    /// verified the property does not exist.
    pub fn add_column(&mut self, id: TableId, name: &str, ty: LogicalType, default: ColumnDefault) {
        let type_text = ty.to_string();
        let default_text = catalog_default_text(&default);
        self.add_column_with_metadata(id, name, ty, default, type_text, default_text);
    }

    pub fn add_column_with_metadata(
        &mut self,
        id: TableId,
        name: &str,
        ty: LogicalType,
        default: ColumnDefault,
        type_text: String,
        default_text: String,
    ) {
        // `column_id` is stamped to the true index by `add_column_to`.
        let col = Column {
            name: name.to_string(),
            ty,
            type_text,
            column_id: ColumnId(0),
            default,
            default_text,
        };
        if let Some(t) = self.node_tables.get_mut(&id) {
            add_column_to(&mut t.columns, &mut t.name_to_idx, col);
        } else if let Some(t) = self.rel_tables.get_mut(&id) {
            add_column_to(&mut t.columns, &mut t.name_to_idx, col);
        }
    }

    /// The `DEFAULT` of a column by physical index (for the insert path); a
    /// missing table/column yields [`ColumnDefault::None`].
    pub fn column_default(&self, id: TableId, col_idx: usize) -> ColumnDefault {
        self.node_tables
            .get(&id)
            .map(|t| &t.columns)
            .or_else(|| self.rel_tables.get(&id).map(|t| &t.columns))
            .and_then(|cols| cols.get(col_idx))
            .map(|c| c.default.clone())
            .unwrap_or_default()
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

    /// Create a relationship table over one or more (already-resolved) FROM-TO
    /// node-table id pairs.
    pub fn create_rel_table(
        &mut self,
        name: &str,
        pairs: &[(TableId, TableId)],
        columns: Vec<(String, LogicalType)>,
        defaults: &[ColumnDefault],
    ) -> Result<TableId> {
        self.create_rel_table_with_metadata(
            name,
            pairs,
            columns,
            defaults,
            &[],
            &[],
            RelStorageDirection::Both,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_rel_table_with_metadata(
        &mut self,
        name: &str,
        pairs: &[(TableId, TableId)],
        columns: Vec<(String, LogicalType)>,
        defaults: &[ColumnDefault],
        type_texts: &[String],
        default_texts: &[String],
        storage_direction: RelStorageDirection,
    ) -> Result<TableId> {
        self.create_rel_table_with_serials(
            name,
            pairs,
            columns,
            defaults,
            type_texts,
            default_texts,
            &[],
            storage_direction,
        )
    }

    /// Like [`Self::create_rel_table_with_metadata`], with `serial_columns`:
    /// each SERIAL property gets its implicit sequence + `nextval` default,
    /// exactly like the node-table path.
    #[allow(clippy::too_many_arguments)]
    pub fn create_rel_table_with_serials(
        &mut self,
        name: &str,
        pairs: &[(TableId, TableId)],
        columns: Vec<(String, LogicalType)>,
        defaults: &[ColumnDefault],
        type_texts: &[String],
        default_texts: &[String],
        serial_columns: &[usize],
        storage_direction: RelStorageDirection,
    ) -> Result<TableId> {
        if self.contains_table(name) {
            return Err(Error::catalog(format!("{name} already exists in catalog.")));
        }
        let mut seen = HashSet::new();
        for &(from, to) in pairs {
            if self.node_table(from).is_none() || self.node_table(to).is_none() {
                return Err(Error::binder(
                    "REL TABLE endpoints must be node tables.".to_string(),
                ));
            }
            if !seen.insert((from, to)) {
                return Err(Error::binder("Found duplicate FROM-TO pairs.".to_string()));
            }
        }

        let mut columns: Vec<Column> = columns
            .into_iter()
            .enumerate()
            .map(|(i, (cname, ty))| {
                let default = defaults.get(i).cloned().unwrap_or_default();
                Column {
                    type_text: column_type_text(&ty, type_texts, i),
                    default_text: column_default_text(&default, default_texts, i),
                    name: cname,
                    ty,
                    column_id: ColumnId(i as u32),
                    default,
                }
            })
            .collect();
        // SERIAL properties: implicit sequence + `nextval` default, like nodes.
        let serial_seqs: Vec<(usize, String)> = serial_columns
            .iter()
            .map(|&i| (i, serial_sequence_name(name, &columns[i].name)))
            .collect();
        for (_, seq) in &serial_seqs {
            if self.contains_sequence(seq) {
                return Err(Error::catalog(format!("{seq} already exists in catalog.")));
            }
        }
        for (i, seq) in &serial_seqs {
            columns[*i].default = ColumnDefault::NextVal(seq.clone());
            if default_texts.get(*i).is_none() {
                columns[*i].default_text.clear();
            }
        }
        let name_to_idx = build_name_index(&columns)?;

        let id = self.alloc_table_id();
        // `alloc_table_id` took the rel's primary id (the first FROM/TO pair — used
        // for `tableID:offset`, storage adjacency, and references). The C++ engine
        // then allocates one id per *additional* pair plus one for the rel *group*
        // entry, so we reserve `pairs.len()` more here (the group id, surfaced by
        // `SHOW_TABLES`, is `id + pairs.len()`). For a single-pair rel this skips by
        // two (one pair + the group) — matching tinysnb (knows=3, studyAt=5, …) — so
        // single-pair behaviour is unchanged; multi-pair rel groups now match Kùzu.
        self.next_table_id
            .fetch_add(pairs.len() as u64, Ordering::Relaxed);
        // One physical (per-pair) table id per FROM-TO pair, drawn from the ids just
        // reserved: `[id, id+1, …, id+pairs.len()-1]` (the group id `id+pairs.len()`
        // is not a member). A rel's runtime `_ID` carries its pair's member id; the
        // member resolves back to this group entry via `member_to_primary`.
        let member_ids: Vec<TableId> = (0..pairs.len() as u64).map(|i| TableId(id.0 + i)).collect();
        for &m in &member_ids {
            self.member_to_primary.insert(m, id);
        }
        self.rel_tables.insert(
            id,
            RelTable {
                id,
                name: name.to_string(),
                pairs: pairs.to_vec(),
                member_ids,
                columns,
                storage_direction,
                comment: None,
                name_to_idx,
            },
        );
        self.name_to_id.insert(name.to_ascii_lowercase(), id);
        // Implicit SERIAL sequences (start 0), pre-checked free above.
        for (_, seq) in serial_seqs {
            self.create_sequence(Sequence::new(seq, 0, 1, 0, i64::MAX, false))?;
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_resolve() {
        let mut cat = Catalog::new();
        let p = cat
            .create_node_table(
                "Person",
                vec![
                    ("name".into(), LogicalType::String),
                    ("age".into(), LogicalType::Int64),
                ],
                &[],
                &[],
                "name",
            )
            .unwrap();
        // Case-insensitive table + column lookup.
        assert_eq!(cat.table_id("person"), Some(p));
        let t = cat.node_table(p).unwrap();
        assert_eq!(t.primary_key_column().name, "name");
        assert!(t.column("AGE").is_some());

        let k = cat
            .create_rel_table(
                "Knows",
                &[(p, p)],
                vec![("since".into(), LogicalType::Int64)],
                &[],
            )
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
            .create_node_table(
                "test",
                vec![("id".into(), LogicalType::Int64)],
                &[],
                &[0],
                "id",
            )
            .unwrap();
        let seq = serial_sequence_name("test", "id");
        assert_eq!(seq, "test_id_serial");
        assert!(cat.contains_sequence(&seq));
        assert_eq!(
            cat.node_table(id).unwrap().columns[0].default,
            ColumnDefault::NextVal(seq.clone())
        );
        // A SERIAL sequence starts at 0: the first nextval yields 0 (no increment).
        assert_eq!(cat.sequence_next_val(&seq).unwrap(), 0);
        assert_eq!(cat.sequence_next_val(&seq).unwrap(), 1);
        // DROP TABLE drops the implicit sequence, so the name is free again.
        cat.drop_table(id);
        assert!(!cat.contains_sequence(&seq));
        assert!(
            cat.create_node_table(
                "test",
                vec![("id".into(), LogicalType::Int64)],
                &[],
                &[0],
                "id",
            )
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
            cat.create_node_table(
                "t",
                vec![("id".into(), LogicalType::Int64)],
                &[],
                &[0],
                "id",
            )
            .is_err()
        );
        assert!(!cat.contains_table("t"));
    }

    #[test]
    fn duplicate_and_missing_pk() {
        let mut cat = Catalog::new();
        cat.create_node_table("T", vec![("id".into(), LogicalType::Int64)], &[], &[], "id")
            .unwrap();
        assert!(
            cat.create_node_table("t", vec![("id".into(), LogicalType::Int64)], &[], &[], "id")
                .is_err()
        );
        assert!(
            cat.create_node_table(
                "U",
                vec![("id".into(), LogicalType::Int64)],
                &[],
                &[],
                "missing"
            )
            .is_err()
        );
    }
}
