//! The binder's output: resolved (bound) statements, expressions, and queries.
//!
//! Variables are assigned [`VarId`]s; properties carry their resolved
//! [`LogicalType`]; functions are split into scalar [`ScalarOp`]s and
//! [`AggOp`]s. The planner consumes these to build the operator tree.

use koko_catalog::RelStorageDirection;
use koko_common::{
    LogicalType, RelMultiplicity, ScalarUdf, TableId, Value, csv_dialect::CsvOptions,
    file_resolver::FileFormat,
};
use koko_function::{AggOp, ScalarOp};
use std::collections::HashMap;
use std::sync::Arc;

/// A pattern-variable identifier (index into [`BoundQuery::vars`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VarId(pub u32);

/// The higher-order list functions that take a lambda.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LambdaKind {
    Transform,
    Filter,
    Reduce,
}

/// A bound (type-resolved) expression.
#[derive(Debug, Clone, PartialEq)]
pub enum BoundExpr {
    Literal(Value),
    /// A symbolic prepared-statement parameter. It exists only in preparation
    /// bindings; executable bindings substitute a concrete value or NULL.
    Parameter {
        name: String,
        ty: LogicalType,
    },
    /// A direct reference to a layout column by index — a planner-synthesized leaf
    /// (no surface syntax). Used where the planner already knows the exact column
    /// and cannot route through a `VarId` (e.g. a decorrelated join's build-side
    /// keys, which live in fresh columns the global `VarId→column` map does not
    /// point at). Compiles straight to [`CompiledExpr::Column`].
    Column {
        col: usize,
        ty: LogicalType,
    },
    /// `var.prop` — a property access resolved to a type.
    Property {
        var: VarId,
        prop: String,
        ty: LogicalType,
    },
    /// `expr.prop` where `expr` evaluates to a whole node/rel *value* (e.g. an
    /// `UNWIND`ed element of `collect(node)`), so the property is read off the
    /// value at runtime rather than from a bound variable's column.
    ValueProperty {
        value: Box<BoundExpr>,
        prop: String,
        ty: LogicalType,
    },
    /// A whole node/rel variable used as a value (e.g. `RETURN a`).
    NodeRef {
        var: VarId,
        ty: LogicalType,
    },
    /// A scalar variable reference (e.g. an `UNWIND … AS x` value).
    ScalarVar {
        var: VarId,
        ty: LogicalType,
    },
    Scalar {
        op: ScalarOp,
        args: Vec<BoundExpr>,
        ty: LogicalType,
    },
    Aggregate {
        op: AggOp,
        distinct: bool,
        /// `None` only for `count(*)`.
        arg: Option<Box<BoundExpr>>,
        ty: LogicalType,
    },
    /// `CAST(expr AS target)` / `CAST(expr, "target")`.
    Cast {
        expr: Box<BoundExpr>,
        target: LogicalType,
    },
    /// A named scalar function call (e.g. `abs(x)`). `name` is lower-cased.
    Call {
        name: String,
        args: Vec<BoundExpr>,
        ty: LogicalType,
    },
    /// A connection-local native Rust scalar callback resolved at bind time.
    Udf {
        function: Arc<ScalarUdf>,
        args: Vec<BoundExpr>,
        ty: LogicalType,
    },
    /// A list literal `[a, b, c]`.
    List {
        elems: Vec<BoundExpr>,
        ty: LogicalType,
    },
    /// A struct literal `{field: expr, …}`.
    Struct {
        fields: Vec<(String, BoundExpr)>,
        ty: LogicalType,
    },
    /// A higher-order list operation (`list_transform`/`filter`/`reduce`, and the
    /// desugaring target of list comprehensions). `params` names the lambda
    /// variable(s) referenced by `body` as [`BoundExpr::LambdaVar`].
    ListLambda {
        kind: LambdaKind,
        list: Box<BoundExpr>,
        params: Vec<String>,
        body: Box<BoundExpr>,
        ty: LogicalType,
    },
    /// A reference to a lambda parameter (resolved at eval from the lambda stack).
    LambdaVar {
        name: String,
        ty: LogicalType,
    },
    /// An `EXISTS {}` / `COUNT {}` subquery, lifted out of the expression and
    /// computed per row into a column (see [`BoundPart::subqueries`]); `id`
    /// indexes that list. `ty` is `BOOL` (EXISTS) or `INT64` (COUNT).
    Subquery {
        id: usize,
        ty: LogicalType,
    },
    /// A `nextval(...)` / `currval(...)` sequence call, lifted out of the
    /// expression and computed per row into a column (it advances mutable
    /// sequence state, which the pure evaluator can't reach — see
    /// [`BoundPart::sequence_calls`]); `id` indexes that list. `ty` is `INT64`.
    SequenceCall {
        id: usize,
        ty: LogicalType,
    },
    /// `CASE`. With `operand` set (simple CASE) each branch condition is compared
    /// to it with **null-safe** equality (NULL matches NULL, matching the engine);
    /// without (searched CASE) each condition is a boolean predicate. The first
    /// matching branch supplies the value; otherwise `else_` (or `NULL`).
    Case {
        operand: Option<Box<BoundExpr>>,
        branches: Vec<(BoundExpr, BoundExpr)>,
        else_: Option<Box<BoundExpr>>,
        ty: LogicalType,
    },
}

