//! The parser's owned Cypher abstract syntax tree.
//!
//! Types and names remain unresolved here: DDL types are source text and variables
//! are names. The binder resolves them against catalog and function metadata.

use koko_common::Value;

/// A top-level statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `EXPLAIN [LOGICAL] <stmt>` / `PROFILE <stmt>` — EXPLAIN validates and
    /// plans without executing; PROFILE executes. Plan rendering is engine-specific
    /// (see `ROADMAP.md` decision `explain-profile-plans`).
    Explain {
        inner: Box<Statement>,
        profile: bool,
    },
    /// `CREATE GRAPH [IF NOT EXISTS] <name> [ANY]`.
    CreateGraph(CreateGraph),
    /// `USE GRAPH <name>` — select a graph for this connection.
    UseGraph {
        name: String,
    },
    /// `DROP GRAPH [IF EXISTS] <name>`.
    DropGraph {
        name: String,
        if_exists: bool,
    },
    /// `CREATE [HASH|ART] INDEX ... FOR (n:Table) ON (n.primary_key)`.
    CreateIndex(CreateIndex),
    /// `DROP INDEX [IF EXISTS] <name>`.
    DropIndex(DropIndex),
    CreateNodeTable(CreateNodeTable),
    CreateRelTable(CreateRelTable),
    /// `DROP TABLE [IF EXISTS] <name>` — remove a node or rel table.
    DropTable(DropTable),
    /// `ALTER TABLE <name> <op>` — add/drop/rename a property, or rename the table.
    Alter(AlterStatement),
    /// `CREATE SEQUENCE [IF NOT EXISTS] <name> [options]`.
    CreateSequence(CreateSequence),
    /// `DROP SEQUENCE [IF EXISTS] <name>`.
    DropSequence(DropSequence),
    /// `COMMENT ON TABLE <name> IS '<text>'`.
    Comment(CommentStmt),
    /// `CREATE NODE/REL TABLE <name> [(FROM a TO b)] AS <query>` (CTAS).
    CreateTableAs(CreateTableAs),
    /// `CREATE TYPE <name> AS <type>` — a user-defined type alias.
    CreateType(CreateType),
    /// `CREATE MACRO <name>(<args>) AS <body>` — a scalar macro definition.
    CreateMacro(CreateMacro),
    /// `DROP MACRO [IF EXISTS] <name>` — remove a scalar macro.
    DropMacro {
        name: String,
        if_exists: bool,
    },
    /// `COPY <table> FROM <source>` — bulk import into a table.
    Copy(CopyStatement),
    /// `COPY (<query>) TO '<path>' [(options)]` — export a query result.
    CopyTo(CopyToStatement),
    /// `EXPORT DATABASE '<directory>' [(options)]` — logical database export.
    ExportDatabase(ExportDatabaseStatement),
    /// `IMPORT DATABASE '<directory>'` — logical database import.
    ImportDatabase(ImportDatabaseStatement),
    Query(RegularQuery),
    /// A transaction-control statement (`BEGIN`/`COMMIT`/`ROLLBACK`/`CHECKPOINT`).
    Transaction(TxnOp),
    /// A standalone `CALL` (config setting or `current_setting`).
    Call(CallStmt),
}

/// The schema mode of a named graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphKind {
    Typed,
    Any,
}

/// `CREATE GRAPH [IF NOT EXISTS] <name> [ANY]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateGraph {
    pub name: String,
    pub if_not_exists: bool,
    pub kind: GraphKind,
}

/// The in-memory primary-key index implementation selected by DDL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    Hash,
    Art,
}

/// `CREATE [HASH|ART] INDEX <name> [IF NOT EXISTS] FOR (n:<table>) ON (n.<property>)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    pub name: String,
    pub if_not_exists: bool,
    pub index_type: IndexType,
    pub variable: String,
    pub table: String,
    pub properties: Vec<String>,
    pub options: Vec<(String, Expr)>,
}

/// `DROP INDEX [IF EXISTS] <name>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIndex {
    pub name: String,
    pub if_exists: bool,
}

/// `DROP TABLE [IF EXISTS] <name>`. Works for both node and rel tables (the kind
/// is resolved from the catalog at bind time).
#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub name: String,
    pub if_exists: bool,
}

