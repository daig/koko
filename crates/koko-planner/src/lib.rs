//! `koko-planner` — turn a [`BoundQuery`]'s match graph into a naive operator
//! tree plus a [`RowLayout`] that assigns every variable/property a concrete
//! column index in the runtime chunk.
//!
//! The P0 planner is a simple greedy join-order: scan the first node, extend
//! along relationships to reach connected nodes (with a residual equality
//! filter when both endpoints are already bound), and cross-product disconnected
//! components. No cost model, no factorization, no pushdown — those are P3.

use koko_binder::{
    BoundCreate, BoundDelete, BoundExpr, BoundMatch, BoundPart, BoundQuery, BoundRegularQuery,
    BoundSet, BoundTableFunc, BoundUnwind, BoundUpdate, CsvLoadOptions, PathSemantic,
    RecursiveFilter, RecursiveMode, SequenceFn, SubqueryKind, VarId, VarKind,
};
use koko_catalog::Catalog;
use koko_common::{ExtendDir, LogicalType, Result, TableId, file_resolver::FileFormat};
use std::collections::{HashMap, HashSet};

mod cost;
mod optimize;
pub use cost::StatsMap;
pub use optimize::{optimize, optimize_regular};

/// Building a whole decorrelated relation is wasteful for a selective outer pipeline; below this
/// estimate, repeated seeded probes retain less state and usually perform less work.
const DECORRELATE_MIN_PROBE_ROWS: f64 = 1024.0;

/// Maps a table column id to its position in the runtime chunk.
#[derive(Debug, Clone)]
pub struct PropCol {
    pub column_id: u32,
    pub col_index: usize,
}

/// A property's location and type in the runtime chunk.
#[derive(Debug, Clone)]
pub struct LayoutProp {
    pub name: String,
    pub col_index: usize,
    pub ty: LogicalType,
}

/// Per-variable column bookkeeping in the [`RowLayout`].
#[derive(Debug, Clone)]
pub enum VarColKind {
    Node {
        /// Representative candidate table (`None` for an unlabeled pattern with no
        /// node tables — an empty scan). Currently informational only.
        table: Option<TableId>,
        label: String,
    },
    Rel {
        /// Representative candidate table (`None` for an unlabeled pattern with no
        /// rel tables — an empty extend). Currently informational only.
        table: Option<TableId>,
        label: String,
        /// Chunk column holding the FROM-node's internal id.
        src_id_col: usize,
        /// Chunk column holding the TO-node's internal id.
        dst_id_col: usize,
    },
    /// A scalar (value-typed) variable occupying a single chunk column.
    Scalar,
}

/// Where a variable's columns live in the runtime chunk.
#[derive(Debug, Clone)]
pub struct VarColumns {
    pub var: VarId,
    pub kind: VarColKind,
    pub id_col: usize,
    pub props: Vec<LayoutProp>,
    /// Column holding the var's *materialized* node/rel value, allocated only
    /// when an expression consumes the value mid-pipeline (audit V12 seam) and
    /// filled by [`PlanOp::MaterializeValues`].
    pub value_col: Option<usize>,
}

/// The runtime column layout: the type of every chunk column and where each
/// variable/property lives.
#[derive(Debug, Clone, Default)]
pub struct RowLayout {
    pub col_types: Vec<LogicalType>,
    vars: Vec<VarColumns>,
    index: HashMap<VarId, usize>,
    /// Result column of each lifted subquery, indexed by its id.
    subquery_cols: Vec<usize>,
    /// Result column of each lifted `nextval`/`currval`, indexed by its id.
    sequence_cols: Vec<usize>,
    /// Every table id → its name, for evaluating `label()`/`labels()` (the
    /// expression evaluator has no catalog).
    pub table_names: HashMap<TableId, String>,
}

impl RowLayout {
    pub fn width(&self) -> usize {
        self.col_types.len()
    }

    /// The column holding subquery `id`'s per-row result.
    pub fn subquery_column(&self, id: usize) -> Option<usize> {
        self.subquery_cols.get(id).copied()
    }

    /// The column holding sequence-call `id`'s per-row result.
    pub fn sequence_column(&self, id: usize) -> Option<usize> {
        self.sequence_cols.get(id).copied()
    }

    fn alloc(&mut self, ty: LogicalType) -> usize {
        let i = self.col_types.len();
        self.col_types.push(ty);
        i
    }

    fn add_var(&mut self, vc: VarColumns) {
        self.index.insert(vc.var, self.vars.len());
        self.vars.push(vc);
    }

    /// Allocate a single value column for a scalar variable, returning its index.
    fn add_scalar(&mut self, var: VarId, ty: LogicalType) -> usize {
        let col = self.alloc(ty);
        self.add_var(VarColumns {
            var,
            kind: VarColKind::Scalar,
            id_col: col,
            props: Vec::new(),
            value_col: None,
        });
        col
    }

    pub fn var(&self, v: VarId) -> &VarColumns {
        &self.vars[self.index[&v]]
    }

    /// The internal `vars` slot `v` currently resolves to. Paired with
    /// [`Self::restore_var_slot`] so the decorrelation pass (P3 step 10b L1) can
    /// re-scope a correlated variable: capture its outer slot, let `build_match`
    /// re-scan it into fresh columns for the join's build side, then point it back
    /// at the outer columns so the rest of the plan resolves it unchanged.
    fn var_slot(&self, v: VarId) -> Option<usize> {
        self.index.get(&v).copied()
    }

    fn restore_var_slot(&mut self, v: VarId, slot: usize) {
        self.index.insert(v, slot);
    }

    pub fn try_var(&self, v: VarId) -> Option<&VarColumns> {
        self.index.get(&v).map(|&i| &self.vars[i])
    }

    /// Every variable that currently has columns in this layout (in carried order
    /// is not guaranteed). Used to find a MERGE's non-key payload columns.
    pub fn var_ids(&self) -> impl Iterator<Item = VarId> + '_ {
        self.index.keys().copied()
    }

    /// Resolve `(var, prop)` to a chunk column index (the resolver contract used
    /// by `koko-expr`). `None` property ⇒ the variable's internal-id column.
    pub fn column(&self, var: VarId, prop: Option<&str>) -> Option<usize> {
        let vc = self.try_var(var)?;
        match prop {
            None => Some(vc.id_col),
            Some(p) => vc
                .props
                .iter()
                .find(|x| x.name.eq_ignore_ascii_case(p))
                .map(|x| x.col_index),
        }
    }
}

/// One candidate table of a [`ScanNode`]: how that table's own columns map into
/// the shared (union) layout columns.
#[derive(Debug, Clone)]
pub struct ScanTable {
    pub table: TableId,
    pub prop_cols: Vec<PropCol>,
}

/// A scan of one or more node tables — populates `var`'s id + property columns.
/// Multiple tables for an unlabeled / multi-label (polymorphic) node: the scan
/// emits each table's rows, filling that table's properties and leaving columns
/// belonging only to other candidate tables NULL.
#[derive(Debug, Clone)]
pub struct ScanNode {
    pub var: VarId,
    pub id_col: usize,
    pub tables: Vec<ScanTable>,
}

/// A primary-key point lookup: the optimizer (see [`crate::optimize`]) rewrites a
/// `Filter(var.<pk> = <expr>)` directly above a single-table [`ScanNode`] into
/// this, turning an O(n) table scan into an O(1) index probe (`find_node_by_pk`).
/// With no `input` it is a constant leaf lookup (≤1 row); with an `input` it is an
/// index nested loop, probing once per input row and preserving the input columns.
/// It populates the same `id_col` + property columns as the scan it replaces.
#[derive(Debug, Clone)]
pub struct IndexScan {
    /// Optional driver for correlated PK lookups (`var.pk = input_expr`).
    pub input: Option<Box<PlanOp>>,
    pub var: VarId,
    pub id_col: usize,
    /// The single candidate table (the rewrite only fires for a non-polymorphic
    /// scan, mirroring the C++ `tableIDs.size() == 1` guard).
    pub table: TableId,
    /// How `table`'s own columns map into the layout (copied from the scan's
    /// [`ScanTable`]).
    pub prop_cols: Vec<PropCol>,
    /// The primary-key value expression to look up. Constant leaf scans fold it
    /// once; correlated scans evaluate it against each input row.
    pub pk_value: BoundExpr,
}

/// One candidate relationship table of an [`Extend`]: how that rel table's own
/// property columns map into the shared (union) layout.
#[derive(Debug, Clone)]
pub struct RelBranch {
    pub rel_table: TableId,
    pub rel_prop_cols: Vec<PropCol>,
}

/// The neighbor side of an extend.
#[derive(Debug, Clone)]
pub enum ExtendTarget {
    /// Bind a new node variable: populate its id + property columns. The neighbor
    /// may belong to any of `to_tables` (the to-node's candidate node tables);
    /// its actual table is recovered from the neighbor's internal id at runtime.
    New {
        to_id_col: usize,
        to_tables: Vec<ScanTable>,
    },
    /// The neighbor is an already-bound variable; keep only rows where the
    /// neighbor's id equals this column.
    Existing { filter_col: usize },
}

/// Follow a relationship from an already-bound node to its neighbors. A
/// (possibly polymorphic) relationship enumerates one [`RelBranch`] per candidate
/// rel table; the extend follows each and unions the neighbors.
#[derive(Debug, Clone)]
pub struct Extend {
    pub input: Box<PlanOp>,
    pub from_id_col: usize,
    pub dir: ExtendDir,
    pub rel_id_col: usize,
    pub branches: Vec<RelBranch>,
    pub target: ExtendTarget,
    /// Input columns that must survive into this extend's output. Initially every
    /// preceding layout column; the projection-pruning pass narrows this to columns
    /// read by the final projection or some physical operator.
    pub carry_cols: Vec<usize>,
    /// Factorization (P3 step 6): when `true`, the optimizer has proven this
    /// extend's introduced columns (the rel, and the new node for a `New` target)
    /// are never read — only counted. The processor then **collapses** the fan-out
    /// into a per-row multiplicity (count the valid neighbors) instead of emitting
    /// one row per neighbor. Set only by [`crate::optimize`]; defaults `false`.
    pub factorize: bool,
}