impl BoundExpr {
    pub fn ty(&self) -> LogicalType {
        match self {
            BoundExpr::Literal(v) => v.logical_type(),
            BoundExpr::Parameter { ty, .. }
            | BoundExpr::Column { ty, .. }
            | BoundExpr::Property { ty, .. }
            | BoundExpr::ValueProperty { ty, .. }
            | BoundExpr::NodeRef { ty, .. }
            | BoundExpr::ScalarVar { ty, .. }
            | BoundExpr::Scalar { ty, .. }
            | BoundExpr::Aggregate { ty, .. }
            | BoundExpr::Call { ty, .. }
            | BoundExpr::Udf { ty, .. }
            | BoundExpr::List { ty, .. }
            | BoundExpr::Struct { ty, .. }
            | BoundExpr::ListLambda { ty, .. }
            | BoundExpr::LambdaVar { ty, .. }
            | BoundExpr::Subquery { ty, .. }
            | BoundExpr::SequenceCall { ty, .. }
            | BoundExpr::Case { ty, .. } => ty.clone(),
            BoundExpr::Cast { target, .. } => target.clone(),
        }
    }

    /// Whether this expression contains an aggregate anywhere.
    pub fn contains_aggregate(&self) -> bool {
        match self {
            BoundExpr::Aggregate { .. } => true,
            BoundExpr::Scalar { args, .. }
            | BoundExpr::Call { args, .. }
            | BoundExpr::Udf { args, .. }
            | BoundExpr::List { elems: args, .. } => args.iter().any(|a| a.contains_aggregate()),
            BoundExpr::Cast { expr, .. } => expr.contains_aggregate(),
            BoundExpr::ValueProperty { value, .. } => value.contains_aggregate(),
            BoundExpr::Struct { fields, .. } => fields.iter().any(|(_, v)| v.contains_aggregate()),
            BoundExpr::ListLambda { list, body, .. } => {
                list.contains_aggregate() || body.contains_aggregate()
            }
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                operand.as_ref().is_some_and(|o| o.contains_aggregate())
                    || branches
                        .iter()
                        .any(|(c, r)| c.contains_aggregate() || r.contains_aggregate())
                    || else_.as_ref().is_some_and(|e| e.contains_aggregate())
            }
            _ => false,
        }
    }

    /// Whether this expression contains a lifted `EXISTS {}`/`COUNT {}` subquery
    /// or a `nextval()`/`currval()` sequence call anywhere. Both are computed
    /// per row into chunk columns by the lifting pass, so they cannot be
    /// referenced from contexts evaluated outside the flat pipeline (e.g. a
    /// recursive rel's per-step lambda filter — audit C2).
    pub fn contains_lifted(&self) -> bool {
        match self {
            BoundExpr::Subquery { .. } | BoundExpr::SequenceCall { .. } => true,
            BoundExpr::Scalar { args, .. }
            | BoundExpr::Call { args, .. }
            | BoundExpr::Udf { args, .. }
            | BoundExpr::List { elems: args, .. } => args.iter().any(|a| a.contains_lifted()),
            BoundExpr::Cast { expr, .. } => expr.contains_lifted(),
            BoundExpr::ValueProperty { value, .. } => value.contains_lifted(),
            BoundExpr::Struct { fields, .. } => fields.iter().any(|(_, v)| v.contains_lifted()),
            BoundExpr::ListLambda { list, body, .. } => {
                list.contains_lifted() || body.contains_lifted()
            }
            BoundExpr::Aggregate { arg, .. } => arg.as_ref().is_some_and(|a| a.contains_lifted()),
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                operand.as_ref().is_some_and(|o| o.contains_lifted())
                    || branches
                        .iter()
                        .any(|(c, r)| c.contains_lifted() || r.contains_lifted())
                    || else_.as_ref().is_some_and(|e| e.contains_lifted())
            }
            _ => false,
        }
    }
}

/// A resolved property (column) of a variable's table.
#[derive(Debug, Clone)]
pub struct PropInfo {
    pub name: String,
    pub column_id: u32,
    pub ty: LogicalType,
}

/// The default upper bound for an unbounded variable-length pattern (and the
/// maximum a query may request), mirroring the C++ `var_length_extend_max_depth`
/// default. Overridable per session via `CALL var_length_extend_max_depth = N`.
pub const MAX_RECURSIVE_DEPTH: u32 = 30;

/// Host-provided metadata inspection for non-CSV local sources. Keeping this
/// as a function pointer preserves the crate DAG: binding owns names/types,
/// while the top-level engine injects the concrete safe-Rust Parquet/NPY readers.
pub type FileSchemaResolver =
    fn(FileFormat, &[String]) -> koko_common::Result<Vec<(String, LogicalType)>>;