/// `CREATE SEQUENCE [IF NOT EXISTS] <name> [START [WITH] n] [INCREMENT [BY] n]
/// [MINVALUE n | NO MINVALUE] [MAXVALUE n | NO MAXVALUE] [CYCLE | NO CYCLE]`.
///
/// Option values are carried as raw `i128` (the lexer's integer width) so the
/// binder can range-check them against `INT64` and emit the out-of-bounds error.
/// A `None` option means "unspecified" (the binder applies the sign-dependent
/// default); `NO MINVALUE`/`NO MAXVALUE` also map to the default.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateSequence {
    pub name: String,
    pub if_not_exists: bool,
    pub start: Option<i128>,
    pub increment: Option<i128>,
    pub min_value: Option<i128>,
    pub max_value: Option<i128>,
    pub cycle: bool,
}

/// `DROP SEQUENCE [IF EXISTS] <name>`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropSequence {
    pub name: String,
    pub if_exists: bool,
}

/// `COMMENT ON TABLE <name> IS '<text>'`.
#[derive(Debug, Clone, PartialEq)]
pub struct CommentStmt {
    pub table: String,
    pub comment: String,
}

/// `CREATE TYPE <name> AS <type>` — register a user-defined type alias (a
/// primitive alias or a `STRUCT(…)`) usable as a column/cast type thereafter.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateType {
    pub name: String,
    /// The raw underlying type string (resolved by the binder).
    pub type_name: String,
}

/// `CREATE MACRO <name>(<positional>, <name> := <default>, …) AS <body>` — a
/// user-defined scalar macro: a parameterized expression template. At a call site
/// the call's arguments are substituted for the parameters in `body` (positional
/// first, then defaults left-to-right; missing trailing defaults fall back to
/// their default expression), and the result is bound in place of the call.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateMacro {
    pub name: String,
    /// Required positional parameter names, in order.
    pub positional: Vec<String>,
    /// Optional `name := default` parameters, in order (the default is a literal
    /// in Cypher, carried as an `Expr` for uniform substitution/rendering).
    pub defaults: Vec<(String, Expr)>,
    /// The macro body expression.
    pub body: Box<Expr>,
}

/// `CREATE NODE TABLE <name> AS <query>` / `CREATE REL TABLE <name> (FROM a TO b)
/// AS <query>` — create a table whose schema is inferred from a query's result,
/// then populate it with that result. The first output column is the primary key
/// (for node tables).
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableAs {
    pub name: String,
    pub is_node: bool,
    /// FROM-TO node-table-name pairs (rel tables only; empty for node tables).
    pub pairs: Vec<(String, String)>,
    /// Parsed `WITH (storage_direction=...)` value for rel CTAS, if present.
    pub storage_direction: Option<String>,
    pub if_not_exists: bool,
    pub query: RegularQuery,
}

/// `ALTER TABLE <table> <op>`.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterStatement {
    pub table: String,
    pub op: AlterOp,
}

/// The mutation an `ALTER TABLE` applies. (Rel-group `ADD/DROP FROM…TO` and
/// `COMMENT ON` are later sub-waves.)
#[derive(Debug, Clone, PartialEq)]
pub enum AlterOp {
    /// `ADD [IF NOT EXISTS] <name> <type> [DEFAULT <expr>]`.
    AddProperty {
        name: String,
        type_name: String,
        default: Option<Expr>,
        if_not_exists: bool,
    },
    /// `DROP [IF EXISTS] <name>`.
    DropProperty { name: String, if_exists: bool },
    /// `RENAME [COLUMN] <old> TO <new>`.
    RenameProperty { old: String, new: String },
    /// `RENAME TO <new>`.
    RenameTable { new: String },
    /// `ADD [IF NOT EXISTS] FROM <from> TO <to>` — add a rel-group endpoint pair.
    AddFromTo {
        from: String,
        to: String,
        if_not_exists: bool,
    },
    /// `DROP [IF EXISTS] FROM <from> TO <to>` — drop a rel-group endpoint pair.
    DropFromTo {
        from: String,
        to: String,
        if_exists: bool,
    },
}

/// A transaction-control operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnOp {
    Begin { read_only: bool },
    Commit,
    Rollback,
    Checkpoint,
}

