# Koko Front-End Spec for Rust Port (P0 Subset)

This spec captures the C++ Koko front-end shapes for the P0 subset (`CREATE NODE TABLE`, `CREATE REL TABLE`, `CREATE` pattern insertion, and `MATCH ... WHERE ... RETURN`) and recommends Rust-idiomatic equivalents. It deliberately omits recursive rels, UNION, multiple query parts (WITH), subqueries, table-group multi-pair rels, foreign/attached tables, and storage/scan concerns — but notes where the C++ model leaves seams that P0 must collapse.

Source references are paths under `/Users/dai/code/koko/src/include`.

---

## 1. DDL shapes (CREATE NODE/REL TABLE)

### Parser side (`parser/ddl/create_table.h`, `create_table_info.h`, `parsed_property_definition.h`)

`CreateTable` is a `Statement` wrapping a single `CreateTableInfo` (plus an optional `QueryScanSource` for `CREATE TABLE ... AS` — out of P0 scope, drop it).

`CreateTableInfo`:
- `TableType type` (`NODE` | `REL` | `REL_GROUP`)
- `std::string tableName`
- `std::vector<ParsedPropertyDefinition> propertyDefinitions`
- `std::unique_ptr<ExtraCreateTableInfo> extraInfo` (polymorphic; node vs rel)
- `ConflictAction onConflict` (`ON_CONFLICT_THROW` default; supports `IF NOT EXISTS`)

`ParsedPropertyDefinition`:
- `ParsedColumnDefinition columnDefinition { name: String, type: String }` — type is the **raw type string** (e.g. `"INT64"`, `"STRING"`); it is NOT resolved to a `LogicalType` until binding.
- `std::unique_ptr<ParsedExpression> defaultExpr` (optional DEFAULT clause)

Extra info subtypes:
- `ExtraCreateNodeTableInfo`: `std::string pKName` (primary-key **column name** — references one of the property definitions), `options_t options`.
- `ExtraCreateRelTableGroupInfo`: `std::string relMultiplicity`, `std::vector<std::pair<std::string,std::string>> srcDstTablePairs` (list of `(fromTableName, toTableName)` name pairs), `options_t options`. **For P0, a REL table has exactly one (from,to) pair.**

Note: there is no separate "RelTableInfo" struct — single-pair rels are modeled as a rel group with one pair. P0 can flatten this.

### Bound / catalog side (`binder/ddl/bound_create_table_info.h`, `binder/ddl/property_definition.h`)

After binding, `CreateTableInfo` → `BoundCreateTableInfo`:
- `CatalogEntryType type` (`NODE_TABLE_ENTRY` | `REL_GROUP_ENTRY`)
- `std::string tableName`
- `ConflictAction onConflict`
- `std::unique_ptr<BoundExtraCreateCatalogEntryInfo> extraInfo`
- `bool isInternal`, `bool hasParent` (drop for P0)

`BoundExtraCreateTableInfo` carries `std::vector<PropertyDefinition> propertyDefinitions`. Subtypes:
- `BoundExtraCreateNodeTableInfo`: + `std::string primaryKeyName` (still by name; the column/property IDs are assigned later when the catalog entry is built).
- `BoundExtraCreateRelTableGroupInfo`: + `srcMultiplicity`, `dstMultiplicity`, `storageDirection`, `std::vector<NodeTableIDPair> nodePairs` — here the **FROM/TO node table names have been resolved to `table_id_t`** via the catalog. P0: one `NodeTableIDPair { srcTableID, dstTableID }`.

`PropertyDefinition` (the bound form):
- `ColumnDefinition { name: String, type: LogicalType }` — type is now a resolved `LogicalType` (parsed string → `LogicalType` happens in `Binder::bindPropertyDefinitions`).
- `std::unique_ptr<ParsedExpression> defaultExpr`