/// Follow a *variable-length* relationship from an already-bound node: BFS/DFS
/// expansion over `rel_tables` in `dir`, enumerating paths whose length is in
/// `[lower, upper]` under the `mode`/`semantic`. Per output path it sets the
/// recursive-rel value column (when `build_value`) and binds the end node.
#[derive(Debug, Clone)]
pub struct VarLengthExtend {
    pub input: Box<PlanOp>,
    pub from_id_col: usize,
    pub dir: ExtendDir,
    pub lower: u32,
    pub upper: u32,
    pub mode: RecursiveMode,
    pub semantic: PathSemantic,
    /// Candidate relationship tables to traverse (all, or the named types).
    pub rel_tables: Vec<TableId>,
    /// Generic column holding the rel variable's `{_NODES, _RELS}` value.
    pub rel_value_col: usize,
    /// Whether to assemble that value (the rel is named or part of a path);
    /// skipped for an anonymous rel used only for connectivity (e.g. `COUNT(*)`).
    pub build_value: bool,
    /// The per-step `(r, n | WHERE …)` filter, if any.
    pub filter: Option<RecursiveFilter>,
    /// The edge-weight rel-property NAME for (ALL) WSHORTEST (resolved to a
    /// column id per rel table at exec).
    pub weight: Option<String>,
    /// Whether this recursive rel is a segment of a NAMED path (`MATCH p = …`)
    /// — selects the WEIGHTED_SP_PATHS vs _DESTINATIONS error wording.
    pub in_named_path: bool,
    pub target: ExtendTarget,
    /// Factorization (P3 step 6): when `true`, this recursive extend's introduced
    /// columns (the path value + new end node) are never read — only counted — so the
    /// processor collapses the per-path fan-out into a multiplicity (count the valid
    /// paths). Set only by [`crate::optimize`]; defaults `false`.
    pub factorize: bool,
}

/// One segment of a named path being assembled by [`PlanOp::ProjectPath`].
#[derive(Debug, Clone)]
pub struct PathSegmentPlan {
    pub rel: PathRel,
    /// The end node of this segment (assembled from its binding).
    pub to_node: VarId,
}

/// How a path segment's relationship contributes to the assembled value.
#[derive(Debug, Clone)]
pub enum PathRel {
    /// A variable-length segment: read its `{_NODES, _RELS}` value column.
    Recursive { value_col: usize },
    /// A single-hop segment: assemble the rel value from this variable.
    Single { rel: VarId },
}

/// Assemble a named path into its `RECURSIVE_REL` value column from the head node
/// and each `(rel, to-node)` segment (interleaving intermediate nodes/rels).
#[derive(Debug, Clone)]
pub struct ProjectPath {
    pub input: Box<PlanOp>,
    pub path_col: usize,
    pub head: VarId,
    pub segments: Vec<PathSegmentPlan>,
}

/// The semantics of a [`PlanOp::HashJoin`]. `Inner` is the step-7/8 equi-join the
/// filter-pushdown emits from a `Filter(equi) above CrossProduct`. `Left` and `Mark`
/// are the **decorrelated** forms (P3 step 10b L1): a correlated `OPTIONAL`/subquery
/// whose correlation reduces to node-id equality is unnested into a build-once join
/// over the probe (outer) side, replacing the per-row nested-loop `Optional`/`Subquery`.
#[derive(Debug, Clone)]
pub enum JoinKind {
    /// Emit `probe × each build match`. NULL probe key ⇒ no output for that row.
    Inner,
    /// Left outer join: emit `probe × each build match`, or one probe row with the
    /// build-side columns (`build_cols`) set to NULL when the probe has no match.
    /// This is the decorrelated `OPTIONAL MATCH` (its introduced vars are the build
    /// columns, NULL when unmatched).
    Left,
    /// Mark join: emit exactly one row per probe row, writing into `mark_col`
    /// whether the probe key matched (EXISTS ⇒ BOOL) or its match count (COUNT ⇒
    /// INT64). Build columns are not emitted. This is the decorrelated lifted
    /// `EXISTS {}`/`COUNT {}` subquery; a surrounding `Filter` realizes `NOT EXISTS`.
    Mark { mark_col: usize, kind: SubqueryKind },
}

/// Where an `UNWIND` element lands in the runtime row.
#[derive(Debug, Clone)]
pub enum UnwindTarget {
    /// A scalar/list element occupies one value column.
    Scalar { col: usize },
    /// A node element is re-exploded into a node binding, so a following `MATCH`
    /// can extend from it and property reads can use the normal layout columns.
    Node {
        id_col: usize,
        prop_tables: Vec<ScanTable>,
    },
}

/// A physical operator (data description; executed by `koko-processor`).
#[derive(Debug, Clone)]
pub enum PlanOp {
    /// Exactly one empty row (the source for match-free queries).
    SingleRow,
    /// Replay the previous part's projected rows (the carried input scope). The
    /// processor supplies the materialized chunks; the carried scalar values
    /// already occupy their layout columns.
    InputScan,
    /// A catalog table function used as a 0→N leaf source (the in-query `CALL`
    /// form): produce its rows from the catalog and land output column `i` in
    /// `cols[i]`. Downstream Filter/Project/Aggregate compose over it unchanged.
    ScanTableFunc {
        func: BoundTableFunc,
        arg: Option<String>,
        cols: Vec<usize>,
    },
    /// A CSV `LOAD FROM` 0→N leaf source: stream the file's rows, landing the value
    /// of declared column `i` in layout column `cols[i]`. `col_names` is the declared
    /// header names (for the header-row auto-detect heuristic).
    LoadScan {
        cols: Vec<usize>,
        col_names: Vec<String>,
        path: String,
        /// Full ordered file set (glob/list-expanded); `path` is its first.
        paths: Vec<String>,
        format: FileFormat,
        options: CsvLoadOptions,
        bare: bool,
    },
    ScanNode(ScanNode),
    /// A primary-key point lookup (an optimizer rewrite of `Filter(pk=expr)` over a
    /// single-table [`ScanNode`]); constant leaf or input-driven correlated lookup.
    IndexScan(IndexScan),
    Extend(Box<Extend>),
    VarLengthExtend(Box<VarLengthExtend>),
    ProjectPath(Box<ProjectPath>),
    CrossProduct {
        left: Box<PlanOp>,
        left_width: usize,
        right: Box<PlanOp>,
        right_width: usize,
    },
    /// A hash join (P3 step 7): the result-preserving rewrite of a
    /// `Filter(<equi-join conjuncts>)` sitting directly above a
    /// [`PlanOp::CrossProduct`], where each conjunct equates an expression over the
    /// probe (left) side to one over the build (right) side. The build side is
    /// materialized into a hash table keyed by its key expressions; the probe side
    /// streams and looks up its matches — turning the O(n·m) cross-product + filter
    /// into O(n + m). A NULL key never matches (Cypher `=` semantics), matching the
    /// filter it replaces. Produces the same columns as the cross product it replaces.
    /// Which input is hashed (the **build**) vs streamed (the **probe**) is a
    /// cost-based choice (P3 step 8: build the smaller side), so each side's columns
    /// are given explicitly rather than assumed left-then-right.
    HashJoin {
        /// Streamed side; drives output.
        probe: Box<PlanOp>,
        /// Hashed side.
        build: Box<PlanOp>,
        /// `(start, len)` of the probe side's columns in the layout.
        probe_cols: (usize, usize),
        /// `(start, len)` of the build side's columns in the layout.
        build_cols: (usize, usize),
        /// `(probe_expr, build_expr)` per equi-join condition; a row matches when
        /// every `probe_expr == build_expr` (by Cypher `=` semantics).
        keys: Vec<(BoundExpr, BoundExpr)>,
        /// Inner (the step-7/8 cross-product rewrite) vs the decorrelated
        /// outer/mark variants (P3 step 10b L1 — see [`JoinKind`]).
        kind: JoinKind,
    },
    Filter {
        input: Box<PlanOp>,
        predicate: BoundExpr,
    },
    /// `UNWIND list AS var`: per input row, evaluate `list` and emit one row per
    /// element into `target`. A NULL/non-list/empty list yields no rows.
    Unwind {
        input: Box<PlanOp>,
        list: BoundExpr,
        target: UnwindTarget,
    },
    /// `OPTIONAL MATCH` — a left join. For each row from `input`, run `pattern`
    /// seeded with that row; emit its matches, or one row with `new_cols` set to
    /// NULL if it produced none.
    Optional {
        input: Box<PlanOp>,
        pattern: Box<PlanOp>,
        new_cols: Vec<usize>,
    },
    /// `EXISTS {}` / `COUNT {}` — a correlated subquery. For each row from `input`,
    /// run `pattern` seeded with that row, count its matches, and write the result
    /// (`count > 0` for EXISTS, the count for COUNT) into `result_col`. Emits one
    /// row per input row.
    Subquery {
        input: Box<PlanOp>,
        pattern: Box<PlanOp>,
        result_col: usize,
        kind: SubqueryKind,
    },
    /// For each input row, advance/read the sequence `name` and write the value
    /// into `result_col`. Emits one row per input row.
    SequenceCall {
        input: Box<PlanOp>,
        func: SequenceFn,
        name: String,
        result_col: usize,
    },
    /// Fill each item's `value_col` with the materialized node/rel value of the
    /// id in `id_col` (audit V12 seam): expressions that consume a node/rel
    /// *value* mid-pipeline (function args, list elements) read the value
    /// column instead of the bare internal id.
    MaterializeValues {
        input: Box<PlanOp>,
        items: Vec<MaterializeItem>,
    },
}

/// One variable to materialize for [`PlanOp::MaterializeValues`].
#[derive(Debug, Clone)]
pub struct MaterializeItem {
    pub id_col: usize,
    pub value_col: usize,
    pub is_node: bool,
}

/// How one carried `WITH` output value is unpacked into this part's layout. The
/// processor reads the previous part's projected value and fills these columns.
#[derive(Debug, Clone)]
pub enum InputSlot {
    /// A carried scalar value lands in this single column.
    Scalar { col: usize },
    /// A carried node value is exploded back into a binding: its internal id into
    /// `id_col`, and its properties through the mapping for the node's runtime
    /// table. Polymorphic nodes must use the actual table's column ids — a
    /// representative table's property order is not valid for every value.
    Node {
        id_col: usize,
        prop_tables: Vec<ScanTable>,
    },
}

/// A planned updating clause (the executor applies these after the part's match).
/// `Create`/`Set`/`Delete` carry their bound form; `Merge` carries a planned match
/// sub-pattern alongside its create-on-miss + `ON CREATE`/`ON MATCH` sets.
#[derive(Debug, Clone)]
pub enum UpdateOp {
    Create(BoundCreate),
    Set(BoundSet),
    Delete(BoundDelete),
    // Boxed: a `MergePlan` (match sub-plan + create + sets + keys) dwarfs the other
    // variants.
    Merge(Box<MergePlan>),
}