/// A standalone `CALL`: either a config assignment (`CALL k = v`) or a
/// function invocation without a query projection.
#[derive(Debug, Clone, PartialEq)]
pub enum CallStmt {
    /// `CALL <key> = <value>` — set a session/config option.
    SetConfig { key: String, value: Expr },
    /// `CALL <name>(<typed expressions>)` — resolved and validated by the binder.
    Function(CallClause),
}

/// A typed function invocation shared by standalone and in-query `CALL`.
#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    pub name: String,
    pub args: Vec<Expr>,
    /// `YIELD col [AS alias], …` — selects and renames declared output columns.
    pub yield_items: Vec<(String, Option<String>)>,
    /// A filter applied immediately after the function source.
    pub where_clause: Option<Expr>,
}

/// One or more [`SingleQuery`]s combined by `UNION` / `UNION ALL`.
///
/// `union_all[i]` is the kind of the boundary between `singles[i]` and
/// `singles[i+1]` (`true` = `UNION ALL`), so `union_all.len() == singles.len() -
/// 1`. A query with no `UNION` has a single element and an empty `union_all`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegularQuery {
    pub singles: Vec<SingleQuery>,
    pub union_all: Vec<bool>,
}

/// `COPY <table> FROM "<file>" [(options)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyStatement {
    pub table: String,
    /// `COPY t(col, …) FROM` — restricts the CSV input to these columns
    /// (others take their defaults). `None` = all non-serial columns.
    pub columns: Option<Vec<String>>,
    /// `COPY t FROM (<query>)` — the source is a query's result rows instead
    /// of files (`file_path` is empty then).
    pub source_query: Option<RegularQuery>,
    pub file_path: String,
    /// Additional source files of a multi-file `COPY t FROM ("a", "b")` /
    /// `[…]` form (loaded in order after `file_path`).
    pub extra_files: Vec<String>,
    /// `BY COLUMN` suffix selecting NPY column-oriented loading.
    pub by_column: bool,
    pub options: Vec<(String, LoadOptVal)>,
}

/// `COPY (<query>) TO '<path>' [(options)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct CopyToStatement {
    pub query: RegularQuery,
    pub path: String,
    pub options: Vec<(String, LoadOptVal)>,
}

/// `EXPORT DATABASE '<directory>' [(options)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportDatabaseStatement {
    pub path: String,
    pub options: Vec<(String, LoadOptVal)>,
}

/// `IMPORT DATABASE '<directory>'`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportDatabaseStatement {
    pub path: String,
}

/// `CREATE NODE TABLE name (col type, …, PRIMARY KEY(col))`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateNodeTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub primary_key: String,
    pub if_not_exists: bool,
    /// External table root/path from `WITH (storage = ...)`.
    pub storage: Option<String>,
    /// External table format from `WITH (format = ...)`.
    pub format: Option<String>,
}

/// `CREATE REL TABLE name (FROM a TO b[, FROM c TO d …], col type, …)`. A rel
/// table may declare multiple FROM-TO node-table pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRelTable {
    pub name: String,
    /// One or more `(from, to)` node-table-name pairs (at least one).
    pub pairs: Vec<(String, String)>,
    pub columns: Vec<ColumnDef>,
    pub if_not_exists: bool,
    /// The `X_Y` multiplicity keyword, if any (default `MANY_MANY` = unconstrained).
    pub multiplicity: koko_common::RelMultiplicity,
    /// Parsed `WITH (storage_direction=...)` value, if present.
    pub storage_direction: Option<String>,
    /// External table root/path from `WITH (storage = ...)`.
    pub storage: Option<String>,
    /// External table format from `WITH (format = ...)`.
    pub format: Option<String>,
}

/// A column declaration in DDL. `type_name` is the raw type string (resolved by
/// the binder); `default` is the optional `DEFAULT <expr>` (resolved/folded by
/// the binder + exec).
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub type_name: String,
    pub default: Option<Expr>,
}