/// Session-level configuration consulted during binding (set via `CALL k = v`).
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The max recursive depth for variable-length patterns (`var_length_extend_max_depth`).
    pub var_length_extend_max_depth: u32,
    /// `CALL disable_map_key_check=...` — `true` (the C++ default) skips the
    /// map-key NULL/duplicate validation; `false` enables it.
    pub disable_map_key_check: bool,
    /// Statement base directory used before configured search paths.
    pub base_dir: std::path::PathBuf,
    /// Connection-local `home_directory`, used for `~/...` spellings.
    pub home_directory: Option<std::path::PathBuf>,
    /// Connection-local comma-separated `file_search_path`.
    pub file_search_path: String,
    /// Metadata hook for Parquet/NPY sources; `None` in standalone binder tests.
    pub file_schema_resolver: Option<FileSchemaResolver>,
    /// Immutable connection-local scalar-UDF registry snapshot.
    pub scalar_udfs: Arc<HashMap<String, Arc<ScalarUdf>>>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            var_length_extend_max_depth: MAX_RECURSIVE_DEPTH,
            disable_map_key_check: true,
            base_dir: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            home_directory: std::env::var_os("HOME").map(std::path::PathBuf::from),
            file_search_path: String::new(),
            file_schema_resolver: None,
            scalar_udfs: Arc::new(HashMap::new()),
        }
    }
}

/// The path-uniqueness semantic of a recursive relationship (bound mirror of the
/// AST's [`koko_parser::ast::PathSemantic`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathSemantic {
    #[default]
    Walk,
    Trail,
    Acyclic,
}

/// The recursive search mode (bound mirror of [`koko_parser::ast::RecursiveMode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecursiveMode {
    #[default]
    All,
    Shortest,
    AllShortest,
    WShortest,
    AllWShortest,
}

/// A bound per-step filter over a recursive pattern (from the `(r, n | WHERE …)`
/// lambda and any inline `{prop: …}`). The predicate is split so each part can be
/// applied at the right point during expansion:
/// - `rel_pred` references only `rel_param` (the current relationship value) and
///   gates *every* relationship in the path (so it also gates emission);
/// - `node_pred` references `node_param` (the node a relationship leads to) and
///   gates only whether that node may be an *intermediate* (start/end excluded).
///
/// Both reference their parameters as [`BoundExpr::LambdaVar`]. The optional
/// projection lists (`{relProj}, {nodeProj}`) name which properties to keep in the
/// assembled *intermediate* relationship/node values (`None` ⇒ all properties;
/// `Some(vec![])` ⇒ none); they never affect the path's bound endpoints.
#[derive(Debug, Clone)]
pub struct RecursiveFilter {
    pub rel_param: String,
    pub node_param: String,
    pub rel_pred: Option<BoundExpr>,
    pub node_pred: Option<BoundExpr>,
    pub rel_proj: Option<Vec<String>>,
    pub node_proj: Option<Vec<String>>,
}

/// The variable-length / recursive specification attached to a [`VarKind::Rel`].
#[derive(Debug, Clone)]
pub struct RecursiveSpec {
    pub lower: u32,
    pub upper: u32,
    pub mode: RecursiveMode,
    pub semantic: PathSemantic,
    pub filter: Option<RecursiveFilter>,
    /// The rel-property NAME that weights each edge for (ALL) WSHORTEST — its
    /// column id is resolved per rel table at exec (the same name can sit at
    /// different column ids across the traversed tables). `None` unweighted.
    pub weight: Option<String>,
}

/// What a pattern variable refers to.
#[derive(Debug, Clone)]
pub enum VarKind {
    Node {
        /// Candidate node tables. A single entry for a labeled (or
        /// rel-narrowed) node; multiple for an unlabeled `()`/`(a)` (all node
        /// tables) or a multi-label `(a:A:B)` pattern. The matched node's actual
        /// table is recovered at runtime from its internal id.
        tables: Vec<TableId>,
        /// A representative label for display/type purposes (the first table's
        /// name). The rendered label of a whole-node value comes from its
        /// runtime table, not this field.
        label: String,
    },
    Rel {
        /// Candidate relationship tables. A single entry for a single-type rel;
        /// multiple for an unlabeled `-[]->`/`-[r]->` (all rel tables) or a
        /// multi-label `-[:a|:b]->` pattern. The matched rel's actual table is
        /// recovered at runtime from its internal id.
        tables: Vec<TableId>,
        /// A representative label (the first candidate table's name).
        label: String,
        /// The FROM-side node variable.
        src: VarId,
        /// The TO-side node variable.
        dst: VarId,
        /// Whether the pattern arrow was directed.
        directed: bool,
        /// `Some` for a variable-length / recursive relationship; its value is a
        /// `RECURSIVE_REL` (`{_NODES, _RELS}`) rather than a single rel. Boxed to
        /// keep the common (non-recursive) `Rel` variant small.
        recursive: Option<Box<RecursiveSpec>>,
    },
    /// A named path `p = (…)`: a `RECURSIVE_REL` value assembled from the head
    /// node and the chain of `(rel, to-node)` segments (in pattern order).
    Path {
        head: VarId,
        segments: Vec<(VarId, VarId)>,
    },
    /// A scalar (value-typed) variable, e.g. introduced by `UNWIND … AS x`.
    Scalar { ty: LogicalType },
}