/// A planned `MERGE`: a seeded match sub-plan (correlated to the bound vars +
/// inline filter) plus the create-on-miss instructions and the conditional sets.
#[derive(Debug, Clone)]
pub struct MergePlan {
    pub match_pattern: PlanOp,
    pub create: BoundCreate,
    pub on_create: BoundSet,
    pub on_match: BoundSet,
    /// Already-bound node variables in the pattern (e.g. the endpoints of
    /// `MATCH (a),(b) MERGE (a)-[e]->(b)`). Their internal ids are part of the
    /// MERGE key (so the same key over different endpoints stays distinct), matching
    /// Kùzu's `planMergeClause`.
    pub key_node_vars: Vec<VarId>,
    /// Kùzu's `suppressDuplicateCreatedOutput`: when set, two input rows with the
    /// same merge key collapse to one output row (and one created node). True only
    /// for a node-only MERGE with no `ON CREATE`/`ON MATCH SET` and no carried
    /// non-key payload column; otherwise every input row emits its own row.
    pub suppress_dup: bool,
}

/// The plan for one query part: the operator tree, the column layout it produces,
/// and the updating clauses to apply after it. `inputs` describes how each carried
/// `WITH` value (in carried order) is unpacked into the layout; empty for the first part.
#[derive(Debug, Clone)]
pub struct PartPlan {
    pub root: PlanOp,
    pub layout: RowLayout,
    pub inputs: Vec<InputSlot>,
    pub update_ops: Vec<UpdateOp>,
}

/// The result of planning a (possibly multi-part) query: one [`PartPlan`] each.
#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub parts: Vec<PartPlan>,
}

/// The plan for a `UNION`/`UNION ALL` query: one [`QueryPlan`] per operand, plus
/// whether to deduplicate the combined result (plain `UNION`).
#[derive(Debug, Clone)]
pub struct RegularPlan {
    pub operands: Vec<QueryPlan>,
    pub distinct: bool,
}

/// Plan a `UNION` query: one [`QueryPlan`] per operand. `stats` (the P3 cost model's
/// input) drives cost-based join order; pass an empty map for the pre-step-8 greedy
/// plan.
pub fn plan_regular(
    rq: &BoundRegularQuery,
    catalog: &Catalog,
    stats: &StatsMap,
) -> Result<RegularPlan> {
    let operands = rq
        .operands
        .iter()
        .map(|q| plan(q, catalog, stats))
        .collect::<Result<Vec<_>>>()?;
    Ok(RegularPlan {
        operands,
        distinct: rq.distinct,
    })
}

/// Plan a bound query: one [`PartPlan`] per part, in order.
pub fn plan(query: &BoundQuery, catalog: &Catalog, stats: &StatsMap) -> Result<QueryPlan> {
    let parts = query
        .parts
        .iter()
        .map(|part| plan_part(query, part, catalog, stats))
        .collect::<Result<Vec<_>>>()?;
    Ok(QueryPlan { parts })
}

/// Plan one query part's reading portion (match graph + unwinds + filters),
/// seeded by the part's carried input scope.
fn plan_part(
    query: &BoundQuery,
    part: &BoundPart,
    catalog: &Catalog,
    stats: &StatsMap,
) -> Result<PartPlan> {
    let mut b = PlanBuilder {
        query,
        catalog,
        stats,
        layout: RowLayout::default(),
        bound: HashSet::new(),
        recursive_value_rels: HashSet::new(),
        recursive_path_rels: HashSet::new(),
    };
    // Every table name, for evaluating `label()`/`labels()` at runtime.
    for id in catalog.node_table_ids() {
        b.layout
            .table_names
            .insert(id, catalog.node_table(id).unwrap().name.clone());
    }
    for id in catalog.rel_table_ids() {
        let name = catalog.rel_table(id).unwrap().name.clone();
        for (member, _, _) in catalog.rel_members(id) {
            b.layout.table_names.insert(member, name.clone());
        }
    }

    // Carried variables occupy the first layout columns; `InputScan` is the base
    // that replays the previous part's projected rows into them. A scalar takes one
    // column; a carried node takes a full binding (id + property columns) so it can
    // be re-extended from and have its properties read.
    let mut inputs = Vec::with_capacity(part.input_vars.len());
    for &v in &part.input_vars {
        let info = query.var(v);
        let slot = if info.is_scalar() {
            InputSlot::Scalar {
                col: b.layout.add_scalar(v, info.scalar_type()),
            }
        } else {
            // A carried node is unpacked through the mapping for its runtime table.
            let scan = b.make_scan(v);
            InputSlot::Node {
                id_col: scan.id_col,
                prop_tables: scan.tables,
            }
        };
        b.bound.insert(v);
        inputs.push(slot);
    }

    let has_input = !part.input_vars.is_empty();
    let match_vars = match_bound_vars(&part.match_);
    let mut pre_unwind =
        pre_match_unwind_vars(&part.unwind, part.where_predicate.as_ref(), &match_vars);
    for u in &part.unwind {
        if query.var(u.var).is_node() && part.match_.node_vars.contains(&u.var) {
            if expr_references_any_var(&u.list, &match_vars) {
                return Err(koko_common::Error::not_implemented(
                    "MATCH after UNWIND of a node value that depends on the same MATCH is not \
                     supported in this phase"
                        .to_string(),
                ));
            }
            pre_unwind.insert(u.var);
        }
    }

    // The part's base source. An in-query table-function scan is a leaf with no
    // MATCH/UNWIND (the binder guards the combination). A CSV `LOAD FROM` is a leaf
    // that may be followed by an UNWIND and/or MATCH. Otherwise the base is the
    // carried scope (`InputScan`) or one dummy row that feeds pre-MATCH UNWIND.
    let mut root = if let Some((first, rest)) = part.table_func_scans.split_first() {
        // Each scan's layout columns follow the function's schema order
        // (YIELD renames positionally, never reorders).
        let scan_op = |b: &mut PlanBuilder, tfs: &koko_binder::BoundTableFuncScan| {
            let cols = tfs
                .columns
                .iter()
                .map(|(var, ty)| b.layout.add_scalar(*var, ty.clone()))
                .collect();
            PlanOp::ScanTableFunc {
                func: tfs.func,
                arg: tfs.arg.clone(),
                cols,
            }
        };
        let mut root = scan_op(&mut b, first);
        // Additional CALLs cross-product onto the accumulated rows.
        for tfs in rest {
            let left_width = b.layout.width();
            let scan = scan_op(&mut b, tfs);
            let right_width = b.layout.width() - left_width;
            root = PlanOp::CrossProduct {
                left: Box::new(root),
                left_width,
                right: Box::new(scan),
                right_width,
            };
        }
        root
    } else if let Some(ls) = &part.load_scan {
        // The carried columns precede the file's; a chained `LOAD FROM` after a
        // `WITH` cross-products the file rows against the carried rows.
        let input_width = b.layout.width();
        let cols = ls
            .columns
            .iter()
            .map(|(var, ty)| b.layout.add_scalar(*var, ty.clone()))
            .collect();
        let load = PlanOp::LoadScan {
            cols,
            col_names: ls.col_names.clone(),
            path: ls.path.clone(),
            paths: ls.paths.clone(),
            format: ls.format,
            options: ls.options.clone(),
            bare: ls.bare,
        };
        if has_input {
            let load_width = b.layout.width() - input_width;
            PlanOp::CrossProduct {
                left: Box::new(PlanOp::InputScan),
                left_width: input_width,
                right: Box::new(load),
                right_width: load_width,
            }
        } else {
            load
        }
    } else if has_input {
        PlanOp::InputScan
    } else {
        PlanOp::SingleRow
    };

    // `WITH … WHERE` from the previous part filters the carried rows before any of
    // this part's own reading clauses. Subqueries it references compute first.
    let mut input_filter_planned: std::collections::HashSet<usize> =
        std::collections::HashSet::new();
    if let Some(pred) = &part.input_filter {
        for id in collect_subquery_ids(pred) {
            let sq = &part.subqueries[id];
            let ty = match sq.kind {
                SubqueryKind::Exists => LogicalType::Bool,
                SubqueryKind::Count => LogicalType::Int64,
            };
            let result_col = b.layout.alloc(ty);
            if b.layout.subquery_cols.len() <= id {
                b.layout.subquery_cols.resize(id + 1, usize::MAX);
            }
            b.layout.subquery_cols[id] = result_col;
            root = plan_subquery(&mut b, root, sq, result_col, &part.subqueries)?;
            input_filter_planned.insert(id);
        }
        root = PlanOp::Filter {
            input: Box::new(root),
            predicate: pred.clone(),
        };
    }

    // UNWIND clauses whose variables must be visible to MATCH drive the MATCH,
    // mirroring the C++ clause-order planner (`UNWIND … MATCH …`). This covers
    // scalar aliases read by the MATCH predicate and node aliases reused directly
    // as graph bindings; if the UNWIND expression itself needs a MATCH variable,
    // it necessarily remains post-MATCH.
    for u in part.unwind.iter().filter(|u| pre_unwind.contains(&u.var)) {
        root = b.make_unwind(root, u);
        if query.var(u.var).is_node() {
            b.bound.insert(u.var);
        }
    }

    // Compose any required MATCH onto the base rows. When there is no base/input and
    // no pre-MATCH UNWIND, let `build_match` anchor directly on a node scan rather
    // than cross-producting through the dummy row.
    if !part.table_func_scans.is_empty() {
        // A MATCH combined with CALL scans composes over the scan rows
        // (cross product driven through the match).
        if !(part.match_.node_vars.is_empty() && part.match_.rel_vars.is_empty()) {
            let base = std::mem::replace(&mut root, PlanOp::SingleRow);
            root = b.build_match(&part.match_, part.where_predicate.as_ref(), Some(base))?;
        }
    } else if part.table_func_scans.is_empty() {
        let bare_match = !has_input && part.load_scan.is_none() && pre_unwind.is_empty();
        let base = if bare_match {
            None
        } else {
            Some(std::mem::replace(&mut root, PlanOp::SingleRow))
        };
        root = b.build_match(&part.match_, part.where_predicate.as_ref(), base)?;
    }

    // Remaining UNWIND operators keep the old post-MATCH position (e.g. `MATCH …
    // UNWIND node.list AS x`), allocating either a scalar column or node binding.
    for u in part.unwind.iter().filter(|u| !pre_unwind.contains(&u.var)) {
        root = b.make_unwind(root, u);
        if query.var(u.var).is_node() {
            b.bound.insert(u.var);
        }
    }
    // Lifted `EXISTS {}` / `COUNT {}` subqueries: each computes a per-row result
    // column (before the WHERE / projection that read it). The inner pattern is a
    // sub-pattern (rooted at the per-row seed) correlated to the bound variables.
    // Materialize node/rel VALUES for variables whose value (not just id) is
    // consumed by this part's expressions (audit V12 seam) — before the WHERE /
    // subqueries / projection that read them.
    {
        let mut consumed: HashSet<VarId> = HashSet::new();
        let mut exprs: Vec<&BoundExpr> = Vec::new();
        if let Some(w) = &part.where_predicate {
            exprs.push(w);
        }
        if let Some(proj) = &part.projection {
            for it in &proj.items {
                if let koko_binder::ProjItem::Scalar { expr, .. } = it {
                    exprs.push(expr);
                }
            }
        }
        for sq in &part.subqueries {
            if let Some(w) = &sq.where_predicate {
                exprs.push(w);
            }
        }
        for e in exprs {
            collect_value_consumed_vars(e, false, &mut consumed);
        }
        let mut items = Vec::new();
        for var in consumed {
            let Some(idx) = b.layout.index.get(&var).copied() else {
                continue;
            };
            if b.layout.vars[idx].value_col.is_some() {
                continue;
            }
            let (ty, is_node) = match &b.layout.vars[idx].kind {
                VarColKind::Node { table, .. } => {
                    (LogicalType::Node(table.unwrap_or(TableId(u64::MAX))), true)
                }
                VarColKind::Rel { table, .. } => {
                    (LogicalType::Rel(table.unwrap_or(TableId(u64::MAX))), false)
                }
                _ => continue,
            };
            let id_col = b.layout.vars[idx].id_col;
            let value_col = b.layout.alloc(ty);
            b.layout.vars[idx].value_col = Some(value_col);
            items.push(MaterializeItem {
                id_col,
                value_col,
                is_node,
            });
        }
        if !items.is_empty() {
            root = PlanOp::MaterializeValues {
                input: Box::new(root),
                items,
            };
        }
    }

    // Ids consumed by an OPTIONAL's WHERE are planned inside that optional's
    // branch (below), not here — their column slots are shared via the id-indexed
    // map either way.
    if b.layout.subquery_cols.len() < part.subqueries.len() {
        b.layout
            .subquery_cols
            .resize(part.subqueries.len(), usize::MAX);
    }
    let optional_scoped: std::collections::HashSet<usize> = part
        .optionals
        .iter()
        .filter_map(|o| o.where_predicate.as_ref())
        .flat_map(collect_subquery_ids)
        .collect();
    // A subquery referenced by another's WHERE is NESTED: it plans inside its
    // parent's pattern (its variables live there), not at the top level.
    let nested_ids: std::collections::HashSet<usize> = part
        .subqueries
        .iter()
        .flat_map(|sq| {
            sq.where_predicate
                .as_ref()
                .map(collect_subquery_ids)
                .unwrap_or_default()
        })
        .collect();
    for (id, sq) in part.subqueries.iter().enumerate() {
        if optional_scoped.contains(&id)
            || nested_ids.contains(&id)
            || input_filter_planned.contains(&id)
        {
            continue;
        }
        let ty = match sq.kind {
            SubqueryKind::Exists => LogicalType::Bool,
            SubqueryKind::Count => LogicalType::Int64,
        };
        let result_col = b.layout.alloc(ty);
        b.layout.subquery_cols[id] = result_col;
        root = plan_subquery(&mut b, root, sq, result_col, &part.subqueries)?;
    }
    // Lifted `nextval`/`currval` calls: each fills a per-row INT64 column by
    // advancing the named sequence (the processor does this with catalog access,
    // since the pure expression evaluator can't reach mutable sequence state).
    // A call the WHERE itself reads computes before the filter; every other call
    // computes AFTER it (audit V11 — C++ advances a projection's nextval only
    // for rows that survive the WHERE, so filtered rows must not consume values).
    let where_seq_ids = part
        .where_predicate
        .as_ref()
        .map(collect_sequence_ids)
        .unwrap_or_default();
    let mut post_filter_calls = Vec::new();
    for (id, sc) in part.sequence_calls.iter().enumerate() {
        let result_col = b.layout.alloc(LogicalType::Int64);
        b.layout.sequence_cols.push(result_col);
        if where_seq_ids.contains(&id) {
            root = PlanOp::SequenceCall {
                input: Box::new(root),
                func: sc.func,
                name: sc.name.clone(),
                result_col,
            };
        } else {
            post_filter_calls.push((sc, result_col));
        }
    }

    if let Some(pred) = &part.where_predicate {
        root = PlanOp::Filter {
            input: Box::new(root),
            predicate: pred.clone(),
        };
    }
    for (sc, result_col) in post_filter_calls {
        root = PlanOp::SequenceCall {
            input: Box::new(root),
            func: sc.func,
            name: sc.name.clone(),
            result_col,
        };
    }

    // `OPTIONAL MATCH` left-joins, in order. Each is a sub-pattern (rooted at the
    // per-row seed) correlated to the already-bound variables; its WHERE filters
    // matches before the NULL decision.
    for opt in &part.optionals {
        let width_before = b.layout.width();
        // A WHERE that reads a lifted subquery must compute it INSIDE this
        // branch, before the branch filter and the NULL decision (audit W2 —
        // computing it in the outer pipeline read an unpopulated column).
        let mut sub_ids: Vec<usize> = opt
            .where_predicate
            .as_ref()
            .map(collect_subquery_ids)
            .unwrap_or_default()
            .into_iter()
            .collect();
        sub_ids.sort_unstable();
        // P3 step 10b L1: decorrelate a sufficiently large, single-key outer pipeline into a
        // build-once Left hash join. Selective or multi-key probes stay seeded: rebuilding their
        // small sub-pipeline avoids materializing a whole relation (or all correlated pairs).
        let correlated = (sub_ids.is_empty()
            && cost::plan_card(&root, b.stats) >= DECORRELATE_MIN_PROBE_ROWS)
            .then(|| b.correlated_nodes(&opt.match_, opt.where_predicate.as_ref()))
            .flatten()
            .filter(|correlated| correlated.len() == 1);
        if let Some(corr) = correlated {
            root = b.build_decorrelated_join(root, &corr, &opt.match_, JoinKind::Left)?;
        } else {
            if b.layout.subquery_cols.len() < part.subqueries.len() {
                b.layout
                    .subquery_cols
                    .resize(part.subqueries.len(), usize::MAX);
            }
            let mut pattern = b.build_match(
                &opt.match_,
                opt.where_predicate.as_ref(),
                Some(PlanOp::InputScan),
            )?;
            for &id in &sub_ids {
                let sq = &part.subqueries[id];
                let ty = match sq.kind {
                    SubqueryKind::Exists => LogicalType::Bool,
                    SubqueryKind::Count => LogicalType::Int64,
                };
                let result_col = b.layout.alloc(ty);
                b.layout.subquery_cols[id] = result_col;
                pattern = plan_subquery(&mut b, pattern, sq, result_col, &part.subqueries)?;
            }
            if let Some(pred) = &opt.where_predicate {
                pattern = PlanOp::Filter {
                    input: Box::new(pattern),
                    predicate: pred.clone(),
                };
            }
            // Columns first allocated by this optional are NULL-extended on no match.
            let new_cols: Vec<usize> = (width_before..b.layout.width()).collect();
            root = PlanOp::Optional {
                input: Box::new(root),
                pattern: Box::new(pattern),
                new_cols,
            };
        }
    }

    // Plan the updating clauses, in order, on the shared builder (so columns for
    // created/merged variables are allocated in the layout — a created/merged node
    // can be carried through `WITH` or projected by `RETURN` — and each `MERGE`'s
    // bound variables are visible to later clauses).
    let update_ops = part
        .updates
        .iter()
        .map(|update| b.plan_update(update))
        .collect::<Result<Vec<_>>>()?;

    Ok(PartPlan {
        root,
        layout: b.layout,
        inputs,
        update_ops,
    })
}