**Minimal P0 fields to represent each:**
- Node table: `name`, `columns: [(name, LogicalType)]`, `primary_key_column_name`.
- Rel table: `name`, `from_table` (name→id), `to_table` (name→id), `columns: [(name, LogicalType)]`. (Multiplicity/direction can default to `MANY/MANY`, `BOTH`.)

---

## 2. Catalog data model for the property graph

### ID types (`common/types/types.h`)

```
oid_t        = u64;  INVALID_OID = u64::MAX
table_id_t   = oid_t  (i.e. u64);  INVALID_TABLE_ID = INVALID_OID
column_id_t  = u32
property_id_t= u32
idx_t        = u32
offset_t     = u64;  INVALID_OFFSET = u64::MAX
internalID_t { offset: offset_t (u64), tableID: table_id_t (u64) }   // a.k.a. nodeID_t / relID_t
```

A NODE's internal ID is exactly `internalID_t = (tableID, offset)` — `tableID` selects the node table, `offset` is the dense row position within that table. This is the system-level node identity and is surfaced as the implicit `_ID` property (`LogicalTypeID::INTERNAL_ID`). Rel IDs use the same `internalID_t` shape (`relID_t`).

### Table catalog entries (`catalog/catalog_entry/...`)

`TableCatalogEntry` (base):
- `table_id_t getTableID()` → backed by `oid` from the `CatalogEntry` base. Table IDs are assigned by the catalog (`Catalog::createTableEntry`) — they are monotonically allocated OIDs, not chosen by the user.
- `PropertyDefinitionCollection propertyCollection` — owns columns/properties and the column/property ID assignment.
- Lookups: `getPropertyID(name) -> property_id_t`, `getColumnID(name) -> column_id_t`, `getColumnID(idx)`, `getProperty(name|idx) -> PropertyDefinition`, `containsProperty`, `getMaxColumnID`.

`PropertyDefinitionCollection` (`catalog/property_definition_collection.h`) — the core of column/property ID assignment:
- `nextColumnID: column_id_t`, `nextPropertyID: property_id_t` — counters incremented on `add`.
- `definitions: map<property_id_t, PropertyDefinition>` (ordered by property_id).
- `columnIDs: map<property_id_t, column_id_t>` — the **property_id → column_id** mapping. They diverge because dropping a column frees a property slot but column IDs may be vacuumed separately (`vacuumColumnIDs`). For a freshly created P0 table with no drops, `property_id == column_id == insertion index`.
- `nameToPropertyIDMap: case_insensitive_map<property_id_t>` — name lookups are **case-insensitive**.
- Rel tables construct the collection with `PropertyDefinitionCollection{1}` — i.e. **column ID 0 is reserved for the NBR_NODE_ID column**, so user rel properties start at column ID 1.

`NodeTableCatalogEntry`:
- `getTableType() == NODE`
- `primaryKeyName: String`; `getPrimaryKeyID() -> property_id_t` (resolved via the collection); `getPrimaryKeyDefinition()`.

`RelGroupCatalogEntry`:
- `getTableType() == REL`
- `srcMultiplicity`, `dstMultiplicity` (`RelMultiplicity::ONE|MANY`), `storageDirection` (`ExtendDirection`).
- `relTableInfos: Vec<RelTableCatalogInfo>` where `RelTableCatalogInfo { nodePair: NodeTableIDPair{srcTableID,dstTableID}, oid }`. So FROM/TO node tables are stored as resolved `table_id_t` pairs; a rel group can hold multiple pairs (P0: exactly one).
- Helpers: `getSrcNodeTableIDSet()`, `getDstNodeTableIDSet()`, `getRelEntryInfo(src,dst)`.

`NodeTableIDPair { srcTableID: table_id_t, dstTableID: table_id_t }` (`catalog/catalog_entry/node_table_id_pair.h`).

### Catalog (`catalog/catalog.h`)

- `containsTable(name|id)`, `getTableCatalogEntry(name) -> *TableCatalogEntry`, `getTableCatalogEntry(id)`, `getNodeTableEntries()`, `getRelGroupEntries()`, `getTableEntries()`.
- `createTableEntry(txn, BoundCreateTableInfo) -> *CatalogEntry` — this is where the table OID and column/property IDs get materialized. P0 can ignore transactions entirely.