/// A reading/updating/returning query, possibly split into multiple parts by
/// `WITH` clauses.
///
/// A query is a sequence of [`QueryPart`]s, each terminated by a `WITH`, followed
/// by a final part (the trailing `reading`/`updating`/`ret` fields) terminated by
/// an optional `RETURN`. A query with no `WITH` has an empty `parts` and is just
/// the final part — structurally identical to the pre-`WITH` shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SingleQuery {
    /// Leading parts, each ending in a `WITH`. Empty for a single-part query.
    pub parts: Vec<QueryPart>,
    /// The final part's reading clauses.
    pub reading: Vec<ReadingClause>,
    /// The final part's updating clauses (`CREATE`/`SET`/`DELETE`), in order.
    pub updating: Vec<UpdatingClause>,
    /// The terminal `RETURN` (absent for a write-only query).
    pub ret: Option<ReturnClause>,
}

/// A `WITH`-terminated query part: reading/updating clauses then a `WITH`.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPart {
    pub reading: Vec<ReadingClause>,
    pub updating: Vec<UpdatingClause>,
    pub with: WithClause,
}

/// `WITH [DISTINCT] items [ORDER BY …] [SKIP n] [LIMIT n] [WHERE pred]`.
///
/// The projection shares [`ReturnClause`]'s shape; `where_clause` is the trailing
/// post-projection filter, applied *after* `ORDER BY`/`SKIP`/`LIMIT` (matching the
/// C++ planner, which appends it after the whole projection body).
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    pub projection: ReturnClause,
    pub where_clause: Option<Expr>,
}

/// A reading clause: a graph `MATCH`, an `UNWIND` list expansion, a typed
/// function call, or a loaded-data source.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadingClause {
    Match(MatchClause),
    Unwind(UnwindClause),
    /// `CALL <name>(<typed expressions>) [YIELD …] [WHERE …]` used as a 0→N
    /// source. Function identity and argument rules are binder-owned.
    Call(CallClause),
    /// `LOAD [WITH HEADERS (col TYPE, …)] FROM "<file>" [(options)]` — scan a
    /// resolved CSV, Parquet, or NPY source as a 0→N row source. The surrounding
    /// query reads, filters, and updates over the loaded columns.
    LoadFrom(LoadFromClause),
}

/// A `LOAD FROM` reading clause.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadFromClause {
    /// `Some` when `WITH HEADERS (name TYPE, …)` gives explicit columns/types;
    /// `None` delegates schema discovery to CSV sniffing or columnar metadata.
    /// Each entry is `(column name, canonical type-name string)`; the binder
    /// resolves the type name like a DDL column type.
    pub headers: Option<Vec<(String, String)>>,
    /// The quoted file path (already `${KOKO_ROOT_DIRECTORY}`-expanded by the runner
    /// for the corpus). May be a glob pattern (`vPerson*.csv`), expanded by the
    /// binder.
    pub path: String,
    /// Additional source files of a `LOAD FROM ["a", "b"]` list (loaded in order
    /// after `path`).
    pub extra_paths: Vec<String>,
    /// Trailing `(key = value, …)` reader options. The binder validates every key:
    /// supported CSV options are honored, unsupported C++ options are rejected, and
    /// unknown options are errors.
    pub options: Vec<(String, LoadOptVal)>,
    /// A `WHERE` immediately following the file (filters the loaded rows). A `WHERE`
    /// that follows a `MATCH` belongs to that match instead.
    pub where_clause: Option<Expr>,
}

/// A scalar/list value of a CSV reader option.
#[derive(Debug, Clone, PartialEq)]
pub enum LoadOptVal {
    Str(String),
    Int(i64),
    /// A float value parses (the binder then rejects it per option with the
    /// C++ "must be a INT64"-style type error).
    Float(f64),
    Bool(bool),
    List(Vec<LoadOptVal>),
}

/// `UNWIND <expr> AS <var>` — expand a list into one row per element.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub expr: Expr,
    pub var: String,
}

/// `MATCH p, … [WHERE pred]` (or `OPTIONAL MATCH`).
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub patterns: Vec<PatternElement>,
    pub optional: bool,
    pub where_clause: Option<Expr>,
    /// A trailing `HINT <join-tree>` (validated by the binder; the
    /// single-order planner otherwise ignores it).
    pub hint: Option<JoinHint>,
}