/// Plan one lifted subquery onto `root`: a cost-gated decorrelated Mark join when the
/// correlation reduces to node-id equality, else the per-row Subquery operator.
/// Shared by the outer pipeline and OPTIONAL-scoped planning (audit W2).
fn plan_subquery(
    b: &mut PlanBuilder,
    root: PlanOp,
    sq: &koko_binder::BoundSubquery,
    result_col: usize,
    all: &[koko_binder::BoundSubquery],
) -> Result<PlanOp> {
    let inner_ids: Vec<usize> = sq
        .where_predicate
        .as_ref()
        .map(|w| collect_subquery_ids(w).into_iter().collect())
        .unwrap_or_default();
    // The decorrelated fast path can't host a WHERE that reads nested
    // subquery columns.
    let correlated = (inner_ids.is_empty()
        && cost::plan_card(&root, b.stats) >= DECORRELATE_MIN_PROBE_ROWS)
        .then(|| b.correlated_nodes(&sq.match_, sq.where_predicate.as_ref()))
        .flatten();
    if let Some(corr) = correlated {
        let kind = JoinKind::Mark {
            mark_col: result_col,
            kind: sq.kind,
        };
        return b.build_decorrelated_join(root, &corr, &sq.match_, kind);
    }
    let mut pattern = b.build_match(
        &sq.match_,
        sq.where_predicate.as_ref(),
        Some(PlanOp::InputScan),
    )?;
    // NESTED subqueries referenced by this one's WHERE compute per inner row,
    // before the filter reads their columns.
    for id in inner_ids {
        let inner = &all[id];
        let ty = match inner.kind {
            SubqueryKind::Exists => LogicalType::Bool,
            SubqueryKind::Count => LogicalType::Int64,
        };
        let col = b.layout.alloc(ty);
        if b.layout.subquery_cols.len() <= id {
            b.layout.subquery_cols.resize(id + 1, usize::MAX);
        }
        b.layout.subquery_cols[id] = col;
        pattern = plan_subquery(b, pattern, inner, col, all)?;
    }
    if let Some(pred) = &sq.where_predicate {
        pattern = PlanOp::Filter {
            input: Box::new(pattern),
            predicate: pred.clone(),
        };
    }
    Ok(PlanOp::Subquery {
        input: Box::new(root),
        pattern: Box::new(pattern),
        result_col,
        kind: sq.kind,
    })
}