---

## 3. AST shape (parser-level)

### Query container (`parser/query/*`)

- `RegularQuery` = `Vec<SingleQuery>` + `Vec<bool> isUnionAll`. **P0: exactly one SingleQuery, no UNION.**
- `SingleQuery` = `Vec<QueryPart> queryParts` + `Vec<ReadingClause> readingClauses` + `Vec<UpdatingClause> updatingClauses` + `Option<ReturnClause>`. **P0: no `QueryPart`s (those are `WITH`-delimited segments).** A `CREATE` query is a SingleQuery with an `InsertClause` in `updatingClauses`; a `MATCH...RETURN` is reading clauses + return clause.
- `QueryPart` = readingClauses + updatingClauses + a `WithClause` (out of P0).

### Reading / updating clauses

- `ReadingClause` base: `ClauseType clauseType`, optional `wherePredicate: ParsedExpression`. **The WHERE predicate lives on the reading clause (the MATCH), not as a standalone clause.**
- `MatchClause : ReadingClause`: `Vec<PatternElement> patternElements`, `MatchClauseType matchClauseType` (`MATCH` | `OPTIONAL_MATCH`), optional join hint (drop). Inherits the WHERE predicate.
- `InsertClause : UpdatingClause` (`ClauseType::INSERT`): `Vec<PatternElement> patternElements`. This is what `CREATE (...)` parses to.

### Graph pattern (`parser/query/graph_pattern/*`)

- `PatternElement`: optional `pathName: String`, a head `NodePattern nodePattern`, and `Vec<PatternElementChain> patternElementChains`. A path `(a)-[r]->(b)-[s]->(c)` = head node `a` + chains `[(r,b),(s,c)]`.
- `PatternElementChain`: `RelPattern relPattern` + `NodePattern nodePattern` (the rel and the node it leads to).
- `NodePattern`: `variableName: String` (may be empty/anonymous), `tableNames: Vec<String>` (the labels — zero, one, or many), `propertyKeyVals: Vec<(String, ParsedExpression)>` (the `{ key: value }` inline map).
- `RelPattern : NodePattern` (so it has variable + labels + property map) plus:
  - `relType: QueryRelType` (`NON_RECURSIVE` for P0; recursive is out of scope)
  - `arrowDirection: ArrowDirection` (`LEFT` | `RIGHT` | `BOTH`)
  - `recursiveInfo` (drop for P0).

### Parsed expressions (`parser/expression/*`)

Base `ParsedExpression`: `ExpressionType type` + `alias: String` + `rawName: String` + `children: Vec<ParsedExpression>`. Subtypes carry extra payload:
- `ParsedLiteralExpression` (LITERAL): `value: Value`.
- `ParsedVariableExpression` (VARIABLE): `variableName: String`.
- `ParsedPropertyExpression` (PROPERTY): `propertyName: String`, single child = the variable; `isStar()` for `a.*`.
- `ParsedFunctionExpression` (FUNCTION — covers scalar, arithmetic, AND aggregates at parse time): `functionName: String`, `isDistinct: bool`, `optionalArguments`, children = args. **Comparison/arithmetic operators are parsed as functions** (e.g. `+`, `=` map to function names) OR as boolean/comparison `ExpressionType`s — see the enum: `OR/XOR/AND/NOT`, `EQUALS/NOT_EQUALS/GREATER_THAN/...`, `IS_NULL/IS_NOT_NULL` are first-class expression types built directly (children only, no subtype).
- `ParsedParameterExpression` (PARAMETER): `$name`.
- `ParsedCaseExpression`, `ParsedSubqueryExpression`, `ParsedLambdaExpression` — out of P0.

### Return / projection (`parser/query/return_with_clause/*`)