/// Everything known about a bound variable.
#[derive(Debug, Clone)]
pub struct VarInfo {
    pub name: String,
    pub anonymous: bool,
    pub kind: VarKind,
    /// All columns of the variable's table (P0 scans every property).
    pub properties: Vec<PropInfo>,
    /// A node/rel variable whose rows are whole *values* (e.g. `UNWIND
    /// collect(a) AS d`) rather than pattern scans; `d.*` then expands the
    /// value struct — `_ID`/`_LABEL` first (C++).
    pub value_backed: bool,
}

impl VarInfo {
    /// A representative table id. For a multi-table (polymorphic) node this is
    /// the first candidate; use [`VarInfo::node_tables`] for the full set.
    pub fn table(&self) -> TableId {
        match &self.kind {
            VarKind::Node { tables, .. } | VarKind::Rel { tables, .. } => tables[0],
            VarKind::Path { .. } | VarKind::Scalar { .. } => {
                unreachable!("path/scalar variable has no table")
            }
        }
    }
    /// The candidate node tables (empty for a non-node variable).
    pub fn node_tables(&self) -> &[TableId] {
        match &self.kind {
            VarKind::Node { tables, .. } => tables,
            _ => &[],
        }
    }
    /// The candidate relationship tables (empty for a non-rel variable).
    pub fn rel_tables(&self) -> &[TableId] {
        match &self.kind {
            VarKind::Rel { tables, .. } => tables,
            _ => &[],
        }
    }
    pub fn label(&self) -> &str {
        match &self.kind {
            VarKind::Node { label, .. } | VarKind::Rel { label, .. } => label,
            VarKind::Path { .. } | VarKind::Scalar { .. } => "",
        }
    }
    pub fn is_node(&self) -> bool {
        matches!(self.kind, VarKind::Node { .. })
    }
    pub fn is_scalar(&self) -> bool {
        matches!(self.kind, VarKind::Scalar { .. })
    }
    /// Whether this is a variable-length / recursive relationship.
    pub fn is_recursive(&self) -> bool {
        matches!(
            self.kind,
            VarKind::Rel {
                recursive: Some(_),
                ..
            }
        )
    }
    /// Whether this is a named-path variable.
    pub fn is_path(&self) -> bool {
        matches!(self.kind, VarKind::Path { .. })
    }
    /// Whether this variable is projected as a whole node/rel value *assembled
    /// from an internal id* (a node, or a plain single-hop rel). Recursive rels
    /// and paths are `RECURSIVE_REL` values carried in a column instead.
    pub fn is_assembled_graph_var(&self) -> bool {
        self.is_node() || (matches!(self.kind, VarKind::Rel { .. }) && !self.is_recursive())
    }
    /// The value type of a scalar variable (`Any` for node/rel vars).
    pub fn scalar_type(&self) -> LogicalType {
        match &self.kind {
            VarKind::Scalar { ty } => ty.clone(),
            _ => LogicalType::Any,
        }
    }
    pub fn property(&self, name: &str) -> Option<&PropInfo> {
        self.properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }
}

/// The reading portion of a query: which node/rel variables to match.
#[derive(Debug, Clone, Default)]
pub struct BoundMatch {
    pub node_vars: Vec<VarId>,
    pub rel_vars: Vec<VarId>,
    /// Named-path variables (`p = …`) introduced by this match; the planner
    /// assembles each into its `RECURSIVE_REL` column after the extends.
    pub path_vars: Vec<VarId>,
}

/// The two existential/count subquery forms (the bound mirror of the AST's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryKind {
    Exists,
    Count,
}

/// A bound `EXISTS {}` / `COUNT {}` subquery: a correlated pattern (referencing
/// outer-scope variables) plus an optional inner `WHERE`. Computed per outer row
/// into a column — `EXISTS` → `count > 0` (BOOL), `COUNT` → the count (INT64).
#[derive(Debug, Clone)]
pub struct BoundSubquery {
    pub kind: SubqueryKind,
    pub match_: BoundMatch,
    pub where_predicate: Option<BoundExpr>,
}

/// The two sequence value functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SequenceFn {
    /// `nextval(name)` — advance the sequence and return the new value.
    NextVal,
    /// `currval(name)` — return the sequence's current value.
    CurrVal,
}

/// A bound `nextval(name)` / `currval(name)` call, lifted to a per-row column
/// (see [`BoundExpr::SequenceCall`]). The sequence `name` is a bind-time constant.
#[derive(Debug, Clone)]
pub struct BoundSequenceCall {
    pub func: SequenceFn,
    pub name: String,
}