/// The lifted-subquery ids (`BoundExpr::Subquery { id }`) referenced by an
/// expression — used to scope their computation into the OPTIONAL branch whose
/// WHERE reads them (audit W2).
fn collect_subquery_ids(e: &BoundExpr) -> std::collections::HashSet<usize> {
    fn walk(e: &BoundExpr, out: &mut std::collections::HashSet<usize>) {
        match e {
            BoundExpr::Subquery { id, .. } => {
                out.insert(*id);
            }
            BoundExpr::Scalar { args, .. }
            | BoundExpr::Call { args, .. }
            | BoundExpr::List { elems: args, .. } => args.iter().for_each(|a| walk(a, out)),
            BoundExpr::Cast { expr, .. } => walk(expr, out),
            BoundExpr::ValueProperty { value, .. } => walk(value, out),
            BoundExpr::Struct { fields, .. } => fields.iter().for_each(|(_, v)| walk(v, out)),
            BoundExpr::ListLambda { list, body, .. } => {
                walk(list, out);
                walk(body, out);
            }
            BoundExpr::Aggregate { arg: Some(a), .. } => walk(a, out),
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                if let Some(o) = operand {
                    walk(o, out);
                }
                for (c, r) in branches {
                    walk(c, out);
                    walk(r, out);
                }
                if let Some(el) = else_ {
                    walk(el, out);
                }
            }
            _ => {}
        }
    }
    let mut out = std::collections::HashSet::new();
    walk(e, &mut out);
    out
}

/// Record variables whose node/rel *value* an expression consumes (audit V12):
/// a `NodeRef` inside a function call, list/struct literal, lambda, CASE, or
/// property-off-value read needs the materialized value, not the bare id.
fn collect_value_consumed_vars(e: &BoundExpr, in_consumer: bool, out: &mut HashSet<VarId>) {
    match e {
        BoundExpr::NodeRef { var, .. } => {
            if in_consumer {
                out.insert(*var);
            }
        }
        BoundExpr::Call { name, args, .. } => {
            // Accessors (and typeof) read identity/schema straight off the bare
            // internal id — materializing their args would assemble full values
            // per row (a 349x lsqb q6 regression when id(person1) marked
            // person1). Only genuinely value-consuming calls mark their args.
            let consumes = !matches!(
                name.as_str(),
                "id" | "offset" | "label" | "labels" | "typeof" | "keys"
            );
            args.iter()
                .for_each(|a| collect_value_consumed_vars(a, consumes, out));
        }
        BoundExpr::List { elems, .. } => elems
            .iter()
            .for_each(|a| collect_value_consumed_vars(a, true, out)),
        BoundExpr::Struct { fields, .. } => fields
            .iter()
            .for_each(|(_, v)| collect_value_consumed_vars(v, true, out)),
        BoundExpr::ValueProperty { value, .. } => collect_value_consumed_vars(value, true, out),
        BoundExpr::ListLambda { list, body, .. } => {
            collect_value_consumed_vars(list, true, out);
            collect_value_consumed_vars(body, true, out);
        }
        // Only `collect(x)` gathers the VALUE; count/min/max/sum work off the
        // bare id (marking count(post)'s arg materialized full node values per
        // pre-aggregation row — an unbounded ldbc IC6 regression).
        BoundExpr::Aggregate {
            op: koko_function::AggOp::Collect,
            arg: Some(a),
            ..
        } => collect_value_consumed_vars(a, true, out),
        BoundExpr::Aggregate { arg: Some(a), .. } => {
            collect_value_consumed_vars(a, in_consumer, out)
        }
        BoundExpr::Scalar { args, .. } => args
            .iter()
            .for_each(|a| collect_value_consumed_vars(a, in_consumer, out)),
        // CAST consumes the value (CAST(r AS STRING) renders the full struct).
        BoundExpr::Cast { expr, .. } => collect_value_consumed_vars(expr, true, out),
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            if let Some(o) = operand {
                collect_value_consumed_vars(o, in_consumer, out);
            }
            for (c, r) in branches {
                collect_value_consumed_vars(c, in_consumer, out);
                collect_value_consumed_vars(r, in_consumer, out);
            }
            if let Some(el) = else_ {
                collect_value_consumed_vars(el, in_consumer, out);
            }
        }
        _ => {}
    }
}

/// All graph variables introduced by a required MATCH block.
fn match_bound_vars(match_: &BoundMatch) -> HashSet<VarId> {
    let mut vars = HashSet::new();
    vars.extend(match_.node_vars.iter().copied());
    vars.extend(match_.rel_vars.iter().copied());
    vars.extend(match_.path_vars.iter().copied());
    vars
}

/// UNWIND variables that must be available before planning the required MATCH.
///
/// The binder currently flattens required MATCH and UNWIND clauses into one block.
/// The explosive `UNWIND i MATCH (p {id: i})` shape is still recoverable: the MATCH
/// predicates reference `i`, and `i`'s list expression does not depend on a graph
/// variable from the MATCH. Walk backwards so dependent UNWINDs (for example
/// `UNWIND xs AS x UNWIND f(x) AS y MATCH (n {id: y})`) move as a prefix while
/// preserving their original order when applied by the caller.
fn pre_match_unwind_vars(
    unwinds: &[BoundUnwind],
    where_pred: Option<&BoundExpr>,
    match_vars: &HashSet<VarId>,
) -> HashSet<VarId> {
    if match_vars.is_empty() {
        return HashSet::new();
    }
    let mut needed = HashSet::new();
    if let Some(pred) = where_pred {
        collect_expr_vars(pred, &mut needed);
    }
    let mut pre = HashSet::new();
    for u in unwinds.iter().rev() {
        if needed.contains(&u.var) && !expr_references_any_var(&u.list, match_vars) {
            pre.insert(u.var);
            collect_expr_vars(&u.list, &mut needed);
        }
    }
    pre
}

fn expr_references_any_var(e: &BoundExpr, vars: &HashSet<VarId>) -> bool {
    let mut refs = HashSet::new();
    collect_expr_vars(e, &mut refs);
    refs.iter().any(|v| vars.contains(v))
}

fn collect_expr_vars(e: &BoundExpr, out: &mut HashSet<VarId>) {
    match e {
        BoundExpr::Literal(_)
        | BoundExpr::Parameter { .. }
        | BoundExpr::Column { .. }
        | BoundExpr::LambdaVar { .. }
        | BoundExpr::Subquery { .. }
        | BoundExpr::SequenceCall { .. } => {}
        BoundExpr::Property { var, .. }
        | BoundExpr::NodeRef { var, .. }
        | BoundExpr::ScalarVar { var, .. } => {
            out.insert(*var);
        }
        BoundExpr::ValueProperty { value, .. } => collect_expr_vars(value, out),
        BoundExpr::Cast { expr, .. } => collect_expr_vars(expr, out),
        BoundExpr::Scalar { args, .. }
        | BoundExpr::Call { args, .. }
        | BoundExpr::Udf { args, .. }
        | BoundExpr::List { elems: args, .. } => {
            for a in args {
                collect_expr_vars(a, out);
            }
        }
        BoundExpr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                collect_expr_vars(a, out);
            }
        }
        BoundExpr::Struct { fields, .. } => {
            for (_, v) in fields {
                collect_expr_vars(v, out);
            }
        }
        BoundExpr::ListLambda { list, body, .. } => {
            collect_expr_vars(list, out);
            collect_expr_vars(body, out);
        }
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            if let Some(o) = operand {
                collect_expr_vars(o, out);
            }
            for (c, r) in branches {
                collect_expr_vars(c, out);
                collect_expr_vars(r, out);
            }
            if let Some(el) = else_ {
                collect_expr_vars(el, out);
            }
        }
    }
}

/// The layout columns allocated for a node variable (the shared return of
/// `alloc_node`).
struct NodeAlloc {
    tables: Vec<TableId>,
    id_col: usize,
    /// `(property name → layout column)` for every union property, in order.
    name_to_col: Vec<(String, usize)>,
}

/// The layout columns allocated for a relationship variable (the return of
/// `alloc_rel`).
struct RelAlloc {
    id_col: usize,
    /// `(property name → layout column)` for every union property, in order.
    name_to_col: Vec<(String, usize)>,
    props: Vec<LayoutProp>,
}

/// One step the `build_match` loop takes: extend to a new node, close a both-bound
/// edge (a residual existence filter), or cross-product in a fresh node scan.
enum RelMove {
    ExtendNew(VarId),
    Close(VarId),
    Cross(VarId),
}

struct PlanBuilder<'q> {
    query: &'q BoundQuery,
    catalog: &'q Catalog,
    stats: &'q StatsMap,
    layout: RowLayout,
    bound: HashSet<VarId>,
    /// Recursive-rel variables whose `{_NODES, _RELS}` value must be assembled
    /// (the rel is named, or it is part of a named path); recomputed per match.
    recursive_value_rels: HashSet<VarId>,
    /// Rels that are segments of a named path (`MATCH p = …`).
    recursive_path_rels: HashSet<VarId>,
}