- `ReturnClause`: wraps a `ProjectionBody`.
- `ProjectionBody`: `isDistinct: bool`, `projectionExpressions: Vec<ParsedExpression>` (each may carry an `alias` via `ParsedExpression::alias`), `orderByExpressions` + `isAscOrders: Vec<bool>`, optional `skipExpression`, optional `limitExpression`. `RETURN *` is represented as a star projection.

---

## 4. Binder QueryGraph concept (`binder/query/query_graph.h`, `binder/binder.h`)

### Binding entry points
`Binder::bind(Statement)` dispatches by statement type. Relevant paths:
- DDL: `bindCreateTableInfo` → `bindCreateNodeTableInfo` / `bindCreateRelTableGroupInfo` → `BoundCreateTableInfo`.
- Query: `bindQuery` → `bindSingleQuery` → `bindQueryPart` → reading/updating/return binders.
- Pattern: `bindGraphPattern(Vec<PatternElement>) -> BoundGraphPattern { QueryGraphCollection, where }`. Internally `bindPatternElement` walks head node + chains, calling `bindQueryNode` and `bindQueryRel`.

### QueryGraph
A `QueryGraph` is one connected component of a MATCH/CREATE pattern:
- `queryNodes: Vec<Rc<NodeExpression>>` + `queryNodeNameToPosMap: Map<String, u32>`
- `queryRels: Vec<Rc<RelExpression>>` + `queryRelNameToPosMap: Map<String, u32>`
- Position-indexed; `MAX_NUM_QUERY_VARIABLES = 64` (bitsets used by the optimizer's `SubqueryGraph` — not needed for P0).
- `QueryGraphCollection` = `Vec<QueryGraph>` (multiple disconnected components in one MATCH). `BoundGraphPattern { queryGraphCollection, where: Rc<Expression> }`.
- `BoundMatchClause` holds a `QueryGraphCollection` + `MatchClauseType`. The WHERE predicate, after binding, becomes the `BoundGraphPattern.where` / projection-body predicate, not a field on the match clause.

### Node/Rel binding & label→tableID resolution
- `bindQueryNode`: from `NodePattern.tableNames` (labels) → `bindNodeTableEntries(names) -> Vec<*TableCatalogEntry>`. Empty labels = all node tables. A `NodeExpression` binds to a **vector of catalog entries** (multi-label); `isMultiLabeled() = entries.len() > 1`. **P0: require exactly one label per node, so one entry.**
- `bindQueryRel`: takes the bound `srcNode`/`dstNode` (left/right resolved by `ArrowDirection`), resolves rel labels via `bindRelGroupEntries`, builds a `RelExpression`. `createNonRecursiveQueryRel` is the P0 path.
- `RelExpression` records `srcNode`, `dstNode` (start/end if directed), `leftNode`/`rightNode` (positional), `directionType: RelDirectionType` (`SINGLE` if directed arrow, `BOTH` if undirected), `relType`.
- `QueryGraphLabelAnalyzer::pruneLabel` prunes impossible labels: for a rel, it intersects candidate rel tables against the src/dst node tables' allowed FROM/TO pairs (this is how `(a:Person)-[:Knows]->(b)` narrows `b` and the rel table). P0 can do a simpler direct check that the chosen rel table's `(srcTableID,dstTableID)` matches the node labels.

`getTableIDs()` on a `NodeOrRelExpression` derives `Vec<table_id_t>` from its entries — this is the bridge from pattern variable to physical table IDs.

---

## 5. Expression representation (binder-level) and DataType binding

### Class hierarchy (`binder/expression/*`)
Base `Expression`:
- `expressionType: ExpressionType`
- `dataType: LogicalType` (every bound expression has a resolved type)
- `uniqueName: String` (identity; equality/hash are by `uniqueName`)
- `alias: String`
- `children: Vec<Rc<Expression>>`

`ExpressionType` enum (`common/enums/expression_type.h`), relevant variants:
- Boolean: `OR, XOR, AND, NOT`
- Comparison: `EQUALS, NOT_EQUALS, GREATER_THAN, GREATER_THAN_EQUALS, LESS_THAN, LESS_THAN_EQUALS`
- Null: `IS_NULL, IS_NOT_NULL`
- `PROPERTY, LITERAL, STAR, VARIABLE, PATH, PATTERN, PARAMETER`
- `FUNCTION` (scalar + arithmetic after binding), `AGGREGATE_FUNCTION` (post-binding; at parse time aggregates are `FUNCTION`)
- `SUBQUERY, CASE_ELSE, GRAPH, LAMBDA` (out of P0)

Subtypes:
- `LiteralExpression` (LITERAL): `value: Value`. `dataType = value.getDataType()`.
- `PropertyExpression` (PROPERTY): `propertyName`, `uniqueVarName` (the bound variable), `rawVariableName`, and `infos: table_id_map<SingleLabelPropertyInfo{exists, isPrimaryKey}>` — i.e. per-table info because the same property name can have different existence/PK status across labels. `uniqueName = "<uniqueVarName>.<propertyName>"`. `isInternalID()` when propertyName == `_ID`.
- `ScalarFunctionExpression` (FUNCTION): holds a bound `ScalarFunction` + `FunctionBindData` (with `resultType`); children = args. Arithmetic (`+`,`-`, etc.) lands here.
- `AggregateFunctionExpression` (AGGREGATE_FUNCTION): bound `AggregateFunction` + bind data, `isDistinct()`.
- `ParameterExpression` (PARAMETER): `parameterName`, `value`; `uniqueName = "$name"`.
- `VariableExpression`, `NodeExpression`/`RelExpression` (PATTERN, see §4), `PathExpression`, `CaseExpression`, `SubqueryExpression`, `LambdaExpression`.

Comparison/boolean/null expressions are created generically (base `Expression` with the right `ExpressionType` + children), not as dedicated subclasses.

### Expression binding (`binder/expression_binder.h`)
`ExpressionBinder::bindExpression` dispatches on parsed `ExpressionType`:
- boolean → `bindBooleanExpression`, comparison → `bindComparisonExpression`, null → `bindNullOperatorExpression`, property → `bindPropertyExpression` (resolves the variable to a node/rel and the property name to a `PropertyExpression` with type from the catalog), function → `bindFunctionExpression` (splits scalar vs aggregate, resolves overloads, produces `resultType`), literal/parameter → direct.
- `foldExpression` constant-folds.
- WHERE: `Binder::bindWhereExpression`. Projection: `bindProjectionList` returns `(expression_vector, aliases)`; `bindProjectionBody` then splits into `groupByExpressions` / `aggregateExpressions` (group-by = projection exprs that aren't aggregates and don't contain them), binds ORDER BY / SKIP / LIMIT.

### DataType
`LogicalType` is identified by `LogicalTypeID` (`common/types/types.h`): `ANY, NODE, REL, RECURSIVE_REL, SERIAL, BOOL, INT8..INT128/UINT.., DOUBLE, FLOAT, DATE, TIMESTAMP[_*], INTERVAL, DECIMAL, INTERNAL_ID, STRING, BLOB, LIST, ARRAY, STRUCT, MAP, UNION, UUID, JSON`. P0 needs roughly: `BOOL, INT64, DOUBLE, STRING, DATE, TIMESTAMP, INTERNAL_ID, NODE, REL` and the DDL type-string parser mapping `"INT64" -> INT64`, etc. Nested/parameterized types (LIST/STRUCT/DECIMAL params) can be deferred.

### Bound query result shape
- `BoundProjectionBody` (`binder/query/return_with_clause/bound_projection_body.h`): `distinct`, `projectionExpressions`, `groupByExpressions`, `aggregateExpressions`, `orderByExpressions` + `isAscOrders`, `skipNumber`, `limitNumber` (all `Rc<Expression>`).
- `BoundReturnClause` wraps a `BoundProjectionBody` + `BoundStatementResult`.
- `NormalizedQueryPart`: `Vec<BoundReadingClause>`, `Vec<BoundUpdatingClause>`, `Option<BoundProjectionBody>`, `projectionBodyPredicate` (the post-WHERE filter expression).
- `BoundInsertInfo` (`binder/query/updating_clause/bound_insert_info.h`): `tableType`, `pattern: Rc<Expression>` (the node/rel expr), `columnExprs`, `columnDataExprs` (parallel: which column ← which value expr), `conflictAction`. This is the bound form of `CREATE (...)`.

---

## 6. Rust-idiomatic recommendation (P0)

Replace C++ `unique_ptr` ASTs with owned `Box`/`Vec` enums, and the `shared_ptr<Expression>` graph (which relies on `uniqueName`-based identity) with arena indices to keep bound expressions cheaply shareable and comparable without `Rc`.

### Shared ID newtypes
```rust
pub struct TableId(pub u64);      // INVALID = u64::MAX
pub struct ColumnId(pub u32);
pub struct PropertyId(pub u32);
pub struct Offset(pub u64);
pub struct InternalId { pub table: TableId, pub offset: Offset } // node/rel identity
```

### Parser AST (owned tree, `Box` for recursion)
```rust
pub enum Statement {
    CreateNodeTable(CreateNodeTable),
    CreateRelTable(CreateRelTable),
    Query(SingleQuery),            // covers CREATE-insert and MATCH...RETURN
}

pub struct CreateNodeTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,       // name + raw type string
    pub primary_key: String,           // column name
    pub if_not_exists: bool,
}
pub struct CreateRelTable {
    pub name: String,
    pub from_table: String,            // name; resolved to TableId in binder
    pub to_table: String,
    pub columns: Vec<ColumnDef>,
    pub if_not_exists: bool,
}
pub struct ColumnDef { pub name: String, pub type_name: String, pub default: Option<Expr> }

pub struct SingleQuery {
    pub reading: Vec<MatchClause>,     // P0: MATCH only
    pub updating: Vec<InsertClause>,   // P0: CREATE only
    pub ret: Option<ReturnClause>,
}

pub struct MatchClause { pub pattern: Vec<PatternElement>, pub optional: bool, pub where_: Option<Expr> }
pub struct InsertClause { pub pattern: Vec<PatternElement> }

pub struct PatternElement {
    pub head: NodePattern,
    pub chains: Vec<(RelPattern, NodePattern)>,
}
pub struct NodePattern {
    pub var: Option<String>,
    pub labels: Vec<String>,                 // P0 binder enforces len()==1 where required
    pub properties: Vec<(String, Expr)>,
}
pub enum Direction { Left, Right, Both }
pub struct RelPattern {
    pub var: Option<String>,
    pub labels: Vec<String>,
    pub direction: Direction,
    pub properties: Vec<(String, Expr)>,
}

pub enum Expr {
    Literal(Value),
    Variable(String),
    Property { var: Box<Expr>, name: String },   // child is usually Variable
    Parameter(String),
    Function { name: String, distinct: bool, args: Vec<Expr> }, // scalar/arith/aggregate pre-binding
    And(Vec<Expr>), Or(Vec<Expr>), Xor(Box<Expr>, Box<Expr>), Not(Box<Expr>),
    Comparison { op: CmpOp, lhs: Box<Expr>, rhs: Box<Expr> },
    IsNull(Box<Expr>), IsNotNull(Box<Expr>),
    Star,
}
pub enum CmpOp { Eq, Ne, Lt, Le, Gt, Ge }

pub struct ReturnClause {
    pub distinct: bool,
    pub items: Vec<ProjectionItem>,          // Star handled as a variant
    pub order_by: Vec<(Expr, bool /*asc*/)>,
    pub skip: Option<Expr>,
    pub limit: Option<Expr>,
}
pub enum ProjectionItem { Star, Expr { expr: Expr, alias: Option<String> } }
```
Mirror C++ by keeping the WHERE predicate on the `MatchClause`, and parsing arithmetic/comparison either as `Function` (scalar) or dedicated `Comparison`/boolean variants. Recommend dedicated variants for boolean/comparison/null (matches the C++ first-class `ExpressionType`s) and `Function` for everything else.

### Catalog
```rust
pub enum LogicalType {
    Bool, Int64, Double, String, Date, Timestamp,
    InternalId, Node(TableId), Rel(TableId),
    // extend as needed; keep a from_str for DDL type strings
}

pub struct Column { pub name: String, pub ty: LogicalType,
                    pub property_id: PropertyId, pub column_id: ColumnId,
                    pub default: Option<Expr> }

pub struct NodeTable {
    pub id: TableId,
    pub name: String,
    pub columns: Vec<Column>,          // ordered; index == property_id for fresh tables
    pub primary_key: PropertyId,
    name_to_idx: HashMap<String /*lowercased*/, usize>,
}
pub struct RelTable {
    pub id: TableId,
    pub name: String,
    pub from: TableId,
    pub to: TableId,
    pub columns: Vec<Column>,          // column_id starts at 1 (slot 0 = nbr id)
    name_to_idx: HashMap<String, usize>,
}

pub struct Catalog {
    node_tables: HashMap<TableId, NodeTable>,
    rel_tables: HashMap<TableId, RelTable>,
    name_to_id: HashMap<String /*lowercased*/, TableId>, // case-insensitive
    next_table_id: u64,
}
impl Catalog {
    pub fn create_node_table(&mut self, ...) -> TableId; // allocates id + column/property ids
    pub fn create_rel_table(&mut self, ...) -> TableId;
    pub fn table_by_name(&self, name: &str) -> Option<&TableEntry>;
    pub fn table_by_id(&self, id: TableId) -> Option<&TableEntry>;
}
```
Key fidelity points to preserve: case-insensitive name lookup; column/property ID counters allocated at table-creation time (not at parse); rel user columns starting at column ID 1; node identity `= (table_id, offset)`.

### Bound query + expression arena
The C++ `Expression` graph is a `shared_ptr` DAG keyed on `uniqueName`. In Rust, use an arena + index handles to get cheap sharing and `Eq`/`Hash` by handle:
```rust
pub struct ExprId(u32);
pub struct ExprArena { exprs: Vec<BoundExpr> }   // ExprId indexes here

pub struct BoundExpr {
    pub ty: LogicalType,
    pub alias: Option<String>,
    pub kind: BoundExprKind,
}
pub enum BoundExprKind {
    Literal(Value),
    Property { var: VarId, table_infos: Vec<(TableId, PropInfo)>, name: String },
    Parameter(String),
    ScalarFn { name: String, args: Vec<ExprId> },
    AggregateFn { name: String, distinct: bool, args: Vec<ExprId> },
    And(Vec<ExprId>), Or(Vec<ExprId>), Not(ExprId),
    Comparison { op: CmpOp, lhs: ExprId, rhs: ExprId },
    IsNull(ExprId), IsNotNull(ExprId),
    Variable(VarId),
}
pub struct PropInfo { pub exists: bool, pub is_primary_key: bool }
```

QueryGraph (bound): position-indexed like C++, with name→pos maps; node/rel variables become entries in a side table keyed by `VarId`.
```rust
pub struct VarId(u32);

pub struct BoundNode {
    pub var: VarId,
    pub name: Option<String>,
    pub table_ids: Vec<TableId>,         // P0: len()==1
    pub internal_id: ExprId,             // the _ID property expr
    pub properties: Vec<(String, ExprId)>,
}
pub enum RelDir { Single, Both }         // Single = had a directed arrow
pub struct BoundRel {
    pub var: VarId,
    pub name: Option<String>,
    pub table_ids: Vec<TableId>,
    pub src: VarId,                      // start node if directed
    pub dst: VarId,                      // end node if directed
    pub direction: RelDir,
    pub properties: Vec<(String, ExprId)>,
}
pub struct QueryGraph {
    pub nodes: Vec<BoundNode>,
    pub rels: Vec<BoundRel>,
    node_name_to_pos: HashMap<String, usize>,
    rel_name_to_pos: HashMap<String, usize>,
}
pub struct QueryGraphCollection { pub graphs: Vec<QueryGraph> } // connected components

pub enum BoundStatement {
    CreateTable(BoundCreateTable),
    Query(BoundQuery),
}
pub struct BoundQuery {
    pub arena: ExprArena,
    pub reading: Vec<BoundReading>,      // BoundMatch { graphs, where: Option<ExprId> }
    pub inserts: Vec<BoundInsert>,       // pattern var + (column, value-expr) pairs
    pub projection: Option<BoundProjectionBody>,
}
pub struct BoundProjectionBody {
    pub distinct: bool,
    pub projection: Vec<ExprId>,
    pub group_by: Vec<ExprId>,
    pub aggregates: Vec<ExprId>,
    pub order_by: Vec<(ExprId, bool)>,
    pub skip: Option<ExprId>,
    pub limit: Option<ExprId>,
}
```

Rationale: enums-with-data replace the C++ inheritance + RTTI casts; an `ExprArena` with `ExprId` handles replaces `shared_ptr<Expression>` and gives O(1) `Eq`/`Hash` (the C++ identity-by-`uniqueName` becomes identity-by-index, with a separate structural-equality pass only if needed for CSE); position-indexed `QueryGraph` with name maps mirrors C++ exactly so the optimizer port stays straightforward. Defer multi-label (`table_ids` len 1), rel groups (single `from`/`to`), WITH/QueryPart chaining, UNION, and recursive rels for P0.

Relevant source files: `/Users/dai/code/koko/src/include/parser/ddl/create_table_info.h`, `/Users/dai/code/koko/src/include/parser/ddl/parsed_property_definition.h`, `/Users/dai/code/koko/src/include/parser/query/{single_query.h,query_part.h}`, `/Users/dai/code/koko/src/include/parser/query/graph_pattern/{node_pattern.h,rel_pattern.h,pattern_element.h,pattern_element_chain.h}`, `/Users/dai/code/koko/src/include/parser/query/reading_clause/{reading_clause.h,match_clause.h}`, `/Users/dai/code/koko/src/include/parser/query/updating_clause/insert_clause.h`, `/Users/dai/code/koko/src/include/parser/query/return_with_clause/{projection_body.h,return_clause.h}`, `/Users/dai/code/koko/src/include/parser/expression/parsed_*.h`, `/Users/dai/code/koko/src/include/common/enums/expression_type.h`, `/Users/dai/code/koko/src/include/binder/binder.h`, `/Users/dai/code/koko/src/include/binder/expression/{expression.h,node_rel_expression.h,node_expression.h,rel_expression.h,property_expression.h,literal_expression.h,parameter_expression.h,scalar_function_expression.h,aggregate_function_expression.h}`, `/Users/dai/code/koko/src/include/binder/query/{query_graph.h,query_graph_label_analyzer.h,normalized_query_part.h}`, `/Users/dai/code/koko/src/include/binder/query/return_with_clause/{bound_projection_body.h,bound_return_clause.h}`, `/Users/dai/code/koko/src/include/binder/query/reading_clause/bound_match_clause.h`, `/Users/dai/code/koko/src/include/binder/query/updating_clause/bound_insert_info.h`, `/Users/dai/code/koko/src/include/binder/ddl/{bound_create_table_info.h,property_definition.h}`, `/Users/dai/code/koko/src/include/catalog/{catalog.h,property_definition_collection.h}`, `/Users/dai/code/koko/src/include/catalog/catalog_entry/{table_catalog_entry.h,node_table_catalog_entry.h,rel_group_catalog_entry.h,node_table_id_pair.h}`, `/Users/dai/code/koko/src/include/common/types/types.h`.