/// A bound `OPTIONAL MATCH`: a left-join block. Its `match_` may reference
/// already-bound variables (the correlation) and introduce new ones; on no match
/// the new variables are NULL-extended. `where_predicate` (inline pattern
/// predicates + the clause's `WHERE`) filters matches *before* the null decision.
#[derive(Debug, Clone)]
pub struct BoundOptionalMatch {
    pub match_: BoundMatch,
    pub where_predicate: Option<BoundExpr>,
}

/// A node created by a `CREATE` clause.
#[derive(Debug, Clone)]
pub struct BoundCreateNode {
    pub var: VarId,
    pub table: TableId,
    pub num_columns: usize,
    pub pk_col: usize,
    /// `(column_index, value_expr)` for the columns the pattern specified.
    pub props: Vec<(usize, BoundExpr)>,
}

/// A relationship created by a `CREATE`/`MERGE` clause.
#[derive(Debug, Clone)]
pub struct BoundCreateRel {
    pub table: TableId,
    pub src: VarId,
    pub dst: VarId,
    pub num_columns: usize,
    pub props: Vec<(usize, BoundExpr)>,
    /// The rel variable, when it is projectable (a `MERGE`d rel); `None` for a
    /// plain `CREATE` rel whose value is never read.
    pub var: Option<VarId>,
}

/// A bound `CREATE`.
#[derive(Debug, Clone, Default)]
pub struct BoundCreate {
    pub nodes: Vec<BoundCreateNode>,
    pub rels: Vec<BoundCreateRel>,
}

/// One bound updating clause, applied (in order) after the part's reading.
#[derive(Debug, Clone)]
pub enum BoundUpdate {
    Create(BoundCreate),
    Set(BoundSet),
    Delete(BoundDelete),
    /// Boxed to keep the enum small (a `MERGE` carries a match + create + two sets).
    Merge(Box<BoundMerge>),
}

/// A bound `MERGE`: a match attempt over the pattern, the create-on-miss
/// instructions for the parts it introduces, and the `ON CREATE`/`ON MATCH` sets.
/// The match and create share the same pattern variables.
#[derive(Debug, Clone)]
pub struct BoundMerge {
    pub match_: BoundMatch,
    /// The inline-property match filter (`AND` of `var.prop = value`).
    pub filter: Option<BoundExpr>,
    pub create: BoundCreate,
    pub on_create: BoundSet,
    pub on_match: BoundSet,
}

/// A bound `SET item, …`.
#[derive(Debug, Clone)]
pub struct BoundSet {
    pub items: Vec<BoundSetItem>,
}

/// One bound `SET` assignment.
#[derive(Debug, Clone)]
pub struct BoundSetItem {
    pub target: BoundSetTarget,
    pub value: BoundExpr,
}

/// The resolved left side of a `SET`.
#[derive(Debug, Clone)]
pub enum BoundSetTarget {
    /// `var.prop = value` — one property (resolved by name against the matched
    /// entity's actual table at runtime, so polymorphic vars work).
    Property { var: VarId, prop: String },
    /// A schemaless property stored inside the hidden table's ordered JSON `data` column.
    DynamicProperty { var: VarId, prop: String },
    /// `var = value` / `var += value` — set the whole node/rel's properties from a
    /// map/struct value.
    Var { var: VarId },
}

/// A bound `[DETACH] DELETE` of one or more node/rel variables.
#[derive(Debug, Clone)]
pub struct BoundDelete {
    pub vars: Vec<VarId>,
    pub detach: bool,
}

/// One projection item.
#[derive(Debug, Clone)]
pub enum ProjItem {
    /// A scalar output: either a group key (no aggregate) or an aggregate item.
    Scalar { name: String, expr: BoundExpr },
    /// A whole node/rel variable (`RETURN a`).
    Var { name: String, var: VarId },
}

/// A bound `ORDER BY` key.
#[derive(Debug, Clone)]
pub enum OrderKey {
    /// References an output column by index (e.g. `ORDER BY <alias>`).
    Output(usize),
    /// An expression evaluated against the input rows (non-aggregate queries).
    Expr(BoundExpr),
    /// An expression evaluated against the projected output row. Used for the C++
    /// aggregate/DISTINCT ORDER BY scope, where only projected expressions/aliases
    /// are visible but scalar expressions over those outputs are still allowed.
    PostProjection(BoundExpr),
}

/// A bound `RETURN`.
#[derive(Debug, Clone)]
pub struct BoundProjection {
    pub distinct: bool,
    pub items: Vec<ProjItem>,
    pub order_by: Vec<(OrderKey, bool)>,
    /// `SKIP`/`LIMIT` counts: constant expressions, folded (and validated as
    /// non-negative integers, a *runtime* error) at execution start.
    pub skip: Option<BoundExpr>,
    pub limit: Option<BoundExpr>,
}

impl BoundProjection {
    pub fn has_aggregates(&self) -> bool {
        self.items.iter().any(|i| match i {
            ProjItem::Scalar { expr, .. } => expr.contains_aggregate(),
            ProjItem::Var { .. } => false,
        })
    }
}