impl PlanBuilder<'_> {
    /// Build the operator tree for one match graph (a required match, or an
    /// `OPTIONAL MATCH` sub-pattern). `base` selects the leaf: `Some(op)` uses `op`
    /// as the root and cross-products/extends the match's fresh scans onto it — used
    /// for the carried scope (`InputScan`), an optional's per-row seed, and a CSV
    /// `LOAD FROM` source; `None` scans the first node directly. Already-bound
    /// variables (in `self.bound`) are reused as correlation points; only new ones
    /// get scanned/extended.
    fn build_match(
        &mut self,
        match_: &BoundMatch,
        where_pred: Option<&BoundExpr>,
        base: Option<PlanOp>,
    ) -> Result<PlanOp> {
        // Recursive rels whose value is needed (named, or part of a named path).
        self.recursive_value_rels.clear();
        self.recursive_path_rels.clear();
        for &p in &match_.path_vars {
            if let VarKind::Path { segments, .. } = &self.query.var(p).kind {
                for &(rel, _) in segments {
                    self.recursive_value_rels.insert(rel);
                    self.recursive_path_rels.insert(rel);
                }
            }
        }
        for &r in &match_.rel_vars {
            if !self.query.var(r).anonymous {
                self.recursive_value_rels.insert(r);
            }
        }

        // Distinct, not-yet-bound node variables in declaration order.
        let mut node_vars: Vec<VarId> = Vec::new();
        for &v in &match_.node_vars {
            if !node_vars.contains(&v) {
                node_vars.push(v);
            }
        }

        // Cost-based anchor selection is result-neutral only when the match has no
        // recursive rel (see [`cheapest_unbound`]); otherwise keep declaration order.
        let cost_safe = !match_
            .rel_vars
            .iter()
            .any(|&r| self.query.var(r).is_recursive());

        // With an input scope, `InputScan` is the base and fresh node scans
        // cross-product onto it. Without one, the first node is the root (or a
        // single empty row when there are no nodes).
        let mut root = match base {
            Some(op) => op,
            None if node_vars.iter().all(|v| self.bound.contains(v)) => {
                return Ok(PlanOp::SingleRow);
            }
            None => {
                // Cost-based anchor: the node whose greedy traversal has the lowest
                // peak cardinality (P3 step 10b L2b — handles hub patterns where the
                // smallest table is the wrong anchor). A selective `var.pk = const`
                // still lands near 1 (its traversal peaks low) so it anchors here and
                // step-5 filter-pushdown turns it into an IndexScan. Stable on ties, so
                // with no stats this is the first node in declaration order.
                let anchor = self
                    .best_anchor(&node_vars, &match_.rel_vars, where_pred, cost_safe)
                    .expect("a not-all-bound match has an unbound node");
                self.bound.insert(anchor);
                PlanOp::ScanNode(self.make_scan(anchor))
            }
        };

        let mut rels_remaining: Vec<VarId> = match_.rel_vars.clone();
        // Cost-based **extend order** (P3 step 10b L2) is result-neutral only when the
        // anchor is (no recursive rel) *and* there is no named path to assemble (whose
        // segments are order-sensitive); otherwise keep the declaration-order greedy.
        let reorder_safe = cost_safe && match_.path_vars.is_empty();

        loop {
            // Pick the next relationship move. Cost-based order closes both-bound edges
            // first (they only filter), then takes the lowest-fan-out extend to a new
            // node, then cross-products — so a cyclic / multi-path pattern follows its
            // selective edges instead of materializing the full node cross-product
            // before the cycle closes (lsqb q2/q3). The declaration-order path (used
            // for recursive / named-path matches) keeps the pre-L2 behavior exactly:
            // extend-to-new first, then close, then cross-product.
            let mv = if reorder_safe {
                let both_bound = |r: VarId| {
                    let (src, dst) = self.rel_endpoints(r);
                    self.bound.contains(&src) && self.bound.contains(&dst)
                };
                if let Some(pos) = rels_remaining.iter().position(|&r| both_bound(r)) {
                    RelMove::Close(rels_remaining.remove(pos))
                } else if let Some(idx) = rels_remaining
                    .iter()
                    .enumerate()
                    .filter(|t| self.rel_extendable_to_new(*t.1))
                    .min_by(|a, b| {
                        self.extend_fanout_of(*a.1)
                            .partial_cmp(&self.extend_fanout_of(*b.1))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
                {
                    RelMove::ExtendNew(rels_remaining.remove(idx))
                } else if let Some(nv) = self.cheapest_unbound(&node_vars, where_pred, cost_safe) {
                    RelMove::Cross(nv)
                } else {
                    break;
                }
            } else if let Some(pos) = rels_remaining
                .iter()
                .position(|&r| self.rel_extendable_to_new(r))
            {
                RelMove::ExtendNew(rels_remaining.remove(pos))
            } else if let Some(pos) = rels_remaining.iter().position(|&r| {
                let (src, dst) = self.rel_endpoints(r);
                self.bound.contains(&src) && self.bound.contains(&dst)
            }) {
                RelMove::Close(rels_remaining.remove(pos))
            } else if let Some(nv) = self.cheapest_unbound(&node_vars, where_pred, cost_safe) {
                RelMove::Cross(nv)
            } else {
                break;
            };

            match mv {
                RelMove::ExtendNew(rel) => {
                    root = if self.query.var(rel).is_recursive() {
                        self.make_var_extend_new(root, rel)
                    } else {
                        self.make_extend_new(root, rel)
                    };
                }
                RelMove::Close(rel) => {
                    root = if self.query.var(rel).is_recursive() {
                        self.make_var_extend_existing(root, rel)
                    } else {
                        self.make_extend_existing(root, rel)
                    };
                }
                RelMove::Cross(nv) => {
                    let left_width = self.layout.width();
                    let scan = PlanOp::ScanNode(self.make_scan(nv));
                    let right_width = self.layout.width() - left_width;
                    self.bound.insert(nv);
                    root = PlanOp::CrossProduct {
                        left: Box::new(root),
                        left_width,
                        right: Box::new(scan),
                        right_width,
                    };
                }
            }
        }

        // Assemble each named path's value, now that all its segment columns exist.
        for &p in &match_.path_vars {
            root = self.make_project_path(root, p);
        }
        Ok(root)
    }

    /// The correlated (already-bound) node variables of a sub-pattern, if it is
    /// cleanly **decorrelatable** into a build-once hash join (P3 step 10b L1).
    ///
    /// Decorrelation is sound only when the correlation reduces to **node-id
    /// equality**: the sub-match shares ≥1 already-bound node with the outer scope,
    /// introduces no named path, and carries no WHERE predicate. The no-predicate
    /// gate is the key safety condition — a predicate could reference a correlated
    /// variable, which [`build_decorrelated_join`] restores to its *outer* column
    /// after planning the build side, so any such reference would mis-resolve.
    /// `None` ⇒ fall back to the per-row nested loop (`Optional`/`Subquery`).
    fn correlated_nodes(
        &self,
        match_: &BoundMatch,
        where_pred: Option<&BoundExpr>,
    ) -> Option<Vec<VarId>> {
        if where_pred.is_some() || !match_.path_vars.is_empty() {
            return None;
        }
        let corr: Vec<VarId> = match_
            .node_vars
            .iter()
            .copied()
            .filter(|v| self.bound.contains(v))
            .collect();
        (!corr.is_empty()).then_some(corr)
    }

    /// Unnest a correlated sub-pattern into a build-once [`PlanOp::HashJoin`] (P3
    /// step 10b L1), replacing the per-row nested-loop `Optional`/`Subquery`. The
    /// `corr` nodes (from [`correlated_nodes`]) are temporarily un-scoped so
    /// `build_match` re-scans them into fresh columns (the build side = the relation
    /// the sub-pattern enumerates, built once); their fresh id columns are the build
    /// keys and the outer columns the probe keys; then they are restored to their
    /// outer columns so the surrounding plan resolves them unchanged. `probe` (the
    /// outer scope) occupies all columns allocated so far; the build side appends.
    fn build_decorrelated_join(
        &mut self,
        probe: PlanOp,
        corr: &[VarId],
        match_: &BoundMatch,
        kind: JoinKind,
    ) -> Result<PlanOp> {
        // Capture each correlated node's outer slot + id column, then un-scope it.
        let saved: Vec<(VarId, usize, usize)> = corr
            .iter()
            .map(|&c| {
                let slot = self.layout.var_slot(c).expect("correlated node is bound");
                (c, slot, self.layout.var(c).id_col)
            })
            .collect();
        for &c in corr {
            self.bound.remove(&c);
        }
        // Build the sub-pattern standalone (fresh scans for the now-unbound nodes).
        let build_start = self.layout.width();
        let build = self.build_match(match_, None, None)?;
        let build_len = self.layout.width() - build_start;
        // The fresh re-scanned id column of each correlated node = the build key;
        // the captured outer column = the probe key.
        let keys: Vec<(BoundExpr, BoundExpr)> = saved
            .iter()
            .map(|&(c, _, outer_col)| {
                let fresh_col = self.layout.var(c).id_col;
                (
                    BoundExpr::Column {
                        col: outer_col,
                        ty: LogicalType::InternalId,
                    },
                    BoundExpr::Column {
                        col: fresh_col,
                        ty: LogicalType::InternalId,
                    },
                )
            })
            .collect();
        // Restore the correlated nodes to their outer columns + bound state.
        for &(c, slot, _) in &saved {
            self.layout.restore_var_slot(c, slot);
            self.bound.insert(c);
        }
        Ok(PlanOp::HashJoin {
            probe: Box::new(probe),
            build: Box::new(build),
            probe_cols: (0, build_start),
            build_cols: (build_start, build_len),
            keys,
            kind,
        })
    }

    /// Plan one updating clause, allocating any created/merged variable columns on
    /// the shared layout (and, for `MERGE`, building its seeded match sub-plan).
    fn plan_update(&mut self, update: &BoundUpdate) -> Result<UpdateOp> {
        match update {
            BoundUpdate::Create(create) => {
                for node in &create.nodes {
                    if self.layout.try_var(node.var).is_none() {
                        self.alloc_node(node.var);
                    }
                }
                // A named created rel is projectable too (`CREATE (a)-[e:R]->(b)
                // RETURN e`), so allocate AND register its columns (alloc_rel,
                // unlike alloc_node, doesn't self-register); the processor fills the
                // id column with the new rel's id.
                for rel in &create.rels {
                    if let Some(var) = rel.var {
                        if self.layout.try_var(var).is_none() {
                            let alloc = self.alloc_rel(var);
                            self.register_rel(var, alloc.id_col, alloc.props);
                        }
                    }
                }
                Ok(UpdateOp::Create(create.clone()))
            }
            BoundUpdate::Set(s) => Ok(UpdateOp::Set(s.clone())),
            BoundUpdate::Delete(d) => Ok(UpdateOp::Delete(d.clone())),
            BoundUpdate::Merge(m) => {
                // Nodes already bound before this MERGE (its endpoints, from a prior
                // MATCH/part) — their ids are part of the merge key. Capture before
                // `build_match` binds the merge's own new variables.
                let mut key_node_vars: Vec<VarId> = Vec::new();
                for &v in &m.match_.node_vars {
                    if self.bound.contains(&v) && !key_node_vars.contains(&v) {
                        key_node_vars.push(v);
                    }
                }
                // Kùzu `suppressDuplicateCreatedOutput` (logical_merge.cpp): a
                // node-only MERGE with no ON CREATE/ON MATCH SET and no carried
                // non-key payload dedups its output by merge key (two same-key input
                // rows → one row + one created node). The layout here holds the
                // carried context (pre-`build_match`, so it excludes the merge's own
                // new vars); every carried var must be a merge endpoint or referenced
                // by an inline create property (i.e. participate in the key) to dedup
                // — else a column like `x` in `UNWIND [5,5] AS x MERGE (p:P {id:5})`
                // is payload and every input row must emit. (Uses the layout, not
                // `self.bound`, which omits scalar UNWIND vars.)
                let suppress_dup = m.on_create.items.is_empty()
                    && m.on_match.items.is_empty()
                    && m.create.rels.is_empty()
                    && {
                        let mut key_vars: HashSet<VarId> = key_node_vars.iter().copied().collect();
                        for node in &m.create.nodes {
                            key_vars.insert(node.var);
                            for (_, e) in &node.props {
                                collect_expr_vars(e, &mut key_vars);
                            }
                        }
                        self.layout.var_ids().all(|v| key_vars.contains(&v))
                    };
                // The seeded match attempt: scan/extend the merge's variables from
                // the already-bound ones (allocating their columns), then filter by
                // the inline properties. The create reuses those same columns.
                let mut pattern =
                    self.build_match(&m.match_, m.filter.as_ref(), Some(PlanOp::InputScan))?;
                if let Some(filter) = &m.filter {
                    pattern = PlanOp::Filter {
                        input: Box::new(pattern),
                        predicate: filter.clone(),
                    };
                }
                Ok(UpdateOp::Merge(Box::new(MergePlan {
                    match_pattern: pattern,
                    create: m.create.clone(),
                    on_create: m.on_create.clone(),
                    on_match: m.on_match.clone(),
                    key_node_vars,
                    suppress_dup,
                })))
            }
        }
    }

    fn rel_endpoints(&self, rel: VarId) -> (VarId, VarId) {
        match &self.query.var(rel).kind {
            VarKind::Rel { src, dst, .. } => (*src, *dst),
            VarKind::Node { .. } | VarKind::Path { .. } | VarKind::Scalar { .. } => {
                unreachable!("rel var is not a relationship")
            }
        }
    }

    /// Fan-out estimate of extending `rel` from its currently-bound endpoint to its
    /// unbound one (drives cost-based extend order; P3 step 10b L2).
    fn extend_fanout_of(&self, rel: VarId) -> f64 {
        let (src, dst) = self.rel_endpoints(rel);
        let from = if self.bound.contains(&src) { src } else { dst };
        cost::extend_fanout(self.query, rel, from, self.stats)
    }

    fn rel_extendable_to_new(&self, rel: VarId) -> bool {
        let (src, dst) = self.rel_endpoints(rel);
        (self.bound.contains(&src) && !self.bound.contains(&dst))
            || (self.bound.contains(&dst) && !self.bound.contains(&src))
    }

    /// The not-yet-bound node variable to scan next. With `cost_safe`, the lowest-
    /// estimated-cardinality one (the cost-based join anchor; see [`cost::node_card`];
    /// stable, so ties / no-stats fall back to declaration order = the pre-step-8
    /// greedy planner). Without `cost_safe`, always declaration order — reordering is
    /// only result-neutral when no recursive rel is present (flipping a var-length
    /// rel's direction reverses the node/rel order in its assembled `RECURSIVE_REL`).
    fn cheapest_unbound(
        &self,
        node_vars: &[VarId],
        where_pred: Option<&BoundExpr>,
        cost_safe: bool,
    ) -> Option<VarId> {
        if !cost_safe {
            return node_vars.iter().copied().find(|v| !self.bound.contains(v));
        }
        node_vars
            .iter()
            .copied()
            .filter(|v| !self.bound.contains(v))
            .min_by(|&a, &b| {
                let ca = cost::node_card(self.query, a, where_pred, self.stats);
                let cb = cost::node_card(self.query, b, where_pred, self.stats);
                ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// The node to anchor the join on: the one whose cost-based greedy traversal has
    /// the lowest **peak** intermediate cardinality (P3 step 10b L2b). The plain
    /// "smallest table" anchor ([`cheapest_unbound`]) is wrong for a hub pattern —
    /// lsqb q2 anchors on the small `Person` (1.7K) but then explodes to ~2.3M,
    /// whereas anchoring on the larger `Comment` (215K) hub, whose edges are all
    /// fan-out-1, keeps the intermediate at ~215K. So we *simulate* the traversal
    /// from each candidate and pick the min-peak one. Stable on ties / no stats
    /// (every peak is the same constant) ⇒ declaration order = the pre-L2b anchor.
    /// Only used when `cost_safe`; otherwise the position-based anchor is kept.
    fn best_anchor(
        &self,
        node_vars: &[VarId],
        rel_vars: &[VarId],
        where_pred: Option<&BoundExpr>,
        cost_safe: bool,
    ) -> Option<VarId> {
        if !cost_safe {
            return node_vars.iter().copied().find(|v| !self.bound.contains(v));
        }
        node_vars
            .iter()
            .copied()
            .filter(|v| !self.bound.contains(v))
            .min_by(|&a, &b| {
                let pa = self.anchor_peak(a, node_vars, rel_vars, where_pred);
                let pb = self.anchor_peak(b, node_vars, rel_vars, where_pred);
                pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Estimated **peak** intermediate cardinality of the cost-based greedy traversal
    /// starting from `anchor` — a read-only what-if used by [`best_anchor`]. It mirrors
    /// the `build_match` loop's move order (close a both-bound edge, else the
    /// lowest-fan-out extend, else cross-product the cheapest node) over estimated
    /// cardinalities only (no layout mutation).
    fn anchor_peak(
        &self,
        anchor: VarId,
        node_vars: &[VarId],
        rel_vars: &[VarId],
        where_pred: Option<&BoundExpr>,
    ) -> f64 {
        let mut bound = self.bound.clone();
        bound.insert(anchor);
        let mut card = cost::node_card(self.query, anchor, where_pred, self.stats);
        let mut peak = card;
        let mut rels: Vec<VarId> = rel_vars.to_vec();
        loop {
            // 1. close a both-bound edge (a residual filter — only shrinks).
            if let Some(pos) = rels.iter().position(|&r| {
                let (s, d) = self.rel_endpoints(r);
                bound.contains(&s) && bound.contains(&d)
            }) {
                rels.remove(pos);
                card = (card * cost::CLOSE_SEL).max(1.0);
                continue;
            }
            // 2. lowest-fan-out extend to a new node.
            let best = rels
                .iter()
                .enumerate()
                .filter_map(|(i, &r)| {
                    let (s, d) = self.rel_endpoints(r);
                    let from = if bound.contains(&s) && !bound.contains(&d) {
                        Some(s)
                    } else if bound.contains(&d) && !bound.contains(&s) {
                        Some(d)
                    } else {
                        None
                    }?;
                    Some((i, r, cost::extend_fanout(self.query, r, from, self.stats)))
                })
                .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((idx, rel, fanout)) = best {
                rels.remove(idx);
                let (s, d) = self.rel_endpoints(rel);
                bound.insert(if bound.contains(&s) { d } else { s });
                card = (card * fanout).max(1.0);
                peak = peak.max(card);
                continue;
            }
            // 3. cross-product the cheapest disconnected node.
            let next = node_vars
                .iter()
                .copied()
                .filter(|v| !bound.contains(v))
                .min_by(|&a, &b| {
                    let ca = cost::node_card(self.query, a, where_pred, self.stats);
                    let cb = cost::node_card(self.query, b, where_pred, self.stats);
                    ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
                });
            if let Some(nv) = next {
                bound.insert(nv);
                card = (card * cost::node_card(self.query, nv, where_pred, self.stats)).max(1.0);
                peak = peak.max(card);
                continue;
            }
            break;
        }
        peak
    }

    fn make_scan(&mut self, var: VarId) -> ScanNode {
        let alloc = self.alloc_node(var);
        let tables = alloc
            .tables
            .iter()
            .map(|&t| ScanTable {
                table: t,
                prop_cols: self.table_prop_cols(t, &alloc.name_to_col),
            })
            .collect();
        ScanNode {
            var,
            id_col: alloc.id_col,
            tables,
        }
    }

    fn make_unwind(&mut self, input: PlanOp, unwind: &BoundUnwind) -> PlanOp {
        let target = if self.query.var(unwind.var).is_node() {
            let scan = self.make_scan(unwind.var);
            UnwindTarget::Node {
                id_col: scan.id_col,
                prop_tables: scan.tables,
            }
        } else {
            let ty = self.query.var(unwind.var).scalar_type();
            UnwindTarget::Scalar {
                col: self.layout.add_scalar(unwind.var, ty),
            }
        };
        PlanOp::Unwind {
            input: Box::new(input),
            list: unwind.list.clone(),
            target,
        }
    }

    /// Allocate a node variable's layout columns (internal id + one column per
    /// union property) and register its [`VarColumns`] binding. Returns the
    /// candidate tables, the id column, and the property-name→column map — enough
    /// for `make_scan` to build the per-table scan maps and for the carried-node
    /// input path to unpack a node value.
    fn alloc_node(&mut self, var: VarId) -> NodeAlloc {
        let info = self.query.var(var);
        let tables = info.node_tables().to_vec();
        let label = info.label().to_string();
        let id_col = self.layout.alloc(LogicalType::InternalId);
        let mut props = Vec::new();
        let mut name_to_col = Vec::with_capacity(info.properties.len());
        for p in &info.properties {
            let col_index = self.layout.alloc(p.ty.clone());
            name_to_col.push((p.name.clone(), col_index));
            props.push(LayoutProp {
                name: p.name.clone(),
                col_index,
                ty: p.ty.clone(),
            });
        }
        self.layout.add_var(VarColumns {
            var,
            kind: VarColKind::Node {
                table: tables.first().copied(),
                label,
            },
            id_col,
            props,
            value_col: None,
        });
        NodeAlloc {
            tables,
            id_col,
            name_to_col,
        }
    }

    /// Map a node table's own columns to their layout columns (by property name)
    /// for one candidate table of a (possibly polymorphic) scan.
    fn table_prop_cols(&self, table: TableId, name_to_col: &[(String, usize)]) -> Vec<PropCol> {
        let entry = self.catalog.node_table(table).expect("scan table exists");
        entry
            .columns
            .iter()
            .filter_map(|c| {
                name_to_col
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(&c.name))
                    .map(|&(_, col_index)| PropCol {
                        column_id: c.column_id.0,
                        col_index,
                    })
            })
            .collect()
    }

    /// Allocate a relationship variable's layout columns (internal id + one column
    /// per union property). Returns the id column, the property-name→column map
    /// (for the per-rel-table branch maps), and the [`LayoutProp`]s (for
    /// `register_rel`).
    fn alloc_rel(&mut self, rel: VarId) -> RelAlloc {
        let info = self.query.var(rel);
        let id_col = self.layout.alloc(LogicalType::InternalId);
        let mut props = Vec::new();
        let mut name_to_col = Vec::with_capacity(info.properties.len());
        for p in &info.properties {
            let col_index = self.layout.alloc(p.ty.clone());
            name_to_col.push((p.name.clone(), col_index));
            props.push(LayoutProp {
                name: p.name.clone(),
                col_index,
                ty: p.ty.clone(),
            });
        }
        RelAlloc {
            id_col,
            name_to_col,
            props,
        }
    }

    /// Map a relationship table's own columns to their layout columns (by name)
    /// for one branch of a (possibly polymorphic) extend.
    fn rel_table_prop_cols(
        &self,
        rel_table: TableId,
        name_to_col: &[(String, usize)],
    ) -> Vec<PropCol> {
        let entry = self.catalog.rel_table(rel_table).expect("rel table exists");
        entry
            .columns
            .iter()
            .filter_map(|c| {
                name_to_col
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(&c.name))
                    .map(|&(_, col_index)| PropCol {
                        column_id: c.column_id.0,
                        col_index,
                    })
            })
            .collect()
    }

    /// Expand a rel variable's candidate group ids to their per-pair member (storage)
    /// table ids — the physical tables a rel pattern probes. A single-pair rel group is
    /// its own sole member (unchanged); a multi-pair group fans out to one member per
    /// FROM-TO pair (a rel's runtime `_ID` carries its pair's member id).
    fn rel_member_tables(&self, rel: VarId) -> Vec<TableId> {
        self.query
            .var(rel)
            .rel_tables()
            .iter()
            .flat_map(|&g| {
                self.catalog
                    .rel_members(g)
                    .into_iter()
                    .map(|(member, _, _)| member)
            })
            .collect()
    }

    /// One [`RelBranch`] per candidate per-pair member table of `rel`.
    fn rel_branches(&self, rel: VarId, name_to_col: &[(String, usize)]) -> Vec<RelBranch> {
        self.rel_member_tables(rel)
            .into_iter()
            .map(|t| RelBranch {
                rel_table: t,
                rel_prop_cols: self.rel_table_prop_cols(t, name_to_col),
            })
            .collect()
    }

    fn register_rel(&mut self, rel: VarId, rel_id_col: usize, props: Vec<LayoutProp>) {
        let (table, label, src, dst) = match &self.query.var(rel).kind {
            VarKind::Rel {
                tables,
                label,
                src,
                dst,
                ..
            } => (tables.first().copied(), label.clone(), *src, *dst),
            VarKind::Node { .. } | VarKind::Path { .. } | VarKind::Scalar { .. } => unreachable!(),
        };
        let src_id_col = self.layout.var(src).id_col;
        let dst_id_col = self.layout.var(dst).id_col;
        self.layout.add_var(VarColumns {
            var: rel,
            kind: VarColKind::Rel {
                table,
                label,
                src_id_col,
                dst_id_col,
            },
            id_col: rel_id_col,
            props,
            value_col: None,
        });
    }

    fn make_extend_new(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let (from, to, dir) = if self.bound.contains(&src) {
            (
                src,
                dst,
                if directed {
                    ExtendDir::Forward
                } else {
                    ExtendDir::Both
                },
            )
        } else {
            (
                dst,
                src,
                if directed {
                    ExtendDir::Backward
                } else {
                    ExtendDir::Both
                },
            )
        };
        let from_id_col = self.layout.var(from).id_col;

        // Allocate relationship columns, then the new node's columns.
        let rel_alloc = self.alloc_rel(rel);
        let rel_id_col = rel_alloc.id_col;
        let scan = self.make_scan(to); // registers `to` and allocates its columns
        self.bound.insert(to);
        self.register_rel(rel, rel_id_col, rel_alloc.props);
        self.bound.insert(rel);
        let branches = self.rel_branches(rel, &rel_alloc.name_to_col);

        PlanOp::Extend(Box::new(Extend {
            input: Box::new(input),
            from_id_col,
            dir,
            rel_id_col,
            branches,
            target: ExtendTarget::New {
                to_id_col: scan.id_col,
                to_tables: scan.tables,
            },
            carry_cols: (0..rel_id_col).collect(),
            factorize: false,
        }))
    }

    fn make_extend_existing(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let from_id_col = self.layout.var(src).id_col;
        let filter_col = self.layout.var(dst).id_col;
        let dir = if directed {
            ExtendDir::Forward
        } else {
            ExtendDir::Both
        };

        let rel_alloc = self.alloc_rel(rel);
        let rel_id_col = rel_alloc.id_col;
        self.register_rel(rel, rel_id_col, rel_alloc.props);
        self.bound.insert(rel);
        let branches = self.rel_branches(rel, &rel_alloc.name_to_col);

        PlanOp::Extend(Box::new(Extend {
            input: Box::new(input),
            from_id_col,
            dir,
            rel_id_col,
            branches,
            target: ExtendTarget::Existing { filter_col },
            carry_cols: (0..rel_id_col).collect(),
            factorize: false,
        }))
    }

    /// The `(lower, upper, mode, semantic)` of a recursive rel variable.
    #[allow(clippy::type_complexity)]
    fn recursive_spec(
        &self,
        rel: VarId,
    ) -> (u32, u32, RecursiveMode, PathSemantic, Option<String>) {
        match &self.query.var(rel).kind {
            VarKind::Rel {
                recursive: Some(s), ..
            } => (s.lower, s.upper, s.mode, s.semantic, s.weight.clone()),
            _ => unreachable!("not a recursive rel"),
        }
    }

    /// The per-step filter of a recursive rel variable, if any.
    fn recursive_filter(&self, rel: VarId) -> Option<RecursiveFilter> {
        match &self.query.var(rel).kind {
            VarKind::Rel {
                recursive: Some(s), ..
            } => s.filter.clone(),
            _ => None,
        }
    }

    /// Allocate the single generic column holding a recursive rel's
    /// `{_NODES, _RELS}` value, registering the variable to resolve to it.
    fn alloc_recursive_value(&mut self, rel: VarId) -> usize {
        let col = self.layout.alloc(LogicalType::RecursiveRel);
        self.layout.add_var(VarColumns {
            var: rel,
            kind: VarColKind::Scalar,
            id_col: col,
            props: Vec::new(),
            value_col: None,
        });
        col
    }

    fn make_var_extend_new(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let (from, to, dir) = if self.bound.contains(&src) {
            (src, dst, fwd_or_both(directed))
        } else {
            (dst, src, bwd_or_both(directed))
        };
        let from_id_col = self.layout.var(from).id_col;
        let (lower, upper, mode, semantic, weight) = self.recursive_spec(rel);
        let build_value = self.recursive_value_rels.contains(&rel);
        let rel_tables = self.rel_member_tables(rel);

        // The rel value column precedes the new node's columns (the input boundary).
        let rel_value_col = self.alloc_recursive_value(rel);
        let scan = self.make_scan(to);
        self.bound.insert(to);
        self.bound.insert(rel);

        PlanOp::VarLengthExtend(Box::new(VarLengthExtend {
            input: Box::new(input),
            from_id_col,
            dir,
            lower,
            upper,
            mode,
            semantic,
            rel_tables,
            rel_value_col,
            build_value,
            filter: self.recursive_filter(rel),
            weight,
            in_named_path: self.recursive_path_rels.contains(&rel),
            target: ExtendTarget::New {
                to_id_col: scan.id_col,
                to_tables: scan.tables,
            },
            factorize: false,
        }))
    }

    fn make_var_extend_existing(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let from_id_col = self.layout.var(src).id_col;
        let filter_col = self.layout.var(dst).id_col;
        let dir = fwd_or_both(directed);
        let (lower, upper, mode, semantic, weight) = self.recursive_spec(rel);
        let build_value = self.recursive_value_rels.contains(&rel);
        let rel_tables = self.rel_member_tables(rel);
        let rel_value_col = self.alloc_recursive_value(rel);
        self.bound.insert(rel);

        PlanOp::VarLengthExtend(Box::new(VarLengthExtend {
            input: Box::new(input),
            from_id_col,
            dir,
            lower,
            upper,
            mode,
            semantic,
            rel_tables,
            rel_value_col,
            build_value,
            filter: self.recursive_filter(rel),
            weight,
            in_named_path: self.recursive_path_rels.contains(&rel),
            target: ExtendTarget::Existing { filter_col },
            factorize: false,
        }))
    }

    /// Build the [`PlanOp::ProjectPath`] that assembles a named path's value from
    /// its head node and `(rel, to-node)` segments.
    fn make_project_path(&mut self, input: PlanOp, path_var: VarId) -> PlanOp {
        let (head, segs) = match &self.query.var(path_var).kind {
            VarKind::Path { head, segments } => (*head, segments.clone()),
            _ => unreachable!("not a path variable"),
        };
        let segments = segs
            .iter()
            .map(|&(rel, to_node)| {
                let rel = if self.query.var(rel).is_recursive() {
                    PathRel::Recursive {
                        value_col: self.layout.var(rel).id_col,
                    }
                } else {
                    PathRel::Single { rel }
                };
                PathSegmentPlan { rel, to_node }
            })
            .collect();
        let path_col = self.layout.alloc(LogicalType::RecursiveRel);
        self.layout.add_var(VarColumns {
            var: path_var,
            kind: VarColKind::Scalar,
            id_col: path_col,
            props: Vec::new(),
            value_col: None,
        });
        PlanOp::ProjectPath(Box::new(ProjectPath {
            input: Box::new(input),
            path_col,
            head,
            segments,
        }))
    }
}

/// `Forward` when directed, else `Both` (undirected pattern).
fn fwd_or_both(directed: bool) -> ExtendDir {
    if directed {
        ExtendDir::Forward
    } else {
        ExtendDir::Both
    }
}

/// `Backward` when directed (extending from the TO side), else `Both`.
fn bwd_or_both(directed: bool) -> ExtendDir {
    if directed {
        ExtendDir::Backward
    } else {
        ExtendDir::Both
    }
}

/// The lifted-sequence ids (`BoundExpr::SequenceCall { id }`) referenced by an
/// expression — used to keep WHERE-read calls before the filter (audit V11).
fn collect_sequence_ids(e: &BoundExpr) -> std::collections::HashSet<usize> {
    fn walk(e: &BoundExpr, out: &mut std::collections::HashSet<usize>) {
        match e {
            BoundExpr::SequenceCall { id, .. } => {
                out.insert(*id);
            }
            BoundExpr::Scalar { args, .. }
            | BoundExpr::Call { args, .. }
            | BoundExpr::List { elems: args, .. } => args.iter().for_each(|a| walk(a, out)),
            BoundExpr::Cast { expr, .. } => walk(expr, out),
            BoundExpr::ValueProperty { value, .. } => walk(value, out),
            BoundExpr::Struct { fields, .. } => fields.iter().for_each(|(_, v)| walk(v, out)),
            BoundExpr::ListLambda { list, body, .. } => {
                walk(list, out);
                walk(body, out);
            }
            BoundExpr::Aggregate { arg: Some(a), .. } => walk(a, out),
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                if let Some(o) = operand {
                    walk(o, out);
                }
                for (c, r) in branches {
                    walk(c, out);
                    walk(r, out);
                }
                if let Some(el) = else_ {
                    walk(el, out);
                }
            }
            _ => {}
        }
    }
    let mut out = std::collections::HashSet::new();
    walk(e, &mut out);
    out
}