/// A join-order `HINT` tree: variables joined pairwise, with `MULTI_JOIN`
/// attaching extra rel variables to a subtree (a worst-case-optimal join).
#[derive(Debug, Clone, PartialEq)]
pub enum JoinHint {
    Var(String),
    Join(Box<JoinHint>, Box<JoinHint>),
    MultiJoin(Box<JoinHint>, Vec<String>),
}

/// An updating clause: `CREATE`, `SET`, `[DETACH] DELETE`, or `MERGE`.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdatingClause {
    Create(CreateClause),
    Set(SetClause),
    Delete(DeleteClause),
    Merge(MergeClause),
}

/// `MERGE <pattern> [ON CREATE SET …] [ON MATCH SET …]` — match the pattern or
/// create it, then apply the corresponding `SET` items.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    pub patterns: Vec<PatternElement>,
    pub on_create: Vec<SetItem>,
    pub on_match: Vec<SetItem>,
}

/// `CREATE p, …`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    pub patterns: Vec<PatternElement>,
}

/// `SET item, …` — one or more property/variable assignments.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub items: Vec<SetItem>,
}

/// A single `SET` assignment: `target = value`.
#[derive(Debug, Clone, PartialEq)]
pub struct SetItem {
    pub target: SetTarget,
    pub value: Expr,
}

/// The left side of a `SET` assignment.
#[derive(Debug, Clone, PartialEq)]
pub enum SetTarget {
    /// `var.prop = …`.
    Property { var: String, name: String },
    /// `var = …` / `var += …` — set the whole node/rel's properties.
    Var(String),
}

/// `[DETACH] DELETE expr, …`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteClause {
    pub exprs: Vec<Expr>,
    pub detach: bool,
}

/// A path pattern: a head node and a chain of `(rel, node)` steps.
///
/// `name` is set for a *named path* `p = (…)-[…]->(…)` (the whole pattern is
/// bound to `p` as a `PATH`/`RECURSIVE_REL` value); `None` for an anonymous one.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternElement {
    pub name: Option<String>,
    pub head: NodePattern,
    pub chains: Vec<(RelPattern, NodePattern)>,
}

/// `(var:Label {k: v, …})`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub var: Option<String>,
    pub labels: Vec<String>,
    pub properties: Vec<(String, Expr)>,
}

/// Arrow direction of a relationship pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `<-[…]-` : right-to-left.
    Left,
    /// `-[…]->` : left-to-right.
    Right,
    /// `-[…]-` : undirected.
    Both,
}

/// `-[var:Label {k: v, …}]->` and its undirected/reverse forms.
#[derive(Debug, Clone, PartialEq)]
pub struct RelPattern {
    pub var: Option<String>,
    pub labels: Vec<String>,
    pub direction: Direction,
    pub properties: Vec<(String, Expr)>,
    /// `Some` for a variable-length / recursive relationship (`*`, `*1..3`,
    /// `* SHORTEST 1..5`, `* TRAIL 2..4`, …); `None` for a plain single hop.
    pub recursive: Option<RecursiveInfo>,
}

/// The path-uniqueness semantic of a recursive relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathSemantic {
    /// Any walk — nodes and relationships may repeat (the default).
    #[default]
    Walk,
    /// No relationship repeats along the path.
    Trail,
    /// No node repeats along the path.
    Acyclic,
}

/// The recursive search mode of a variable-length relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecursiveMode {
    /// Enumerate every path within the length bounds (the default).
    #[default]
    All,
    /// One (arbitrary) shortest path to each reachable destination.
    Shortest,
    /// All shortest paths to each reachable destination.
    AllShortest,
    /// Weighted shortest path (Dijkstra), minimizing the summed weight
    /// property — `* WSHORTEST(weightCol)`.
    WShortest,
    /// All weighted-shortest paths — `* ALL WSHORTEST(weightCol)`.
    AllWShortest,
}

/// The per-step lambda of a recursive relationship:
/// `(r, n | WHERE pred | {relProj, …}, {nodeProj, …})`. Any of the predicate /
/// projection parts may be absent. `rel_var`/`node_var` may be `_` (unused).
#[derive(Debug, Clone, PartialEq)]
pub struct RecursiveLambda {
    pub rel_var: String,
    pub node_var: String,
    /// `WHERE` predicate evaluated at each expansion step (over `rel_var`/`node_var`).
    pub predicate: Option<Expr>,
    /// Projected relationship-property expressions, present iff a projection
    /// clause was given (`Some(vec![])` for an explicit `{}`).
    pub rel_projection: Option<Vec<Expr>>,
    /// Projected intermediate-node-property expressions (parallel to `rel_projection`).
    pub node_projection: Option<Vec<Expr>>,
}