/// A bound `UNWIND <list> AS var`, applied (in order) after the match.
#[derive(Debug, Clone)]
pub struct BoundUnwind {
    pub var: VarId,
    pub list: BoundExpr,
}

/// The catalog-introspection table functions, as a binder-side mirror of the
/// parser's `TableFunc` (the planner/processor crates do not depend on
/// `koko-parser`, so the bound tree carries its own copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundTableFunc {
    ShowTables,
    ShowSequences,
    TableInfo,
    ShowMacros,
    ShowFunctions,
    DbVersion,
    ShowOfficialExtensions,
    CacheArrayColumn,
    ClearWarnings,
    ShowIndexes,
    ShowWarnings,
    ShowConnection,
    StorageInfo,
    StatsInfo,
    CurrentSetting,
    BmInfo,
    ShowLoadedExtensions,
}

/// A bound in-query table-function scan: a 0→N leaf source whose output columns
/// are registered as scalar variables (so a surrounding `WHERE`/`RETURN` binds
/// against them with no extra machinery). `columns` is `(var, type)` per output
/// column, in column order; the rows are produced at execution from the catalog
/// via [`crate::table_func_rows`].
#[derive(Debug, Clone)]
pub struct BoundTableFuncScan {
    pub func: BoundTableFunc,
    pub arg: Option<String>,
    pub columns: Vec<(VarId, LogicalType)>,
}

/// CSV reader options for `COPY` and typed `LOAD FROM`.
pub type CsvLoadOptions = CsvOptions;

/// A bound CSV `LOAD FROM` leaf source: registered scalar variables, the file
/// path, reader options, and whether the columns came from bare LOAD sniffing.
/// Bare LOAD keeps STRING types in this phase, but execution may normalize
/// list-looking strings to C++'s canonical display form.
#[derive(Debug, Clone)]
pub struct BoundLoadScan {
    pub columns: Vec<(VarId, LogicalType)>,
    /// The declared column names, in order (for the header-row auto-detect heuristic).
    pub col_names: Vec<String>,
    pub path: String,
    /// The full ordered file set (glob-expanded / `["a","b"]` list); `path` is
    /// its first element, kept for the header/sniff decisions.
    pub paths: Vec<String>,
    /// Format selected once by the central resolver.
    pub format: FileFormat,
    pub options: CsvLoadOptions,
    pub bare: bool,
}

/// One bound query part: reading + optional updating, terminated by a projection.
///
/// A part reads from an *input scope* — scalar variables (`input_vars`) carried
/// from the previous part's `WITH` (empty for the first part) — and produces an
/// *output scope* via its `projection`. A non-terminal part's projection is a
/// `WITH` (whose outputs are `carried` into the next part); the terminal part's
/// projection is the `RETURN` (`None` for a write-only query).
#[derive(Debug, Clone, Default)]
pub struct BoundPart {
    /// Scalar variables carried in from the previous part's `WITH`, in column
    /// order. Empty for the first part.
    pub input_vars: Vec<VarId>,
    /// `WITH … WHERE` predicate from the *previous* part, bound against this
    /// part's input (carried) scope and applied before any of this part's own
    /// reading clauses.
    pub input_filter: Option<BoundExpr>,
    pub match_: BoundMatch,
    /// An in-query table-function scan that is the part's *sole* base source
    /// (mutually exclusive with `match_`/`unwind` in this phase); the planner uses
    /// it instead of `build_match` as the leaf, and `where_predicate` filters it.
    pub table_func_scans: Vec<BoundTableFuncScan>,
    /// A CSV `LOAD FROM` base source. Unlike `table_func_scan`, it *may* be
    /// followed by a `MATCH` (the planner composes the match onto the load rows),
    /// enabling `LOAD … MATCH … CREATE` relationship bulk-load.
    pub load_scan: Option<BoundLoadScan>,
    /// `UNWIND` expansions applied (in order) after the match.
    pub unwind: Vec<BoundUnwind>,
    pub where_predicate: Option<BoundExpr>,
    /// `EXISTS {}` / `COUNT {}` subqueries referenced by this part's expressions,
    /// indexed by the `id` carried in [`BoundExpr::Subquery`]. Each is computed
    /// per row into a column after the match (before the WHERE / projection).
    pub subqueries: Vec<BoundSubquery>,
    /// `nextval`/`currval` calls referenced by this part's expressions, indexed by
    /// the `id` in [`BoundExpr::SequenceCall`]. Each is computed per row into a
    /// column (advancing sequence state) before the WHERE / projection.
    pub sequence_calls: Vec<BoundSequenceCall>,
    /// `OPTIONAL MATCH` left-join blocks, applied (in order) after the required
    /// match / unwinds / WHERE.
    pub optionals: Vec<BoundOptionalMatch>,
    /// Updating clauses (`CREATE`/`SET`/`DELETE`), applied in order after the
    /// reading clauses and before the part's projection.
    pub updates: Vec<BoundUpdate>,
    pub projection: Option<BoundProjection>,
    /// The scalar variables this part's `WITH` projection introduces into the
    /// next part's scope (parallel to `projection.items`). Empty for the terminal
    /// part (a `RETURN` carries nothing forward).
    pub carried: Vec<VarId>,
}

/// A bound read/write query: a sequence of parts sharing one variable arena.
#[derive(Debug, Clone)]
pub struct BoundQuery {
    /// Global variable arena; `VarId`s index into this across all parts.
    pub vars: Vec<VarInfo>,
    /// Query parts in order. The last is terminal (its projection is the
    /// `RETURN`, or it is a write); every earlier part ends in a `WITH`.
    pub parts: Vec<BoundPart>,
}

impl BoundQuery {
    pub fn var(&self, id: VarId) -> &VarInfo {
        &self.vars[id.0 as usize]
    }

    /// The result columns of this query (its terminal part's projection): one
    /// `(name, type)` per output column. Empty for a write-only query.
    pub fn result_columns(&self) -> Vec<(String, LogicalType)> {
        let Some(proj) = self.parts.last().and_then(|p| p.projection.as_ref()) else {
            return Vec::new();
        };
        proj.items
            .iter()
            .map(|it| match it {
                ProjItem::Scalar { name, expr } => (name.clone(), expr.ty()),
                ProjItem::Var { name, var } => (name.clone(), self.var_value_type(*var)),
            })
            .collect()
    }

    fn var_value_type(&self, var: VarId) -> LogicalType {
        match &self.var(var).kind {
            VarKind::Node { tables, .. } => LogicalType::Node(tables[0]),
            VarKind::Rel {
                recursive: Some(_), ..
            } => LogicalType::RecursiveRel,
            VarKind::Rel { tables, .. } => LogicalType::Rel(tables[0]),
            VarKind::Path { .. } => LogicalType::RecursiveRel,
            VarKind::Scalar { ty } => ty.clone(),
        }
    }
}

/// One or more [`BoundQuery`] operands combined by `UNION` / `UNION ALL`.
///
/// Each operand binds in its own variable namespace. `distinct` is `true` only
/// for a plain multi-operand `UNION` (dedup the combined result over all
/// columns); it is `false` for `UNION ALL` and for a single operand.
#[derive(Debug, Clone)]
pub struct BoundRegularQuery {
    pub operands: Vec<BoundQuery>,
    pub distinct: bool,
}

/// A column `DEFAULT` as resolved by the binder. `Const` carries an *unevaluated*
/// bound expression (the exec layer folds it to a `Value` via the real evaluator —
/// the binder can't reach `koko-expr`); `NextVal` keeps the sequence name (per-row,
/// mutable). The exec layer maps this to a catalog `ColumnDefault`.
#[derive(Debug, Clone)]
pub enum BoundColumnDefault {
    None,
    Const(BoundExpr),
    NextVal(String),
}

/// Display-only catalog metadata for a bound column. Execution still uses
/// [`LogicalType`] and [`BoundColumnDefault`], but C++ `TABLE_INFO` renders the
/// source default text and `SERIAL` as a logical type name.
#[derive(Debug, Clone)]
pub struct BoundColumnMetadata {
    pub type_text: String,
    pub default_text: String,
}

/// Parquet compression codecs accepted by the C++ 0.17.0 export surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoundParquetCompression {
    Uncompressed,
    #[default]
    Snappy,
    Zstd,
    Gzip,
    Lz4Raw,
}

/// Validated, format-specific output options. Keeping this an enum prevents
/// execution from accidentally applying CSV settings to a Parquet writer.
#[derive(Debug, Clone)]
pub enum BoundOutputOptions {
    Csv(CsvLoadOptions),
    Parquet {
        compression: BoundParquetCompression,
    },
}

impl BoundOutputOptions {
    pub fn format(&self) -> FileFormat {
        match self {
            Self::Csv(_) => FileFormat::Csv,
            Self::Parquet { .. } => FileFormat::Parquet,
        }
    }
}