/// Variable-length / recursive metadata attached to a [`RelPattern`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecursiveInfo {
    /// `(lower, upper)` length bounds. `None` means unspecified — the binder
    /// defaults a missing lower to 1 and a missing upper to the max depth.
    pub bounds: (Option<u32>, Option<u32>),
    pub mode: RecursiveMode,
    pub semantic: PathSemantic,
    pub lambda: Option<RecursiveLambda>,
    /// The rel-property name that weights each edge for (ALL) WSHORTEST.
    pub weight_col: Option<String>,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

/// An (unbound) expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// An integer literal beyond `u128` (raw text, `-`-prefixed when negated):
    /// binds to the C++ Conversion cast error.
    OverflowInt(String),
    Variable(String),
    /// `expr.name`.
    Property {
        base: Box<Expr>,
        name: String,
    },
    Parameter(String),
    /// A scalar or aggregate function call (disambiguated in the binder).
    Function {
        name: String,
        distinct: bool,
        args: Vec<Expr>,
        /// Per-argument name for the `name := expr` call syntax (e.g.
        /// `union_value(a := 1)`, `struct_pack(x := 2)`). Parallel to `args`
        /// (`None` for a positional argument); an empty vector means all
        /// arguments are positional.
        arg_names: Vec<Option<String>>,
    },
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Xor(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Comparison {
        op: CmpOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Arithmetic {
        op: ArithOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Negate(Box<Expr>),
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    /// A list literal `[a, b, c]`.
    List(Vec<Expr>),
    /// A struct literal `{field: expr, …}` (the `struct_pack` form).
    Struct(Vec<(String, Expr)>),
    /// A lambda `x -> body` / `(x, y) -> body` (only as a higher-order-function arg).
    Lambda {
        params: Vec<String>,
        body: Box<Expr>,
    },
    /// A list comprehension `[var IN list [WHERE pred] [| projection]]`.
    ListComprehension {
        var: String,
        list: Box<Expr>,
        predicate: Option<Box<Expr>>,
        projection: Option<Box<Expr>>,
    },
    /// `CASE [operand] (WHEN cond THEN result)+ [ELSE result] END`. With an
    /// `operand` (simple CASE) each `cond` is compared to it for equality; without
    /// (searched CASE) each `cond` is a boolean predicate.
    Case {
        operand: Option<Box<Expr>>,
        when_thens: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    /// `*` — only valid inside `count(*)` and `RETURN *`.
    Star,
    /// A pattern comprehension `[(a)-[:R]->(b) | expr]` — parsed for C++
    /// parity (the binder validates the pattern's labels; evaluation is not
    /// yet implemented, matching every oracle-probed use erroring at bind).
    PatternComprehension {
        pattern: PatternElement,
        projection: Option<Box<Expr>>,
    },
    /// `EXISTS { MATCH … [WHERE …] }` (→ BOOL) or `COUNT { … }` (→ INT64): a
    /// correlated subquery over the enclosing scope.
    Subquery {
        kind: SubqueryKind,
        patterns: Vec<PatternElement>,
        where_clause: Option<Box<Expr>>,
    },
}

/// The two existential/count subquery forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubqueryKind {
    Exists,
    Count,
}

/// `RETURN [DISTINCT] items [ORDER BY …] [SKIP n] [LIMIT n]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub distinct: bool,
    pub items: Vec<ProjectionItem>,
    pub order_by: Vec<(Expr, bool)>, // (expr, is_ascending)
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}

/// One projection item.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionItem {
    /// `RETURN *` — all bound variables.
    Star,
    /// `RETURN a.*` — all properties of node/rel variable `a`.
    AllProperties(String),
    /// `RETURN a.state.*` — all FIELDS of a struct-typed expression, spread as
    /// `struct_extract(base, field)` columns.
    AllStructFields(Expr),
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}