/// A bound top-level statement.
#[derive(Debug, Clone)]
pub enum BoundStatement {
    CreateNodeTable {
        name: String,
        columns: Vec<(String, LogicalType)>,
        /// Per-column `DEFAULT`s, aligned to `columns` by index.
        defaults: Vec<BoundColumnDefault>,
        metadata: Vec<BoundColumnMetadata>,
        primary_key: String,
        if_not_exists: bool,
        /// Indices of `SERIAL` columns (auto-incrementing; physically `INT64`).
        serial_columns: Vec<usize>,
        /// Local read-only `icebug-disk` root/path, when this is an external table.
        icebug_storage: Option<String>,
    },
    CreateRelTable {
        name: String,
        /// Resolved `(from, to)` node-table id pairs (at least one).
        pairs: Vec<(TableId, TableId)>,
        columns: Vec<(String, LogicalType)>,
        /// Per-column `DEFAULT`s, aligned to `columns` by index.
        defaults: Vec<BoundColumnDefault>,
        metadata: Vec<BoundColumnMetadata>,
        if_not_exists: bool,
        multiplicity: RelMultiplicity,
        storage_direction: RelStorageDirection,
        /// Local read-only `icebug-disk` root/path, when this is an external table.
        icebug_storage: Option<String>,
    },
    DropTable {
        /// The user-typed table name (used verbatim in the result message).
        name: String,
        /// `Some(id)` if the table exists and should be dropped; `None` means it
        /// is absent but `IF EXISTS` was given, so execution emits a skip message.
        table: Option<TableId>,
    },
    Alter {
        table: TableId,
        /// The catalog's canonical table name (used in result/error messages).
        table_name: String,
        op: BoundAlterOp,
    },
    /// `CREATE SEQUENCE` — option values resolved + range-checked to `INT64`.
    CreateSequence {
        name: String,
        if_not_exists: bool,
        start: i64,
        increment: i64,
        min: i64,
        max: i64,
        cycle: bool,
    },
    /// `DROP SEQUENCE [IF EXISTS]`.
    DropSequence {
        name: String,
        if_exists: bool,
    },
    /// `COMMENT ON TABLE` — set a table's comment.
    Comment {
        table: TableId,
        table_name: String,
        comment: String,
    },
    /// `CREATE TYPE <name> AS <type>` — register a type alias (`ty` is resolved).
    CreateType {
        name: String,
        ty: LogicalType,
    },
    /// `CREATE … AS <query>` (CTAS): the schema is `columns` (first = PK for a node
    /// table), inferred from the query's result; execution runs `query` and
    /// inserts each result row.
    CreateTableAs {
        name: String,
        is_node: bool,
        pairs: Vec<(TableId, TableId)>,
        storage_direction: RelStorageDirection,
        if_not_exists: bool,
        columns: Vec<(String, LogicalType)>,
        query: Box<BoundRegularQuery>,
    },
    Copy(BoundCopy),
    CopyTo(BoundCopyTo),
    ExportDatabase(BoundExportDatabase),
    ImportDatabase(BoundImportDatabase),
    Query(Box<BoundRegularQuery>),
}

/// A bound `ALTER TABLE` mutation. Property existence/conflict checks happen at
/// execution (they produce `Runtime exception`s or `IF`-guarded skip messages);
/// the binder resolves types and the `DEFAULT` value, and rejects dropping a
/// primary-key column.
#[derive(Debug, Clone)]
pub enum BoundAlterOp {
    AddProperty {
        name: String,
        ty: LogicalType,
        /// The column `DEFAULT` (resolved at exec into the existing-row backfill +
        /// the stored catalog default).
        default: BoundColumnDefault,
        metadata: BoundColumnMetadata,
        if_not_exists: bool,
    },
    DropProperty {
        name: String,
        if_exists: bool,
    },
    RenameProperty {
        old: String,
        new: String,
    },
    RenameTable {
        new: String,
    },
    /// Add a rel-group endpoint pair (resolved node-table ids).
    AddFromTo {
        from: TableId,
        to: TableId,
        if_not_exists: bool,
    },
    /// Drop a rel-group endpoint pair (resolved node-table ids).
    DropFromTo {
        from: TableId,
        to: TableId,
        if_exists: bool,
    },
}

/// A bound `COPY <table> FROM "<file>"`.
#[derive(Debug, Clone)]
pub struct BoundCopy {
    pub table: TableId,
    pub is_node: bool,
    /// Partial column list (`COPY t(a, b) FROM …`); `None` = all.
    pub columns: Option<Vec<String>>,
    pub file_path: String,
    /// Additional files of a multi-file COPY, loaded in order after
    /// `file_path`.
    pub extra_files: Vec<String>,
    /// Format selected once by the central resolver (`None` for query sources).
    pub format: Option<FileFormat>,
    /// `BY COLUMN` selects NPY's column-oriented source interpretation.
    pub by_column: bool,
    /// `COPY t FROM (<query>)` — the bound source query (file fields unused).
    pub source_query: Option<Box<BoundRegularQuery>>,
    pub options: CsvLoadOptions,
}

/// A bound `COPY (<query>) TO '<path>'` export. `columns` is the exact result
/// schema the writer must emit, including the empty-result case.
#[derive(Debug, Clone)]
pub struct BoundCopyTo {
    pub query: Box<BoundRegularQuery>,
    pub columns: Vec<(String, LogicalType)>,
    pub path: String,
    pub options: BoundOutputOptions,
}

/// A bound logical database export. Catalog reads and filesystem creation are
/// execution responsibilities; binding only validates the requested format and options.
#[derive(Debug, Clone)]
pub struct BoundExportDatabase {
    pub path: String,
    pub options: BoundOutputOptions,
    pub schema_only: bool,
}

/// A bound logical database import. Directory preflight and mutation remain an
/// atomic execution operation.
#[derive(Debug, Clone)]
pub struct BoundImportDatabase {
    pub path: String,
}
