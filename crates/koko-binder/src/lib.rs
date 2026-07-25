//! `koko-binder` — name/type resolution and query-graph construction.
//!
//! Turns the parser's [`ast`](koko_parser::ast) into a [`BoundStatement`]:
//! resolves labels to tables, variables to [`VarId`]s, properties to typed
//! columns, and functions to [`ScalarOp`]/[`AggOp`]. Inline pattern properties
//! in `MATCH` become equality predicates; in `CREATE` they become column
//! values. A query is bound part-by-part (`WITH` boundaries) and operand-by-
//! operand (`UNION`); `OPTIONAL MATCH` becomes a left-join block; `EXISTS`/`COUNT`
//! subqueries are lifted into per-row result columns; `$name` parameters are
//! substituted from the provided values. Deliberately unsupported constructs surface as categorized
//! `Binder`/`NotImplemented` errors rather than reaching execution.

mod bound;

pub use bound::*;

use koko_catalog::{Catalog, RelStorageDirection};
use koko_common::{
    Error, LogicalType, Result, TableId, Value,
    file_resolver::{FileFormat, FileResolverConfig, resolve_files},
};
use koko_function::{AggOp, ScalarOp};
use koko_parser::{ast, expr_to_cypher};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Clone)]
struct PreparedParameterEnv {
    types: Rc<std::cell::RefCell<HashMap<String, LogicalType>>>,
    error: Rc<std::cell::RefCell<Option<Error>>>,
}

/// A preparation-only binding plus the binder-inferred type of each symbolic parameter.
pub struct PreparedBinding {
    pub statement: BoundStatement,
    pub parameter_types: HashMap<String, LogicalType>,
}

fn bind_storage_direction(value: Option<&str>) -> Result<RelStorageDirection> {
    match value {
        None => Ok(RelStorageDirection::Both),
        Some(s) if s.eq_ignore_ascii_case("FWD") => Ok(RelStorageDirection::Fwd),
        Some(s) if s.eq_ignore_ascii_case("BWD") => Ok(RelStorageDirection::Bwd),
        Some(s) if s.eq_ignore_ascii_case("BOTH") => Ok(RelStorageDirection::Both),
        Some(s) => Err(Error::runtime(format!(
            "Cannot parse {s} as ExtendDirection."
        ))),
    }
}

fn column_type_text(type_name: &str, ty: &LogicalType) -> String {
    if type_name.eq_ignore_ascii_case("SERIAL") {
        "SERIAL".to_string()
    } else {
        ty.to_string()
    }
}

fn default_expr_text(default: Option<&ast::Expr>, serial: bool) -> String {
    match default {
        Some(expr) => expr_to_cypher(expr),
        None if serial => String::new(),
        None => "NULL".to_string(),
    }
}

fn column_metadata(
    cols: &[ast::ColumnDef],
    bound: &[(String, LogicalType)],
    serial_columns: &[usize],
) -> Vec<BoundColumnMetadata> {
    cols.iter()
        .zip(bound)
        .enumerate()
        .map(|(i, (c, (_, ty)))| BoundColumnMetadata {
            type_text: column_type_text(&c.type_name, ty),
            default_text: default_expr_text(c.default.as_ref(), serial_columns.contains(&i)),
        })
        .collect()
}

/// Bind a parsed statement against the catalog, substituting concrete parameter values.
pub fn bind_statement(
    catalog: &Catalog,
    stmt: &ast::Statement,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
) -> Result<BoundStatement> {
    bind_statement_with_env(catalog, stmt, params, config, None)
}

/// Bind for preparation with symbolic parameters. `initial_types` contains types
/// supplied to `prepare_with_params`; absent entries remain `Any` until constrained
/// by the regular binder's assignment, comparison, function, or boolean rules.
pub fn bind_statement_for_prepare(
    catalog: &Catalog,
    stmt: &ast::Statement,
    params: &[String],
    initial_types: &HashMap<String, LogicalType>,
    config: &SessionConfig,
) -> Result<PreparedBinding> {
    let mut types = HashMap::new();
    for name in params {
        types.insert(
            name.clone(),
            initial_types.get(name).cloned().unwrap_or(LogicalType::Any),
        );
    }
    let env = PreparedParameterEnv {
        types: Rc::new(std::cell::RefCell::new(types)),
        error: Rc::new(std::cell::RefCell::new(None)),
    };
    let values = HashMap::new();
    let statement = bind_statement_with_env(catalog, stmt, &values, config, Some(env.clone()))?;
    if let Some(error) = env.error.borrow_mut().take() {
        return Err(error);
    }
    let parameter_types = env.types.borrow().clone();
    Ok(PreparedBinding {
        statement,
        parameter_types,
    })
}

fn bind_statement_with_env(
    catalog: &Catalog,
    stmt: &ast::Statement,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
    env: Option<PreparedParameterEnv>,
) -> Result<BoundStatement> {
    match stmt {
        ast::Statement::Query(query) => bind_regular_query(catalog, query, params, config, env),
        ast::Statement::CreateTableAs(create) => {
            bind_create_table_as(catalog, create, params, config, env)
        }
        other => Binder::with_env(catalog, params, config, env).bind_nonquery(other),
    }
}

/// Bind `CREATE … AS <query>`: bind the inner query, infer its result schema (the
/// first column is the primary key for a node table), and validate.
fn bind_create_table_as(
    catalog: &Catalog,
    c: &ast::CreateTableAs,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
    env: Option<PreparedParameterEnv>,
) -> Result<BoundStatement> {
    let BoundStatement::Query(query) = bind_regular_query(catalog, &c.query, params, config, env)?
    else {
        unreachable!("bind_regular_query yields a Query");
    };
    let columns = query
        .operands
        .first()
        .map(|op| op.result_columns())
        .unwrap_or_default();
    if columns.is_empty() {
        return Err(Error::binder("Subquery returns no columns".to_string()));
    }
    if c.is_node {
        // The first result column becomes the primary key.
        let pk_ty = &columns[0].1;
        if !(pk_ty.is_numeric() || matches!(pk_ty, LogicalType::String)) {
            return Err(Error::binder(format!(
                "Invalid primary key column type {pk_ty}. Primary keys must be either STRING or a \
                 numeric type."
            )));
        }
    } else if c.pairs.len() != 1 {
        return Err(Error::binder(
            "Multiple FROM/TO pairs are not supported for CREATE REL TABLE AS.".to_string(),
        ));
    }
    let resolve = |n: &str| {
        catalog
            .node_table_by_name(n)
            .map(|nt| nt.id)
            .ok_or_else(|| Error::binder(format!("Table {n} does not exist.")))
    };
    let pairs = c
        .pairs
        .iter()
        .map(|(f, t)| Ok((resolve(f)?, resolve(t)?)))
        .collect::<Result<Vec<_>>>()?;
    let storage_direction = if c.is_node {
        RelStorageDirection::Both
    } else {
        bind_storage_direction(c.storage_direction.as_deref())?
    };
    // The duplicate-name check runs last (after the inner query and endpoints are
    // bound), so a malformed-and-duplicate CTAS reports its malformed error first.
    if !c.if_not_exists && catalog.contains_table(&c.name) {
        return Err(Error::binder(format!(
            "{} already exists in catalog.",
            c.name
        )));
    }
    Ok(BoundStatement::CreateTableAs {
        name: c.name.clone(),
        is_node: c.is_node,
        pairs,
        storage_direction,
        if_not_exists: c.if_not_exists,
        columns,
        query,
    })
}

/// Bind a `UNION`/`UNION ALL` query: each operand binds in its own variable
/// namespace, then the operands are validated for compatibility.
fn bind_regular_query(
    catalog: &Catalog,
    rq: &ast::RegularQuery,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
    env: Option<PreparedParameterEnv>,
) -> Result<BoundStatement> {
    let operands = rq
        .singles
        .iter()
        .map(|single| Binder::with_env(catalog, params, config, env.clone()).bind_query(single))
        .collect::<Result<Vec<_>>>()?;

    if operands.len() > 1 {
        // Mixing UNION and UNION ALL in one query is forbidden (either all
        // boundaries are UNION ALL or none are).
        let any_all = rq.union_all.iter().any(|&a| a);
        let all_all = rq.union_all.iter().all(|&a| a);
        if any_all && !all_all {
            return Err(Error::binder(
                "Union and union all can not be used together.".to_string(),
            ));
        }
        // Operands must agree on column count and (positionally) on type. Column
        // names are not checked — the result inherits the first operand's names.
        let cols0 = operands[0].result_columns();
        for op in &operands[1..] {
            let cols = op.result_columns();
            if cols.len() != cols0.len() {
                return Err(Error::binder(
                    "The number of columns to union/union all must be the same.".to_string(),
                ));
            }
            for ((name, ty), (_, ty0)) in cols.iter().zip(&cols0) {
                if !union_compatible(ty, ty0) {
                    return Err(Error::binder(format!(
                        "{name} has data type {ty} but {ty0} was expected."
                    )));
                }
            }
        }
    }

    // Plain UNION (no ALL boundary) deduplicates; UNION ALL and a lone operand do not.
    let distinct = !rq.union_all.is_empty() && rq.union_all.iter().all(|&a| !a);
    Ok(BoundStatement::Query(Box::new(BoundRegularQuery {
        operands,
        distinct,
    })))
}

/// Whether two `UNION` column types are compatible: identical, or one is an
/// untyped NULL (`Any`). Matches the C++ exact-type rule (no implicit unification).
fn union_compatible(a: &LogicalType, b: &LogicalType) -> bool {
    a == b || *a == LogicalType::Any || *b == LogicalType::Any
}

impl From<ast::TableFunc> for BoundTableFunc {
    fn from(f: ast::TableFunc) -> Self {
        match f {
            ast::TableFunc::ShowTables => BoundTableFunc::ShowTables,
            ast::TableFunc::ShowSequences => BoundTableFunc::ShowSequences,
            ast::TableFunc::TableInfo => BoundTableFunc::TableInfo,
            ast::TableFunc::ShowMacros => BoundTableFunc::ShowMacros,
            ast::TableFunc::ShowFunctions => BoundTableFunc::ShowFunctions,
            ast::TableFunc::DbVersion => BoundTableFunc::DbVersion,
            ast::TableFunc::ShowOfficialExtensions => BoundTableFunc::ShowOfficialExtensions,
            ast::TableFunc::CacheArrayColumn => BoundTableFunc::CacheArrayColumn,
            ast::TableFunc::ClearWarnings => BoundTableFunc::ClearWarnings,
            ast::TableFunc::ShowIndexes => BoundTableFunc::ShowIndexes,
            ast::TableFunc::ShowWarnings => BoundTableFunc::ShowWarnings,
            ast::TableFunc::ShowConnection => BoundTableFunc::ShowConnection,
            ast::TableFunc::StorageInfo => BoundTableFunc::StorageInfo,
            ast::TableFunc::StatsInfo => BoundTableFunc::StatsInfo,
            ast::TableFunc::CurrentSetting => BoundTableFunc::CurrentSetting,
            ast::TableFunc::BmInfo => BoundTableFunc::BmInfo,
            ast::TableFunc::ShowLoadedExtensions => BoundTableFunc::ShowLoadedExtensions,
        }
    }
}

/// Map a parser table function to its bound mirror (the bound tree's own copy,
/// since the planner/processor don't depend on `koko-parser`).
fn bound_table_func(f: ast::TableFunc) -> BoundTableFunc {
    f.into()
}

/// The canonical UPPERCASE name of a table function (C++ error rendering).
fn table_func_display_name(f: ast::TableFunc) -> &'static str {
    match f {
        ast::TableFunc::ShowTables => "SHOW_TABLES",
        ast::TableFunc::ShowSequences => "SHOW_SEQUENCES",
        ast::TableFunc::TableInfo => "TABLE_INFO",
        ast::TableFunc::ShowMacros => "SHOW_MACROS",
        ast::TableFunc::ShowFunctions => "SHOW_FUNCTIONS",
        ast::TableFunc::DbVersion => "DB_VERSION",
        ast::TableFunc::ShowOfficialExtensions => "SHOW_OFFICIAL_EXTENSIONS",
        ast::TableFunc::CacheArrayColumn => "_CACHE_ARRAY_COLUMN_LOCALLY",
        ast::TableFunc::ClearWarnings => "CLEAR_WARNINGS",
        ast::TableFunc::ShowIndexes => "SHOW_INDEXES",
        ast::TableFunc::ShowWarnings => "SHOW_WARNINGS",
        ast::TableFunc::ShowConnection => "SHOW_CONNECTION",
        ast::TableFunc::StorageInfo => "STORAGE_INFO",
        ast::TableFunc::StatsInfo => "STATS_INFO",
        ast::TableFunc::CurrentSetting => "CURRENT_SETTING",
        ast::TableFunc::BmInfo => "BM_INFO",
        ast::TableFunc::ShowLoadedExtensions => "SHOW_LOADED_EXTENSIONS",
    }
}

/// The table function of a source query that is exactly one table-function
/// scan (`COPY t FROM SHOW_OFFICIAL_EXTENSIONS()`), else `None`.
fn table_func_source_name(q: &ast::RegularQuery) -> Option<&'static str> {
    if !q.union_all.is_empty() {
        return None;
    }
    let single = match q.singles.as_slice() {
        [only] => only,
        _ => return None,
    };
    if !single.parts.is_empty() || !single.updating.is_empty() {
        return None;
    }
    match single.reading.as_slice() {
        [ast::ReadingClause::TableFuncScan(tf)] => Some(table_func_display_name(tf.func)),
        _ => None,
    }
}

const TABLE_FUNC_DB: &str = "main(graph)";

/// The output-column schema (`(name, type)` in column order) a catalog table
/// function produces. Mirrors the headers emitted by [`table_func_rows`] exactly,
/// so the in-query scan can register one scalar variable per produced column.
/// Bind a standalone-`CALL` config value against the option's declared input type
/// (C++ `bindStandaloneCall`): a floating-point value never implicitly casts into
/// an integral option (a bespoke check ahead of the general gate — the numeric
/// catch-all in `hasImplicitCast` would otherwise admit it), then the assignment
/// implicit-cast gate applies. Returns the bound expr coerced to `dst`; the caller
/// constant-folds it (out-of-range values overflow there, e.g. `timeout=-1`).
pub fn bind_config_value(
    catalog: &Catalog,
    expr: &ast::Expr,
    config: &SessionConfig,
    dst: &LogicalType,
) -> Result<BoundExpr> {
    let params = HashMap::new();
    let bound = Binder::new(catalog, &params, config).bind_expr(expr)?;
    let src = bound.ty();
    let integral = matches!(dst, LogicalType::Int(_) | LogicalType::UInt128);
    // C++ `isFloatingPoint` = {DOUBLE, FLOAT, SERIAL, DECIMAL}; the message renders
    // the ID-level type name (bare DECIMAL, without precision/scale).
    let float_like = matches!(
        src,
        LogicalType::Float | LogicalType::Double | LogicalType::Serial | LogicalType::Decimal(_, _)
    );
    if float_like && integral {
        let src_name = match &src {
            LogicalType::Decimal(_, _) => "DECIMAL".to_string(),
            other => other.name(),
        };
        return Err(Error::binder(format!(
            "Expression {} has data type {} but expected {}. Implicit cast is not supported.",
            expr_name(expr),
            src_name,
            dst.name()
        )));
    }
    assignable_or_err(&bound, dst, &expr_name(expr))?;
    Ok(coerce_to(bound, dst))
}

/// Runtime-owned state needed by table functions whose rows are not catalog
/// metadata. The binder owns schemas; the executing connection supplies rows.
pub trait TableFuncRuntime: Sync {
    fn current_setting(&self, key: &str) -> Value;
    fn warning_rows(&self) -> Vec<Vec<Value>>;
    fn show_table_rows(&self) -> Option<Vec<Vec<Value>>> {
        None
    }
    fn clear_warnings(&self);
    fn macro_rows(&self) -> Vec<Vec<Value>>;
    fn memory_usage(&self) -> koko_common::MemoryUsage;
    fn table_stats(&self, _table_id: TableId) -> Option<koko_common::TableStats> {
        None
    }
}

pub fn table_func_schema(
    catalog: &Catalog,
    func: BoundTableFunc,
    arg: Option<&str>,
    extra_args: &[String],
) -> Result<Vec<(String, LogicalType)>> {
    let s = str::to_string;
    let schema = match func {
        BoundTableFunc::ShowTables => vec![
            (s("id"), LogicalType::Int64),
            (s("name"), LogicalType::String),
            (s("type"), LogicalType::String),
            (s("database name"), LogicalType::String),
            (s("comment"), LogicalType::String),
        ],
        BoundTableFunc::ShowSequences => vec![
            (s("name"), LogicalType::String),
            (s("database name"), LogicalType::String),
            (s("start value"), LogicalType::Int64),
            (s("increment"), LogicalType::Int64),
            (s("min value"), LogicalType::Int64),
            (s("max value"), LogicalType::Int64),
            (s("cycle"), LogicalType::Bool),
        ],
        BoundTableFunc::TableInfo => {
            let id = table_info_target(catalog, arg)?;
            let mut schema = vec![
                (s("property id"), LogicalType::Int64),
                (s("name"), LogicalType::String),
                (s("type"), LogicalType::String),
                (s("default expression"), LogicalType::String),
            ];
            // A node table's last column is the primary-key flag; a rel table's is
            // the storage direction (matching `table_func_rows`).
            if catalog.node_table(id).is_some() {
                schema.push((s("primary key"), LogicalType::Bool));
            } else {
                schema.push((s("storage_direction"), LogicalType::String));
            }
            schema
        }
        BoundTableFunc::ShowMacros => vec![
            (s("name"), LogicalType::String),
            (s("definition"), LogicalType::String),
        ],
        BoundTableFunc::ShowFunctions => vec![
            (s("name"), LogicalType::String),
            (s("type"), LogicalType::String),
            (s("signature"), LogicalType::String),
        ],
        BoundTableFunc::DbVersion => vec![(s("version"), LogicalType::String)],
        BoundTableFunc::ShowOfficialExtensions => vec![
            (s("name"), LogicalType::String),
            (s("description"), LogicalType::String),
        ],
        // Internal cache hint: validates its (table, ARRAY column) arguments
        // and yields nothing.
        BoundTableFunc::ClearWarnings => Vec::new(),
        BoundTableFunc::CacheArrayColumn => {
            let table = arg.unwrap_or_default();
            let column = extra_args.first().map(String::as_str).unwrap_or_default();
            let id = catalog
                .table_id(table)
                .ok_or_else(|| Error::binder(format!("Table {table} does not exist!")))?;
            let col_ty = catalog
                .node_table(id)
                .and_then(|nt| nt.column(column).map(|c| c.ty.clone()))
                .ok_or_else(|| {
                    Error::binder(format!("Column {column} does not exist in table {table}."))
                })?;
            if !matches!(col_ty, LogicalType::Array(_, _)) {
                return Err(Error::binder(format!(
                    "Column {column} is not of the expected type ARRAY."
                )));
            }
            Vec::new()
        }
        BoundTableFunc::ShowIndexes => vec![
            (s("table_name"), LogicalType::String),
            (s("index_name"), LogicalType::String),
            (s("index_type"), LogicalType::String),
            (
                s("property_names"),
                LogicalType::List(Box::new(LogicalType::String)),
            ),
            (s("extension_loaded"), LogicalType::Bool),
            (s("index_definition"), LogicalType::String),
        ],
        BoundTableFunc::ShowWarnings => vec![
            (s("query_id"), LogicalType::Int(koko_common::IntKind::U64)),
            (s("message"), LogicalType::String),
            (s("file_path"), LogicalType::String),
            (
                s("line_number"),
                LogicalType::Int(koko_common::IntKind::U64),
            ),
            (s("skipped_line_or_record"), LogicalType::String),
        ],
        BoundTableFunc::ShowConnection => {
            show_connection_target(catalog, arg)?;
            vec![
                (s("source table name"), LogicalType::String),
                (s("destination table name"), LogicalType::String),
                (s("source table primary key"), LogicalType::String),
                (s("destination table primary key"), LogicalType::String),
            ]
        }
        BoundTableFunc::StorageInfo => {
            existing_table_target(catalog, arg)?;
            [
                ("table_type", LogicalType::String),
                ("node_group_id", LogicalType::Int64),
                ("node_chunk_id", LogicalType::Int64),
                ("residency", LogicalType::String),
                ("column_name", LogicalType::String),
                ("data_type", LogicalType::String),
                ("start_page_idx", LogicalType::Int64),
                ("num_pages", LogicalType::Int64),
                ("num_values", LogicalType::Int64),
                ("min", LogicalType::String),
                ("max", LogicalType::String),
                ("compression", LogicalType::String),
            ]
            .into_iter()
            .map(|(n, t)| (s(n), t))
            .collect()
        }
        BoundTableFunc::StatsInfo => {
            let id = existing_table_target(catalog, arg)?;
            let table = catalog.node_table(id).ok_or_else(|| {
                Error::binder(format!(
                    "Stats from a non-node table {} is not supported yet!",
                    arg.unwrap_or_default()
                ))
            })?;
            let mut schema = Vec::with_capacity(table.columns.len() + 1);
            schema.push((s("cardinality"), LogicalType::Int64));
            schema.extend(table.columns.iter().map(|property| {
                (
                    format!("{}_distinct_count", property.name),
                    LogicalType::Int64,
                )
            }));
            schema
        }
        // One column NAMED BY THE KEY, one row: the setting's value.
        BoundTableFunc::CurrentSetting => {
            vec![(arg.unwrap_or_default().to_string(), LogicalType::String)]
        }
        BoundTableFunc::BmInfo => vec![
            (s("mem_limit"), LogicalType::Int(koko_common::IntKind::U64)),
            (s("mem_usage"), LogicalType::Int(koko_common::IntKind::U64)),
        ],
        BoundTableFunc::ShowLoadedExtensions => vec![
            (s("extension name"), LogicalType::String),
            (s("extension source"), LogicalType::String),
            (s("extension path"), LogicalType::String),
        ],
    };
    Ok(schema)
}

/// `show_connection` accepts only a REL table name — anything else (a node
/// table, or a missing table) is the verbatim C++ binder error.
fn show_connection_target(catalog: &Catalog, arg: Option<&str>) -> Result<TableId> {
    arg.and_then(|name| catalog.table_id(name))
        .filter(|id| catalog.rel_table(*id).is_some())
        .ok_or_else(|| {
            Error::binder("Show connection can only be called on a rel table!".to_string())
        })
}

/// `storage_info` / `stats_info` require an existing table.
fn existing_table_target(catalog: &Catalog, arg: Option<&str>) -> Result<TableId> {
    let name = arg.unwrap_or_default();
    catalog
        .table_id(name)
        .ok_or_else(|| Error::binder(format!("Table {name} does not exist!")))
}

/// Produce a table function's rows from catalog metadata plus the explicit
/// executing connection/database context.
pub fn table_func_rows(
    catalog: &Catalog,
    func: BoundTableFunc,
    arg: Option<&str>,
    runtime: &dyn TableFuncRuntime,
) -> Result<Vec<Vec<Value>>> {
    table_func_rows_inner(catalog, func, arg, runtime)
}

fn table_func_rows_inner(
    catalog: &Catalog,
    func: BoundTableFunc,
    arg: Option<&str>,
    runtime: &dyn TableFuncRuntime,
) -> Result<Vec<Vec<Value>>> {
    let rows = match func {
        BoundTableFunc::ShowSequences => catalog
            .sequences_sorted()
            .iter()
            .map(|seq| {
                vec![
                    Value::String(seq.name.clone()),
                    Value::String(TABLE_FUNC_DB.to_string()),
                    Value::Int64(seq.display_val()),
                    Value::Int64(seq.increment),
                    Value::Int64(seq.min),
                    Value::Int64(seq.max),
                    Value::Bool(seq.cycle),
                ]
            })
            .collect(),
        BoundTableFunc::ShowTables => {
            if let Some(rows) = runtime.show_table_rows() {
                return Ok(rows);
            }
            let mut rows = Vec::new();
            for id in catalog.node_table_ids() {
                let name = catalog.node_table(id).expect("listed id").name.clone();
                rows.push(table_func_table_row(
                    id.0,
                    &name,
                    "NODE",
                    catalog.table_comment(id),
                ));
            }
            for id in catalog.rel_table_ids() {
                let rel = catalog.rel_table(id).expect("listed id");
                let name = rel.name.clone();
                // `SHOW_TABLES` shows the rel *group* id, which the engine allocates
                // after the per-pair ids: group = primary id + pairs.
                let group_id = id.0 + rel.pairs.len() as u64;
                rows.push(table_func_table_row(
                    group_id,
                    &name,
                    "REL",
                    catalog.table_comment(id),
                ));
            }
            rows
        }
        BoundTableFunc::TableInfo => {
            let id = table_info_target(catalog, arg)?;
            if let Some(t) = catalog.node_table(id) {
                t.columns
                    .iter()
                    .map(|c| {
                        let is_pk = c.column_id.0 as usize == t.primary_key;
                        vec![
                            Value::Int64(c.column_id.0 as i64),
                            Value::String(c.name.clone()),
                            Value::String(c.type_text.clone()),
                            Value::String(c.default_text.clone()),
                            Value::Bool(is_pk),
                        ]
                    })
                    .collect()
            } else {
                let t = catalog
                    .rel_table(id)
                    .expect("table_info_target resolved a catalog table");
                t.columns
                    .iter()
                    .map(|c| {
                        vec![
                            // Rel property ids start at 1: index 0 is the internal
                            // NBR_NODE_ID column.
                            Value::Int64(c.column_id.0 as i64 + 1),
                            Value::String(c.name.clone()),
                            Value::String(c.type_text.clone()),
                            Value::String(c.default_text.clone()),
                            Value::String(t.storage_direction.as_str().to_string()),
                        ]
                    })
                    .collect()
            }
        }
        BoundTableFunc::ShowMacros => runtime.macro_rows(),
        // The full oracle catalog, verbatim and in its (stable) iteration order —
        // one row per overload, including functions this engine does not (yet)
        // implement: `show_functions` reports the *contract*, not the coverage.
        BoundTableFunc::ShowFunctions => koko_function::catalog_data::FUNCTION_CATALOG
            .iter()
            .map(|(name, kind, sig)| {
                vec![
                    Value::String((*name).to_string()),
                    Value::String((*kind).to_string()),
                    Value::String((*sig).to_string()),
                ]
            })
            .collect(),
        BoundTableFunc::DbVersion => {
            vec![vec![Value::String(TABLE_FUNC_DB_VERSION.to_string())]]
        }
        BoundTableFunc::CacheArrayColumn => Vec::new(),
        BoundTableFunc::ClearWarnings => {
            runtime.clear_warnings();
            Vec::new()
        }
        BoundTableFunc::ShowOfficialExtensions => OFFICIAL_EXTENSIONS
            .iter()
            .map(|(n, d)| {
                vec![
                    Value::String((*n).to_string()),
                    Value::String((*d).to_string()),
                ]
            })
            .collect(),
        BoundTableFunc::ShowIndexes => catalog
            .indexes()
            .into_iter()
            .map(|index| {
                let table_name = catalog
                    .node_table(index.table_id)
                    .expect("index table exists")
                    .name
                    .clone();
                let properties = index
                    .property_names
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect();
                let property_definition = index
                    .property_names
                    .iter()
                    .map(|name| format!("n.`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                vec![
                    Value::String(table_name.clone()),
                    Value::String(index.name.clone()),
                    Value::String(index.index_type.name().to_string()),
                    Value::List(properties),
                    Value::Bool(true),
                    Value::String(format!(
                        "CREATE {} INDEX `{}` FOR (n:`{}`) ON ({});",
                        index.index_type.name(),
                        index.name,
                        table_name,
                        property_definition
                    )),
                ]
            })
            .collect(),
        BoundTableFunc::ShowWarnings => runtime.warning_rows(),
        BoundTableFunc::ShowConnection => {
            let id = show_connection_target(catalog, arg)?;
            let rel = catalog.rel_table(id).expect("validated rel table");
            let pk_of = |tid: TableId| {
                catalog
                    .node_table(tid)
                    .map(|nt| nt.columns[nt.primary_key].name.clone())
                    .unwrap_or_default()
            };
            rel.pairs
                .iter()
                .map(|&(from, to)| {
                    vec![
                        Value::String(
                            catalog
                                .node_table(from)
                                .map(|t| t.name.clone())
                                .unwrap_or_default(),
                        ),
                        Value::String(
                            catalog
                                .node_table(to)
                                .map(|t| t.name.clone())
                                .unwrap_or_default(),
                        ),
                        Value::String(pk_of(from)),
                        Value::String(pk_of(to)),
                    ]
                })
                .collect()
        }
        // Physical native-storage internals are not modeled by the in-memory engine, so this
        // returns zero rows. Native durability/physical introspection is permanently deferred.
        BoundTableFunc::StorageInfo => {
            existing_table_target(catalog, arg)?;
            Vec::new()
        }
        BoundTableFunc::StatsInfo => {
            let id = existing_table_target(catalog, arg)?;
            let table = catalog.node_table(id).ok_or_else(|| {
                Error::binder(format!(
                    "Stats from a non-node table {} is not supported yet!",
                    arg.unwrap_or_default()
                ))
            })?;
            let stats = runtime
                .table_stats(id)
                .unwrap_or_else(|| koko_common::TableStats::with_columns(table.columns.len()));
            let mut row = Vec::with_capacity(table.columns.len() + 1);
            row.push(Value::Int64(stats.num_tuples() as i64));
            row.extend((0..table.columns.len()).map(|column| {
                Value::Int64(
                    stats
                        .column(column)
                        .map(koko_common::ColumnStats::num_distinct)
                        .unwrap_or(0) as i64,
                )
            }));
            vec![row]
        }
        BoundTableFunc::CurrentSetting => {
            let key = arg.unwrap_or_default().to_ascii_lowercase();
            vec![vec![Value::String(
                runtime.current_setting(&key).to_result_string(),
            )]]
        }
        BoundTableFunc::BmInfo => {
            let usage = runtime.memory_usage();
            let u64v = |n: u64| Value::IntX {
                value: n as i128,
                kind: koko_common::IntKind::U64,
            };
            vec![vec![u64v(usage.limit.unwrap_or(0)), u64v(usage.current)]]
        }
        // No dynamically-loaded extensions in this build — 0 rows.
        BoundTableFunc::ShowLoadedExtensions => Vec::new(),
    };
    Ok(rows)
}

/// Resolve `TABLE_INFO('t')`'s argument to its catalog table id, with the exact
/// error messages the standalone form produces.
fn table_info_target(catalog: &Catalog, arg: Option<&str>) -> Result<TableId> {
    let name =
        arg.ok_or_else(|| Error::binder("TABLE_INFO requires a table name argument.".to_string()))?;
    catalog
        .table_id(name)
        .ok_or_else(|| Error::catalog(format!("{name} does not exist in catalog.")))
}

/// The version string `db_version()` reports — pinned to the vendored oracle.
const TABLE_FUNC_DB_VERSION: &str = "0.17.0";

/// `SHOW_OFFICIAL_EXTENSIONS()` rows, verbatim from the vendored oracle.
const OFFICIAL_EXTENSIONS: &[(&str, &str)] = &[
    ("ADBC", "Adds support for reading from ADBC data sources"),
    ("ALGO", "Adds support for graph algorithms"),
    ("AZURE", "Adds support for reading from azure blob storage"),
    ("DELTA", "Adds support for reading from delta tables"),
    ("DUCKDB", "Adds support for reading from duckdb tables"),
    ("FTS", "Adds support for full-text search indexes"),
    (
        "HTTPFS",
        "Adds support for reading and writing files over a HTTP(S)/S3 filesystem",
    ),
    ("ICEBERG", "Adds support for reading from iceberg tables"),
    ("JSON", "Adds support for JSON operations"),
    ("LLM", "Adds support for LLM operations"),
    (
        "NEO4J",
        "Adds support for migrating nodes and rels from neo4j to koko",
    ),
    ("POSTGRES", "Adds support for reading from POSTGRES tables"),
    ("SQLITE", "Adds support for reading from SQLITE tables"),
    (
        "UNITY_CATALOG",
        "Adds support for scanning delta tables registered in unity catalog",
    ),
];

/// A `SHOW_TABLES` row (id, name, type, database name, comment).
fn table_func_table_row(id: u64, name: &str, kind: &str, comment: &str) -> Vec<Value> {
    vec![
        Value::Int64(id as i64),
        Value::String(name.to_string()),
        Value::String(kind.to_string()),
        Value::String(TABLE_FUNC_DB.to_string()),
        Value::String(comment.to_string()),
    ]
}

/// The bound reading clauses of one query part.
struct BoundReading {
    match_: BoundMatch,
    table_func_scans: Vec<BoundTableFuncScan>,
    load_scan: Option<BoundLoadScan>,
    unwind: Vec<BoundUnwind>,
    where_predicate: Option<BoundExpr>,
    optionals: Vec<BoundOptionalMatch>,
}

#[derive(Clone)]
struct ProjectionOutputRef {
    index: usize,
    ty: LogicalType,
}

impl ProjectionOutputRef {
    fn as_column(&self) -> BoundExpr {
        BoundExpr::Column {
            col: self.index,
            ty: self.ty.clone(),
        }
    }
}

/// Convert parsed CSV options into the supported reader configuration. C++ accepts
/// a fixed option set and rejects unknown names; keep the same contract here so a
/// misspelled or unsupported option never silently changes load semantics.
fn bind_csv_options(opts: &[(String, ast::LoadOptVal)]) -> Result<CsvLoadOptions> {
    let mut o = CsvLoadOptions::default();
    let mut seen = HashSet::new();
    for (key, val) in opts {
        let key_upper = key.to_ascii_uppercase();
        if !seen.insert(key_upper.clone()) {
            continue;
        }
        match key_upper.as_str() {
            "HEADER" => o.header = Some(opt_bool(&key_upper, val)?),
            "DELIM" | "DELIMITER" => o.delimiter = Some(opt_char(&key_upper, val)?),
            "QUOTE" => o.quote = Some(opt_char(&key_upper, val)?),
            "ESCAPE" => o.escape = Some(opt_char(&key_upper, val)?),
            "SKIP" => {
                o.skip = opt_int(
                    &key_upper,
                    val,
                    "Skip number must be a non-negative integer",
                )?
            }
            "AUTODETECT" | "AUTO_DETECT" => o.auto_detect = opt_bool(&key_upper, val)?,
            "PARALLEL" => o.parallel = opt_bool(&key_upper, val)?,
            "IGNORE_ERRORS" => o.ignore_errors = opt_bool(&key_upper, val)?,
            "LIST_UNBRACED" => o.list_unbraced = opt_bool(&key_upper, val)?,
            "NULL_STRINGS" => o.null_strings = opt_string_list(&key_upper, val)?,
            "SAMPLE_SIZE" => {
                let size = opt_int(
                    &key_upper,
                    val,
                    "Sample size must be a non-negative integer",
                )?;
                o.sample_size = if size == 0 { 256 } else { size };
            }
            "FILE_FORMAT" => {
                let fmt = opt_string(&key_upper, val)?;
                validate_file_format_option(&fmt)?;
                o.file_format = Some(fmt);
            }
            "FROM" => o.from = Some(opt_string(&key_upper, val)?),
            "TO" => o.to = Some(opt_string(&key_upper, val)?),
            _ => {
                return Err(Error::binder(format!(
                    "Unrecognized csv parsing option: {key_upper}."
                )));
            }
        }
    }
    if o.list_unbraced && o.delimiter.is_none() {
        o.delimiter = Some(b',');
    }
    Ok(o)
}

fn validate_columnar_input_options(
    format: FileFormat,
    opts: &[(String, ast::LoadOptVal)],
    allow_endpoints: bool,
) -> Result<()> {
    if format == FileFormat::Csv {
        return Ok(());
    }
    let valid = opts.iter().all(|(name, _)| {
        name.eq_ignore_ascii_case("FILE_FORMAT")
            || name.eq_ignore_ascii_case("IGNORE_ERRORS")
            || (allow_endpoints
                && (name.eq_ignore_ascii_case("FROM") || name.eq_ignore_ascii_case("TO")))
    });
    if valid {
        return Ok(());
    }
    Err(Error::binder(match format {
        FileFormat::Parquet => {
            "Copy from Parquet cannot have options other than IGNORE_ERRORS.".to_string()
        }
        FileFormat::Npy => {
            "Copy from numpy cannot have options other than IGNORE_ERRORS.".to_string()
        }
        FileFormat::Csv => unreachable!(),
    }))
}

fn hinted_input_format(path: &str, opts: &[(String, ast::LoadOptVal)]) -> Result<FileFormat> {
    if let Some((_, value)) = opts
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("FILE_FORMAT"))
    {
        return FileFormat::parse(&opt_string("FILE_FORMAT", value)?);
    }
    FileFormat::infer(std::path::Path::new(path))
}

fn dedupe_options(opts: &[(String, ast::LoadOptVal)]) -> Vec<(String, ast::LoadOptVal)> {
    let mut seen = HashSet::new();
    opts.iter()
        .filter_map(|(key, value)| {
            let key_upper = key.to_ascii_uppercase();
            seen.insert(key_upper.clone())
                .then(|| (key_upper, value.clone()))
        })
        .collect()
}

fn bind_output_options(
    path: &str,
    opts: &[(String, ast::LoadOptVal)],
) -> Result<BoundOutputOptions> {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("");
    if extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("gzip") {
        return Err(Error::Io(
            "Writing to compressed files is not supported yet.".to_string(),
        ));
    }
    if extension.eq_ignore_ascii_case("csv") {
        bind_csv_options(opts).map(BoundOutputOptions::Csv)
    } else if extension.eq_ignore_ascii_case("parquet") {
        bind_parquet_output_options(opts)
    } else {
        Err(Error::runtime(format!(
            "Exporting query result to the '{extension}' file is currently not supported."
        )))
    }
}

fn bind_parquet_output_options(opts: &[(String, ast::LoadOptVal)]) -> Result<BoundOutputOptions> {
    let mut compression = BoundParquetCompression::default();
    for (key, value) in dedupe_options(opts) {
        if key != "COMPRESSION" {
            return Err(Error::runtime(format!(
                "Unrecognized parquet option: {key}."
            )));
        }
        let ast::LoadOptVal::Str(codec) = value else {
            return Err(Error::runtime(format!(
                "Parquet compression option expects a string value, got: {}.",
                option_type_name(&value)
            )));
        };
        compression = match codec.to_ascii_uppercase().as_str() {
            "UNCOMPRESSED" => BoundParquetCompression::Uncompressed,
            "SNAPPY" => BoundParquetCompression::Snappy,
            "ZSTD" => BoundParquetCompression::Zstd,
            "GZIP" => BoundParquetCompression::Gzip,
            "LZ4_RAW" => BoundParquetCompression::Lz4Raw,
            _ => {
                return Err(Error::runtime(format!(
                    "Unrecognized parquet compression option: {codec}."
                )));
            }
        };
    }
    Ok(BoundOutputOptions::Parquet { compression })
}

fn option_type_name(value: &ast::LoadOptVal) -> &'static str {
    match value {
        ast::LoadOptVal::Bool(_) => "BOOL",
        ast::LoadOptVal::Int(_) => "INT64",
        ast::LoadOptVal::Float(_) => "DOUBLE",
        ast::LoadOptVal::Str(_) => "STRING",
        ast::LoadOptVal::List(_) => "ANY[]",
    }
}

/// Resolve a not-yet-created output/directory spelling without touching the
/// filesystem. Search paths apply only to input discovery; destinations use
/// the statement base directory, while `~/...` uses the session home.
fn resolve_destination_path(path: &str, config: &SessionConfig) -> String {
    let path_ref = std::path::Path::new(path);
    let resolved = if path_ref.is_absolute() {
        path_ref.to_path_buf()
    } else if path == "~" {
        config
            .home_directory
            .clone()
            .unwrap_or_else(|| config.base_dir.join(path_ref))
    } else if let Some(relative) = path.strip_prefix("~/") {
        config
            .home_directory
            .as_ref()
            .map(|home| home.join(relative))
            .unwrap_or_else(|| config.base_dir.join(path_ref))
    } else {
        config.base_dir.join(path_ref)
    };
    resolved.to_string_lossy().into_owned()
}

fn opt_bool(key: &str, val: &ast::LoadOptVal) -> Result<bool> {
    match val {
        ast::LoadOptVal::Bool(b) => Ok(*b),
        ast::LoadOptVal::Int(1) => Ok(true),
        ast::LoadOptVal::Int(0) => Ok(false),
        ast::LoadOptVal::Str(s) if s.eq_ignore_ascii_case("true") || s == "1" => Ok(true),
        ast::LoadOptVal::Str(s) if s.eq_ignore_ascii_case("false") || s == "0" => Ok(false),
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a boolean."
        ))),
    }
}

fn opt_int(key: &str, val: &ast::LoadOptVal, negative_msg: &str) -> Result<usize> {
    match val {
        ast::LoadOptVal::Int(n) if *n >= 0 => Ok(*n as usize),
        ast::LoadOptVal::Int(_) => Err(Error::runtime(negative_msg.to_string())),
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a INT64."
        ))),
    }
}

fn opt_string(key: &str, val: &ast::LoadOptVal) -> Result<String> {
    match val {
        ast::LoadOptVal::Str(s) => Ok(s.clone()),
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a string."
        ))),
    }
}

fn opt_char(key: &str, val: &ast::LoadOptVal) -> Result<u8> {
    let s = opt_string(key, val)?;
    match s.as_bytes() {
        [byte] => Ok(*byte),
        [b'\\', b't'] => Ok(b'\t'),
        [b'\\', byte] => Ok(*byte),
        _ => Err(Error::binder(
            "Copy csv option value must be a single character with an optional escape character."
                .to_string(),
        )),
    }
}

fn opt_string_list(key: &str, val: &ast::LoadOptVal) -> Result<Vec<String>> {
    match val {
        ast::LoadOptVal::List(items) => items
            .iter()
            .map(|item| match item {
                ast::LoadOptVal::Str(s) => Ok(s.clone()),
                _ => Err(Error::binder(format!(
                    "The type of csv parsing option {key} must be a STRING[]."
                ))),
            })
            .collect(),
        _ => Err(Error::binder(format!(
            "The type of csv parsing option {key} must be a STRING[]."
        ))),
    }
}

fn validate_file_format_option(fmt: &str) -> Result<()> {
    FileFormat::parse(fmt).map(|_| ())
}

/// A subquery captured (with its raw pattern) during `&self` expression binding,
/// to be bound — which introduces variables, needing `&mut self` — afterwards.
struct PendingSubquery {
    id: usize,
    kind: ast::SubqueryKind,
    patterns: Vec<ast::PatternElement>,
    where_clause: Option<ast::Expr>,
}

struct Binder<'c> {
    catalog: &'c Catalog,
    /// Provided values for query parameters (`$name`), substituted at bind time.
    params: &'c HashMap<String, Value>,
    /// Present only for preparation bindings; shared by UNION/subquery binders.
    prepared_parameters: Option<PreparedParameterEnv>,
    /// Full session configuration, retained for binding nested query sources.
    session_config: SessionConfig,
    /// The max recursive depth for variable-length patterns (session config).
    max_recursive_depth: u32,
    /// `disable_map_key_check` (session config; `true` = C++ default, no check).
    disable_map_key_check: bool,
    vars: Vec<VarInfo>,
    scope: HashMap<String, VarId>,
    anon: u32,
    /// Names and logical types of lambda parameters currently in scope (innermost
    /// last). `RefCell` lets `&self` expression binders push/pop without an `&mut` ripple.
    lambda_params: std::cell::RefCell<Vec<(String, LogicalType)>>,
    /// Next subquery id, and subqueries captured during expression binding (to be
    /// pattern-bound afterwards). `Cell`/`RefCell` so the `&self` binders can stage
    /// them without an `&mut` ripple.
    subquery_count: std::cell::Cell<usize>,
    pending_subqueries: std::cell::RefCell<Vec<PendingSubquery>>,
    /// `nextval`/`currval` calls staged during this part's expression binding,
    /// drained into the part's `sequence_calls` (ids are per-part indices). `&self`
    /// binders stage them through the `RefCell`.
    pending_sequence_calls: std::cell::RefCell<Vec<BoundSequenceCall>>,
}

impl<'c> Binder<'c> {
    fn new(
        catalog: &'c Catalog,
        params: &'c HashMap<String, Value>,
        config: &SessionConfig,
    ) -> Self {
        Self::with_env(catalog, params, config, None)
    }

    fn with_env(
        catalog: &'c Catalog,
        params: &'c HashMap<String, Value>,
        config: &SessionConfig,
        prepared_parameters: Option<PreparedParameterEnv>,
    ) -> Self {
        Self {
            catalog,
            params,
            prepared_parameters,
            session_config: config.clone(),
            max_recursive_depth: config.var_length_extend_max_depth,
            disable_map_key_check: config.disable_map_key_check,
            vars: Vec::new(),
            scope: HashMap::new(),
            anon: 0,
            lambda_params: std::cell::RefCell::new(Vec::new()),
            subquery_count: std::cell::Cell::new(0),
            pending_subqueries: std::cell::RefCell::new(Vec::new()),
            pending_sequence_calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn bind_parameter(&self, name: &str) -> BoundExpr {
        if let Some(env) = &self.prepared_parameters {
            let ty = env
                .types
                .borrow()
                .get(name)
                .cloned()
                .unwrap_or(LogicalType::Any);
            BoundExpr::Parameter {
                name: name.to_string(),
                ty,
            }
        } else {
            self.params
                .get(name)
                .map(|value| BoundExpr::Literal(value.clone()))
                .unwrap_or(BoundExpr::Literal(Value::Null))
        }
    }

    fn merge_parameter_type(&self, name: &str, target: &LogicalType) -> LogicalType {
        if *target == LogicalType::Any {
            return self
                .prepared_parameters
                .as_ref()
                .and_then(|env| env.types.borrow().get(name).cloned())
                .unwrap_or(LogicalType::Any);
        }
        let Some(env) = &self.prepared_parameters else {
            return target.clone();
        };
        let current = env
            .types
            .borrow()
            .get(name)
            .cloned()
            .unwrap_or(LogicalType::Any);
        let merged = if current == LogicalType::Any {
            Some(target.clone())
        } else if current == *target {
            Some(current.clone())
        } else if is_numeric_type(&current) && is_numeric_type(target) {
            koko_function::comparison_common_type(&current, target)
        } else {
            None
        };
        match merged {
            Some(merged) => {
                env.types
                    .borrow_mut()
                    .insert(name.to_string(), merged.clone());
                merged
            }
            None => {
                let mut error = env.error.borrow_mut();
                if error.is_none() {
                    *error = Some(Error::binder(format!(
                        "Parameter ${name} has conflicting type constraints {current} and {target}."
                    )));
                }
                current
            }
        }
    }

    fn constrain_parameter(&self, expression: &mut BoundExpr, target: &LogicalType) {
        if let BoundExpr::Parameter { name, ty } = expression {
            *ty = self.merge_parameter_type(name, target);
        }
    }

    fn capture_parameter_constraints(&self, expression: &BoundExpr) {
        match expression {
            BoundExpr::Cast { expr, target } => {
                if let BoundExpr::Parameter { name, .. } = expr.as_ref() {
                    self.merge_parameter_type(name, target);
                }
                self.capture_parameter_constraints(expr);
            }
            BoundExpr::Scalar { args, .. }
            | BoundExpr::Call { args, .. }
            | BoundExpr::List { elems: args, .. } => {
                for argument in args {
                    self.capture_parameter_constraints(argument);
                }
            }
            BoundExpr::Aggregate {
                arg: Some(argument),
                ..
            } => self.capture_parameter_constraints(argument),
            BoundExpr::ValueProperty { value, .. } => self.capture_parameter_constraints(value),
            BoundExpr::Struct { fields, .. } => {
                for (_, value) in fields {
                    self.capture_parameter_constraints(value);
                }
            }
            BoundExpr::ListLambda { list, body, .. } => {
                self.capture_parameter_constraints(list);
                self.capture_parameter_constraints(body);
            }
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                if let Some(operand) = operand {
                    self.capture_parameter_constraints(operand);
                }
                for (condition, result) in branches {
                    self.capture_parameter_constraints(condition);
                    self.capture_parameter_constraints(result);
                }
                if let Some(otherwise) = else_ {
                    self.capture_parameter_constraints(otherwise);
                }
            }
            _ => {}
        }
    }

    fn coerce_to(&self, mut expression: BoundExpr, target: &LogicalType) -> BoundExpr {
        self.constrain_parameter(&mut expression, target);
        coerce_to(expression, target)
    }

    /// Bind a non-query statement (DDL / `COPY`). Queries route through
    /// [`bind_regular_query`] so each `UNION` operand gets a fresh namespace.
    fn bind_nonquery(&mut self, stmt: &ast::Statement) -> Result<BoundStatement> {
        match stmt {
            // EXPLAIN/PROFILE are handled at the connection layer (the inner
            // statement binds when executed) — this arm is unreachable there.
            ast::Statement::Explain { inner, .. } => self.bind_nonquery(inner),
            ast::Statement::CreateNodeTable(t) => self.bind_create_node_table(t),
            ast::Statement::CreateRelTable(t) => self.bind_create_rel_table(t),
            ast::Statement::DropTable(d) => self.bind_drop_table(d),
            ast::Statement::Alter(a) => self.bind_alter(a),
            ast::Statement::CreateSequence(s) => self.bind_create_sequence(s),
            ast::Statement::DropSequence(d) => self.bind_drop_sequence(d),
            ast::Statement::Comment(c) => self.bind_comment(c),
            ast::Statement::CreateType(t) => self.bind_create_type(t),
            ast::Statement::Copy(c) => self.bind_copy(c),
            ast::Statement::CopyTo(copy) => self.bind_copy_to(copy),
            ast::Statement::ExportDatabase(export) => self.bind_export_database(export),
            ast::Statement::ImportDatabase(import) => self.bind_import_database(import),
            ast::Statement::Query(_) | ast::Statement::CreateTableAs(_) => {
                unreachable!("queries/CTAS route through bind_statement")
            }
            // Transaction-control and CALL statements are handled by the connection
            // before binding (they touch session/transaction state, not the catalog).
            ast::Statement::Transaction(_)
            | ast::Statement::Call(_)
            | ast::Statement::CreateGraph(_)
            | ast::Statement::UseGraph { .. }
            | ast::Statement::CreateIndex(_)
            | ast::Statement::DropIndex(_)
            | ast::Statement::DropGraph { .. } => {
                unreachable!("transaction/CALL statements are handled pre-bind")
            }
            // Macro definition/removal mutate the macro registry directly (koko
            // layer) and macro *calls* are expanded to plain expressions before
            // binding, so the binder never sees a macro statement.
            ast::Statement::CreateMacro(_) | ast::Statement::DropMacro { .. } => {
                unreachable!("macro statements are handled pre-bind")
            }
        }
    }

    fn resolve_copy_target(&self, name: &str) -> Option<(TableId, bool)> {
        if let Some(table) = self.catalog.table_id(name) {
            return Some((table, false));
        }
        for rel_id in self.catalog.rel_table_ids() {
            let rel = self.catalog.rel_table(rel_id)?;
            for ((from, to), member) in rel.pairs.iter().zip(&rel.member_ids) {
                let from_name = &self.catalog.node_table(*from)?.name;
                let to_name = &self.catalog.node_table(*to)?.name;
                let legacy = format!("{}_{}_{}", rel.name, from_name, to_name);
                if legacy.eq_ignore_ascii_case(name) {
                    return Some((*member, true));
                }
            }
        }
        None
    }

    fn select_copy_rel_member(
        &self,
        table: TableId,
        table_name: &str,
        options: &CsvLoadOptions,
        legacy_target: bool,
    ) -> Result<TableId> {
        if legacy_target {
            return Ok(table);
        }
        let rel = self
            .catalog
            .rel_table(table)
            .expect("COPY target kind is relationship");
        let selectors = options.from.as_deref().zip(options.to.as_deref());
        if rel.pairs.len() > 1 && selectors.is_none() {
            return Err(Error::binder(format!(
                "The table {table_name} has multiple FROM and TO pairs defined in the schema. A \
                 specific pair of FROM and TO options is expected when copying data into the \
                 {table_name} table."
            )));
        }
        if options.from.is_some() != options.to.is_some() {
            return Err(Error::binder(
                "Both FROM and TO options are required when selecting a relationship pair."
                    .to_string(),
            ));
        }
        let Some((from_name, to_name)) = selectors else {
            return Ok(rel.member_ids[0]);
        };
        for (((from, to), member), _) in rel
            .pairs
            .iter()
            .zip(&rel.member_ids)
            .zip(std::iter::repeat(()))
        {
            let from_matches = self
                .catalog
                .node_table(*from)
                .is_some_and(|node| node.name.eq_ignore_ascii_case(from_name));
            let to_matches = self
                .catalog
                .node_table(*to)
                .is_some_and(|node| node.name.eq_ignore_ascii_case(to_name));
            if from_matches && to_matches {
                return Ok(*member);
            }
        }
        Err(Error::binder(format!(
            "Rel table {table_name} does not contain {from_name}-{to_name} from-to pair."
        )))
    }

    fn bind_copy(&self, c: &ast::CopyStatement) -> Result<BoundStatement> {
        // The target table resolves before option validation (C++ order:
        // `COPY missing FROM ... (bad_opt=...)` blames the table).
        let (mut table, legacy_target) = self
            .resolve_copy_target(&c.table)
            .ok_or_else(|| Error::binder(format!("Table {} does not exist.", c.table)))?;
        // A table-function source takes no reader options — the C++ binder
        // rejects any before validating individual option names.
        if !c.options.is_empty()
            && c.source_query
                .as_ref()
                .is_some_and(|q| table_func_source_name(q).is_some())
        {
            return Err(Error::binder(
                "No option is supported when copying from table functions.".to_string(),
            ));
        }
        if c.source_query.is_none() {
            let format = hinted_input_format(&c.file_path, &c.options)?;
            if c.by_column && format != FileFormat::Npy {
                let format_name = match format {
                    FileFormat::Csv => "csv",
                    FileFormat::Parquet => "parquet",
                    FileFormat::Npy => unreachable!(),
                };
                return Err(Error::binder(format!(
                    "Copy by column with {format_name} file type is not supported."
                )));
            }
            validate_columnar_input_options(format, &c.options, true)?;
        }
        let options = bind_csv_options(&c.options)?;
        // File sources are fully expanded and validated once during binding.
        // Execution consumes only canonical concrete paths.
        let is_node = match self.catalog.table_kind(table) {
            Some(koko_catalog::TableKind::Node) => true,
            Some(koko_catalog::TableKind::Rel) => false,
            None => return Err(Error::binder(format!("Table {} does not exist.", c.table))),
        };
        if c.by_column && !is_node {
            return Err(Error::binder(
                "Copy by column is not supported for relationship table.".to_string(),
            ));
        }
        if !is_node {
            table = self.select_copy_rel_member(table, &c.table, &options, legacy_target)?;
        }
        // A query source binds now (against the pre-COPY catalog), and its
        // column count must match the COPY input width (C++ bind error).
        let source_query = match &c.source_query {
            Some(q) => {
                let BoundStatement::Query(bq) = bind_regular_query(
                    self.catalog,
                    q,
                    self.params,
                    &self.session_config,
                    self.prepared_parameters.clone(),
                )?
                else {
                    unreachable!("bind_regular_query yields a Query");
                };
                let got = bq
                    .operands
                    .first()
                    .map(|op| op.result_columns().len())
                    .unwrap_or(0);
                let expected = match &c.columns {
                    Some(cols) => {
                        if is_node {
                            cols.len()
                        } else {
                            2 + cols.len()
                        }
                    }
                    None => {
                        if is_node {
                            self.catalog
                                .node_table(table)
                                .map(|nt| {
                                    nt.columns
                                        .iter()
                                        .filter(|col| {
                                            col.ty != LogicalType::Serial
                                                && !matches!(
                                                    col.default,
                                                    koko_catalog::ColumnDefault::NextVal(_)
                                                )
                                        })
                                        .count()
                                })
                                .unwrap_or(0)
                        } else {
                            self.catalog
                                .rel_table(table)
                                .map(|rt| 2 + rt.columns.len())
                                .unwrap_or(2)
                        }
                    }
                };
                if got != expected {
                    // A table-function-only source names the FUNCTION in the
                    // mismatch (C++ `SHOW_OFFICIAL_EXTENSIONS has 2 columns but
                    // 1 columns were expected.`); a general query says "Query".
                    return Err(Error::binder(match table_func_source_name(q) {
                        Some(fname) => format!(
                            "{fname} has {got} columns but {expected} columns were expected."
                        ),
                        None => format!(
                            "Query returns {got} columns but {expected} columns were expected."
                        ),
                    }));
                }
                Some(bq)
            }
            None => None,
        };
        let (file_path, extra_files, format) = if source_query.is_some() {
            (c.file_path.clone(), c.extra_files.clone(), None)
        } else {
            let spellings: Vec<String> = std::iter::once(&c.file_path)
                .chain(c.extra_files.iter())
                .cloned()
                .collect();
            let resolver = FileResolverConfig {
                base_dir: self.session_config.base_dir.clone(),
                home_directory: self.session_config.home_directory.clone(),
                file_search_path: self.session_config.file_search_path.clone(),
            };
            let resolved = resolve_files(&spellings, &resolver, options.file_format.as_deref())?;
            let format = resolved[0].format;
            validate_columnar_input_options(format, &c.options, !is_node)?;
            let mut paths = resolved
                .into_iter()
                .map(|file| file.path.to_string_lossy().into_owned());
            let file_path = paths.next().expect("non-empty source spellings");
            (file_path, paths.collect(), Some(format))
        };
        if let Some(format) = format {
            let paths: Vec<String> = std::iter::once(file_path.clone())
                .chain(extra_files.iter().cloned())
                .collect();
            let expected = match &c.columns {
                Some(columns) => columns.len() + usize::from(!is_node) * 2,
                None if is_node => self
                    .catalog
                    .node_table(table)
                    .map(|node| {
                        node.columns
                            .iter()
                            .filter(|column| {
                                column.ty != LogicalType::Serial
                                    && !matches!(
                                        column.default,
                                        koko_catalog::ColumnDefault::NextVal(_)
                                    )
                            })
                            .count()
                    })
                    .unwrap_or(0),
                None => self
                    .catalog
                    .rel_table(table)
                    .map_or(2, |rel| 2 + rel.columns.len()),
            };
            if format == FileFormat::Npy && c.by_column {
                if paths.len() != expected {
                    return Err(Error::binder(format!(
                        "Number of columns mismatch. Expected {expected} but got {}.",
                        paths.len()
                    )));
                }
            } else if format != FileFormat::Csv {
                let inspect = self.session_config.file_schema_resolver.ok_or_else(|| {
                    Error::not_implemented(format!(
                        "{format:?} metadata inspection is unavailable in this binder context."
                    ))
                })?;
                let source = inspect(format, &paths)?;
                if source.len() != expected {
                    return Err(Error::binder(format!(
                        "Number of columns mismatch. Expected {expected} but got {}.",
                        source.len()
                    )));
                }
            }
        }
        Ok(BoundStatement::Copy(BoundCopy {
            table,
            is_node,
            columns: c.columns.clone(),
            file_path,
            extra_files,
            format,
            by_column: c.by_column,
            source_query,
            options,
        }))
    }

    fn bind_copy_to(&self, copy: &ast::CopyToStatement) -> Result<BoundStatement> {
        // C++ dispatches the output function before binding the query, so an
        // unsupported extension wins over errors inside the query.
        let options = bind_output_options(&copy.path, &copy.options)?;
        let BoundStatement::Query(query) = bind_regular_query(
            self.catalog,
            &copy.query,
            self.params,
            &self.session_config,
            self.prepared_parameters.clone(),
        )?
        else {
            unreachable!("bind_regular_query yields a Query");
        };
        let columns = query
            .operands
            .first()
            .map(|operand| operand.result_columns())
            .unwrap_or_default();
        Ok(BoundStatement::CopyTo(BoundCopyTo {
            query,
            columns,
            path: resolve_destination_path(&copy.path, &self.session_config),
            options,
        }))
    }

    fn bind_export_database(
        &self,
        export: &ast::ExportDatabaseStatement,
    ) -> Result<BoundStatement> {
        let mut options = dedupe_options(&export.options);
        let format = if let Some((_, value)) = options.iter().find(|(key, _)| key == "FORMAT") {
            let ast::LoadOptVal::Str(format) = value else {
                return Err(Error::binder(
                    "The type of format option must be a string.".to_string(),
                ));
            };
            if format.eq_ignore_ascii_case("csv") {
                FileFormat::Csv
            } else if format.eq_ignore_ascii_case("parquet") {
                FileFormat::Parquet
            } else {
                return Err(Error::binder(
                    "Export database currently only supports csv and parquet files.".to_string(),
                ));
            }
        } else {
            FileFormat::Parquet
        };
        options.retain(|(key, _)| key != "FORMAT");

        let schema_only =
            if let Some((_, value)) = options.iter().find(|(key, _)| key == "SCHEMA_ONLY") {
                let ast::LoadOptVal::Bool(schema_only) = value else {
                    return Err(Error::binder(
                        "The 'SCHEMA_ONLY' option must have a BOOL value.".to_string(),
                    ));
                };
                *schema_only
            } else {
                false
            };
        if schema_only && export.options.len() != 1 {
            return Err(Error::binder(
                "When 'SCHEMA_ONLY' option is set to true in export database, no other options are allowed."
                    .to_string(),
            ));
        }
        options.retain(|(key, _)| key != "SCHEMA_ONLY");

        let output_options = if schema_only {
            BoundOutputOptions::Parquet {
                compression: BoundParquetCompression::default(),
            }
        } else {
            match format {
                FileFormat::Csv => bind_csv_options(&options).map(BoundOutputOptions::Csv)?,
                FileFormat::Parquet if !options.is_empty() => {
                    return Err(Error::binder(
                        "Only export to csv can have options.".to_string(),
                    ));
                }
                FileFormat::Parquet => BoundOutputOptions::Parquet {
                    compression: BoundParquetCompression::default(),
                },
                FileFormat::Npy => unreachable!("validated above"),
            }
        };
        Ok(BoundStatement::ExportDatabase(BoundExportDatabase {
            path: resolve_destination_path(&export.path, &self.session_config),
            options: output_options,
            schema_only,
        }))
    }

    fn bind_import_database(
        &self,
        import: &ast::ImportDatabaseStatement,
    ) -> Result<BoundStatement> {
        Ok(BoundStatement::ImportDatabase(BoundImportDatabase {
            path: resolve_destination_path(&import.path, &self.session_config),
        }))
    }

    // ---- DDL ----

    fn bind_create_node_table(&self, t: &ast::CreateNodeTable) -> Result<BoundStatement> {
        let columns = self.bind_columns(&t.columns)?;
        if !columns
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(&t.primary_key))
        {
            return Err(Error::binder(format!(
                "Primary key {} does not match any of the predefined node properties.",
                t.primary_key
            )));
        }
        // The C++ PK type allow-list works on the *physical* type: every integer
        // width, STRING (also BLOB's physical), FLOAT/DOUBLE, and the int-backed
        // temporal/DECIMAL/UUID types are valid; BOOL, INTERVAL and nested types
        // are not (bind_ddl.cpp validatePrimaryKey, oracle-verified matrix).
        if let Some((_, pk_ty)) = columns
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&t.primary_key))
        {
            let valid = pk_ty.is_numeric()
                || matches!(
                    pk_ty,
                    LogicalType::String
                        | LogicalType::Blob
                        | LogicalType::Uuid
                        | LogicalType::Serial
                        | LogicalType::Date
                        | LogicalType::Timestamp
                        | LogicalType::TimestampNs
                        | LogicalType::TimestampMs
                        | LogicalType::TimestampSec
                        | LogicalType::TimestampTz
                );
            if !valid {
                return Err(Error::binder(format!(
                    "Invalid primary key column type {pk_ty}. Primary keys must be either STRING \
                     or a numeric type."
                )));
            }
        }
        let defaults = self.bind_column_defaults(&t.columns, &columns)?;
        // SERIAL columns auto-increment on insert (the raw type string is `SERIAL`,
        // which `from_ddl_str` resolves to the physical INT64).
        let serial_columns: Vec<usize> = t
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.type_name.eq_ignore_ascii_case("SERIAL"))
            .map(|(i, _)| i)
            .collect();
        let metadata = column_metadata(&t.columns, &columns, &serial_columns);
        // The duplicate-name check runs last, so a malformed-and-duplicate
        // statement reports its malformed error first (matching the C++ binder).
        // `IF NOT EXISTS` defers the skip to execution.
        if !t.if_not_exists && self.catalog.contains_table(&t.name) {
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                t.name
            )));
        }
        let icebug_storage = t
            .format
            .as_deref()
            .filter(|format| format.eq_ignore_ascii_case("icebug-disk"))
            .map(|_| t.storage.clone().unwrap_or_default());
        Ok(BoundStatement::CreateNodeTable {
            name: t.name.clone(),
            columns,
            defaults,
            metadata,
            primary_key: t.primary_key.clone(),
            if_not_exists: t.if_not_exists,
            serial_columns,
            icebug_storage,
        })
    }

    fn bind_create_rel_table(&self, t: &ast::CreateRelTable) -> Result<BoundStatement> {
        let resolve = |n: &str| {
            self.catalog
                .node_table_by_name(n)
                .map(|nt| nt.id)
                .ok_or_else(|| {
                    if self.catalog.table_id(n).is_some() {
                        Error::binder(format!("{n} is not of type NODE."))
                    } else {
                        Error::binder(format!("Table {n} does not exist."))
                    }
                })
        };
        // Column types resolve before the FROM/TO endpoints (C++ order: an
        // invalid property type reports even when an endpoint table is missing).
        let columns = self.bind_columns(&t.columns)?;
        let pairs = t
            .pairs
            .iter()
            .map(|(f, to)| Ok((resolve(f)?, resolve(to)?)))
            .collect::<Result<Vec<_>>>()?;
        let icebug_storage = t
            .format
            .as_deref()
            .filter(|format| format.eq_ignore_ascii_case("icebug-disk"))
            .map(|_| t.storage.clone().unwrap_or_default());
        let endpoints_are_icebug = pairs.iter().all(|(from, to)| {
            self.catalog.is_icebug_table(*from) && self.catalog.is_icebug_table(*to)
        });
        let endpoints_include_icebug = pairs.iter().any(|(from, to)| {
            self.catalog.is_icebug_table(*from) || self.catalog.is_icebug_table(*to)
        });
        if icebug_storage.is_some() != endpoints_are_icebug
            || endpoints_include_icebug && !endpoints_are_icebug
        {
            return Err(Error::binder(
                "Cannot mix icebug-disk tables with non-icebug-disk tables in CREATE REL TABLE.",
            ));
        }
        let defaults = self.bind_column_defaults(&t.columns, &columns)?;
        let metadata = column_metadata(&t.columns, &columns, &[]);
        let mut seen = HashSet::new();
        for ((from_name, to_name), pair) in t.pairs.iter().zip(&pairs) {
            if !seen.insert(*pair) {
                return Err(Error::binder(format!(
                    "Found duplicate FROM-TO {from_name}-{to_name} pairs."
                )));
            }
        }
        let storage_direction = bind_storage_direction(t.storage_direction.as_deref())?;
        // The duplicate-name check runs after endpoint resolution, so a bad-endpoint
        // duplicate (e.g. `CREATE REL TABLE knows(FROM Nope TO Nope)`) reports the
        // endpoint error first. Covers REL TABLE and REL TABLE GROUP.
        if !t.if_not_exists && self.catalog.contains_table(&t.name) {
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                t.name
            )));
        }
        Ok(BoundStatement::CreateRelTable {
            name: t.name.clone(),
            pairs,
            columns,
            defaults,
            metadata,
            if_not_exists: t.if_not_exists,
            multiplicity: t.multiplicity,
            storage_direction,
            icebug_storage,
        })
    }

    /// Resolve each column's `DEFAULT` (aligned to the already-bound `(name, ty)`
    /// columns) into a [`BoundColumnDefault`].
    fn bind_column_defaults(
        &self,
        cols: &[ast::ColumnDef],
        bound: &[(String, LogicalType)],
    ) -> Result<Vec<BoundColumnDefault>> {
        cols.iter()
            .zip(bound)
            .map(|(c, (_, ty))| self.resolve_default(c.default.as_ref(), &c.type_name, ty))
            .collect()
    }

    /// Classify a column `DEFAULT`: `nextval('seq')` → per-row `NextVal`; anything
    /// else → a `Const` bound expression wrapped in a cast to the column type (the
    /// exec layer folds it). `SERIAL` columns may not carry an explicit default.
    fn resolve_default(
        &self,
        default: Option<&ast::Expr>,
        type_name: &str,
        ty: &LogicalType,
    ) -> Result<BoundColumnDefault> {
        let Some(expr) = default else {
            return Ok(BoundColumnDefault::None);
        };
        if type_name.eq_ignore_ascii_case("SERIAL") {
            return Err(Error::binder(
                "No DEFAULT value should be set for SERIAL columns".to_string(),
            ));
        }
        if let ast::Expr::Function { name, args, .. } = expr {
            if sequence_fn(name) == Some(SequenceFn::NextVal) {
                return match args.as_slice() {
                    [ast::Expr::Literal(Value::String(s))] => {
                        Ok(BoundColumnDefault::NextVal(s.clone()))
                    }
                    _ => Err(Error::not_implemented(
                        "nextval default requires a string-literal sequence name".to_string(),
                    )),
                };
            }
        }
        let bound = self.bind_expr(expr)?;
        // The default value obeys the assignment implicit-cast gate at bind time
        // (C++ `implicitCastIfNecessary` in `bindColumnDefinitions`): `INT64 DEFAULT 'x'`
        // is a binder error, not a deferred conversion failure.
        assignable_or_err(&bound, ty, &expr_name(expr))?;
        Ok(BoundColumnDefault::Const(BoundExpr::Cast {
            expr: Box::new(bound),
            target: ty.clone(),
        }))
    }

    fn bind_drop_table(&self, d: &ast::DropTable) -> Result<BoundStatement> {
        match self.catalog.table_id(&d.name) {
            Some(id) => {
                // A node table referenced by a relationship table cannot be dropped
                // (it would dangle the rel's FROM/TO endpoints).
                if self.catalog.table_kind(id) == Some(koko_catalog::TableKind::Node) {
                    if let Some(rel) = self.catalog.rel_table_referencing(id) {
                        let node = self.catalog.node_table(id).expect("node table exists");
                        return Err(Error::binder(format!(
                            "Cannot delete node table {} because it is referenced by relationship \
                             table {}.",
                            node.name, rel
                        )));
                    }
                }
                Ok(BoundStatement::DropTable {
                    name: d.name.clone(),
                    table: Some(id),
                })
            }
            // `IF EXISTS` turns a missing table into a skip (execution emits a message);
            // without it, a missing table is a bind error.
            None if d.if_exists => Ok(BoundStatement::DropTable {
                name: d.name.clone(),
                table: None,
            }),
            None => Err(Error::binder(format!("Table {} does not exist.", d.name))),
        }
    }

    fn bind_alter(&self, a: &ast::AlterStatement) -> Result<BoundStatement> {
        let id = self
            .catalog
            .table_id(&a.table)
            .ok_or_else(|| Error::binder(format!("Table {} does not exist.", a.table)))?;
        if self.catalog.is_icebug_table(id) {
            return Err(Error::binder(format!(
                "Cannot alter table {}: icebug-disk tables are immutable.",
                self.table_name(id)
            )));
        }
        // Messages echo the catalog's canonical table name (matching the C++ engine,
        // which uses `entry->getName()`), independent of the user's casing.
        let table_name = self.table_name(id);
        let op = match &a.op {
            ast::AlterOp::AddProperty {
                name,
                type_name,
                default,
                if_not_exists,
            } => {
                let ty = self.resolve_ddl_type(type_name)?;
                let bound_default = self.resolve_default(default.as_ref(), type_name, &ty)?;
                let metadata = BoundColumnMetadata {
                    type_text: column_type_text(type_name, &ty),
                    default_text: default_expr_text(default.as_ref(), false),
                };
                BoundAlterOp::AddProperty {
                    name: name.clone(),
                    ty,
                    default: bound_default,
                    metadata,
                    if_not_exists: *if_not_exists,
                }
            }
            ast::AlterOp::DropProperty { name, if_exists } => {
                // Reject dropping the primary key (a node table's identity column).
                if let Some(nt) = self.catalog.node_table(id) {
                    if nt
                        .column(name)
                        .is_some_and(|c| c.column_id.0 as usize == nt.primary_key)
                    {
                        return Err(Error::binder(format!(
                            "Cannot drop property {name} in table {table_name} because it is used \
                             as primary key."
                        )));
                    }
                }
                BoundAlterOp::DropProperty {
                    name: name.clone(),
                    if_exists: *if_exists,
                }
            }
            ast::AlterOp::RenameProperty { old, new } => BoundAlterOp::RenameProperty {
                old: old.clone(),
                new: new.clone(),
            },
            ast::AlterOp::RenameTable { new } => BoundAlterOp::RenameTable { new: new.clone() },
            ast::AlterOp::AddFromTo {
                from,
                to,
                if_not_exists,
            } => {
                self.require_rel_table(id, &a.table)?;
                BoundAlterOp::AddFromTo {
                    from: self.resolve_node_table(from)?,
                    to: self.resolve_node_table(to)?,
                    if_not_exists: *if_not_exists,
                }
            }
            ast::AlterOp::DropFromTo {
                from,
                to,
                if_exists,
            } => {
                self.require_rel_table(id, &a.table)?;
                BoundAlterOp::DropFromTo {
                    from: self.resolve_node_table(from)?,
                    to: self.resolve_node_table(to)?,
                    if_exists: *if_exists,
                }
            }
        };
        Ok(BoundStatement::Alter {
            table: id,
            table_name,
            op,
        })
    }

    /// Validate a `HINT <join-tree>` against its MATCH clause (C++ hint binder;
    /// the single-order planner otherwise ignores the tree). Checks, in oracle
    /// order: correlation with previous patterns, anonymous pattern parts,
    /// unknown hint names, pattern-variable coverage, pairwise join
    /// resolvability, and rel storage-direction compatibility.
    fn validate_join_hint(
        &self,
        hint: &ast::JoinHint,
        m: &ast::MatchClause,
        pre_scope: &std::collections::HashSet<String>,
    ) -> Result<()> {
        use std::collections::HashSet;
        // Pattern inventory: node vars, rel vars with endpoint names + table.
        let mut nodes: Vec<String> = Vec::new();
        let mut rels: Vec<(String, String, String, Vec<TableId>)> = Vec::new();
        let mut anonymous = false;
        for pe in &m.patterns {
            match &pe.head.var {
                Some(v) => nodes.push(v.clone()),
                None => anonymous = true,
            }
            let mut prev = pe.head.var.clone().unwrap_or_default();
            for (rel, node) in &pe.chains {
                let node_name = match &node.var {
                    Some(v) => {
                        nodes.push(v.clone());
                        v.clone()
                    }
                    None => {
                        anonymous = true;
                        String::new()
                    }
                };
                match &rel.var {
                    Some(rv) => {
                        let (src, dst) = match rel.direction {
                            ast::Direction::Left => (node_name.clone(), prev.clone()),
                            _ => (prev.clone(), node_name.clone()),
                        };
                        let tables = self.resolve_rel_tables(&rel.labels).unwrap_or_default();
                        rels.push((rv.clone(), src, dst, tables));
                    }
                    None => anonymous = true,
                }
                prev = node_name;
            }
        }
        // 1. Correlation: a pattern variable that already existed in scope.
        if nodes
            .iter()
            .chain(rels.iter().map(|(r, _, _, _)| r))
            .any(|v| pre_scope.contains(v))
        {
            return Err(Error::Raw(
                "Hint join pattern has correlation with previous patterns. This is not \
                 supported yet."
                    .to_string(),
            ));
        }
        // 2. Anonymous parts cannot be hinted ('patter' typo is verbatim C++).
        if anonymous {
            return Err(Error::binder(
                "Cannot hint join order in a match patter with anonymous node or relationship."
                    .to_string(),
            ));
        }
        // 3/4. Hint names must be pattern vars; every pattern var must appear.
        let mut hint_vars: Vec<String> = Vec::new();
        collect_hint_vars(hint, &mut hint_vars);
        let pattern_vars: HashSet<&str> = nodes
            .iter()
            .map(|s| s.as_str())
            .chain(rels.iter().map(|(r, _, _, _)| r.as_str()))
            .collect();
        for v in &hint_vars {
            if !pattern_vars.contains(v.as_str()) {
                return Err(Error::binder(format!(
                    "Cannot bind {v} to a node or relationship pattern"
                )));
            }
        }
        let hinted: HashSet<&str> = hint_vars.iter().map(|s| s.as_str()).collect();
        for v in pattern_vars {
            if !hinted.contains(v) {
                return Err(Error::binder(format!("Cannot find {v} in join hint.")));
            }
        }
        // 5/6. Pairwise resolvability + storage direction, bottom-up.
        self.resolve_hint_tree(hint, &rels)?;
        Ok(())
    }

    /// Resolve one hint subtree to its variable set, checking that each JOIN's
    /// sides connect through a rel endpoint and that the rel's anchor side is
    /// storable (fwd/bwd) for its table.
    fn resolve_hint_tree(
        &self,
        t: &ast::JoinHint,
        rels: &[(String, String, String, Vec<TableId>)],
    ) -> Result<std::collections::HashSet<String>> {
        use std::collections::HashSet;
        match t {
            ast::JoinHint::Var(v) => Ok(HashSet::from([v.clone()])),
            ast::JoinHint::MultiJoin(l, names) => {
                let mut acc = self.resolve_hint_tree(l, rels)?;
                for n in names {
                    if let Some((_, src, dst, _)) = rels.iter().find(|(r, _, _, _)| r == n) {
                        if !acc.contains(src) && !acc.contains(dst) {
                            return Err(Error::binder(format!(
                                "Cannot resolve join condition between {} and Scan({n}).",
                                render_hint(l, rels)
                            )));
                        }
                    }
                    acc.insert(n.clone());
                }
                Ok(acc)
            }
            ast::JoinHint::Join(l, r) => {
                let left = self.resolve_hint_tree(l, rels)?;
                let right = self.resolve_hint_tree(r, rels)?;
                let mut connected = false;
                for (rel, src, dst, tables) in rels {
                    let (rel_in, other) = if left.contains(rel) {
                        (&left, &right)
                    } else if right.contains(rel) {
                        (&right, &left)
                    } else {
                        continue;
                    };
                    let src_in_rel_side = rel_in.contains(src);
                    let dst_in_rel_side = rel_in.contains(dst);
                    let src_in_other = other.contains(src);
                    let dst_in_other = other.contains(dst);
                    // A rel connects the two sides only through an endpoint in
                    // the *other* side; one fully internal to its own side
                    // contributes nothing (JOIN(Scan(a,e),Scan(b)) with c).
                    if src_in_other || dst_in_other {
                        // The rel's *anchor* is whichever endpoint it meets
                        // first: dst-anchored extends backward, which a
                        // fwd-only table cannot serve.
                        let anchored_dst =
                            (dst_in_rel_side || dst_in_other) && !(src_in_rel_side || src_in_other);
                        let anchored_src =
                            (src_in_rel_side || src_in_other) && !(dst_in_rel_side || dst_in_other);
                        for &tid in tables {
                            if let Some(rt) = self.catalog.rel_table(tid) {
                                use koko_catalog::RelStorageDirection as Dir;
                                if anchored_dst && rt.storage_direction == Dir::Fwd {
                                    return Err(Error::runtime(format!(
                                        "Failed to get bwd data for rel table \"{}\", please \
                                         set the storage direction to BOTH",
                                        rt.name
                                    )));
                                }
                                if anchored_src && rt.storage_direction == Dir::Bwd {
                                    return Err(Error::runtime(format!(
                                        "Failed to get fwd data for rel table \"{}\", please \
                                         set the storage direction to BOTH",
                                        rt.name
                                    )));
                                }
                            }
                        }
                        connected = true;
                    }
                }
                if !connected {
                    return Err(Error::binder(format!(
                        "Cannot resolve join condition between {} and {}.",
                        render_hint(l, rels),
                        render_hint(r, rels)
                    )));
                }
                Ok(left.union(&right).cloned().collect())
            }
        }
    }

    /// Resolve a node-table name to its id (the endpoint of a FROM-TO pair).
    /// A name that exists as a non-node table is typed ("R is not of type
    /// NODE."), a missing one reports absence.
    fn resolve_node_table(&self, name: &str) -> Result<TableId> {
        self.catalog
            .node_table_by_name(name)
            .map(|t| t.id)
            .ok_or_else(|| {
                if self.catalog.table_id(name).is_some() {
                    Error::binder(format!("{name} is not of type NODE."))
                } else {
                    Error::binder(format!("Table {name} does not exist."))
                }
            })
    }

    /// Require that an `ALTER … FROM…TO` targets a relationship table.
    fn require_rel_table(&self, id: TableId, name: &str) -> Result<()> {
        if self.catalog.rel_table(id).is_none() {
            return Err(Error::binder(format!(
                "Table {name} is not a relationship table."
            )));
        }
        Ok(())
    }

    /// The catalog's canonical name for a table id (node or rel).
    fn table_name(&self, id: TableId) -> String {
        self.catalog
            .node_table(id)
            .map(|t| t.name.clone())
            .or_else(|| self.catalog.rel_table(id).map(|t| t.name.clone()))
            .unwrap_or_default()
    }

    fn bind_create_sequence(&self, s: &ast::CreateSequence) -> Result<BoundStatement> {
        // The duplicate-name check (separate sequence namespace) precedes value
        // validation, matching the C++ binder; `IF NOT EXISTS` defers the skip to
        // execution.
        if !s.if_not_exists && self.catalog.contains_sequence(&s.name) {
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                s.name
            )));
        }
        let to_i64 = |v: i128| -> Result<i64> {
            i64::try_from(v).map_err(|_| {
                Error::binder("Out of bounds: SEQUENCE accepts integers within INT64.".to_string())
            })
        };
        let increment = match s.increment {
            Some(v) => to_i64(v)?,
            None => 1,
        };
        if increment == 0 {
            return Err(Error::binder("INCREMENT must be non-zero.".to_string()));
        }
        // Defaults flip with the increment sign: ascending counts up from 1 to
        // INT64_MAX; descending counts down from -1 to INT64_MIN.
        let min = match s.min_value {
            Some(v) => to_i64(v)?,
            None if increment > 0 => 1,
            None => i64::MIN,
        };
        let max = match s.max_value {
            Some(v) => to_i64(v)?,
            None if increment > 0 => i64::MAX,
            None => -1,
        };
        let start = match s.start {
            Some(v) => to_i64(v)?,
            None if increment > 0 => min,
            None => max,
        };
        if max < min {
            return Err(Error::binder(
                "SEQUENCE MAXVALUE should be greater than or equal to MINVALUE.".to_string(),
            ));
        }
        if start < min || start > max {
            return Err(Error::binder(
                "SEQUENCE START value should be between MINVALUE and MAXVALUE.".to_string(),
            ));
        }
        Ok(BoundStatement::CreateSequence {
            name: s.name.clone(),
            if_not_exists: s.if_not_exists,
            start,
            increment,
            min,
            max,
            cycle: s.cycle,
        })
    }

    fn bind_create_type(&self, t: &ast::CreateType) -> Result<BoundStatement> {
        if self.catalog.contains_user_type(&t.name) {
            return Err(Error::binder(format!("Duplicated type name: {}.", t.name)));
        }
        // The underlying type is itself resolved through the registry, so a UDT may
        // alias an earlier UDT.
        let ty = self.resolve_ddl_type(&t.type_name)?;
        Ok(BoundStatement::CreateType {
            name: t.name.clone(),
            ty,
        })
    }

    /// Resolve a DDL type string, consulting the user-defined-type registry first
    /// (so an alias like `SMALLINT`/`DESCRIPTION` resolves to its underlying type).
    fn resolve_ddl_type(&self, s: &str) -> Result<LogicalType> {
        if let Some(ty) = self.catalog.user_type(s) {
            return Ok(ty);
        }
        // Pass a resolver so a UDT alias *nested* inside a STRUCT/LIST/MAP/ARRAY
        // (which the top-level check above can't see) still resolves.
        LogicalType::from_ddl_str_with(s, &|name| self.catalog.user_type(name))
    }

    /// Resolve a `CAST` target type, consulting the user-defined-type registry first.
    fn resolve_cast_type(&self, s: &str) -> Result<LogicalType> {
        if let Some(ty) = self.catalog.user_type(s) {
            return Ok(ty);
        }
        LogicalType::from_cast_str_with(s, &|name| self.catalog.user_type(name))
    }

    fn bind_comment(&self, c: &ast::CommentStmt) -> Result<BoundStatement> {
        let id = self
            .catalog
            .table_id(&c.table)
            .ok_or_else(|| Error::binder(format!("Table {} does not exist.", c.table)))?;
        Ok(BoundStatement::Comment {
            table: id,
            table_name: self.table_name(id),
            comment: c.comment.clone(),
        })
    }

    fn bind_drop_sequence(&self, d: &ast::DropSequence) -> Result<BoundStatement> {
        // Without `IF EXISTS`, a missing sequence is a bind error; with it, the
        // skip message is emitted at execution.
        if !d.if_exists && !self.catalog.contains_sequence(&d.name) {
            return Err(Error::binder(format!(
                "Sequence {} does not exist.",
                d.name
            )));
        }
        Ok(BoundStatement::DropSequence {
            name: d.name.clone(),
            if_exists: d.if_exists,
        })
    }

    /// Take the `nextval`/`currval` calls staged during this part's expression
    /// binding (ids are their indices), to attach to the part.
    fn drain_sequence_calls(&self) -> Vec<BoundSequenceCall> {
        std::mem::take(&mut *self.pending_sequence_calls.borrow_mut())
    }

    /// Bind `nextval('seq')` / `currval('seq')`. The sequence name must be a string
    /// literal (it is resolved per row, and advancing it mutates catalog state, so
    /// the call is lifted to a per-row column rather than evaluated in the pure
    /// expression engine).
    fn bind_sequence_call(&self, func: SequenceFn, args: &[ast::Expr]) -> Result<BoundExpr> {
        let fname = match func {
            SequenceFn::NextVal => "nextval",
            SequenceFn::CurrVal => "currval",
        };
        if args.len() != 1 {
            return Err(Error::binder(format!(
                "{fname} expects exactly one argument (the sequence name)"
            )));
        }
        let ast::Expr::Literal(Value::String(seq_name)) = &args[0] else {
            return Err(Error::not_implemented(format!(
                "{fname} requires a string-literal sequence name"
            )));
        };
        let id = self.pending_sequence_calls.borrow().len();
        self.pending_sequence_calls
            .borrow_mut()
            .push(BoundSequenceCall {
                func,
                name: seq_name.clone(),
            });
        Ok(BoundExpr::SequenceCall {
            id,
            ty: LogicalType::Int64,
        })
    }

    fn bind_columns(&self, cols: &[ast::ColumnDef]) -> Result<Vec<(String, LogicalType)>> {
        cols.iter()
            .map(|c| {
                // The internal identity names are reserved (C++ reservedInPropertyLookup).
                if matches!(
                    c.name.to_ascii_lowercase().as_str(),
                    "_id" | "_label" | "_src" | "_dst" | "_nodes" | "_rels"
                ) {
                    return Err(Error::binder(format!(
                        "{} is a reserved property name.",
                        c.name
                    )));
                }
                Ok((c.name.clone(), self.resolve_ddl_type(&c.type_name)?))
            })
            .collect()
    }

    // ---- queries ----

    fn bind_query(&mut self, q: &ast::SingleQuery) -> Result<BoundQuery> {
        // A required MATCH after OPTIONAL MATCH gets its own implicit part
        // (as if `WITH *` preceded it): the optional's rows — NULLs included —
        // carry forward, and the required pattern joins/extends from them,
        // which is the sequential clause semantics.
        let q = split_required_after_optional(q);
        let q = &q;
        let mut parts = Vec::new();
        // Scalar variables carried in from the previous part's WITH, and that
        // part's WITH-WHERE (bound against the carried scope). Empty for the
        // first part. The current `self.scope` already holds the carried vars.
        let mut input_vars: Vec<VarId> = Vec::new();
        let mut input_filter: Option<BoundExpr> = None;

        // Leading WITH-terminated parts.
        for qp in &q.parts {
            let reading = self.bind_reading(&qp.reading)?;
            // Updating clauses may precede a `WITH` (write-before-WITH); bind them
            // before the projection so any created variables are in scope for it.
            let updates = self.bind_updating(&qp.updating)?;
            let projection = self.bind_with_projection(&qp.with.projection)?;
            // Bind subqueries referenced in this part's reading-WHERE / projection
            // (correlated to the part's match scope, still current here).
            let subqueries = self.drain_subqueries()?;
            let sequence_calls = self.drain_sequence_calls();
            // Reset scope to the carried scalar variables, then bind the WITH-WHERE
            // in that new scope (it is applied after ORDER BY/SKIP/LIMIT — see the
            // planner — so it becomes the *next* part's input filter).
            let carried = self.carry_forward(&projection)?;
            // A predicate containing an unbound $param is dropped entirely —
            // C++ passes all rows (see unbound-param in docs/DIVERGENCES.md
            // history), unlike a NULL predicate which would drop them.
            let next_filter = match &qp.with.where_clause {
                Some(w) if !self.has_unbound_param(w) => Some(self.bind_expr(w)?),
                _ => None,
            };
            // A subquery in the WITH ... WHERE stays pending: it belongs to
            // the NEXT part (its ids follow the just-reset counter, and it
            // binds with the carried scope during that part's drain).
            if !self.pending_sequence_calls.borrow().is_empty() {
                return Err(Error::not_implemented(
                    "a sequence call in a WITH ... WHERE is not supported in this phase"
                        .to_string(),
                ));
            }
            parts.push(BoundPart {
                input_vars,
                input_filter,
                match_: reading.match_,
                table_func_scans: reading.table_func_scans,
                load_scan: reading.load_scan,
                unwind: reading.unwind,
                where_predicate: reading.where_predicate,
                subqueries,
                sequence_calls,
                optionals: reading.optionals,
                updates,
                projection: Some(projection),
                carried: carried.clone(),
            });
            input_vars = carried;
            input_filter = next_filter;
        }

        // The terminal part: reading + optional updating + optional RETURN. A
        // `RETURN` after a write is now allowed (it projects the post-write rows).
        let reading = self.bind_reading(&q.reading)?;
        let updates = self.bind_updating(&q.updating)?;
        let projection = match &q.ret {
            Some(r) => Some(self.bind_projection(r)?),
            None => None,
        };
        let subqueries = self.drain_subqueries()?;
        let sequence_calls = self.drain_sequence_calls();
        if updates.is_empty() && projection.is_none() {
            return Err(Error::binder(
                "a query must either RETURN results or write data".to_string(),
            ));
        }
        parts.push(BoundPart {
            input_vars,
            input_filter,
            match_: reading.match_,
            table_func_scans: reading.table_func_scans,
            load_scan: reading.load_scan,
            unwind: reading.unwind,
            where_predicate: reading.where_predicate,
            subqueries,
            sequence_calls,
            optionals: reading.optionals,
            updates,
            projection,
            carried: Vec::new(),
        });

        Ok(BoundQuery {
            vars: std::mem::take(&mut self.vars),
            parts,
        })
    }

    /// Bind a part's reading clauses (`MATCH` / `OPTIONAL MATCH` / `UNWIND`) in
    /// the current scope, in order, so each clause sees the variables bound before
    /// it. Required matches and unwinds accumulate into one block; each
    /// `OPTIONAL MATCH` becomes its own left-join block (applied after them). A
    /// required `MATCH`/`UNWIND` *after* an `OPTIONAL MATCH` is deferred.
    fn bind_reading(&mut self, reading: &[ast::ReadingClause]) -> Result<BoundReading> {
        let mut predicates = Vec::new();
        let mut match_ = BoundMatch::default();
        let mut unwind = Vec::new();
        let mut optionals = Vec::new();
        let mut table_func_scans = Vec::new();
        let mut load_scan = None;
        for clause in reading {
            match clause {
                ast::ReadingClause::LoadFrom(l) => {
                    let hinted_format = hinted_input_format(&l.path, &l.options)?;
                    validate_columnar_input_options(hinted_format, &l.options, false)?;
                    let mut options = bind_csv_options(&l.options)?;
                    let spellings: Vec<String> = std::iter::once(&l.path)
                        .chain(l.extra_paths.iter())
                        .cloned()
                        .collect();
                    let resolver = FileResolverConfig {
                        base_dir: self.session_config.base_dir.clone(),
                        home_directory: self.session_config.home_directory.clone(),
                        file_search_path: self.session_config.file_search_path.clone(),
                    };
                    let resolved =
                        resolve_files(&spellings, &resolver, options.file_format.as_deref())?;
                    let format = resolved[0].format;
                    validate_columnar_input_options(format, &l.options, false)?;
                    let paths: Vec<String> = resolved
                        .iter()
                        .map(|file| file.path.to_string_lossy().into_owned())
                        .collect();
                    let source_schema = if format == FileFormat::Csv {
                        None
                    } else {
                        let inspect = self.session_config.file_schema_resolver.ok_or_else(|| {
                            Error::not_implemented(format!(
                                "{format:?} metadata inspection is unavailable in this binder context."
                            ))
                        })?;
                        Some(inspect(format, &paths)?)
                    };
                    // Every file must share the sniffed arity (C++ "Number of
                    // columns mismatch." across a multi-file / glob source).
                    if format == FileFormat::Csv && paths.len() > 1 {
                        let arity0 =
                            koko_common::csv_dialect::sniffed_arity(&paths[0], &options, None);
                        for p in &paths[1..] {
                            let a = koko_common::csv_dialect::sniffed_arity(p, &options, None);
                            if let (Some(n), Some(m)) = (arity0, a) {
                                if n != m {
                                    return Err(Error::binder(format!(
                                        "Number of columns mismatch. Expected {n} but got {m}."
                                    )));
                                }
                            }
                        }
                    }
                    // The sniffing/column binding below use the first file.
                    let l = &ast::LoadFromClause {
                        headers: l.headers.clone(),
                        path: paths[0].clone(),
                        extra_paths: Vec::new(),
                        options: l.options.clone(),
                        where_clause: l.where_clause.clone(),
                    };
                    // Register each output column as a scalar variable, so a following
                    // WHERE/RETURN/CREATE/MATCH binds against it (the WHERE and MATCH
                    // that follow are bound afterwards, like a table-func scan).
                    let (columns, col_names, bare) = match (
                        l.headers.as_ref(),
                        source_schema.as_ref(),
                    ) {
                        // Typed `LOAD WITH HEADERS (col TYPE, …)`: declared columns.
                        // The declared count must match the source schema.
                        (Some(headers), source_schema) => {
                            let actual = source_schema.map(Vec::len).or_else(|| {
                                koko_common::csv_dialect::sniffed_arity(
                                    &l.path,
                                    &options,
                                    Some(headers.len()),
                                )
                            });
                            if actual.is_some_and(|actual| actual != headers.len()) {
                                return Err(Error::binder(format!(
                                    "Number of columns mismatch. Expected {} but got {}.",
                                    headers.len(),
                                    actual.unwrap()
                                )));
                            }
                            let mut columns = Vec::with_capacity(headers.len());
                            let mut col_names = Vec::with_capacity(headers.len());
                            for (index, (name, type_name)) in headers.iter().enumerate() {
                                let ty = self.resolve_ddl_type(type_name)?;
                                if let Some(source_schema) = source_schema {
                                    let (_, actual_ty) = &source_schema[index];
                                    if !actual_ty.to_string().eq_ignore_ascii_case(&ty.to_string())
                                    {
                                        return Err(Error::binder(format!(
                                            "{name} has data type {actual_ty} but {ty} was expected."
                                        )));
                                    }
                                }
                                let var = self.add_scalar_var(name.clone(), ty.clone());
                                columns.push((var, ty));
                                col_names.push(name.clone());
                            }
                            (columns, col_names, false)
                        }
                        (None, Some(source_schema)) => {
                            let mut columns = Vec::with_capacity(source_schema.len());
                            let mut col_names = Vec::with_capacity(source_schema.len());
                            for (index, (name, ty)) in source_schema.iter().enumerate() {
                                let var = self.add_scalar_var(name.clone(), ty.clone());
                                let positional = format!("column{index}");
                                if !self
                                    .scope
                                    .keys()
                                    .any(|bound| bound.eq_ignore_ascii_case(&positional))
                                {
                                    self.scope.insert(positional, var);
                                }
                                columns.push((var, ty.clone()));
                                col_names.push(name.clone());
                            }
                            (columns, col_names, false)
                        }
                        (None, None) => {
                            // Bare LOAD: the header decision is C++'s type-conflict
                            // sniff (audit W3 — a header-ish looking first data row
                            // must NOT be dropped). The decision is recorded into
                            // options.header so the scan honors it deterministically.
                            let col_names = match options.header {
                                Some(h) => koko_common::csv_dialect::sniff_columns(
                                    std::path::Path::new(&l.path),
                                    options.delimiter,
                                    options.quote,
                                    options.escape,
                                    options.auto_detect,
                                    options.parallel,
                                    h,
                                )?,
                                None => {
                                    let (h, names) = koko_common::csv_dialect::sniff_bare_columns(
                                        std::path::Path::new(&l.path),
                                        &options,
                                    )?;
                                    options.header = Some(h);
                                    names
                                }
                            };
                            // A header cell may carry a TYPED name
                            // (`age:UINT64`, `height:HEIGHT` — UDT aliases
                            // resolve): the declared type wins; other columns
                            // type from the sampled data (C++ sniffer:
                            // INT64/DECIMAL/DOUBLE/BOOL/temporal/UUID, else
                            // STRING).
                            let mut declared: Vec<Option<LogicalType>> =
                                Vec::with_capacity(col_names.len());
                            let mut col_names: Vec<String> = col_names
                                .into_iter()
                                .map(|n| match n.rsplit_once(':') {
                                    Some((base, ty_str)) if options.header == Some(true) => {
                                        match self.resolve_ddl_type(ty_str.trim()) {
                                            Ok(ty) => {
                                                declared.push(Some(ty));
                                                base.trim().to_string()
                                            }
                                            Err(_) => {
                                                declared.push(None);
                                                n
                                            }
                                        }
                                    }
                                    _ => {
                                        declared.push(None);
                                        n
                                    }
                                })
                                .collect();
                            let mut used_names = HashSet::new();
                            for name in &mut col_names {
                                let base = name.clone();
                                let mut suffix = 1usize;
                                while !used_names.insert(name.clone()) {
                                    *name = format!("{base}_{suffix}");
                                    suffix += 1;
                                }
                            }
                            let col_types = koko_common::csv_dialect::sniff_column_types(
                                std::path::Path::new(&l.path),
                                &options,
                                options.header == Some(true),
                                col_names.len(),
                            )?;
                            let mut columns = Vec::with_capacity(col_names.len());
                            for ((name, sniffed), decl) in
                                col_names.iter().zip(col_types).zip(declared)
                            {
                                let ty = decl.unwrap_or(sniffed);
                                let var = self.add_scalar_var(name.clone(), ty.clone());
                                columns.push((var, ty));
                            }
                            (columns, col_names, true)
                        }
                    };
                    load_scan = Some(BoundLoadScan {
                        columns,
                        col_names,
                        path: l.path.clone(),
                        paths,
                        format,
                        options,
                        bare,
                    });
                    // A `WHERE` on the LOAD filters the loaded rows; it references the
                    // columns just registered, so bind it now (like a table-func scan).
                    if let Some(w) = &l.where_clause {
                        if !self.has_unbound_param(w) {
                            predicates.push(self.bind_expr(w)?);
                        }
                    }
                }
                ast::ReadingClause::TableFuncScan(t) => {
                    let func = bound_table_func(t.func);
                    // Apply the YIELD renames: every yielded name must be one
                    // of the function's output columns, and the clause must
                    // cover ALL of them (C++ rules); the output names then get
                    // registered as scalar variables, colliding names erroring
                    // like any redefinition.
                    let schema =
                        table_func_schema(self.catalog, func, t.arg.as_deref(), &t.extra_args)?;
                    // YIELD is POSITIONAL: item i must name the function's
                    // i-th output column (a reorder is "Unknown … name"), and
                    // the list must cover every column; aliases rename.
                    let out_names: Vec<String> = if t.yield_items.is_empty() {
                        schema.iter().map(|(n, _)| n.clone()).collect()
                    } else {
                        if t.yield_items.len() > schema.len() {
                            return Err(Error::binder(
                                "The number of variables in the yield clause exceeds the number \
                                 of output variables of the table function."
                                    .to_string(),
                            ));
                        }
                        for (i, (col, _)) in t.yield_items.iter().enumerate() {
                            let matches_pos = schema
                                .get(i)
                                .is_some_and(|(n, _)| n.eq_ignore_ascii_case(col));
                            if !matches_pos {
                                return Err(Error::binder(format!(
                                    "Unknown table function output variable name: {col}."
                                )));
                            }
                        }
                        if t.yield_items.len() != schema.len() {
                            return Err(Error::binder(
                                "Output variables must all appear in the yield clause.".to_string(),
                            ));
                        }
                        t.yield_items
                            .iter()
                            .map(|(col, alias)| alias.clone().unwrap_or_else(|| col.clone()))
                            .collect()
                    };
                    let mut columns = Vec::with_capacity(out_names.len());
                    for (name, (_, ty)) in out_names.into_iter().zip(schema.iter()) {
                        if self.scope.keys().any(|k| k.eq_ignore_ascii_case(&name)) {
                            return Err(Error::binder(format!("Variable {name} already exists.")));
                        }
                        columns.push((self.add_scalar_var(name, ty.clone()), ty.clone()));
                    }
                    // The WHERE binds *after* (it references these columns and
                    // anything bound earlier in the part).
                    if let Some(w) = &t.where_clause {
                        if !self.has_unbound_param(w) {
                            predicates.push(self.bind_expr(w)?);
                        }
                    }
                    table_func_scans.push(BoundTableFuncScan {
                        func,
                        arg: t.arg.clone(),
                        columns,
                    });
                }
                ast::ReadingClause::Match(m) if m.optional => {
                    let mut opt_match = BoundMatch::default();
                    let mut opt_preds = Vec::new();
                    let pre_scope: std::collections::HashSet<String> =
                        self.scope.keys().cloned().collect();
                    // Variables bound before this OPTIONAL clause (id < boundary) are
                    // the left rows it joins against; narrowing their endpoints would
                    // reduce the outer driving cardinality (fixes match7.S25). Fresh
                    // endpoints introduced within the pattern still narrow normally.
                    let boundary = self.vars.len();
                    for pe in &m.patterns {
                        self.bind_match_pattern(
                            pe,
                            &mut opt_match,
                            &mut opt_preds,
                            Some(boundary),
                        )?;
                    }
                    if let Some(hint) = &m.hint {
                        self.validate_join_hint(hint, m, &pre_scope)?;
                    }
                    if let Some(w) = &m.where_clause {
                        if !self.has_unbound_param(w) {
                            opt_preds.push(self.bind_expr(w)?);
                        }
                    }
                    optionals.push(BoundOptionalMatch {
                        match_: opt_match,
                        where_predicate: combine_and(opt_preds),
                    });
                }
                ast::ReadingClause::Match(m) => {
                    let pre_scope: std::collections::HashSet<String> =
                        self.scope.keys().cloned().collect();
                    for pe in &m.patterns {
                        self.bind_match_pattern(pe, &mut match_, &mut predicates, None)?;
                    }
                    if let Some(hint) = &m.hint {
                        self.validate_join_hint(hint, m, &pre_scope)?;
                    }
                    if let Some(w) = &m.where_clause {
                        if !self.has_unbound_param(w) {
                            predicates.push(self.bind_expr(w)?);
                        }
                    }
                }
                ast::ReadingClause::Unwind(u) => {
                    // Redefining an in-scope variable is the C++ binder error
                    // (names compare case-insensitively).
                    if self.scope.keys().any(|k| k.eq_ignore_ascii_case(&u.var)) {
                        return Err(Error::binder(format!("Variable {} already exists.", u.var)));
                    }
                    if !optionals.is_empty() {
                        return Err(Error::not_implemented(
                            "UNWIND after OPTIONAL MATCH is not supported in this phase"
                                .to_string(),
                        ));
                    }
                    let list = self.bind_expr(&u.expr)?;
                    let elem_ty = match list.ty() {
                        LogicalType::List(inner) | LogicalType::Array(inner, _) => *inner,
                        // A NULL / untyped expression unwinds to zero rows. A
                        // STRING stays permissive: bare-LOAD columns type as
                        // STRING here even when the cells hold list text (C++
                        // sniffs them as LIST — see the LOAD typing gap). Any
                        // other scalar is the C++ bind error.
                        LogicalType::Any | LogicalType::String => LogicalType::Any,
                        other => {
                            return Err(Error::binder(format!(
                                "{} has data type {other} but LIST was expected.",
                                expr_name(&u.expr)
                            )));
                        }
                    };
                    let var = self.add_unwind_var(u.var.clone(), elem_ty);
                    unwind.push(BoundUnwind { var, list });
                }
            }
        }
        // A table-function scan combines with MATCH (cross product, planned as
        // the base) but not with UNWIND/OPTIONAL in this phase.
        if !table_func_scans.is_empty() && (!unwind.is_empty() || !optionals.is_empty()) {
            return Err(Error::not_implemented(
                "a table-function CALL combined with UNWIND/OPTIONAL MATCH is not supported \
                 in this phase"
                    .to_string(),
            ));
        }
        // A `LOAD FROM` is a base source. It may be followed by a MATCH (the
        // LOAD … MATCH … CREATE rel-load pattern, composed by the planner), but not
        // combined with another leaf source.
        if load_scan.is_some() && !table_func_scans.is_empty() {
            return Err(Error::not_implemented(
                "LOAD FROM combined with a table-function CALL is not supported".to_string(),
            ));
        }
        Ok(BoundReading {
            match_,
            table_func_scans,
            load_scan,
            unwind,
            where_predicate: combine_and(predicates),
            optionals,
        })
    }

    /// Bind a part's updating clauses (`CREATE`). Returns `None` if there are none.
    /// Bind a part's updating clauses (`CREATE`/`SET`/`DELETE`) in order.
    fn bind_updating(&mut self, updating: &[ast::UpdatingClause]) -> Result<Vec<BoundUpdate>> {
        let mut out = Vec::with_capacity(updating.len());
        for clause in updating {
            match clause {
                ast::UpdatingClause::Create(c) => {
                    let mut create = BoundCreate::default();
                    for pe in &c.patterns {
                        self.bind_create_pattern(pe, &mut create)?;
                    }
                    // Every pattern part resolved to an already-bound variable —
                    // nothing to create is a bind error (C++ bindCreateClause).
                    if create.nodes.is_empty() && create.rels.is_empty() {
                        return Err(Error::binder(
                            "Cannot resolve any node or relationship to create.".to_string(),
                        ));
                    }
                    out.push(BoundUpdate::Create(create));
                }
                ast::UpdatingClause::Set(s) => out.push(BoundUpdate::Set(self.bind_set(s)?)),
                ast::UpdatingClause::Delete(d) => {
                    out.push(BoundUpdate::Delete(self.bind_delete(d)?))
                }
                ast::UpdatingClause::Merge(m) => {
                    out.push(BoundUpdate::Merge(Box::new(self.bind_merge(m)?)))
                }
            }
        }
        Ok(out)
    }

    /// Bind a `SET` clause.
    fn bind_set(&self, set: &ast::SetClause) -> Result<BoundSet> {
        self.bind_set_items(&set.items)
    }

    /// Bind a list of `SET` assignments (shared by `SET` and `MERGE`'s
    /// `ON CREATE`/`ON MATCH SET`): resolve each target and reject PK writes.
    fn bind_set_items(&self, set_items: &[ast::SetItem]) -> Result<BoundSet> {
        let mut items = Vec::with_capacity(set_items.len());
        for item in set_items {
            let mut value = self.bind_expr(&item.value)?;
            let target = match &item.target {
                ast::SetTarget::Property { var, name } => {
                    let id = self.set_target_var(var)?;
                    let info = &self.vars[id.0 as usize];
                    let dynamic = info.property(name).is_none() && self.is_any_var(id);
                    if dynamic {
                        BoundSetTarget::DynamicProperty {
                            var: id,
                            prop: name.clone(),
                        }
                    } else {
                        let prop = info.property(name).ok_or_else(|| {
                            Error::binder(format!("Cannot find property {name} for {var}."))
                        })?;
                        let prop_name = prop.name.clone();
                        let prop_ty = prop.ty.clone();
                        adopt_struct_field_names(&mut value, &prop_ty);
                        assignable_or_err(&value, &prop_ty, &expr_name(&item.value))?;
                        value = self.coerce_to(value, &prop_ty);
                        // A primary-key column cannot be updated. For polymorphic nodes,
                        // this only rejects candidate tables whose own PK has this name;
                        // tables lacking the property are skipped by the runtime setter.
                        for &t in info.node_tables() {
                            let nt = self.catalog.node_table(t).unwrap();
                            if nt.primary_key_column().name.eq_ignore_ascii_case(name) {
                                return Err(Error::binder(format!(
                                    "Cannot set property {name} in table {} because it is used as \
                                     primary key. Try delete and then insert.",
                                    nt.name
                                )));
                            }
                        }
                        BoundSetTarget::Property {
                            var: id,
                            prop: prop_name,
                        }
                    }
                }
                ast::SetTarget::Var(var) => {
                    let id = self.set_target_var(var)?;
                    // A whole-map `SET a = {…}` is C++'s `SET a = properties`
                    // surface: each listed key is a property write, and unlisted
                    // properties are preserved. Validate literal keys at bind time
                    // against the target's current (possibly endpoint-pruned) property
                    // set, and reject primary-key writes just like `SET a.pk = …`.
                    if let ast::Expr::Struct(fields) = &item.value {
                        if self.is_any_var(id) {
                            items.push(BoundSetItem {
                                target: BoundSetTarget::Var { var: id },
                                value,
                            });
                            continue;
                        }
                        let info = &self.vars[id.0 as usize];
                        for (k, _) in fields {
                            if info.property(k).is_none() {
                                return Err(Error::binder(format!(
                                    "Cannot find property {k} for {var}."
                                )));
                            }
                        }
                        for &t in info.node_tables() {
                            let nt = self.catalog.node_table(t).unwrap();
                            let pk = &nt.primary_key_column().name;
                            if fields.iter().any(|(k, _)| k.eq_ignore_ascii_case(pk)) {
                                return Err(Error::binder(format!(
                                    "Cannot set property {pk} in table {} because it is used as \
                                     primary key. Try delete and then insert.",
                                    nt.name
                                )));
                            }
                        }
                        if let BoundExpr::Struct {
                            fields: bound_fields,
                            ..
                        } = &value
                        {
                            for (k, v) in bound_fields {
                                let prop = info.property(k).unwrap();
                                // Render the field's *value* expression (C++ reports the
                                // assigned expression, not the property name).
                                let ename = fields
                                    .iter()
                                    .find(|(fk, _)| fk.eq_ignore_ascii_case(k))
                                    .map(|(_, ve)| expr_name(ve))
                                    .unwrap_or_else(|| k.clone());
                                assignable_or_err(v, &prop.ty, &ename)?;
                            }
                        }
                    }
                    BoundSetTarget::Var { var: id }
                }
            };
            items.push(BoundSetItem { target, value });
        }
        Ok(BoundSet { items })
    }

    /// Resolve a `SET` target variable, requiring it be a (non-recursive) node or
    /// relationship.
    fn set_target_var(&self, name: &str) -> Result<VarId> {
        let id = self.lookup_var(name)?;
        let info = &self.vars[id.0 as usize];
        if info.is_recursive() || !matches!(info.kind, VarKind::Node { .. } | VarKind::Rel { .. }) {
            return Err(Error::binder(format!(
                "Cannot set expression {name} with type VARIABLE. Expect node or rel pattern."
            )));
        }
        Ok(id)
    }

    /// Bind a `[DETACH] DELETE`: each target must be a node/relationship variable.
    fn bind_delete(&self, del: &ast::DeleteClause) -> Result<BoundDelete> {
        let mut vars = Vec::with_capacity(del.exprs.len());
        for e in &del.exprs {
            let ast::Expr::Variable(name) = e else {
                // C++ types the rejection by expression kind (PROPERTY etc.).
                return Err(Error::binder(format!(
                    "Cannot delete expression {} with type {}. Expect node or rel pattern.",
                    expr_name(e),
                    ast_expr_kind(e)
                )));
            };
            let id = self.lookup_var(name)?;
            let info = &self.vars[id.0 as usize];
            match &info.kind {
                VarKind::Node { .. } => {}
                VarKind::Rel {
                    recursive: Some(_), ..
                } => {
                    return Err(Error::binder(format!(
                        "Cannot delete expression {name} with type VARIABLE. Expect node or rel pattern."
                    )));
                }
                VarKind::Rel { directed, .. } => {
                    if del.detach {
                        return Err(Error::binder(
                            "Detach delete on rel tables is not supported.".to_string(),
                        ));
                    }
                    if !*directed {
                        return Err(Error::binder(
                            "Delete undirected rel is not supported.".to_string(),
                        ));
                    }
                }
                VarKind::Path { .. } | VarKind::Scalar { .. } => {
                    return Err(Error::binder(format!(
                        "Cannot delete expression {name} with type VARIABLE. Expect node or rel pattern."
                    )));
                }
            }
            vars.push(id);
        }
        Ok(BoundDelete {
            vars,
            detach: del.detach,
        })
    }

    fn bind_match_pattern(
        &mut self,
        pe: &ast::PatternElement,
        match_: &mut BoundMatch,
        predicates: &mut Vec<BoundExpr>,
        opt_boundary: Option<usize>,
    ) -> Result<()> {
        let head = self.bind_match_node(&pe.head)?;
        match_.node_vars.push(head);
        self.inline_predicates(head, &pe.head.properties, predicates)?;
        self.any_label_predicates(head, &pe.head.labels, predicates);

        let mut prev = head;
        let mut segments = Vec::with_capacity(pe.chains.len());
        for (rel, node) in &pe.chains {
            let node_var = self.bind_match_node(node)?;
            let rel_var = self.bind_match_rel(rel, prev, node_var, opt_boundary)?;
            self.any_label_predicates(rel_var, &rel.labels, predicates);
            self.any_label_predicates(node_var, &node.labels, predicates);
            self.inline_predicates(node_var, &node.properties, predicates)?;
            // Inline properties on a recursive rel (`[e:knows* {date: …}]`) are a
            // per-step filter over each rel in the path (handled with the lambda
            // predicate), not a single-rel equality — deferred.
            if rel.recursive.is_none() {
                self.inline_predicates(rel_var, &rel.properties, predicates)?;
            }
            match_.node_vars.push(node_var);
            match_.rel_vars.push(rel_var);
            segments.push((rel_var, node_var));
            prev = node_var;
        }

        // A named path `p = (…)` binds `p` to the whole pattern as a
        // `RECURSIVE_REL` value (assembled by the planner/processor).
        if let Some(name) = &pe.name {
            let path = self.add_var(
                Some(name.clone()),
                VarKind::Path { head, segments },
                Vec::new(),
            );
            match_.path_vars.push(path);
        }
        Ok(())
    }

    /// Bind a node pattern in MATCH context (reusing an in-scope variable).
    fn bind_match_node(&mut self, np: &ast::NodePattern) -> Result<VarId> {
        if let Some(name) = &np.var {
            if let Some(&id) = self.scope.get(name) {
                if !self.vars[id.0 as usize].is_node() {
                    return Err(Error::binder(format!(
                        "Cannot bind {name} as node pattern."
                    )));
                }
                // Re-stating labels on an in-scope node *unions* them into the
                // candidate set (Kùzu semantics): `MATCH (a:A:B) MATCH (a:C)`
                // treats `a` as `A ∪ B ∪ C`.
                if !np.labels.is_empty() {
                    let add = self.resolve_node_tables(&np.labels)?;
                    self.union_node_tables(id, &add);
                }
                return Ok(id);
            }
        }
        let tables = self.resolve_node_tables(&np.labels)?;
        // An unlabeled pattern over an empty database has no representative table;
        // its label is empty and it scans nothing.
        let label = tables
            .first()
            .and_then(|&t| self.catalog.node_table(t))
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let props = self.node_props_union(&tables);
        Ok(self.add_var(np.var.clone(), VarKind::Node { tables, label }, props))
    }

    /// Add tables to an in-scope node variable's candidate set (re-match union).
    fn union_node_tables(&mut self, var: VarId, add: &[TableId]) {
        let info = &self.vars[var.0 as usize];
        if !matches!(info.kind, VarKind::Node { .. }) {
            return;
        }
        let mut tables = info.node_tables().to_vec();
        let mut changed = false;
        for &t in add {
            if !tables.contains(&t) {
                tables.push(t);
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let label = self.catalog.node_table(tables[0]).unwrap().name.clone();
        let props = self.node_props_union(&tables);
        let info = &mut self.vars[var.0 as usize];
        info.kind = VarKind::Node { tables, label };
        info.properties = props;
    }

    fn bind_match_rel(
        &mut self,
        rp: &ast::RelPattern,
        left: VarId,
        right: VarId,
        opt_boundary: Option<usize>,
    ) -> Result<VarId> {
        let (src, dst, directed) = match rp.direction {
            ast::Direction::Right => (left, right, true),
            ast::Direction::Left => (right, left, true),
            ast::Direction::Both => (left, right, false),
        };
        if let Some(name) = &rp.var {
            if let Some(&id) = self.scope.get(name) {
                let expected = if rp.recursive.is_some() {
                    "RECURSIVE_REL"
                } else {
                    "REL"
                };
                let actual = self.var_data_type_name(id);
                if actual != expected {
                    return Err(Error::binder(format!(
                        "{name} has data type {actual} but {expected} was expected."
                    )));
                }
                return Err(Error::binder(format!(
                    "Bind relationship {name} to relationship with same name is not supported."
                )));
            }
        }
        // A variable-length / recursive relationship binds differently: its value
        // is a `RECURSIVE_REL` (no per-property columns), the endpoints are *not*
        // narrowed (intermediate nodes may use any table along the path), and it
        // carries length bounds + search semantics.
        if let Some(rec) = &rp.recursive {
            return self.bind_recursive_rel(rp, rec, src, dst, directed);
        }
        // Candidate relationship tables (named types, or all when unlabeled),
        // then keep only those that connect the endpoints given their current
        // node-table sets and the arrow direction. Accumulate the node tables
        // each endpoint is thereby pinned to (the union over kept rels).
        let candidates = self.resolve_rel_tables(&rp.labels)?;
        let declared = candidates.clone();
        let src_tables = self.vars[src.0 as usize].node_tables().to_vec();
        let dst_tables = self.vars[dst.0 as usize].node_tables().to_vec();
        let mut kept = Vec::new();
        let mut src_narrow = Vec::new();
        let mut dst_narrow = Vec::new();
        for &r in &candidates {
            let rt = self.catalog.rel_table(r).unwrap();
            // A multi-pair rel table connects if ANY of its FROM-TO pairs does;
            // each connecting pair contributes to the endpoint narrowing.
            let mut connects = false;
            for &(rf, rtt) in &rt.pairs {
                let fwd = src_tables.contains(&rf) && dst_tables.contains(&rtt);
                // An undirected pattern also matches the reverse orientation.
                let bwd = !directed && src_tables.contains(&rtt) && dst_tables.contains(&rf);
                if fwd {
                    push_unique(&mut src_narrow, rf);
                    push_unique(&mut dst_narrow, rtt);
                }
                if bwd {
                    push_unique(&mut src_narrow, rtt);
                    push_unique(&mut dst_narrow, rf);
                }
                connects |= fwd || bwd;
            }
            if connects {
                kept.push(r);
            }
        }
        // When some candidate connects, narrow the endpoints to the tables those
        // rels pin them to and bind only the connecting rels. When NONE connects
        // (an over-constrained / wrong-label pattern, e.g. a node forced to be two
        // disjoint labels), the pattern is simply unsatisfiable: bind the declared
        // candidates without narrowing and let execution yield zero rows — Kùzu
        // returns an empty result here, not an error.
        // Storage-direction constraints (C++ binder): a pattern matching both
        // fwd-only and bwd-only tables has no common scan direction; a
        // bwd-only table cannot be queried at all.
        {
            use koko_catalog::RelStorageDirection as Dir;
            let name = rp.var.as_deref().unwrap_or("");
            let dirs: Vec<Dir> = kept
                .iter()
                .filter_map(|&r| self.catalog.rel_table(r).map(|rt| rt.storage_direction))
                .collect();
            let has_fwd_only = dirs.contains(&Dir::Fwd);
            let has_bwd_only = dirs.contains(&Dir::Bwd);
            if has_fwd_only && has_bwd_only {
                return Err(Error::binder(format!(
                    "There are no common storage directions among the rel tables matched by \
                     pattern '{name}' (some tables have storage direction 'fwd' while others \
                     have storage direction 'bwd'). Scanning different tables matching the \
                     same pattern in different directions is currently unsupported."
                )));
            }
            if has_bwd_only {
                return Err(Error::binder(format!(
                    "Querying table matched in rel pattern '{name}' with bwd-only storage \
                     direction isn't supported."
                )));
            }
        }
        // C++ rejects an UNDIRECTED pattern when any matched rel table is not
        // stored BOTH ways (binder.cpp validateRelDirection) — the bwd scan a
        // reversed orientation needs would be missing.
        if !directed {
            let any_partial = kept.iter().any(|&r| {
                self.catalog.rel_table(r).is_some_and(|rt| {
                    rt.storage_direction != koko_catalog::RelStorageDirection::Both
                })
            });
            if any_partial {
                let name = rp.var.as_deref().unwrap_or("");
                return Err(Error::binder(format!(
                    "Undirected rel pattern '{name}' has at least one matched rel table with \
                     storage type 'fwd' or 'bwd'. Undirected rel patterns are only supported \
                     if every matched rel table has storage type 'both'."
                )));
            }
        }
        let tables = if kept.is_empty() {
            candidates
        } else {
            // Don't narrow an endpoint bound by a scope enclosing an OPTIONAL MATCH:
            // narrowing only prunes the scan (the runtime extend still filters by real
            // adjacency, so dropping the prune is result-safe), but for the optional's
            // left rows it would wrongly reduce the driving cardinality (match7.S25).
            let outer_bound = |v: VarId| matches!(opt_boundary, Some(b) if (v.0 as usize) < b);
            if !outer_bound(src) {
                self.narrow_node_to_set(src, &src_narrow);
            }
            if !outer_bound(dst) {
                self.narrow_node_to_set(dst, &dst_narrow);
            }
            kept
        };
        // An unlabeled pattern over a database with no rel tables has no
        // representative table; its label is empty and it extends nothing.
        let label = tables
            .first()
            .and_then(|&t| self.catalog.rel_table(t))
            .map(|t| t.name.clone())
            .unwrap_or_default();
        // Properties resolve against the *declared* candidate set (labels as
        // written), not the connectivity-kept subset: C++ binds `e.year` on
        // `-[e:knows|:studyAt]->` even when the endpoints keep only `knows`,
        // pruning the write / reading NULL where the column is absent.
        let props = self.rel_props_union(&declared);
        Ok(self.add_var(
            rp.var.clone(),
            VarKind::Rel {
                tables,
                label,
                src,
                dst,
                directed,
                recursive: None,
            },
            props,
        ))
    }

    /// Bind a variable-length / recursive relationship: resolve its candidate rel
    /// tables (named, or all when unlabeled), normalize + validate the length
    /// bounds, and record the search mode/semantic. Endpoints keep their declared
    /// labels (only the path's two ends are constrained; intermediates are free).
    fn bind_recursive_rel(
        &mut self,
        rp: &ast::RelPattern,
        rec: &ast::RecursiveInfo,
        src: VarId,
        dst: VarId,
        directed: bool,
    ) -> Result<VarId> {
        let name = rp.var.clone().unwrap_or_default();
        let max_depth = self.max_recursive_depth;
        let lower = rec.bounds.0.unwrap_or(1);
        let upper = rec.bounds.1.unwrap_or(max_depth);
        if lower > upper {
            return Err(Error::binder(format!(
                "Lower bound of rel {name} is greater than upperBound."
            )));
        }
        // SHORTEST / ALL SHORTEST require a lower bound of exactly 1 (C++).
        if !matches!(rec.mode, ast::RecursiveMode::All) && lower != 1 {
            return Err(Error::binder(
                "Lower bound of shortest/all_shortest path must be 1.".to_string(),
            ));
        }
        if upper > max_depth {
            return Err(Error::binder(format!(
                "Upper bound of rel {name} exceeds maximum: {max_depth}."
            )));
        }
        let tables = self.resolve_rel_tables(&rp.labels)?;
        let label = tables
            .first()
            .and_then(|&t| self.catalog.rel_table(t))
            .map(|t| t.name.clone())
            .unwrap_or_default();
        let filter = self.bind_recursive_filter(rp, rec)?;
        // A (ALL) WSHORTEST weight column resolves against the rel table(s): it
        // must exist and NOT be INT128 (C++ rejects that width).
        let weight = match &rec.weight_col {
            Some(col) => {
                let ty = tables
                    .iter()
                    .find_map(|&t| {
                        self.catalog.rel_table(t).and_then(|rt| {
                            rt.columns
                                .iter()
                                .find(|c| c.name.eq_ignore_ascii_case(col))
                                .map(|c| c.ty.clone())
                        })
                    })
                    .ok_or_else(|| {
                        Error::binder(format!("Cannot find property {col} for {name}."))
                    })?;
                // Supported weights are the ≤64-bit integers plus FLOAT/DOUBLE;
                // INT128/UINT128 and DECIMAL are rejected (with the type name).
                let supported = matches!(
                    ty,
                    LogicalType::Int(k) if k.byte_width() <= 8
                ) || matches!(ty, LogicalType::Double | LogicalType::Float);
                if !supported {
                    return Err(Error::binder(format!(
                        "{ty} weight type is not supported for weighted shortest path."
                    )));
                }
                Some(col.clone())
            }
            None => None,
        };
        let spec = RecursiveSpec {
            lower,
            upper,
            mode: bind_recursive_mode(rec.mode),
            semantic: bind_path_semantic(rec.semantic),
            filter,
            weight,
        };
        // A recursive rel exposes no per-property columns (its value is assembled).
        Ok(self.add_var(
            rp.var.clone(),
            VarKind::Rel {
                tables,
                label,
                src,
                dst,
                directed,
                recursive: Some(Box::new(spec)),
            },
            Vec::new(),
        ))
    }

    /// Bind a recursive pattern's per-step filter: the `(r, n | WHERE …)` lambda
    /// predicate plus any inline `{prop: …}` (folded into the relationship part as
    /// `r.prop = …`). The bound predicate's top-level `AND` conjuncts are split by
    /// which lambda parameter they reference (a node-referencing conjunct gates
    /// intermediates; the rest gate every relationship).
    fn bind_recursive_filter(
        &self,
        rp: &ast::RelPattern,
        rec: &ast::RecursiveInfo,
    ) -> Result<Option<RecursiveFilter>> {
        let lambda_pred = rec.lambda.as_ref().and_then(|l| l.predicate.as_ref());
        // C++ validates the node projection list before the rel list (with both
        // invalid, the node item is blamed — oracle-verified).
        let node_proj = rec
            .lambda
            .as_ref()
            .and_then(|l| l.node_projection.as_ref())
            .map(|es| projection_names(es))
            .transpose()?;
        let rel_proj = rec
            .lambda
            .as_ref()
            .and_then(|l| l.rel_projection.as_ref())
            .map(|es| projection_names(es))
            .transpose()?;
        let any_dynamic_labels = self.catalog.any_tables().is_some() && !rp.labels.is_empty();
        if lambda_pred.is_none()
            && rp.properties.is_empty()
            && rel_proj.is_none()
            && node_proj.is_none()
            && !any_dynamic_labels
        {
            return Ok(None);
        }
        let (rel_param, node_param) = match &rec.lambda {
            Some(l) => (l.rel_var.clone(), l.node_var.clone()),
            None => ("__rel".to_string(), "__node".to_string()),
        };

        let mut rel_parts = Vec::new();
        let mut node_parts = Vec::new();

        if let Some(pred) = lambda_pred {
            // Bind the predicate with `rel_param`/`node_param` as lambda variables.
            let depth = self.lambda_params.borrow().len();
            self.lambda_params
                .borrow_mut()
                .push((rel_param.clone(), LogicalType::Any));
            self.lambda_params
                .borrow_mut()
                .push((node_param.clone(), LogicalType::Any));
            let bound = self.bind_expr(pred);
            self.lambda_params.borrow_mut().truncate(depth);
            let bound = bound?;
            // A lifted subquery/sequence column can't be evaluated inside the
            // per-step lambda (it runs against an empty chunk — audit C2), so
            // reject cleanly here. C++ answers these; real support comes with
            // the scoped subquery-lifting redesign (M2). Ledger: DIVERGENCES.md.
            if bound.contains_lifted() {
                return Err(Error::binder(
                    "EXISTS/COUNT subqueries and sequence functions are not supported in \
                     a recursive relationship's per-step filter."
                        .to_string(),
                ));
            }
            for conj in split_and(bound) {
                let depend_on_node = mentions_lambda(&conj, &node_param);
                let depend_on_rel = mentions_lambda(&conj, &rel_param);
                if depend_on_node && depend_on_rel {
                    return Err(Error::binder(format!(
                        "Cannot evaluate {} because it depends on both {} and {}.",
                        self.bound_expr_name(&conj),
                        node_param,
                        rel_param
                    )));
                } else if depend_on_node {
                    node_parts.push(conj);
                } else {
                    rel_parts.push(conj);
                }
            }
        }

        if any_dynamic_labels {
            let label = BoundExpr::ValueProperty {
                value: Box::new(BoundExpr::LambdaVar {
                    name: rel_param.clone(),
                    ty: LogicalType::Any,
                }),
                prop: "label".to_string(),
                ty: LogicalType::String,
            };
            let mut labels = rp.labels.iter().map(|name| BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args: vec![
                    label.clone(),
                    BoundExpr::Literal(Value::String(name.clone())),
                ],
                ty: LogicalType::Bool,
            });
            if let Some(first) = labels.next() {
                rel_parts.push(labels.fold(first, |left, right| BoundExpr::Scalar {
                    op: ScalarOp::Or,
                    args: vec![left, right],
                    ty: LogicalType::Bool,
                }));
            }
        }

        // Inline `{prop: val}` ⇒ `rel_param.prop = val` on every relationship.
        for (k, v) in &rp.properties {
            let val = self.bind_expr(v)?;
            let entity = BoundExpr::LambdaVar {
                name: rel_param.clone(),
                ty: LogicalType::Any,
            };
            let lhs = if self.catalog.any_tables().is_some() {
                BoundExpr::ValueProperty {
                    value: Box::new(BoundExpr::ValueProperty {
                        value: Box::new(entity),
                        prop: "data".to_string(),
                        ty: LogicalType::Json,
                    }),
                    prop: k.clone(),
                    ty: LogicalType::Any,
                }
            } else {
                BoundExpr::ValueProperty {
                    value: Box::new(entity),
                    prop: k.clone(),
                    ty: LogicalType::Any,
                }
            };
            rel_parts.push(BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args: vec![lhs, val],
                ty: LogicalType::Bool,
            });
        }

        Ok(Some(RecursiveFilter {
            rel_param,
            node_param,
            rel_pred: combine_and(rel_parts),
            node_pred: combine_and(node_parts),
            rel_proj,
            node_proj,
        }))
    }

    /// Render a bound expression for recursive-lambda binder diagnostics. This is
    /// intentionally close to the C++ expression `toString()` form used in the
    /// oracle error for mixed node/relationship-dependent conjuncts.
    fn bound_expr_name(&self, e: &BoundExpr) -> String {
        match e {
            BoundExpr::Literal(v) => v.to_result_string(),
            BoundExpr::Parameter { name, .. } => format!("${name}"),
            BoundExpr::Column { col, .. } => format!("#{col}"),
            BoundExpr::Property { var, prop, .. } => {
                format!("{}.{}", self.vars[var.0 as usize].name, prop)
            }
            BoundExpr::ValueProperty { value, prop, .. } => {
                format!("{}.{}", self.bound_expr_name(value), prop)
            }
            BoundExpr::NodeRef { var, .. } | BoundExpr::ScalarVar { var, .. } => {
                self.vars[var.0 as usize].name.clone()
            }
            BoundExpr::LambdaVar { name, .. } => name.clone(),
            BoundExpr::Scalar { op, args, .. } => {
                let args = args
                    .iter()
                    .map(|a| self.bound_expr_name(a))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}({args})", scalar_op_name(*op))
            }
            BoundExpr::Aggregate {
                op, distinct, arg, ..
            } => {
                let distinct = if *distinct { "DISTINCT " } else { "" };
                let arg = arg
                    .as_ref()
                    .map(|a| self.bound_expr_name(a))
                    .unwrap_or_else(|| "*".to_string());
                format!(
                    "{}({distinct}{arg})",
                    format!("{op:?}").to_ascii_uppercase()
                )
            }
            // This diagnostic mirrors the source expression; omit binder-inserted
            // coercions, which C++ does not expose in its mixed-dependency error.
            BoundExpr::Cast { expr, .. } => self.bound_expr_name(expr),
            // A property extract renders as the access it came from (`n.ID`).
            BoundExpr::Call { name, args, .. }
                if name.eq_ignore_ascii_case("struct_extract") && args.len() == 2 =>
            {
                let prop = match &args[1] {
                    BoundExpr::Literal(Value::String(s)) => s.clone(),
                    other => self.bound_expr_name(other),
                };
                format!("{}.{prop}", self.bound_expr_name(&args[0]))
            }
            BoundExpr::Call { name, args, .. }
                if name.eq_ignore_ascii_case("id") && args.len() == 1 =>
            {
                format!("{}._ID", self.bound_expr_name(&args[0]))
            }
            BoundExpr::Call { name, args, .. } => {
                let args = args
                    .iter()
                    .map(|a| self.bound_expr_name(a))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}({args})", name.to_ascii_uppercase())
            }
            BoundExpr::Udf { function, args, .. } => {
                let args = args
                    .iter()
                    .map(|argument| self.bound_expr_name(argument))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}({args})", function.name.to_ascii_uppercase())
            }
            BoundExpr::List { elems, .. } => {
                let elems = elems
                    .iter()
                    .map(|a| self.bound_expr_name(a))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{elems}]")
            }
            BoundExpr::Struct { fields, .. } => {
                let fields = fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", self.bound_expr_name(v)))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{fields}}}")
            }
            BoundExpr::ListLambda {
                params, list, body, ..
            } => {
                format!(
                    "{} -> {} IN {}",
                    params.join(", "),
                    self.bound_expr_name(body),
                    self.bound_expr_name(list)
                )
            }
            BoundExpr::Subquery { id, .. } => format!("SUBQUERY#{id}"),
            BoundExpr::SequenceCall { id, .. } => format!("SEQUENCE#{id}"),
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                let mut out = String::from("CASE");
                if let Some(operand) = operand {
                    out.push(' ');
                    out.push_str(&self.bound_expr_name(operand));
                }
                for (cond, result) in branches {
                    out.push_str(" WHEN ");
                    out.push_str(&self.bound_expr_name(cond));
                    out.push_str(" THEN ");
                    out.push_str(&self.bound_expr_name(result));
                }
                if let Some(else_) = else_ {
                    out.push_str(" ELSE ");
                    out.push_str(&self.bound_expr_name(else_));
                }
                out.push_str(" END");
                out
            }
        }
    }

    fn bind_create_pattern(
        &mut self,
        pe: &ast::PatternElement,
        create: &mut BoundCreate,
    ) -> Result<()> {
        // The missing-PK check is deferred until the whole element binds: C++
        // reports a rel-property type error before a missing node PK
        // (binder_error #36, oracle order).
        let mut deferred_pk: Vec<String> = Vec::new();
        // An unlabeled CREATE endpoint adopts the adjacent rel's schema table
        // (C++: `CREATE (a)-[:T]->(b)` with T: A→Foo binds a to A, b to Foo).
        let mut hints: Vec<Option<TableId>> = vec![None; 1 + pe.chains.len()];
        for (i, (rel, _)) in pe.chains.iter().enumerate() {
            if rel.labels.is_empty() {
                continue;
            }
            let Ok(cands) = self.resolve_rel_tables(&rel.labels) else {
                continue;
            };
            let [rt] = cands.as_slice() else { continue };
            let Some(entry) = self.catalog.rel_table(*rt) else {
                continue;
            };
            let [(from, to)] = entry.pairs.as_slice() else {
                continue;
            };
            let (near, far) = match rel.direction {
                ast::Direction::Left => (*to, *from),
                _ => (*from, *to),
            };
            hints[i].get_or_insert(near);
            hints[i + 1].get_or_insert(far);
        }
        let head = self.bind_create_node(&pe.head, create, &mut deferred_pk, hints[0])?;
        let mut prev = head;
        for (i, (rel, node)) in pe.chains.iter().enumerate() {
            let node_var = self.bind_create_node(node, create, &mut deferred_pk, hints[i + 1])?;
            self.bind_create_rel(rel, prev, node_var, create)?;
            prev = node_var;
        }
        if let Some(msg) = deferred_pk.into_iter().next() {
            return Err(Error::binder(msg));
        }
        Ok(())
    }

    fn bind_create_node(
        &mut self,
        np: &ast::NodePattern,
        create: &mut BoundCreate,
        deferred_pk: &mut Vec<String>,
        table_hint: Option<TableId>,
    ) -> Result<VarId> {
        // Reference an already-bound variable instead of creating a new node.
        if let Some(name) = &np.var {
            if let Some(&id) = self.scope.get(name) {
                if !self.vars[id.0 as usize].is_node() {
                    return Err(Error::binder(format!("Variable {name} is not a node.")));
                }
                return Ok(id);
            }
        }
        let table = match table_hint {
            Some(t) if np.labels.is_empty() => t,
            _ => self.resolve_single_node_label(np.var.as_deref().unwrap_or(""), &np.labels)?,
        };
        let entry = self.catalog.node_table(table).unwrap();
        let label = entry.name.clone();
        let num_columns = entry.columns.len();
        let pk_col = entry.primary_key;
        let props = if self.catalog.is_any_node_table(table) {
            vec![
                (
                    1,
                    BoundExpr::Literal(Value::List(if np.labels.is_empty() {
                        vec![Value::String("_nodes".to_string())]
                    } else {
                        np.labels.iter().cloned().map(Value::String).collect()
                    })),
                ),
                (2, self.any_data_expr(&np.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &np.properties {
                let col = entry
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = np.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // A string *literal* bypasses the implicit-cast gate: C++ casts it
                // to the column type at execution ('30' -> 30 succeeds; 'hh' is the
                // Conversion "Cast failed. Could not convert ..." — oracle-verified).
                let string_literal = matches!(&be, BoundExpr::Literal(Value::String(_)));
                if !string_literal {
                    assignable_or_err(&be, &entry.columns[col].ty, &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, &entry.columns[col].ty)));
            }
            props
        };
        // C++ validates the primary key at *bind*: a CREATE must supply the PK
        // unless the column fills itself (SERIAL / a DEFAULT — oracle-verified:
        // `DEFAULT nextval(...)` PKs create fine). Same wording, var as typed
        // (anonymous → empty, giving the oracle's double space).
        if let Some(pk) = entry.columns.get(pk_col) {
            let provided = props.iter().any(|(c, _)| *c == pk_col);
            let self_filling =
                pk.ty == LogicalType::Serial || pk.default != koko_catalog::ColumnDefault::None;
            if !provided && !self_filling {
                deferred_pk.push(format!(
                    "Create node {} expects primary key {} as input.",
                    np.var.as_deref().unwrap_or(""),
                    pk.name
                ));
            }
        }
        let var_props = self.node_props(table);
        let var = self.add_var(
            np.var.clone(),
            VarKind::Node {
                tables: vec![table],
                label,
            },
            var_props,
        );
        create.nodes.push(BoundCreateNode {
            var,
            table,
            num_columns,
            pk_col,
            props,
        });
        Ok(var)
    }

    fn bind_create_rel(
        &mut self,
        rp: &ast::RelPattern,
        left: VarId,
        right: VarId,
        create: &mut BoundCreate,
    ) -> Result<()> {
        // A recursive rel cannot be created (matches the C++ binder).
        if rp.recursive.is_some() {
            let name = rp.var.clone().unwrap_or_default();
            return Err(Error::binder(format!(
                "Cannot create recursive rel {name}."
            )));
        }
        let (src, dst) = match rp.direction {
            ast::Direction::Right => (left, right),
            ast::Direction::Left => (right, left),
            ast::Direction::Both => {
                return Err(Error::binder(
                    "Create undirected relationship is not supported. Try create 2 directed \
                     relationships instead."
                        .to_string(),
                ));
            }
        };
        let src_tables = self.vars[src.0 as usize].node_tables().to_vec();
        let dst_tables = self.vars[dst.0 as usize].node_tables().to_vec();

        // Resolve the rel type from the endpoints, mirroring C++ `bindInsertRel` /
        // `tryPruneMultiLabeled` (bind_updating_clause.cpp). An explicitly-typed rel
        // contributes its named table(s) as candidates; an *untyped* rel (`[k]`)
        // ranges over every rel table. We keep the candidates declaring a FROM-TO
        // pair that connects the created endpoints:
        //   • exactly 1 → use it (this is how an untyped rel infers its type),
        //   • 0         → "Cannot find a valid label … that connects …",
        //   • >1        → "Create rel … with multiple rel labels is not supported."
        // For polymorphic/unlabeled endpoints this stays a candidate check; the
        // processor resolves the exact per-pair member from each row's runtime
        // internal ids and errors if a particular row is incompatible.
        let candidates: Vec<TableId> = if let Some(tables) = self.catalog.any_tables() {
            vec![tables.edges]
        } else if rp.labels.is_empty() {
            self.catalog.rel_table_ids()
        } else {
            let mut ids = Vec::with_capacity(rp.labels.len());
            for label in &rp.labels {
                let rel = self
                    .catalog
                    .rel_table_by_name(label)
                    .ok_or_else(|| Error::binder(format!("Table {label} does not exist.")))?;
                ids.push(rel.id);
            }
            ids
        };
        // C++ query-graph label analyzer (`QueryGraphLabelAnalyzer::pruneNode`,
        // query_graph_label_analyzer.cpp:76-127), which `bindInsertInfos`
        // (bind_updating_clause.cpp:131) runs with `throwOnViolate = true` over a CREATE
        // pattern *before* resolving the rel. Each endpoint's labels are pruned to those
        // valid for its position — a FROM table of some candidate for the src, a TO table
        // for the dst — and an empty result means an explicit label that cannot occupy
        // that slot. We report it here, naming the offending node and the labels its
        // position accepts, ahead of the pair-level resolution below. The src is checked
        // before the dst, matching the pattern's node order for the forward arrows this
        // guards. (MATCH uses `throwOnViolate = false`, so this lives on the create path.)
        let mut valid_from: Vec<TableId> = Vec::new();
        let mut valid_to: Vec<TableId> = Vec::new();
        for &id in &candidates {
            if let Some(r) = self.catalog.rel_table(id) {
                for &(f, t) in &r.pairs {
                    if !valid_from.contains(&f) {
                        valid_from.push(f);
                    }
                    if !valid_to.contains(&t) {
                        valid_to.push(t);
                    }
                }
            }
        }
        for (node, tables, valid) in [
            (src, &src_tables, &valid_from),
            (dst, &dst_tables, &valid_to),
        ] {
            if valid.is_empty() || tables.iter().any(|t| valid.contains(t)) {
                continue;
            }
            let name = self.vars[node.0 as usize].name.clone();
            let labels = valid
                .iter()
                .filter_map(|&t| self.catalog.node_table(t).map(|n| n.name.clone()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::binder(format!(
                "Query node {name} violates schema. Expected labels are {labels}."
            )));
        }
        // C++ `RelExpression::isMultiLabeled()`: more than one candidate rel type, or
        // a single rel group spanning more than one FROM-TO pair. Only this case takes
        // the `tryPruneMultiLabeled` path (and its diagnostics); a single-pair rel is
        // used directly, with a wrong-side endpoint caught by the query-graph label
        // analyzer ("… violates schema …", not yet ported).
        let is_multi = candidates.len() > 1
            || candidates.iter().any(|&id| {
                self.catalog
                    .rel_table(id)
                    .is_some_and(|r| r.pairs.len() > 1)
            });
        let mut matched: Vec<TableId> = Vec::new();
        for &id in &candidates {
            let connects = self
                .catalog
                .rel_table(id)
                .unwrap()
                .pairs
                .iter()
                .any(|&(f, t)| {
                    if src == dst && f != t {
                        return false;
                    }
                    src_tables.contains(&f) && dst_tables.contains(&t)
                });
            if connects {
                matched.push(id);
            }
        }
        if matched.len() != 1 {
            let rel_disp = rp.var.clone().unwrap_or_default();
            if matched.len() > 1 {
                return Err(Error::binder(format!(
                    "Create rel {rel_disp} with multiple rel labels is not supported."
                )));
            }
            // A single-pair rel whose endpoints fail the *pair* test but passed the
            // per-endpoint label check above — only a self-loop over a multi-labeled
            // node reaches here (e.g. `(a)-[:R]->(a)` with R: X→Y and a labeled both X
            // and Y). C++ narrows the node and proceeds; we keep the prior message
            // rather than silently mis-routing the edge. (The common wrong-side cases
            // are reported as schema violations above.) Multi-labeled rels fall through
            // to the `tryPruneMultiLabeled` diagnostic.
            if !is_multi && candidates.len() == 1 {
                let rel_name = self.catalog.rel_table(candidates[0]).unwrap().name.clone();
                return Err(Error::binder(format!(
                    "Nodes are not connected through relationship table {rel_name}."
                )));
            }
            // Labeled rels blame the label against the endpoint tables; an
            // unlabeled rel gets the neighbour-nodes wording (both oracle-pinned:
            // issue 5716 vs create_empty).
            if !rp.labels.is_empty() {
                let name_of = |tables: &[TableId]| -> String {
                    tables
                        .first()
                        .and_then(|&t| self.catalog.node_table(t))
                        .map(|n| n.name.clone())
                        .unwrap_or_default()
                };
                return Err(Error::binder(format!(
                    "Cannot find a valid label in {rel_disp} that connects {} and {}.",
                    name_of(&src_tables),
                    name_of(&dst_tables),
                )));
            }
            return Err(Error::binder(format!(
                "Cannot find a label for relationship {rel_disp} that connects to all of its \
                 neighbour nodes."
            )));
        }
        let rel_id = matched[0];
        let rel = self.catalog.rel_table(rel_id).unwrap();
        // A multi-pair rel (a rel group) cannot anchor a new edge on an
        // endpoint matched under several node tables — the target pair is
        // ambiguous (C++ "bound by multiple node labels"; a single-pair rel
        // pins its endpoints itself, issue 3906).
        if rel.pairs.len() > 1
            && [left, right]
                .iter()
                .any(|&e| self.vars[e.0 as usize].node_tables().len() > 1)
        {
            let rel_disp = rp.var.clone().unwrap_or_default();
            return Err(Error::binder(format!(
                "Create rel {rel_disp} bound by multiple node labels is not supported."
            )));
        }
        let (rel_name, num_columns) = (rel.name.clone(), rel.columns.len());
        let props = if self.catalog.is_any_rel_table(rel_id) {
            vec![
                (
                    1,
                    BoundExpr::Literal(Value::String(
                        rp.labels
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "_edges".to_string()),
                    )),
                ),
                (2, self.any_data_expr(&rp.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &rp.properties {
                let col = rel
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = rp.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // String literals cast at execution (see the CREATE-node note).
                if !matches!(&be, BoundExpr::Literal(Value::String(_))) {
                    assignable_or_err(&be, &rel.columns[col].ty, &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, &rel.columns[col].ty)));
            }
            props
        };
        // A *named* created rel is projectable (e.g. `CREATE (a)-[e:R]->(b) RETURN
        // e` / `id(e)`): mint a Rel-typed variable so RETURN/WITH can reference it,
        // exactly as a MERGE-created rel does. (An anonymous rel needs no variable.)
        // The processor fills this var's column with the new rel's internal id.
        let var = if rp.var.is_some() {
            let var_props = self.rel_props(rel_id);
            Some(self.add_var(
                rp.var.clone(),
                VarKind::Rel {
                    tables: vec![rel_id],
                    label: rel_name.clone(),
                    src,
                    dst,
                    directed: true,
                    recursive: None,
                },
                var_props,
            ))
        } else {
            None
        };
        create.rels.push(BoundCreateRel {
            table: rel_id,
            src,
            dst,
            num_columns,
            props,
            var,
        });
        Ok(())
    }

    // ---- MERGE ----

    /// Bind a `MERGE`: bind the (single) pattern once as match variables, collect
    /// the inline-property match filter, and build create-on-miss instructions for
    /// the parts the MERGE introduces (already-bound endpoints are reused).
    fn bind_merge(&mut self, merge: &ast::MergeClause) -> Result<BoundMerge> {
        let mut match_ = BoundMatch::default();
        let mut filters = Vec::new();
        let mut create = BoundCreate::default();

        // Each comma-separated pattern element joins into one combined match — a
        // cross-product when the elements are disconnected (the planner builds
        // that) — governed by a single existence: if every element matched it is an
        // ON MATCH, otherwise the MERGE creates every part it introduced and applies
        // ON CREATE. A variable shared across elements (or already in scope) is
        // matched/reused, never re-created.
        for pe in &merge.patterns {
            let head_bound = pe
                .head
                .var
                .as_ref()
                .is_some_and(|n| self.scope.contains_key(n));
            let head = self.bind_match_node(&pe.head)?;
            match_.node_vars.push(head);
            self.inline_predicates(head, &pe.head.properties, &mut filters)?;
            self.any_label_predicates(head, &pe.head.labels, &mut filters);
            if !head_bound {
                self.merge_create_node(head, &pe.head, &mut create)?;
            }

            let mut prev = head;
            for (rel, node) in &pe.chains {
                let node_bound = node
                    .var
                    .as_ref()
                    .is_some_and(|n| self.scope.contains_key(n));
                let node_var = self.bind_match_node(node)?;
                let rel_var = self.bind_match_rel(rel, prev, node_var, None)?;
                self.any_label_predicates(rel_var, &rel.labels, &mut filters);
                self.any_label_predicates(node_var, &node.labels, &mut filters);
                self.inline_predicates(node_var, &node.properties, &mut filters)?;
                self.inline_predicates(rel_var, &rel.properties, &mut filters)?;
                match_.node_vars.push(node_var);
                match_.rel_vars.push(rel_var);
                if !node_bound {
                    self.merge_create_node(node_var, node, &mut create)?;
                }
                self.merge_create_rel(rel_var, rel, prev, node_var, &mut create)?;
                prev = node_var;
            }
        }

        // Every pattern part resolved to an already-bound variable — nothing to
        // merge-create is the same C++ bind error as an empty CREATE.
        if create.nodes.is_empty() && create.rels.is_empty() {
            return Err(Error::binder(
                "Cannot resolve any node or relationship to create.".to_string(),
            ));
        }
        let on_create = self.bind_set_items(&merge.on_create)?;
        let on_match = self.bind_set_items(&merge.on_match)?;
        Ok(BoundMerge {
            match_,
            filter: combine_and(filters),
            create,
            on_create,
            on_match,
        })
    }

    /// Build the create-on-miss instruction for a `MERGE`d node (an existing,
    /// freshly-bound match var); its inline properties become the insert values.
    fn merge_create_node(
        &self,
        var: VarId,
        np: &ast::NodePattern,
        create: &mut BoundCreate,
    ) -> Result<()> {
        let table = self.resolve_single_node_label(np.var.as_deref().unwrap_or(""), &np.labels)?;
        let entry = self.catalog.node_table(table).unwrap();
        let num_columns = entry.columns.len();
        let pk_col = entry.primary_key;
        let props = if self.catalog.is_any_node_table(table) {
            vec![
                (
                    1,
                    BoundExpr::Literal(Value::List(if np.labels.is_empty() {
                        vec![Value::String("_nodes".to_string())]
                    } else {
                        np.labels.iter().cloned().map(Value::String).collect()
                    })),
                ),
                (2, self.any_data_expr(&np.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &np.properties {
                let col = entry
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = np.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // String literals cast at execution (see the CREATE-node note).
                if !matches!(&be, BoundExpr::Literal(Value::String(_))) {
                    assignable_or_err(&be, &entry.columns[col].ty, &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, &entry.columns[col].ty)));
            }
            props
        };
        create.nodes.push(BoundCreateNode {
            var,
            table,
            num_columns,
            pk_col,
            props,
        });
        Ok(())
    }

    /// Build the create-on-miss instruction for a `MERGE`d relationship.
    fn merge_create_rel(
        &self,
        rel_var: VarId,
        rp: &ast::RelPattern,
        left: VarId,
        right: VarId,
        create: &mut BoundCreate,
    ) -> Result<()> {
        if rp.labels.len() != 1 {
            return Err(Error::binder(
                "a MERGE relationship must specify exactly one type".to_string(),
            ));
        }
        let (src, dst) = match rp.direction {
            ast::Direction::Right => (left, right),
            ast::Direction::Left => (right, left),
            ast::Direction::Both => {
                return Err(Error::binder(
                    "MERGE requires a directed relationship".to_string(),
                ));
            }
        };
        let rel_id = if let Some(tables) = self.catalog.any_tables() {
            tables.edges
        } else {
            self.catalog
                .rel_table_by_name(&rp.labels[0])
                .ok_or_else(|| Error::binder(format!("Table {} does not exist.", rp.labels[0])))?
                .id
        };
        let rel = self.catalog.rel_table(rel_id).unwrap();
        let num_columns = rel.columns.len();
        let props = if self.catalog.is_any_rel_table(rel_id) {
            vec![
                (1, BoundExpr::Literal(Value::String(rp.labels[0].clone()))),
                (2, self.any_data_expr(&rp.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &rp.properties {
                let col = rel
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = rp.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // String literals cast at execution (see the CREATE-node note).
                if !matches!(&be, BoundExpr::Literal(Value::String(_))) {
                    assignable_or_err(&be, &rel.columns[col].ty, &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, &rel.columns[col].ty)));
            }
            props
        };
        create.rels.push(BoundCreateRel {
            table: rel_id,
            src,
            dst,
            num_columns,
            props,
            var: Some(rel_var),
        });
        Ok(())
    }

    // ---- projection ----

    fn bind_projection(&mut self, r: &ast::ReturnClause) -> Result<BoundProjection> {
        let mut items = Vec::new();
        for item in &r.items {
            match item {
                ast::ProjectionItem::Star => {
                    // `RETURN *` expands to the variables currently IN SCOPE, in
                    // declaration order — not every variable ever declared
                    // (`self.vars`). After a `WITH`, earlier-part variables leave scope
                    // (and a carried `WITH a` re-binds `a` to a fresh id), so a global
                    // expansion would project a variable the final part's layout has no
                    // column for → a planner panic. A variable is in scope iff the scope
                    // maps its name back to this very id (which also excludes a
                    // shadowed/renamed older binding of the same name).
                    let named: Vec<(VarId, String)> = self
                        .vars
                        .iter()
                        .enumerate()
                        .filter(|(i, v)| {
                            !v.anonymous && self.scope.get(&v.name) == Some(&VarId(*i as u32))
                        })
                        .map(|(i, v)| (VarId(i as u32), v.name.clone()))
                        .collect();
                    if named.is_empty() {
                        return Err(Error::binder(
                            "RETURN or WITH * is not allowed when there are no variables in scope."
                                .to_string(),
                        ));
                    }
                    for (var, name) in named {
                        let info = &self.vars[var.0 as usize];
                        // Nodes / plain rels are assembled from their id; scalars,
                        // recursive rels and paths project as a value column.
                        if info.is_assembled_graph_var() {
                            items.push(ProjItem::Var { name, var });
                        } else {
                            items.push(ProjItem::Scalar {
                                name,
                                expr: self.var_ref_expr(var),
                            });
                        }
                    }
                }
                ast::ProjectionItem::AllProperties(name) => {
                    self.expand_all_properties(name, &mut items)?;
                }
                ast::ProjectionItem::AllStructFields(base) => {
                    self.expand_all_struct_fields(base, &mut items)?;
                }
                ast::ProjectionItem::Expr { expr, alias } => {
                    // A bare *node/plain-rel* variable projects the whole assembled
                    // value; a scalar / recursive-rel / path variable falls through
                    // to a value expression (read straight from its column).
                    if let ast::Expr::Variable(name) = expr {
                        if let Some(&var) = self.scope.get(name) {
                            if self.vars[var.0 as usize].is_assembled_graph_var() {
                                items.push(ProjItem::Var {
                                    name: alias.clone().unwrap_or_else(|| name.clone()),
                                    var,
                                });
                                continue;
                            }
                        }
                    }
                    let be = self
                        .bind_expr(expr)
                        .map_err(|e| rewrite_nested_agg(e, alias.as_deref()))?;
                    let name = alias.clone().unwrap_or_else(|| expr_name(expr));
                    items.push(ProjItem::Scalar { name, expr: be });
                }
            }
        }

        self.finish_projection(r.distinct, items, r)
    }

    /// Expand `<expr>.*` into one `struct_extract(base, field)` column per
    /// field of the struct-typed base, in declared field order (C++ names each
    /// column `STRUCT_EXTRACT(<base>, <field>)`).
    fn expand_all_struct_fields(&self, base: &ast::Expr, items: &mut Vec<ProjItem>) -> Result<()> {
        let bound = self.bind_expr(base)?;
        let LogicalType::Struct(fields) = bound.ty() else {
            return Err(Error::binder(format!(
                "Cannot spread the fields of a non-struct expression {}.",
                expr_name(base)
            )));
        };
        let base_name = expr_name(base);
        for (fname, fty) in fields {
            items.push(ProjItem::Scalar {
                name: format!("STRUCT_EXTRACT({base_name},{fname})"),
                expr: Self::property_extract_call(bound.clone(), &fname, fty.clone()),
            });
        }
        Ok(())
    }

    /// Expand `a.*` into one scalar projection item per property of node/rel `a`,
    /// in column order, retaining the qualified `a.property` output name.
    fn expand_all_properties(&self, name: &str, items: &mut Vec<ProjItem>) -> Result<()> {
        let var = self.lookup_var(name)?;
        let info = &self.vars[var.0 as usize];
        if info.is_scalar() {
            return Err(Error::binder(format!(
                "Variable {name} is not a node or relationship."
            )));
        }
        // A value-backed node expands its value struct: `_ID`/`_LABEL` lead
        // (C++ node-value fields), then the properties.
        if info.value_backed && info.is_node() {
            for (pname, pty) in [
                ("_id", LogicalType::InternalId),
                ("_label", LogicalType::String),
            ] {
                items.push(ProjItem::Scalar {
                    name: format!("{name}.{pname}"),
                    expr: Self::property_extract_call(self.var_ref_expr(var), pname, pty),
                });
            }
        }
        let props: Vec<(String, LogicalType)> = info
            .properties
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();
        for (pname, pty) in props {
            items.push(ProjItem::Scalar {
                name: format!("{name}.{pname}"),
                expr: BoundExpr::Property {
                    var,
                    prop: pname,
                    ty: pty,
                },
            });
        }
        Ok(())
    }

    /// Attach the `ORDER BY`/`SKIP`/`LIMIT` modifiers (shared by `RETURN` and
    /// `WITH`) to already-bound projection items. `ORDER BY` may reference an
    /// output column by alias/variable name, or be an arbitrary expression over
    /// Attach the `ORDER BY`/`SKIP`/`LIMIT` modifiers (shared by `RETURN` and
    /// `WITH`) to already-bound projection items.
    ///
    /// C++ binds aggregate/DISTINCT ORDER BY after clearing the input scope and
    /// exposing only the projection expressions/aliases. Non-aggregate,
    /// non-DISTINCT ORDER BY keeps the input scope, with exact output-name
    /// references using the projected value.
    fn finish_projection(
        &self,
        distinct: bool,
        items: Vec<ProjItem>,
        r: &ast::ReturnClause,
    ) -> Result<BoundProjection> {
        let output_scope = self.projection_output_scope(&items);
        let output_only_order = distinct || projection_has_aggregates(&items);
        let mut order_by = Vec::new();
        for (e, asc) in &r.order_by {
            let key = if output_only_order {
                let expr = self.bind_order_expr_in_output_scope(e, &output_scope)?;
                validate_order_key_type(e, &expr.ty())?;
                match expr {
                    BoundExpr::Column { col, .. } => OrderKey::Output(col),
                    other => OrderKey::PostProjection(other),
                }
            } else if let ast::Expr::Variable(name) = e {
                if let Some(out) = output_scope.get(name) {
                    validate_order_key_type(e, &out.ty)?;
                    OrderKey::Output(out.index)
                } else {
                    let expr = self.bind_expr(e)?;
                    validate_order_key_type(e, &expr.ty())?;
                    OrderKey::Expr(expr)
                }
            } else {
                let expr = self.bind_expr(e)?;
                validate_order_key_type(e, &expr.ty())?;
                OrderKey::Expr(expr)
            };
            order_by.push((key, *asc));
        }
        Ok(BoundProjection {
            distinct,
            items,
            order_by,
            skip: r
                .skip
                .as_ref()
                .map(|e| self.bind_skip_limit(e))
                .transpose()?,
            limit: r
                .limit
                .as_ref()
                .map(|e| self.bind_skip_limit(e))
                .transpose()?,
        })
    }

    /// Bind a `SKIP`/`LIMIT` count (C++ `bindSkipLimit`): the expression must be
    /// constant — a parameter or a literal expression (foldable arithmetic like
    /// `LIMIT 1 + 1` included) — else the binder error fires here. The folded
    /// value is validated as a non-negative integer at execution (a *runtime*
    /// error, matching the C++ stage split).
    fn bind_skip_limit(&self, e: &ast::Expr) -> Result<BoundExpr> {
        let bound = self.bind_expr(e)?;
        if !skip_limit_constant(&bound) {
            return Err(Error::binder(
                "The number of rows to skip/limit must be a parameter/literal expression."
                    .to_string(),
            ));
        }
        Ok(bound)
    }

    fn projection_output_scope(&self, items: &[ProjItem]) -> HashMap<String, ProjectionOutputRef> {
        let mut scope = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            let (name, ty) = match item {
                ProjItem::Scalar { name, expr } => (name.clone(), expr.ty()),
                ProjItem::Var { name, var } => (name.clone(), self.var_value_type(*var)),
            };
            scope
                .entry(name)
                .or_insert(ProjectionOutputRef { index, ty });
        }
        scope
    }

    /// Whether `e` references a `$param` with no bound value (see `bind_expr`:
    /// such a tree binds to NULL wholesale, like C++).
    /// Bind a path pattern used as an expression. The projection-less form
    /// (`WHERE (a)-[:R]->()`) is the openCypher existential pattern predicate
    /// — it binds as an EXISTS subquery over the pattern. The comprehension
    /// form (`[pattern | expr]`) only validates its labels (evaluation is not
    /// implemented; every oracle-probed use errors at bind).
    fn bind_pattern_expression(
        &self,
        pattern: &ast::PatternElement,
        projection: Option<&ast::Expr>,
    ) -> Result<BoundExpr> {
        if projection.is_none() {
            let id = self.subquery_count.get();
            self.subquery_count.set(id + 1);
            self.pending_subqueries.borrow_mut().push(PendingSubquery {
                id,
                kind: ast::SubqueryKind::Exists,
                patterns: vec![pattern.clone()],
                where_clause: None,
            });
            return Ok(BoundExpr::Subquery {
                id,
                ty: LogicalType::Bool,
            });
        }
        self.validate_pattern_comprehension(pattern)
    }

    /// Bind-time validation for a pattern comprehension: every node label /
    /// rel type must exist (the C++ binder errors on the first missing one —
    /// this is as far as every oracle-probed use gets); evaluation itself is
    /// not implemented.
    fn validate_pattern_comprehension(&self, pattern: &ast::PatternElement) -> Result<BoundExpr> {
        let mut labels: Vec<&str> = pattern.head.labels.iter().map(String::as_str).collect();
        for (rel, node) in &pattern.chains {
            labels.extend(rel.labels.iter().map(String::as_str));
            labels.extend(node.labels.iter().map(String::as_str));
        }
        for l in labels {
            if self.catalog.table_id(l).is_none() {
                return Err(Error::binder(format!("Table {l} does not exist.")));
            }
        }
        Err(Error::not_implemented(
            "pattern comprehensions are not supported in this phase".to_string(),
        ))
    }

    fn has_unbound_param(&self, e: &ast::Expr) -> bool {
        if self.prepared_parameters.is_some() {
            return false;
        }
        use ast::Expr as E;
        let any = |es: &[E]| es.iter().any(|x| self.has_unbound_param(x));
        match e {
            E::Parameter(name) => !self.params.contains_key(name),
            E::Literal(_) | E::OverflowInt(_) | E::Variable(_) | E::Star => false,
            E::Property { base, .. } => self.has_unbound_param(base),
            E::Function { args, .. } => any(args),
            E::And(es) | E::Or(es) | E::List(es) => any(es),
            E::Xor(a, b)
            | E::Comparison { lhs: a, rhs: b, .. }
            | E::Arithmetic { lhs: a, rhs: b, .. } => {
                self.has_unbound_param(a) || self.has_unbound_param(b)
            }
            E::Not(x) | E::Negate(x) | E::IsNull(x) | E::IsNotNull(x) => self.has_unbound_param(x),
            E::Struct(fields) => fields.iter().any(|(_, x)| self.has_unbound_param(x)),
            E::Lambda { body, .. } => self.has_unbound_param(body),
            E::ListComprehension {
                list,
                predicate,
                projection,
                ..
            } => {
                self.has_unbound_param(list)
                    || predicate
                        .as_deref()
                        .is_some_and(|x| self.has_unbound_param(x))
                    || projection
                        .as_deref()
                        .is_some_and(|x| self.has_unbound_param(x))
            }
            E::Case {
                operand,
                when_thens,
                else_,
            } => {
                operand
                    .as_deref()
                    .is_some_and(|x| self.has_unbound_param(x))
                    || when_thens
                        .iter()
                        .any(|(c, r)| self.has_unbound_param(c) || self.has_unbound_param(r))
                    || else_.as_deref().is_some_and(|x| self.has_unbound_param(x))
            }
            E::Subquery { where_clause, .. } => where_clause
                .as_deref()
                .is_some_and(|x| self.has_unbound_param(x)),
            E::PatternComprehension { projection, .. } => projection
                .as_deref()
                .is_some_and(|x| self.has_unbound_param(x)),
        }
    }

    fn bind_order_expr_in_output_scope(
        &self,
        e: &ast::Expr,
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        if let Some(out) = scope.get(&expr_name(e)) {
            return Ok(out.as_column());
        }
        if self.has_unbound_param(e) {
            return Ok(BoundExpr::Literal(Value::Null));
        }
        match e {
            ast::Expr::PatternComprehension {
                pattern,
                projection,
            } => self.bind_pattern_expression(pattern, projection.as_deref()),
            ast::Expr::Literal(v) => Ok(BoundExpr::Literal(v.clone())),
            ast::Expr::OverflowInt(text) => Err(overflow_int_error(text)),
            ast::Expr::Variable(name) => scope
                .get(name)
                .map(ProjectionOutputRef::as_column)
                .ok_or_else(|| Error::binder(format!("Variable {name} is not in scope."))),
            ast::Expr::Property { base, name } => {
                let value = self.bind_order_expr_in_output_scope(base, scope)?;
                let display = expr_name(base);
                self.bind_value_property(value, name, &display)
            }
            ast::Expr::Parameter(name) => Ok(self.bind_parameter(name)),
            ast::Expr::Function {
                name,
                args,
                arg_names,
                ..
            } => self.bind_output_function(name, args, arg_names, scope, e),
            ast::Expr::And(terms) => self.bind_output_bool(ScalarOp::And, terms, scope),
            ast::Expr::Or(terms) => self.bind_output_bool(ScalarOp::Or, terms, scope),
            ast::Expr::Xor(a, b) => {
                self.bind_output_scalar(ScalarOp::Xor, &[a.as_ref(), b.as_ref()], scope)
            }
            ast::Expr::Not(a) => self.bind_output_scalar(ScalarOp::Not, &[a.as_ref()], scope),
            ast::Expr::Comparison { op, lhs, rhs } => {
                self.bind_output_scalar(cmp_op(*op), &[lhs.as_ref(), rhs.as_ref()], scope)
            }
            ast::Expr::Arithmetic { op, lhs, rhs } => {
                self.bind_output_scalar(arith_op(*op), &[lhs.as_ref(), rhs.as_ref()], scope)
            }
            ast::Expr::Negate(a) => self.bind_output_scalar(ScalarOp::Neg, &[a.as_ref()], scope),
            ast::Expr::IsNull(a) => self.bind_output_scalar(ScalarOp::IsNull, &[a.as_ref()], scope),
            ast::Expr::IsNotNull(a) => {
                self.bind_output_scalar(ScalarOp::IsNotNull, &[a.as_ref()], scope)
            }
            ast::Expr::List(items) => {
                let bound = items
                    .iter()
                    .map(|e| self.bind_order_expr_in_output_scope(e, scope))
                    .collect::<Result<Vec<_>>>()?;
                // Element type = the C++ `LIST_CREATION` combine — see
                // `list_literal_elem_type`.
                let elem_ty = list_literal_elem_type(&bound);
                // Each element must implicitly cast to the combined element type —
                // C++ `ListCreationFunction::bindFunc` casts every argument to the
                // combined type via `implicitCastIfNecessary` (POSITIONAL for
                // nested literals: struct/union member names adopt the combined
                // type's), so an incompatible element is a *bind-time*
                // rejection, not a deferred runtime cast failure.
                let mut bound = bound;
                for elem in &mut bound {
                    adopt_struct_field_names(elem, &elem_ty);
                    adopt_union_member_names(elem, &elem_ty);
                }
                for (item, elem) in items.iter().zip(&bound) {
                    assignable_or_err(elem, &elem_ty, &expr_name(item))?;
                }
                let elems = bound
                    .into_iter()
                    .map(|expression| self.coerce_to(expression, &elem_ty))
                    .collect();
                Ok(BoundExpr::List {
                    elems,
                    ty: LogicalType::List(Box::new(elem_ty)),
                })
            }
            ast::Expr::Struct(fields) => {
                check_struct_field_dups(fields)?;
                let bfields = fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), self.bind_order_expr_in_output_scope(v, scope)?)))
                    .collect::<Result<Vec<_>>>()?;
                // An all-NULL field types as STRING (C++ ANY-default) in the
                // struct literal's declared type.
                let ty = LogicalType::Struct(
                    bfields
                        .iter()
                        .map(|(k, v)| {
                            let t = match v.ty() {
                                LogicalType::Any => LogicalType::String,
                                t => t,
                            };
                            (k.clone(), t)
                        })
                        .collect(),
                );
                Ok(BoundExpr::Struct {
                    fields: bfields,
                    ty,
                })
            }
            ast::Expr::Case {
                operand,
                when_thens,
                else_,
            } => self.bind_output_case(operand.as_deref(), when_thens, else_.as_deref(), scope),
            ast::Expr::Star => Err(Error::binder(
                "'*' is only valid as the argument of count(*) or in RETURN *".to_string(),
            )),
            ast::Expr::Lambda { .. } => Err(Error::binder(
                "a lambda is only valid as an argument to a list function".to_string(),
            )),
            ast::Expr::ListComprehension { .. } | ast::Expr::Subquery { .. } => {
                Err(Error::not_implemented(format!(
                    "{} in ORDER BY over projected output is not supported in this phase",
                    expr_name(e)
                )))
            }
        }
    }

    fn bind_output_function(
        &self,
        name: &str,
        args: &[ast::Expr],
        arg_names: &[Option<String>],
        scope: &HashMap<String, ProjectionOutputRef>,
        full_expr: &ast::Expr,
    ) -> Result<BoundExpr> {
        if let Some(b) = self.bind_named_ctor(name, args, arg_names, &|a| {
            self.bind_order_expr_in_output_scope(a, scope)
        })? {
            return Ok(b);
        }
        if let Some(b) = self.bind_keys(name, args)? {
            return Ok(b);
        }
        if name.eq_ignore_ascii_case("cast") {
            if args.len() != 2 {
                return Err(Error::binder(
                    "cast expects (expression, type) arguments".to_string(),
                ));
            }
            let target = match &args[1] {
                ast::Expr::Literal(Value::String(s)) => self.resolve_cast_type(s)?,
                _ => {
                    return Err(Error::binder(
                        "the second argument to cast must be a type-name string".to_string(),
                    ));
                }
            };
            let expr = self.bind_order_expr_in_output_scope(&args[0], scope)?;
            return Ok(BoundExpr::Cast {
                expr: Box::new(expr),
                target,
            });
        }
        if sequence_fn(name).is_some() || AggOp::from_name(name).is_some() {
            return Err(Error::binder(format!(
                "Variable {} is not in scope.",
                expr_name(full_expr)
            )));
        }
        let lk = match name.to_ascii_lowercase().as_str() {
            "list_transform" => Some(LambdaKind::Transform),
            "list_filter" => Some(LambdaKind::Filter),
            "list_reduce" => Some(LambdaKind::Reduce),
            _ => None,
        };
        if lk.is_some() {
            return Err(Error::binder(
                "a lambda is only valid as an argument to a projected list function".to_string(),
            ));
        }
        if let Some(function) = self
            .session_config
            .scalar_udfs
            .get(&name.to_ascii_lowercase())
            .cloned()
        {
            let bound = args
                .iter()
                .map(|argument| self.bind_order_expr_in_output_scope(argument, scope))
                .collect::<Result<Vec<_>>>()?;
            return self.bind_scalar_udf(function, bound);
        }
        if koko_function::is_scalar(name) {
            let mut bound = args
                .iter()
                .map(|a| self.bind_order_expr_in_output_scope(a, scope))
                .collect::<Result<Vec<_>>>()?;
            let lname = name.to_ascii_lowercase();
            if matches!(lname.as_str(), "coalesce" | "ifnull" | "greatest" | "least") {
                let common =
                    common_type_strict(bound.iter().map(|a| a.ty())).unwrap_or(LogicalType::String);
                bound = bound
                    .into_iter()
                    .map(|argument| self.coerce_to(argument, &common))
                    .collect();
            }
            // An empty list literal adopts the OTHER list argument's element
            // type (C++ canCastStatically): LIST_CAT(['7','3'], []) binds as
            // STRING[] × STRING[] instead of a bind-time mismatch.
            if bound.len() == 2 {
                for i in 0..2 {
                    if let Some(child) = bound[1 - i].ty().list_child().cloned() {
                        if let BoundExpr::List { elems, ty } = &mut bound[i] {
                            if elems.is_empty() {
                                *ty = LogicalType::List(Box::new(child));
                            }
                        }
                    }
                }
            }
            bound = coerce_string_params(&lname, bound);
            let arg_types: Vec<LogicalType> = bound.iter().map(|a| a.ty()).collect();
            let ty = koko_function::scalar_func_result_type(name, &arg_types)?;
            // With `disable_map_key_check=false`, map() validates its keys at
            // eval (NULL keys reject) — routed via the internal checked name.
            let lname = if lname == "map" && !self.disable_map_key_check {
                "map_checked".to_string()
            } else {
                lname
            };
            return Ok(BoundExpr::Call {
                name: lname,
                args: bound,
                ty,
            });
        }
        Err(unknown_function_error(name))
    }

    fn bind_output_bool(
        &self,
        op: ScalarOp,
        terms: &[ast::Expr],
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        let args = terms
            .iter()
            .map(|t| self.bind_order_expr_in_output_scope(t, scope))
            .collect::<Result<Vec<_>>>()?;
        Ok(BoundExpr::Scalar {
            op,
            args,
            ty: LogicalType::Bool,
        })
    }

    fn bind_output_scalar(
        &self,
        op: ScalarOp,
        args: &[&ast::Expr],
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        let args = args
            .iter()
            .map(|a| self.bind_order_expr_in_output_scope(a, scope))
            .collect::<Result<Vec<_>>>()?;
        if op.is_comparison() && args.len() == 2 {
            let (l, r) = (args[0].ty(), args[1].ty());
            if !koko_function::comparison_comparable(&l, &r) {
                for a in &args {
                    if let Some(Err(e)) = try_const_eval(a) {
                        return Err(e);
                    }
                }
                return Err(Error::binder(format!(
                    "Type Mismatch: Cannot compare types {l} and {r}"
                )));
            }
        }
        let arg_types: Vec<LogicalType> = args.iter().map(|a| a.ty()).collect();
        let ty = koko_function::scalar_result_type(op, &arg_types)?;
        Ok(BoundExpr::Scalar { op, args, ty })
    }

    fn bind_output_case(
        &self,
        operand: Option<&ast::Expr>,
        when_thens: &[(ast::Expr, ast::Expr)],
        else_: Option<&ast::Expr>,
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        let bound_operand = operand
            .map(|o| self.bind_order_expr_in_output_scope(o, scope))
            .transpose()?
            .map(Box::new);
        let branches = when_thens
            .iter()
            .map(|(cond, res)| {
                Ok((
                    self.bind_order_expr_in_output_scope(cond, scope)?,
                    self.bind_order_expr_in_output_scope(res, scope)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let belse = else_
            .map(|e| self.bind_order_expr_in_output_scope(e, scope))
            .transpose()?
            .map(Box::new);
        let ty = branches
            .iter()
            .rev()
            .map(|(_, r)| r.ty())
            .find(|t| *t != LogicalType::Any)
            .or_else(|| {
                belse
                    .as_ref()
                    .map(|e| e.ty())
                    .filter(|t| *t != LogicalType::Any)
            })
            .unwrap_or(LogicalType::Any);
        let branches = branches
            .into_iter()
            .map(|(condition, result)| (condition, self.coerce_to(result, &ty)))
            .collect();
        let belse = belse.map(|expression| Box::new(self.coerce_to(*expression, &ty)));
        Ok(BoundExpr::Case {
            operand: bound_operand,
            branches,
            else_: belse,
            ty,
        })
    }

    /// Bind a `WITH` projection (D2a: scalar values and aggregates only).
    ///
    /// Differs from `RETURN`: every item must be aliased (a bare variable supplies
    /// its own name); carrying a node/relationship variable forward is deferred to
    /// a later phase; `ORDER BY` requires `SKIP`/`LIMIT`; and output column names
    /// must be unique. Mirrors the C++ binder's `bindWithClause` checks.
    fn bind_with_projection(&mut self, r: &ast::ReturnClause) -> Result<BoundProjection> {
        let mut items: Vec<ProjItem> = Vec::new();
        for item in &r.items {
            match item {
                ast::ProjectionItem::Star => {
                    // `WITH *` carries every in-scope variable forward, in
                    // declaration (VarId) order for a deterministic layout.
                    if self.scope.is_empty() {
                        return Err(Error::binder(
                            "RETURN or WITH * is not allowed when there are no variables in scope."
                                .to_string(),
                        ));
                    }
                    let mut named: Vec<(VarId, String)> =
                        self.scope.iter().map(|(n, &v)| (v, n.clone())).collect();
                    named.sort_by_key(|(v, _)| v.0);
                    for (var, name) in named {
                        items.push(self.with_item(name, self.var_ref_expr(var))?);
                    }
                }
                // `WITH a.*` expands to unaliased property expressions, which C++
                // rejects like any other unaliased WITH item.
                ast::ProjectionItem::AllProperties(_) | ast::ProjectionItem::AllStructFields(_) => {
                    return Err(Error::binder(
                        "Expression in WITH must be aliased (use AS).".to_string(),
                    ));
                }
                ast::ProjectionItem::Expr { expr, alias } => {
                    let name = match (alias, expr) {
                        (Some(a), _) => a.clone(),
                        // A bare variable supplies its own name; anything else must
                        // be explicitly aliased.
                        (None, ast::Expr::Variable(n)) => n.clone(),
                        (None, _) => {
                            return Err(Error::binder(
                                "Expression in WITH must be aliased (use AS).".to_string(),
                            ));
                        }
                    };
                    let be = self
                        .bind_expr(expr)
                        .map_err(|e| rewrite_nested_agg(e, Some(name.as_str())))?;
                    items.push(self.with_item(name, be)?);
                }
            }
        }

        // Column names (the aliases) must be unique.
        let mut seen = std::collections::HashSet::new();
        for it in &items {
            let name = match it {
                ProjItem::Scalar { name, .. } | ProjItem::Var { name, .. } => name,
            };
            if !seen.insert(name.clone()) {
                return Err(Error::binder(format!(
                    "Multiple result columns with the same name {name} are not supported."
                )));
            }
        }

        let proj = self.finish_projection(r.distinct, items, r)?;
        if !proj.order_by.is_empty() && proj.skip.is_none() && proj.limit.is_none() {
            return Err(Error::binder(
                "In WITH clause, ORDER BY must be followed by SKIP or LIMIT.".to_string(),
            ));
        }
        Ok(proj)
    }

    /// Build a single WITH projection item. A bare **node** variable is carried
    /// forward as a whole-node binding (D2b — its id + properties are
    /// re-materialized in the next part). Carrying a **relationship** variable, or
    /// any other node/rel-typed expression, is still deferred.
    fn with_item(&self, name: String, expr: BoundExpr) -> Result<ProjItem> {
        // A bare node *or* relationship variable is carried as a whole value
        // (assembled at the projection). A node is re-exploded into a binding in
        // the next part; a relationship is carried as a scalar value, with property
        // access via `ValueProperty` (see `carry_forward`).
        if let BoundExpr::NodeRef { var, .. } = &expr {
            // A node / plain rel is carried as a binding (re-exploded next part);
            // a recursive rel or path is carried as a `RECURSIVE_REL` value column.
            if self.vars[var.0 as usize].is_assembled_graph_var() {
                return Ok(ProjItem::Var { name, var: *var });
            }
            return Ok(ProjItem::Scalar { name, expr });
        }
        // A node/rel-typed EXPRESSION (e.g. a CASE over two nodes) is carried
        // as a scalar VALUE column — property access via ValueProperty, and it
        // renders as the whole value (issue 2589).
        Ok(ProjItem::Scalar { name, expr })
    }

    /// After a `WITH`, replace the scope with its projected variables (the carried
    /// scope for the next part) and return their `VarId`s in column order. Scalars
    /// carry their value; a carried node becomes a fresh node binding (same
    /// table/label/properties) re-scoped under its alias, so a later
    /// `MATCH (a)-[…]` or `a.prop` resolves to it.
    fn carry_forward(&mut self, projection: &BoundProjection) -> Result<Vec<VarId>> {
        self.scope.clear();
        let mut carried = Vec::with_capacity(projection.items.len());
        for item in &projection.items {
            let var = match item {
                ProjItem::Scalar { name, expr } => self.add_scalar_var(name.clone(), expr.ty()),
                ProjItem::Var { name, var } => {
                    if self.vars[var.0 as usize].is_node() {
                        // Node kinds are self-contained (table + label); copy the
                        // binding + property list so it re-materializes next part.
                        let info = &self.vars[var.0 as usize];
                        let kind = info.kind.clone();
                        let props = info.properties.clone();
                        self.add_var(Some(name.clone()), kind, props)
                    } else {
                        // A relationship is carried as a scalar VALUE — its endpoint
                        // VarIds don't survive the part boundary, so it can't be a
                        // re-exploded binding; property access uses `ValueProperty`.
                        let ty = self.var_value_type(*var);
                        self.add_scalar_var(name.clone(), ty)
                    }
                }
            };
            carried.push(var);
        }
        Ok(carried)
    }

    // ---- expressions ----

    fn bind_property_expr(&self, base: &ast::Expr, name: &str) -> Result<BoundExpr> {
        if let ast::Expr::Variable(var) = base {
            let lambda_type = self
                .lambda_params
                .borrow()
                .iter()
                .rev()
                .find(|(name, _)| name == var)
                .map(|(_, ty)| ty.clone());
            if let Some(lambda_type) = lambda_type {
                return Ok(Self::property_extract_call(
                    BoundExpr::LambdaVar {
                        name: var.clone(),
                        ty: lambda_type,
                    },
                    name,
                    LogicalType::Any,
                ));
            }

            let id = self.lookup_var(var)?;
            let info = &self.vars[id.0 as usize];
            // A path / variable-length rel has no direct properties (C++ types
            // the rejection rather than reporting a missing property).
            if info.is_recursive() || matches!(info.kind, VarKind::Path { .. }) {
                return Err(Error::binder(format!(
                    "{var} has data type RECURSIVE_REL but (NODE,REL,STRUCT,ANY) was expected."
                )));
            }
            if !info.is_scalar() {
                // The internal identity columns are not user-accessible as
                // properties on *pattern* variables (use id()/label(); C++
                // reserved-name check). A value-backed node/rel exposes them as
                // ordinary value-struct fields (`UNWIND collect(a) AS d` →
                // `d._id` works).
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "_id" | "_label" | "_src" | "_dst" | "_nodes" | "_rels"
                ) {
                    if info.value_backed {
                        return Ok(Self::property_extract_call(
                            self.var_ref_expr(id),
                            name,
                            match name.to_ascii_lowercase().as_str() {
                                "_id" => LogicalType::InternalId,
                                _ => LogicalType::String,
                            },
                        ));
                    }
                    return Err(Error::binder(format!(
                        "{name} is reserved for system usage. External access is not allowed."
                    )));
                }
                if let Some(prop) = info.property(name) {
                    return Ok(BoundExpr::Property {
                        var: id,
                        prop: prop.name.clone(),
                        ty: prop.ty.clone(),
                    });
                }
                // `a.rowid` / `e.rowid` — the internal offset (C++ rowid
                // pseudo-property), i.e. offset(id(x)).
                if name.eq_ignore_ascii_case("rowid") {
                    let synth = ast::Expr::Function {
                        name: "offset".to_string(),
                        distinct: false,
                        args: vec![ast::Expr::Function {
                            name: "id".to_string(),
                            distinct: false,
                            args: vec![ast::Expr::Variable(var.clone())],
                            arg_names: vec![None],
                        }],
                        arg_names: vec![None],
                    };
                    return self.bind_expr(&synth);
                }
                if self.is_any_var(id) {
                    let data = info
                        .property("data")
                        .expect("ANY variables carry the hidden data column");
                    return Ok(BoundExpr::ValueProperty {
                        value: Box::new(BoundExpr::Property {
                            var: id,
                            prop: data.name.clone(),
                            ty: data.ty.clone(),
                        }),
                        prop: name.to_string(),
                        ty: LogicalType::Any,
                    });
                }
                return Err(Error::binder(format!(
                    "Cannot find property {name} for {var}."
                )));
            }

            return self.bind_value_property(self.var_ref_expr(id), name, var);
        }

        let value = self.bind_expr(base)?;
        let display = expr_name(base);
        self.bind_value_property(value, name, &display)
    }

    fn bind_value_property(
        &self,
        value: BoundExpr,
        name: &str,
        display: &str,
    ) -> Result<BoundExpr> {
        let value_ty = value.ty();
        // Node/rel *values* expose their identity struct fields directly
        // (`d._id`, `e2._SRC` — case-insensitive, like C++ value structs).
        if matches!(value_ty, LogicalType::Node(_) | LogicalType::Rel(_)) {
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(), "_id" | "_label" | "_src" | "_dst") {
                let ty = match lower.as_str() {
                    "_id" | "_src" | "_dst" => LogicalType::InternalId,
                    _ => LogicalType::String,
                };
                return Ok(Self::property_extract_call(value, &lower, ty));
            }
        }
        match value_ty {
            // The sentinel table id (nodes()/rels() elements — the concrete
            // table is only known per value) resolves properties at eval.
            LogicalType::Node(t) if t.0 == u64::MAX => Ok(BoundExpr::ValueProperty {
                value: Box::new(value),
                prop: name.to_string(),
                ty: LogicalType::Any,
            }),
            LogicalType::Rel(t) if t.0 == u64::MAX => Ok(BoundExpr::ValueProperty {
                value: Box::new(value),
                prop: name.to_string(),
                ty: LogicalType::Any,
            }),
            LogicalType::Node(t) => {
                let ty = self
                    .catalog
                    .node_table(t)
                    .and_then(|nt| nt.column(name))
                    .map(|c| c.ty.clone())
                    .ok_or_else(|| {
                        Error::binder(format!("Cannot find property {name} for {display}."))
                    })?;
                Ok(BoundExpr::ValueProperty {
                    value: Box::new(value),
                    prop: name.to_string(),
                    ty,
                })
            }
            LogicalType::Rel(t) => {
                let ty = self
                    .catalog
                    .rel_table(t)
                    .and_then(|rt| rt.column(name))
                    .map(|c| c.ty.clone())
                    .ok_or_else(|| {
                        Error::binder(format!("Cannot find property {name} for {display}."))
                    })?;
                Ok(BoundExpr::ValueProperty {
                    value: Box::new(value),
                    prop: name.to_string(),
                    ty,
                })
            }
            LogicalType::Struct(fields) => {
                let ty = fields
                    .iter()
                    .find(|(field, _)| field.eq_ignore_ascii_case(name))
                    .map(|(_, ty)| ty.clone())
                    .ok_or_else(|| Error::binder(format!("Invalid struct field name: {name}.")))?;
                Ok(Self::property_extract_call(value, name, ty))
            }
            LogicalType::Map(_, value_ty) => {
                Ok(Self::property_extract_call(value, name, *value_ty))
            }
            LogicalType::Any => Ok(Self::property_extract_call(value, name, LogicalType::Any)),
            other => Err(Error::binder(format!(
                "{display} has data type {other} but (NODE,REL,STRUCT,ANY) was expected."
            ))),
        }
    }

    fn property_extract_call(value: BoundExpr, name: &str, ty: LogicalType) -> BoundExpr {
        BoundExpr::Call {
            name: "struct_extract".to_string(),
            args: vec![value, BoundExpr::Literal(Value::String(name.to_string()))],
            ty,
        }
    }

    fn bind_expr(&self, e: &ast::Expr) -> Result<BoundExpr> {
        // C++ nullifies the WHOLE expression containing an unbound parameter
        // (oracle: `coalesce($x, 5)` and `$x IS NULL` both yield NULL, not 5 /
        // True) — the parameter's ANY type swallows the tree.
        if self.has_unbound_param(e) {
            return Ok(BoundExpr::Literal(Value::Null));
        }
        let result = match e {
            ast::Expr::PatternComprehension {
                pattern,
                projection,
            } => self.bind_pattern_expression(pattern, projection.as_deref()),
            ast::Expr::Literal(v) => Ok(BoundExpr::Literal(v.clone())),
            ast::Expr::Variable(name) => {
                // A lambda parameter in scope resolves to a LambdaVar (not a
                // pattern variable / column).
                let lambda_type = self
                    .lambda_params
                    .borrow()
                    .iter()
                    .rev()
                    .find(|(parameter, _)| parameter == name)
                    .map(|(_, ty)| ty.clone());
                if let Some(ty) = lambda_type {
                    return Ok(BoundExpr::LambdaVar {
                        name: name.clone(),
                        ty,
                    });
                }
                let var = self.lookup_var(name)?;
                let info = &self.vars[var.0 as usize];
                if info.is_scalar() {
                    Ok(BoundExpr::ScalarVar {
                        var,
                        ty: info.scalar_type(),
                    })
                } else {
                    Ok(BoundExpr::NodeRef {
                        var,
                        ty: self.var_value_type(var),
                    })
                }
            }
            ast::Expr::OverflowInt(text) => Err(overflow_int_error(text)),
            ast::Expr::Property { base, name } => self.bind_property_expr(base, name),
            ast::Expr::Parameter(name) => Ok(self.bind_parameter(name)),
            ast::Expr::Function {
                name,
                distinct,
                args,
                arg_names,
            } => self.bind_function(name, *distinct, args, arg_names),
            ast::Expr::And(terms) => self.bind_bool(ScalarOp::And, terms),
            ast::Expr::Or(terms) => self.bind_bool(ScalarOp::Or, terms),
            ast::Expr::Xor(a, b) => self.bind_scalar(ScalarOp::Xor, &[a.as_ref(), b.as_ref()]),
            ast::Expr::Not(a) => self.bind_scalar(ScalarOp::Not, &[a.as_ref()]),
            ast::Expr::Comparison { op, lhs, rhs } => {
                self.bind_scalar(cmp_op(*op), &[lhs.as_ref(), rhs.as_ref()])
            }
            ast::Expr::Arithmetic { op, lhs, rhs } => {
                self.bind_scalar(arith_op(*op), &[lhs.as_ref(), rhs.as_ref()])
            }
            ast::Expr::Negate(a) => self.bind_scalar(ScalarOp::Neg, &[a.as_ref()]),
            ast::Expr::IsNull(a) => self.bind_scalar(ScalarOp::IsNull, &[a.as_ref()]),
            ast::Expr::IsNotNull(a) => self.bind_scalar(ScalarOp::IsNotNull, &[a.as_ref()]),
            ast::Expr::List(items) => {
                let bound = items
                    .iter()
                    .map(|e| self.bind_expr(e))
                    .collect::<Result<Vec<_>>>()?;
                // Element type = the C++ `LIST_CREATION` combine (lattice, then the
                // mixed-type STRING / first-element fallback; empty or all-NULL
                // literals default to `INT64[]`) — see `list_literal_elem_type`.
                let elem_ty = list_literal_elem_type(&bound);
                // Each element must implicitly cast to the combined element type —
                // C++ `ListCreationFunction::bindFunc` casts every argument to the
                // combined type via `implicitCastIfNecessary` (POSITIONAL for
                // nested literals: struct/union member names adopt the combined
                // type's), so an incompatible element is a *bind-time*
                // rejection, not a deferred runtime cast failure.
                let mut bound = bound;
                for elem in &mut bound {
                    adopt_struct_field_names(elem, &elem_ty);
                    adopt_union_member_names(elem, &elem_ty);
                }
                for (item, elem) in items.iter().zip(&bound) {
                    assignable_or_err(elem, &elem_ty, &expr_name(item))?;
                }
                let elems = bound
                    .into_iter()
                    .map(|expression| self.coerce_to(expression, &elem_ty))
                    .collect();
                Ok(BoundExpr::List {
                    elems,
                    ty: LogicalType::List(Box::new(elem_ty)),
                })
            }
            ast::Expr::Struct(fields) => {
                check_struct_field_dups(fields)?;
                let bfields = fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), self.bind_expr(v)?)))
                    .collect::<Result<Vec<_>>>()?;
                // An all-NULL field types as STRING (C++ ANY-default) in the
                // struct literal's declared type.
                let ty = LogicalType::Struct(
                    bfields
                        .iter()
                        .map(|(k, v)| {
                            let t = match v.ty() {
                                LogicalType::Any => LogicalType::String,
                                t => t,
                            };
                            (k.clone(), t)
                        })
                        .collect(),
                );
                Ok(BoundExpr::Struct {
                    fields: bfields,
                    ty,
                })
            }
            ast::Expr::Lambda { .. } => Err(Error::binder(
                "a lambda is only valid as an argument to a list function".to_string(),
            )),
            ast::Expr::ListComprehension {
                var,
                list,
                predicate,
                projection,
            } => self.bind_comprehension(var, list, predicate.as_deref(), projection.as_deref()),
            ast::Expr::Case {
                operand,
                when_thens,
                else_,
            } => self.bind_case(operand.as_deref(), when_thens, else_.as_deref()),
            ast::Expr::Star => Err(Error::binder(
                "'*' is only valid as the argument of count(*) or in RETURN *".to_string(),
            )),
            ast::Expr::Subquery {
                kind,
                patterns,
                where_clause,
            } => {
                // Stage the subquery (its pattern is bound later, in `&mut`
                // context — see `drain_subqueries`) and reference it by id.
                let id = self.subquery_count.get();
                self.subquery_count.set(id + 1);
                self.pending_subqueries.borrow_mut().push(PendingSubquery {
                    id,
                    kind: *kind,
                    patterns: patterns.clone(),
                    where_clause: where_clause.as_deref().cloned(),
                });
                let ty = match kind {
                    ast::SubqueryKind::Exists => LogicalType::Bool,
                    ast::SubqueryKind::Count => LogicalType::Int64,
                };
                Ok(BoundExpr::Subquery { id, ty })
            }
        };
        if let Ok(expression) = &result {
            self.capture_parameter_constraints(expression);
        }
        result
    }

    /// Bind the patterns of subqueries staged during this part's expression
    /// binding. Each binds in the current scope (outer variables visible) plus
    /// fresh inner variables that are then dropped from scope. Returns them
    /// ordered by id. A NESTED subquery (staged while binding another's WHERE)
    /// binds depth-first, while its parent's inner variables are still in
    /// scope (it references them).
    fn drain_subqueries(&mut self) -> Result<Vec<BoundSubquery>> {
        let pending = std::mem::take(&mut *self.pending_subqueries.borrow_mut());
        let mut out: Vec<Option<BoundSubquery>> = (0..pending.len()).map(|_| None).collect();
        for ps in pending {
            self.bind_one_subquery(ps, &mut out)?;
        }
        // Ids are part-local (index into this part's `subqueries`); reset for the
        // next part.
        self.subquery_count.set(0);
        Ok(out
            .into_iter()
            .map(|o| o.expect("every id filled"))
            .collect())
    }

    /// Bind one staged subquery; any subqueries staged while binding its WHERE
    /// bind recursively BEFORE this one's inner scope is dropped.
    fn bind_one_subquery(
        &mut self,
        ps: PendingSubquery,
        out: &mut Vec<Option<BoundSubquery>>,
    ) -> Result<()> {
        let saved_scope = self.scope.clone();
        let mut match_ = BoundMatch::default();
        let mut preds = Vec::new();
        for pe in &ps.patterns {
            self.bind_match_pattern(pe, &mut match_, &mut preds, None)?;
        }
        if let Some(w) = &ps.where_clause {
            preds.push(self.bind_expr(w)?);
        }
        let nested = std::mem::take(&mut *self.pending_subqueries.borrow_mut());
        for n in nested {
            if n.id >= out.len() {
                out.resize(n.id + 1, None);
            }
            self.bind_one_subquery(n, out)?;
        }
        // Inner variables are scoped to the subquery.
        self.scope = saved_scope;
        let kind = match ps.kind {
            ast::SubqueryKind::Exists => SubqueryKind::Exists,
            ast::SubqueryKind::Count => SubqueryKind::Count,
        };
        if ps.id >= out.len() {
            out.resize(ps.id + 1, None);
        }
        out[ps.id] = Some(BoundSubquery {
            kind,
            match_,
            where_predicate: combine_and(preds),
        });
        Ok(())
    }

    /// Bind `list_transform`/`list_filter`/`list_reduce` (a list plus a lambda).
    fn bind_list_lambda(&self, kind: LambdaKind, args: &[ast::Expr]) -> Result<BoundExpr> {
        if args.len() != 2 {
            return Err(Error::binder(
                "list lambda functions take a list and a lambda".to_string(),
            ));
        }
        let list = self.bind_expr(&args[0])?;
        let (params, body_ast) = match &args[1] {
            ast::Expr::Lambda { params, body } => (params.clone(), body.as_ref()),
            other => {
                let fn_name = match kind {
                    LambdaKind::Transform => "LIST_TRANSFORM",
                    LambdaKind::Filter => "LIST_FILTER",
                    LambdaKind::Reduce => "LIST_REDUCE",
                };
                return Err(Error::binder(format!(
                    "The second argument of {fn_name} should be a lambda expression but got {}.",
                    ast_expr_kind(other)
                )));
            }
        };
        let want = if kind == LambdaKind::Reduce { 2 } else { 1 };
        if params.len() != want {
            return Err(Error::binder(format!(
                "this lambda expects {want} parameter(s) but got {}",
                params.len()
            )));
        }
        // An untyped (NULL) list argument is the C++ binder type error — the
        // expression name of a NULL literal renders empty, giving the
        // verbatim ` has data type ANY but LIST was expected.`
        if list.ty() == LogicalType::Any {
            return Err(Error::binder(format!(
                "{} has data type ANY but LIST was expected.",
                expr_name(&args[0])
            )));
        }
        let elem_ty = match list.ty() {
            LogicalType::List(inner) | LogicalType::Array(inner, _) => *inner,
            _ => LogicalType::Any,
        };
        let depth = self.lambda_params.borrow().len();
        self.lambda_params.borrow_mut().extend(
            params
                .iter()
                .map(|parameter| (parameter.clone(), elem_ty.clone())),
        );
        let body = self.bind_expr(body_ast);
        self.lambda_params.borrow_mut().truncate(depth);
        let body = body?;
        // A filter lambda must produce BOOL (C++ bindFunc validation).
        if kind == LambdaKind::Filter && !matches!(body.ty(), LogicalType::Bool | LogicalType::Any)
        {
            return Err(Error::binder(
                "LIST_FILTER requires the result type of lambda expression be BOOL.".to_string(),
            ));
        }
        let ty = match kind {
            LambdaKind::Transform => LogicalType::List(Box::new(body.ty())),
            LambdaKind::Filter => LogicalType::List(Box::new(elem_ty)),
            LambdaKind::Reduce => body.ty(),
        };
        Ok(BoundExpr::ListLambda {
            kind,
            list: Box::new(list),
            params,
            body: Box::new(body),
            ty,
        })
    }

    /// Desugar `[var IN list [WHERE pred] [| proj]]` into filter/transform lambdas.
    fn bind_comprehension(
        &self,
        var: &str,
        list: &ast::Expr,
        predicate: Option<&ast::Expr>,
        projection: Option<&ast::Expr>,
    ) -> Result<BoundExpr> {
        let mut current = self.bind_expr(list)?;
        let elem_ty = |t: &LogicalType| match t {
            LogicalType::List(inner) | LogicalType::Array(inner, _) => (**inner).clone(),
            _ => LogicalType::Any,
        };
        let depth = self.lambda_params.borrow().len();
        self.lambda_params
            .borrow_mut()
            .push((var.to_string(), elem_ty(&current.ty())));
        let result = (|| {
            if let Some(pred) = predicate {
                let body = self.bind_expr(pred)?;
                let ty = LogicalType::List(Box::new(elem_ty(&current.ty())));
                current = BoundExpr::ListLambda {
                    kind: LambdaKind::Filter,
                    list: Box::new(current.clone()),
                    params: vec![var.to_string()],
                    body: Box::new(body),
                    ty,
                };
            }
            if let Some(proj) = projection {
                let body = self.bind_expr(proj)?;
                let ty = LogicalType::List(Box::new(body.ty()));
                current = BoundExpr::ListLambda {
                    kind: LambdaKind::Transform,
                    list: Box::new(current.clone()),
                    params: vec![var.to_string()],
                    body: Box::new(body),
                    ty,
                };
            }
            Ok(current.clone())
        })();
        self.lambda_params.borrow_mut().truncate(depth);
        result
    }

    /// Bind a `CASE`. The operand (if any) is kept so the simple form can use
    /// null-safe equality at runtime; the searched form has no operand and each
    /// condition is a boolean predicate.
    fn bind_case(
        &self,
        operand: Option<&ast::Expr>,
        when_thens: &[(ast::Expr, ast::Expr)],
        else_: Option<&ast::Expr>,
    ) -> Result<BoundExpr> {
        let bound_operand = operand
            .map(|o| self.bind_expr(o))
            .transpose()?
            .map(Box::new);
        let branches = when_thens
            .iter()
            .map(|(cond, res)| Ok((self.bind_expr(cond)?, self.bind_expr(res)?)))
            .collect::<Result<Vec<_>>>()?;
        let belse = else_.map(|e| self.bind_expr(e)).transpose()?.map(Box::new);
        // Result type: the LAST non-`Any` THEN type (else the ELSE type) — oracle:
        // `THEN 2.5 … THEN 1` is INT64, `THEN 1 … THEN 'a'` is STRING. Every other
        // THEN/ELSE must be implicitly castable to it (validated in order — a
        // mismatch is the C++ "Implicit cast is not supported" error naming the
        // branch), and in the operand form each WHEN value must likewise cast to
        // the operand's type (checked after the branches, matching C++ order).
        let ty = branches
            .iter()
            .rev()
            .map(|(_, r)| r.ty())
            .find(|t| *t != LogicalType::Any)
            .or_else(|| {
                belse
                    .as_ref()
                    .map(|e| e.ty())
                    .filter(|t| *t != LogicalType::Any)
            })
            .unwrap_or(LogicalType::Any);
        for ((_, r), (_, r_ast)) in branches.iter().zip(when_thens) {
            assignable_or_err(r, &ty, &expr_name(r_ast))?;
        }
        if let (Some(b), Some(e_ast)) = (&belse, else_) {
            assignable_or_err(b, &ty, &expr_name(e_ast))?;
        }
        if let Some(op) = &bound_operand {
            let op_ty = op.ty();
            for ((c, _), (c_ast, _)) in branches.iter().zip(when_thens) {
                assignable_or_err(c, &op_ty, &expr_name(c_ast))?;
            }
        }
        let op_ty = bound_operand.as_ref().map(|o| o.ty());
        let branches = branches
            .into_iter()
            .map(|(c, r)| {
                let c = match &op_ty {
                    Some(target) => self.coerce_to(c, target),
                    None => c,
                };
                (c, self.coerce_to(r, &ty))
            })
            .collect();
        let belse = belse.map(|expression| Box::new(self.coerce_to(*expression, &ty)));
        Ok(BoundExpr::Case {
            operand: bound_operand,
            branches,
            else_: belse,
            ty,
        })
    }

    fn bind_bool(&self, op: ScalarOp, terms: &[ast::Expr]) -> Result<BoundExpr> {
        let mut args = terms
            .iter()
            .map(|term| self.bind_expr(term))
            .collect::<Result<Vec<_>>>()?;
        for (term, argument) in terms.iter().zip(&mut args) {
            self.constrain_parameter(argument, &LogicalType::Bool);
            bool_arg_or_err(term, argument)?;
        }
        Ok(BoundExpr::Scalar {
            op,
            args,
            ty: LogicalType::Bool,
        })
    }

    fn bind_scalar(&self, op: ScalarOp, raw_args: &[&ast::Expr]) -> Result<BoundExpr> {
        let mut args = raw_args
            .iter()
            .map(|a| self.bind_expr(a))
            .collect::<Result<Vec<_>>>()?;
        if op.is_arithmetic() && args.len() == 2 {
            let left = args[0].ty();
            let right = args[1].ty();
            if left == LogicalType::Any && right != LogicalType::Any {
                args[0] = self.coerce_to(args[0].clone(), &right);
            } else if right == LogicalType::Any && left != LogicalType::Any {
                args[1] = self.coerce_to(args[1].clone(), &left);
            }
        }
        if op.is_comparison() && args.len() == 2 {
            let (l, r) = (args[0].ty(), args[1].ty());
            if !koko_function::comparison_comparable(&l, &r) {
                // C++ folds literal-argument calls while binding them, so a
                // constant operand's own error (`date(2012)` → the date-parse
                // Conversion error) surfaces BEFORE the comparison check.
                for a in &args {
                    if let Some(Err(e)) = try_const_eval(a) {
                        return Err(e);
                    }
                }
                return Err(Error::binder(format!(
                    "Type Mismatch: Cannot compare types {l} and {r}"
                )));
            }
            let common = koko_function::comparison_common_type(&l, &r);
            if let Some(common) = common {
                args = args
                    .into_iter()
                    .map(|argument| self.coerce_to(argument, &common))
                    .collect();
            }
        }
        if matches!(op, ScalarOp::Xor | ScalarOp::Not) {
            for (expr, argument) in raw_args.iter().zip(&mut args) {
                self.constrain_parameter(argument, &LogicalType::Bool);
                bool_arg_or_err(expr, argument)?;
            }
        }
        let arg_types: Vec<LogicalType> = args.iter().map(|a| a.ty()).collect();
        let ty = koko_function::scalar_result_type(op, &arg_types)?;
        Ok(BoundExpr::Scalar { op, args, ty })
    }

    fn bind_cast(&self, args: &[ast::Expr]) -> Result<BoundExpr> {
        if args.len() != 2 {
            return Err(Error::binder(
                "cast expects (expression, type) arguments".to_string(),
            ));
        }
        let target = match &args[1] {
            ast::Expr::Literal(Value::String(s)) => self.resolve_cast_type(s)?,
            _ => {
                return Err(Error::binder(
                    "the second argument to cast must be a type-name string".to_string(),
                ));
            }
        };
        let expr = self.bind_expr(&args[0])?;
        Ok(BoundExpr::Cast {
            expr: Box::new(expr),
            target,
        })
    }

    /// Bind `union_value(tag := v)` / `struct_pack(f := v, …)` — the named-argument
    /// constructors. Returns `Some` when `name` is one of them, so both function-bind
    /// paths (`bind_function` and `bind_output_function`) share one implementation.
    /// `bind_arg` binds a sub-expression in the caller's scope.
    fn bind_named_ctor(
        &self,
        name: &str,
        args: &[ast::Expr],
        arg_names: &[Option<String>],
        bind_arg: &dyn Fn(&ast::Expr) -> Result<BoundExpr>,
    ) -> Result<Option<BoundExpr>> {
        let arg_name = |i: usize| arg_names.get(i).and_then(|o| o.clone());
        if name.eq_ignore_ascii_case("union_value") {
            if args.len() != 1 {
                return Err(Error::binder(
                    "Function UNION_VALUE takes exactly one argument.".to_string(),
                ));
            }
            let tag = arg_name(0).ok_or_else(|| {
                Error::binder(
                    "Function UNION_VALUE requires a named argument, e.g. union_value(tag := value)."
                        .to_string(),
                )
            })?;
            // C++ casts an ANY-typed argument (e.g. an untyped NULL) to STRING before
            // building the single-member union type `(tag, argType)`.
            let mut val = bind_arg(&args[0])?;
            if val.ty() == LogicalType::Any {
                val = self.coerce_to(val, &LogicalType::String);
            }
            let field_ty = val.ty();
            return Ok(Some(BoundExpr::Call {
                name: "union_value".to_string(),
                args: vec![val],
                ty: LogicalType::Union(vec![(tag, field_ty)]),
            }));
        }
        if name.eq_ignore_ascii_case("struct_pack") {
            let mut fields = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                // An explicit `field := value` name wins; otherwise C++ infers the
                // field name from a variable/property expression, and errors on
                // anything else (e.g. a literal).
                let fname = match arg_name(i) {
                    Some(n) => n,
                    None => match a {
                        ast::Expr::Variable(v) => v.clone(),
                        ast::Expr::Property { name, .. } => name.clone(),
                        _ => {
                            return Err(Error::binder(format!(
                                "Cannot infer field name for {}.",
                                expr_name(a)
                            )));
                        }
                    },
                };
                fields.push((fname, bind_arg(a)?));
            }
            check_struct_field_dups(&fields)?;
            let ty = LogicalType::Struct(fields.iter().map(|(k, v)| (k.clone(), v.ty())).collect());
            return Ok(Some(BoundExpr::Struct { fields, ty }));
        }
        Ok(None)
    }

    /// Bind `keys(node|rel)` → a constant `LIST(STRING)` of the value's property
    /// names (the multi-label union, in schema order), matching C++'s bind-time
    /// rewrite (`struct/keys_function.cpp`). `keys(NULL)` → `NULL`. Returns `None`
    /// when `name` isn't `keys`, so both function-bind paths share it.
    fn bind_keys(&self, name: &str, args: &[ast::Expr]) -> Result<Option<BoundExpr>> {
        if !name.eq_ignore_ascii_case("keys") {
            return Ok(None);
        }
        if args.len() != 1 {
            return Err(Error::binder(
                "Function KEYS takes exactly one argument.".to_string(),
            ));
        }
        // C++ rewrites a null-literal argument to a null literal.
        if matches!(&args[0], ast::Expr::Literal(Value::Null)) {
            return Ok(Some(BoundExpr::Literal(Value::Null)));
        }
        // A node/rel *variable* carries its resolved (multi-label unioned) property
        // list — the same source `RETURN a.*` expands (schema order). This is the
        // corpus path; anything else falls back to the bound value's single-table type.
        let names: Vec<String> = if let ast::Expr::Variable(v) = &args[0] {
            let var = self.lookup_var(v)?;
            let info = &self.vars[var.0 as usize];
            if info.is_scalar() {
                return Err(Error::binder(format!(
                    "Variable {v} is not a node or relationship."
                )));
            }
            info.properties.iter().map(|p| p.name.clone()).collect()
        } else {
            let bound = self.bind_expr(&args[0])?;
            match bound.ty() {
                LogicalType::Node(t) => self.node_props(t).into_iter().map(|p| p.name).collect(),
                LogicalType::Rel(t) => self.rel_props(t).into_iter().map(|p| p.name).collect(),
                LogicalType::Any => return Ok(Some(BoundExpr::Literal(Value::Null))),
                other => {
                    return Err(koko_function::scalarfn::signature_error(
                        "keys",
                        &[other],
                        &["(NODE)", "(REL)"],
                    ));
                }
            }
        };
        let elems = names
            .into_iter()
            .map(|n| BoundExpr::Literal(Value::String(n)))
            .collect();
        Ok(Some(BoundExpr::List {
            elems,
            ty: LogicalType::List(Box::new(LogicalType::String)),
        }))
    }

    fn bind_scalar_udf(
        &self,
        function: std::sync::Arc<koko_common::ScalarUdf>,
        arguments: Vec<BoundExpr>,
    ) -> Result<BoundExpr> {
        if arguments.len() != function.parameter_types.len() {
            return Err(Error::binder(format!(
                "Scalar function {} expects {} arguments but received {}.",
                function.name,
                function.parameter_types.len(),
                arguments.len()
            )));
        }
        for (position, (argument, expected)) in
            arguments.iter().zip(&function.parameter_types).enumerate()
        {
            if !assignable(&argument.ty(), expected) {
                return Err(Error::binder(format!(
                    "Scalar function {} argument {} has type {}, expected {}.",
                    function.name,
                    position + 1,
                    argument.ty(),
                    expected
                )));
            }
        }
        let arguments = arguments
            .into_iter()
            .zip(&function.parameter_types)
            .map(|(argument, expected)| self.coerce_to(argument, expected))
            .collect();
        let ty = function.result_type.clone();
        Ok(BoundExpr::Udf {
            function,
            args: arguments,
            ty,
        })
    }

    fn bind_function(
        &self,
        name: &str,
        distinct: bool,
        args: &[ast::Expr],
        arg_names: &[Option<String>],
    ) -> Result<BoundExpr> {
        if name.eq_ignore_ascii_case("cast") {
            return self.bind_cast(args);
        }
        // `cost(e)` is defined only for a (ALL) WSHORTEST recursive rel — a
        // plain or unweighted recursive rel is a bind-time error.
        if name.eq_ignore_ascii_case("cost") {
            if let [ast::Expr::Variable(v)] = args {
                let weighted = self.scope.get(v).is_some_and(|&var| {
                    matches!(
                        &self.vars[var.0 as usize].kind,
                        VarKind::Rel { recursive: Some(spec), .. } if spec.weight.is_some()
                    )
                });
                if !weighted {
                    return Err(Error::binder(format!(
                        "Cost function is not defined for {v}"
                    )));
                }
            }
        }
        if let Some(b) = self.bind_named_ctor(name, args, arg_names, &|a| self.bind_expr(a))? {
            return Ok(b);
        }
        if let Some(b) = self.bind_keys(name, args)? {
            return Ok(b);
        }
        if let Some(func) = sequence_fn(name) {
            return self.bind_sequence_call(func, args);
        }
        if let Some(op) = AggOp::from_name(name) {
            // percentileDisc(x, p): the percentile is a constant second
            // argument, folded into the op.
            if let AggOp::PercentileDisc(_) = op {
                if args.len() != 2 {
                    return Err(Error::binder(format!(
                        "aggregate {name} expects a value and a percentile"
                    )));
                }
                let arg = self.bind_expr(&args[0])?;
                let pexpr = self.bind_expr(&args[1])?;
                let p = match try_const_eval(&pexpr) {
                    Some(Ok(v)) => v.as_f64().unwrap_or(0.5),
                    Some(Err(e)) => return Err(e),
                    None => {
                        return Err(Error::binder(
                            "the percentile argument must be a constant".to_string(),
                        ));
                    }
                };
                let ty = arg.ty();
                return Ok(BoundExpr::Aggregate {
                    op: AggOp::PercentileDisc(p.to_bits()),
                    distinct,
                    arg: Some(Box::new(arg)),
                    ty,
                });
            }
            if args.len() == 1 && args[0] == ast::Expr::Star {
                if op != AggOp::Count {
                    return Err(Error::binder(format!(
                        "{name}(*) is not a valid aggregate."
                    )));
                }
                return Ok(BoundExpr::Aggregate {
                    op: AggOp::CountStar,
                    distinct,
                    arg: None,
                    ty: LogicalType::Int64,
                });
            }
            if args.len() != 1 {
                return Err(Error::binder(format!(
                    "aggregate {name} expects exactly one argument"
                )));
            }
            let arg = self.bind_expr(&args[0])?;
            if arg.contains_aggregate() {
                let display = format!(
                    "{}({})",
                    name.to_uppercase(),
                    args.iter().map(expr_name).collect::<Vec<_>>().join(",")
                );
                return Err(Error::binder(format!(
                    "Expression {display} contains nested aggregation."
                )));
            }
            // The catalog aggregate gate (C++ matchAggregateFunction): exact
            // type-ID matching, no casts; failures carry the full DISTINCT-
            // annotated overload block.
            if let Some(err) = koko_function::aggregate_signature_error(name, &[arg.ty()], distinct)
            {
                return Err(err);
            }
            // SUM/AVG require a numeric argument (mirrors the scalar-arithmetic
            // operand check); otherwise they would silently produce 0.
            if matches!(op, AggOp::Sum | AggOp::Avg) {
                let at = arg.ty();
                if !at.is_numeric() && at != LogicalType::Any {
                    return Err(Error::binder(format!(
                        "Function {name} expects a numeric argument but got {at}."
                    )));
                }
            }
            let ty = koko_function::agg_result_type(op, &arg.ty())?;
            return Ok(BoundExpr::Aggregate {
                op,
                distinct,
                arg: Some(Box::new(arg)),
                ty,
            });
        }
        let lk = match name.to_ascii_lowercase().as_str() {
            "list_transform" => Some(LambdaKind::Transform),
            "list_filter" => Some(LambdaKind::Filter),
            "list_reduce" => Some(LambdaKind::Reduce),
            _ => None,
        };
        if let Some(kind) = lk {
            return self.bind_list_lambda(kind, args);
        }
        // START_NODE/END_NODE are C++ REWRITE functions: over a rel PATTERN
        // variable they rewrite to its bound endpoint node variables (per-row
        // for undirected patterns); rel VALUES fall through to the scalar
        // eval over materialized endpoints.
        if let ("start_node" | "end_node", [ast::Expr::Variable(rv)]) =
            (name.to_ascii_lowercase().as_str(), args)
        {
            if let Ok(id) = self.lookup_var(rv) {
                if let VarKind::Rel {
                    src,
                    dst,
                    recursive: None,
                    ..
                } = &self.vars[id.0 as usize].kind
                {
                    let target = if name.eq_ignore_ascii_case("start_node") {
                        *src
                    } else {
                        *dst
                    };
                    return Ok(self.var_ref_expr(target));
                }
            }
        }
        // List-predicate quantifiers `ANY|ALL|NONE|SINGLE (x IN list WHERE p)`
        // (parser-desugared to `name(list, x -> p)`): bound as a size
        // comparison over LIST_FILTER, matching the C++ rewrite.
        if let quant @ ("any" | "all" | "none" | "single") = name.to_ascii_lowercase().as_str() {
            if args.len() == 2 && matches!(&args[1], ast::Expr::Lambda { .. }) {
                let filtered = self.bind_list_lambda(LambdaKind::Filter, args)?;
                let size_of = |e: BoundExpr| BoundExpr::Call {
                    name: "size".to_string(),
                    args: vec![e],
                    ty: LogicalType::Int64,
                };
                let int_lit = |n: i64| BoundExpr::Literal(Value::Int64(n));
                let (op, lhs, rhs) = match quant {
                    "any" => (ScalarOp::Gt, size_of(filtered), int_lit(0)),
                    "none" => (ScalarOp::Eq, size_of(filtered), int_lit(0)),
                    "single" => (ScalarOp::Eq, size_of(filtered), int_lit(1)),
                    _ => {
                        let whole = self.bind_expr(&args[0])?;
                        (ScalarOp::Eq, size_of(filtered), size_of(whole))
                    }
                };
                return Ok(BoundExpr::Scalar {
                    op,
                    args: vec![lhs, rhs],
                    ty: LogicalType::Bool,
                });
            }
        }
        if let Some(function) = self
            .session_config
            .scalar_udfs
            .get(&name.to_ascii_lowercase())
            .cloned()
        {
            if distinct {
                return Err(Error::binder(format!(
                    "DISTINCT is not valid for scalar function {}.",
                    function.name
                )));
            }
            if args
                .iter()
                .any(|argument| matches!(argument, ast::Expr::Lambda { .. }))
            {
                return Err(Error::binder(format!(
                    "{} does not support lambda input.",
                    function.name.to_uppercase()
                )));
            }
            let bound = args
                .iter()
                .map(|argument| self.bind_expr(argument))
                .collect::<Result<Vec<_>>>()?;
            return self.bind_scalar_udf(function, bound);
        }
        if koko_function::is_scalar(name) {
            // A lambda argument to a non-lambda function is the C++ binder error
            // (the lambda functions were dispatched above).
            if args.iter().any(|a| matches!(a, ast::Expr::Lambda { .. })) {
                return Err(Error::binder(format!(
                    "{} does not support lambda input.",
                    name.to_uppercase()
                )));
            }
            let mut bound: Vec<BoundExpr> = args
                .iter()
                .map(|a| self.bind_expr(a))
                .collect::<Result<_>>()?;
            // NULL-coalescing functions return the common supertype of their args,
            // and each arg is coerced to it (so coalesce(1, 1.5) is DOUBLE and
            // yields 1.000000). greatest/least keep their exact DATE/TIMESTAMP
            // signatures and are validated by koko-function.
            let lname = name.to_ascii_lowercase();
            // `properties(list, key)` binds with C++'s three checks: the key is
            // a literal string; the list holds node/rel values; and (when the
            // element tables are knowable) the key names an existing property.
            if lname == "properties" && args.len() == 2 {
                if !matches!(&args[1], ast::Expr::Literal(Value::String(_))) {
                    return Err(Error::binder(
                        "Expected literal input as the second argument for PROPERTIES()."
                            .to_string(),
                    ));
                }
                let t0 = bound[0].ty();
                let elem_ok = match &t0 {
                    LogicalType::List(inner) | LogicalType::Array(inner, _) => matches!(
                        inner.as_ref(),
                        LogicalType::Node(_)
                            | LogicalType::Rel(_)
                            | LogicalType::RecursiveRel
                            | LogicalType::Any
                    ),
                    _ => false,
                };
                if !elem_ok {
                    return Err(Error::binder(format!(
                        "Cannot extract properties from {t0}."
                    )));
                }
                if let ast::Expr::Literal(Value::String(key)) = &args[1] {
                    let lower = key.to_ascii_lowercase();
                    if !matches!(lower.as_str(), "_id" | "_label" | "_src" | "_dst")
                        && !self.catalog.any_table_has_property(key)
                    {
                        return Err(Error::binder(format!("Invalid property name: {key}.")));
                    }
                }
            }
            if matches!(lname.as_str(), "coalesce" | "ifnull") {
                // C++ `coalesce.cpp` bindFunc: result type = the combined arg type
                // (ANY, i.e. all-null, defaults to STRING), and every arg is then bound
                // to that type. An arg that can't *implicitly* cast to it is a bind-time
                // error (`implicitCastIfNecessary`), not a deferred runtime conversion —
                // e.g. `coalesce(1, "hello")` → `expected INT64`.
                // C++ falls back to STRING only when the type pair has NO max
                // in the lattice (coalesce(1, true) → STRING '1'); a pair WITH
                // a max keeps it, and a non-castable arg is then the bind error
                // below (coalesce(1, "hello") errors — corpus-pinned).
                let mut common = match common_type_strict(bound.iter().map(|a| a.ty())) {
                    Some(t) => t,
                    None => LogicalType::String,
                };
                if common == LogicalType::Any {
                    common = LogicalType::String;
                }
                for (arg, b) in args.iter().zip(bound.iter()) {
                    if !assignable(&b.ty(), &common) {
                        return Err(Error::binder(format!(
                            "Expression {} has data type {} but expected {}. Implicit cast is not supported.",
                            expr_name(arg),
                            b.ty().name(),
                            common.name()
                        )));
                    }
                }
                bound = bound
                    .into_iter()
                    .map(|argument| self.coerce_to(argument, &common))
                    .collect();
            }
            // regexp_replace's optional 4th (options) argument must be a
            // STRING literal (C++ bindFunc checks: non-literal first, then
            // the literal's type).
            if lname == "regexp_replace" && args.len() == 4 {
                match &args[3] {
                    ast::Expr::Literal(Value::String(opt)) => {
                        if opt != "g" {
                            return Err(Error::binder(
                                "regex_replace can only support global replace option: g."
                                    .to_string(),
                            ));
                        }
                    }
                    ast::Expr::Literal(v) => {
                        return Err(Error::binder(format!(
                            "{} has data type {} but STRING was expected.",
                            expr_name(&args[3]),
                            v.logical_type().name()
                        )));
                    }
                    other => {
                        return Err(Error::binder(format!(
                            "{} has type {} but LITERAL was expected.",
                            expr_name(other),
                            ast_expr_kind(other)
                        )));
                    }
                }
            }
            // The signature gate runs on the PRE-coercion types so its error
            // renders the original Actual line (C++ matchFunction order:
            // `list_to_string([1,2,3], 0)` shows `(INT64[],INT64)`).
            let pre_types: Vec<LogicalType> = bound.iter().map(|a| a.ty()).collect();
            koko_function::scalarfn::signature_gate(name, &pre_types)?;
            // C++ binds the element argument of these list functions to the
            // list's child type: a non-castable element is the assignment
            // error (list_contains) or the bespoke bind error (append/prepend).
            if bound.len() == 2 {
                if let LogicalType::List(elem) | LogicalType::Array(elem, _) = bound[0].ty() {
                    // An empty list literal statically casts to any list type
                    // (LIST_APPEND([], '5') is fine — the result is STRING[]).
                    let empty_list =
                        matches!(&bound[0], BoundExpr::List { elems, .. } if elems.is_empty());
                    let ety = bound[1].ty();
                    let concrete =
                        !empty_list && *elem != LogicalType::Any && ety != LogicalType::Any;
                    if concrete && lname == "list_contains" {
                        // A pair with no comparison combine at all is the
                        // bespoke C++ error, child type first.
                        if !koko_function::comparison_comparable(&elem, &ety) {
                            return Err(Error::binder(format!(
                                "Cannot compare {elem} and {ety} in list_contains function."
                            )));
                        }
                        // Otherwise C++ unifies (child, element) on the
                        // comparison lattice and blames whichever side can't
                        // cast: list_contains(['1','2'], 2) blames the
                        // STRING[] list expecting INT64[];
                        // list_contains([1,2,3], '2') blames the '2'.
                        let common = koko_function::comparison_common_type(&elem, &ety)
                            .unwrap_or_else(|| (*elem).clone());
                        if !assignable(&elem, &common) {
                            return Err(Error::binder(format!(
                                "Expression {} has data type {} but expected {}. \
                                 Implicit cast is not supported.",
                                expr_name(&args[0]),
                                LogicalType::List(elem.clone()),
                                LogicalType::List(Box::new(common))
                            )));
                        }
                        if !assignable(&ety, &common) {
                            return Err(Error::binder(format!(
                                "Expression {} has data type {} but expected {}. \
                                 Implicit cast is not supported.",
                                expr_name(&args[1]),
                                ety.name(),
                                common.name()
                            )));
                        }
                    }
                    if concrete
                        && !assignable(&ety, &elem)
                        && matches!(lname.as_str(), "list_append" | "list_prepend")
                    {
                        return Err(Error::binder(format!(
                            "Cannot bind {} with parameter type {} and {}.",
                            name.to_uppercase(),
                            bound[0].ty(),
                            ety
                        )));
                    }
                }
            }
            // An empty list literal adopts the OTHER list argument's element
            // type (C++ canCastStatically): LIST_CAT(['7','3'], []) binds as
            // STRING[] × STRING[] instead of a bind-time mismatch.
            if bound.len() == 2 {
                for i in 0..2 {
                    if let Some(child) = bound[1 - i].ty().list_child().cloned() {
                        if let BoundExpr::List { elems, ty } = &mut bound[i] {
                            if elems.is_empty() {
                                *ty = LogicalType::List(Box::new(child));
                            }
                        }
                    }
                }
            }
            bound = coerce_string_params(&lname, bound);
            let arg_types: Vec<LogicalType> = bound.iter().map(|a| a.ty()).collect();
            let ty = koko_function::scalar_func_result_type(name, &arg_types)?;
            // With `disable_map_key_check=false`, map() validates its keys at
            // eval (NULL keys reject) — routed via the internal checked name.
            let lname = if lname == "map" && !self.disable_map_key_check {
                "map_checked".to_string()
            } else {
                lname
            };
            return Ok(BoundExpr::Call {
                name: lname,
                args: bound,
                ty,
            });
        }
        // An unrecognized function name (not a built-in scalar/aggregate/lambda,
        // sequence fn, cast, or — after macro expansion — a macro).
        Err(unknown_function_error(name))
    }

    // ---- helpers ----

    fn add_var(&mut self, name: Option<String>, kind: VarKind, properties: Vec<PropInfo>) -> VarId {
        let id = VarId(self.vars.len() as u32);
        let (name, anonymous) = match name {
            Some(n) => {
                self.scope.insert(n.clone(), id);
                (n, false)
            }
            None => {
                let n = format!("_anon_{}", self.anon);
                self.anon += 1;
                (n, true)
            }
        };
        self.vars.push(VarInfo {
            name,
            anonymous,
            kind,
            properties,
            value_backed: false,
        });
        id
    }

    /// Introduce an `UNWIND` alias. Node-valued list elements become a normal node
    /// binding (id + properties re-exploded by the planner/processor) so a later
    /// `MATCH (alias)-[...]` can reuse them like the C++ node replacement path.
    fn add_unwind_var(&mut self, name: String, ty: LogicalType) -> VarId {
        match ty {
            LogicalType::Node(table) => match self.catalog.node_table(table) {
                Some(entry) => {
                    let label = entry.name.clone();
                    let props = self.node_props(table);
                    let id = self.add_var(
                        Some(name),
                        VarKind::Node {
                            tables: vec![table],
                            label,
                        },
                        props,
                    );
                    self.vars[id.0 as usize].value_backed = true;
                    id
                }
                None => self.add_scalar_var(name, LogicalType::Node(table)),
            },
            ty => self.add_scalar_var(name, ty),
        }
    }

    /// Introduce a scalar (value-typed) variable, e.g. from `UNWIND … AS name`.
    fn add_scalar_var(&mut self, name: String, ty: LogicalType) -> VarId {
        self.add_var(Some(name), VarKind::Scalar { ty }, Vec::new())
    }

    fn lookup_var(&self, name: &str) -> Result<VarId> {
        // C++ variable names are case-insensitive (`UNWIND [1] AS a RETURN A`
        // resolves; defining `A` after `a` is "already exists").
        self.scope
            .get(name)
            .copied()
            .or_else(|| {
                self.scope
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| *v)
            })
            .ok_or_else(|| Error::binder(format!("Variable {name} is not in scope.")))
    }

    fn var_value_type(&self, var: VarId) -> LogicalType {
        match &self.vars[var.0 as usize].kind {
            // Unlabeled patterns may have no candidate catalog table (empty
            // scan/extend). The runtime then produces zero rows, so the concrete
            // node/rel table id is unobservable; use Any as a placeholder instead
            // of indexing the empty candidate set during binding.
            VarKind::Node { tables, .. } => tables
                .first()
                .copied()
                .map(LogicalType::Node)
                .unwrap_or(LogicalType::Any),
            VarKind::Rel {
                recursive: Some(_), ..
            } => LogicalType::RecursiveRel,
            VarKind::Rel { tables, .. } => tables
                .first()
                .copied()
                .map(LogicalType::Rel)
                .unwrap_or(LogicalType::Any),
            VarKind::Path { .. } => LogicalType::RecursiveRel,
            VarKind::Scalar { ty } => ty.clone(),
        }
    }

    fn var_data_type_name(&self, var: VarId) -> String {
        match &self.vars[var.0 as usize].kind {
            VarKind::Node { .. } => "NODE".to_string(),
            VarKind::Rel {
                recursive: Some(_), ..
            } => "RECURSIVE_REL".to_string(),
            VarKind::Rel { .. } => "REL".to_string(),
            VarKind::Path { .. } => "RECURSIVE_REL".to_string(),
            VarKind::Scalar { ty } => ty.to_string(),
        }
    }

    /// A bound reference to a whole variable (a scalar value, or a node/rel value).
    fn var_ref_expr(&self, var: VarId) -> BoundExpr {
        let info = &self.vars[var.0 as usize];
        if info.is_scalar() {
            BoundExpr::ScalarVar {
                var,
                ty: info.scalar_type(),
            }
        } else {
            BoundExpr::NodeRef {
                var,
                ty: self.var_value_type(var),
            }
        }
    }

    fn ensure_table_readable(&self, table: TableId) -> Result<()> {
        if let Some(error) = self
            .catalog
            .icebug_table(table)
            .and_then(|icebug| icebug.load_error.as_ref())
        {
            return Err(Error::runtime(error.clone()));
        }
        Ok(())
    }

    /// Resolve a MATCH node pattern's labels to its candidate table set: every
    /// node table when unlabeled (`()`/`(a)`), or the named tables for a labeled
    /// / multi-label (`(a:A:B)`) pattern. The matched node's actual table is
    /// recovered at runtime from its internal id.
    fn resolve_node_tables(&self, labels: &[String]) -> Result<Vec<TableId>> {
        if let Some(tables) = self.catalog.any_tables() {
            return Ok(vec![tables.nodes]);
        }
        if labels.is_empty() {
            // An unlabeled pattern matches every node table; with none, it matches
            // nothing — e.g. `MATCH (n) DELETE n` on an empty database is a no-op,
            // not an error (the scan over zero tables simply yields zero rows).
            let tables = self.catalog.node_table_ids();
            for &table in &tables {
                self.ensure_table_readable(table)?;
            }
            return Ok(tables);
        }
        let mut tables = Vec::with_capacity(labels.len());
        for l in labels {
            match self.catalog.node_table_by_name(l) {
                Some(t) if !tables.contains(&t.id) => {
                    self.ensure_table_readable(t.id)?;
                    tables.push(t.id);
                }
                Some(_) => {} // duplicate label, ignore
                None => return Err(Error::binder(format!("Table {l} does not exist."))),
            }
        }
        Ok(tables)
    }

    /// Resolve a node pattern that must denote exactly one table (CREATE/MERGE
    /// target). C++ wording, with the pattern variable as typed (anonymous →
    /// empty, giving the oracle's double space).
    fn resolve_single_node_label(&self, var: &str, labels: &[String]) -> Result<TableId> {
        if let Some(tables) = self.catalog.any_tables() {
            return Ok(tables.nodes);
        }
        if labels.is_empty() {
            // An unlabeled CREATE binds to the catalog's only node table; with
            // several candidates C++ uses the *multiple*-labels wording, with
            // none the empty-labels one (oracle-verified).
            let ids = self.catalog.node_table_ids();
            match ids.len() {
                1 => return Ok(ids[0]),
                0 => {
                    return Err(Error::binder(format!(
                        "Create node {var} with empty node labels is not supported."
                    )));
                }
                _ => {
                    return Err(Error::binder(format!(
                        "Create node {var} with multiple node labels is not supported."
                    )));
                }
            }
        }
        if labels.len() > 1 {
            return Err(Error::binder(format!(
                "Create node {var} with multiple node labels is not supported."
            )));
        }
        match self.catalog.node_table_by_name(&labels[0]) {
            Some(t) => {
                self.ensure_table_readable(t.id)?;
                Ok(t.id)
            }
            None => Err(Error::binder(format!(
                "Table {} does not exist.",
                labels[0]
            ))),
        }
    }

    /// The union (by name; first table's type wins) of all columns across
    /// `tables` — the property set of a (possibly polymorphic) node.
    fn node_props_union(&self, tables: &[TableId]) -> Vec<PropInfo> {
        if tables.len() == 1 {
            return self.node_props(tables[0]);
        }
        let mut props: Vec<PropInfo> = Vec::new();
        for &t in tables {
            for c in &self.catalog.node_table(t).unwrap().columns {
                match props
                    .iter_mut()
                    .find(|p| p.name.eq_ignore_ascii_case(&c.name))
                {
                    // A same-named property whose type differs across the
                    // candidate tables scans as the promoted common type
                    // (INT64+DOUBLE → DOUBLE, else STRING); the scan casts
                    // each table's raw value up (`promote_prop`).
                    Some(p) if p.ty != c.ty => p.ty = promote_property_type(&p.ty, &c.ty),
                    Some(_) => {}
                    None => props.push(PropInfo {
                        name: c.name.clone(),
                        column_id: c.column_id.0,
                        ty: c.ty.clone(),
                    }),
                }
            }
        }
        props
    }

    /// Candidate relationship tables for a MATCH rel pattern: the named types, or
    /// every relationship table when unlabeled (`-[]->` / `-[r]->`).
    fn resolve_rel_tables(&self, labels: &[String]) -> Result<Vec<TableId>> {
        if let Some(tables) = self.catalog.any_tables() {
            return Ok(vec![tables.edges]);
        }
        if labels.is_empty() {
            // An unlabeled pattern matches every relationship table; with none, it
            // matches nothing — e.g. `MATCH ()-[e]->() DELETE e` on an empty
            // database is a no-op, not an error (the extend over zero tables simply
            // yields zero rows). Mirrors `resolve_node_tables`.
            return Ok(self.catalog.rel_table_ids());
        }
        let mut v = Vec::with_capacity(labels.len());
        for l in labels {
            match self.catalog.rel_table_by_name(l) {
                Some(t) if !v.contains(&t.id) => v.push(t.id),
                Some(_) => {}
                None => return Err(Error::binder(format!("Table {l} does not exist."))),
            }
        }
        Ok(v)
    }

    /// The union (by name; first table's type wins) of all columns across the
    /// candidate relationship `tables`.
    fn rel_props_union(&self, tables: &[TableId]) -> Vec<PropInfo> {
        if tables.len() == 1 {
            return self.rel_props(tables[0]);
        }
        let mut props: Vec<PropInfo> = Vec::new();
        for &t in tables {
            for c in &self.catalog.rel_table(t).unwrap().columns {
                match props
                    .iter_mut()
                    .find(|p| p.name.eq_ignore_ascii_case(&c.name))
                {
                    // Heterogeneous same-named property → the promoted common
                    // type (see node_props_union).
                    Some(p) if p.ty != c.ty => p.ty = promote_property_type(&p.ty, &c.ty),
                    Some(_) => {}
                    None => props.push(PropInfo {
                        name: c.name.clone(),
                        column_id: c.column_id.0,
                        ty: c.ty.clone(),
                    }),
                }
            }
        }
        props
    }

    /// Narrow a node variable to the intersection of its current candidate tables
    /// with `tables` (used when a relationship pins its endpoints). Recomputes the
    /// representative label and union property set. Keeps the common case
    /// single-table; only genuinely unconstrained nodes stay polymorphic.
    fn narrow_node_to_set(&mut self, var: VarId, tables: &[TableId]) {
        let info = &self.vars[var.0 as usize];
        if !matches!(info.kind, VarKind::Node { .. }) {
            return;
        }
        let narrowed: Vec<TableId> = info
            .node_tables()
            .iter()
            .copied()
            .filter(|t| tables.contains(t))
            .collect();
        // Unchanged, or (defensively) empty — leave the binding as is.
        if narrowed.is_empty() || narrowed == info.node_tables() {
            return;
        }
        let label = self.catalog.node_table(narrowed[0]).unwrap().name.clone();
        let info = &mut self.vars[var.0 as usize];
        info.kind = VarKind::Node {
            tables: narrowed,
            label,
        };
        // The property union stays that of the *declared* candidate set:
        // C++ resolves `b.orgCode` on an unlabeled-but-rel-narrowed `b` against
        // every node table (reads yield NULL / SETs prune where the column is
        // absent); narrowing only prunes the scan.
    }

    fn node_props(&self, table: TableId) -> Vec<PropInfo> {
        self.catalog
            .node_table(table)
            .unwrap()
            .columns
            .iter()
            .map(|c| PropInfo {
                name: c.name.clone(),
                column_id: c.column_id.0,
                ty: c.ty.clone(),
            })
            .collect()
    }

    fn rel_props(&self, table: TableId) -> Vec<PropInfo> {
        self.catalog
            .rel_table(table)
            .unwrap()
            .columns
            .iter()
            .map(|c| PropInfo {
                name: c.name.clone(),
                column_id: c.column_id.0,
                ty: c.ty.clone(),
            })
            .collect()
    }

    fn is_any_var(&self, var: VarId) -> bool {
        let info = &self.vars[var.0 as usize];
        info.node_tables()
            .iter()
            .any(|&table| self.catalog.is_any_node_table(table))
            || info
                .rel_tables()
                .iter()
                .any(|&table| self.catalog.is_any_rel_table(table))
    }

    fn any_data_expr(&self, properties: &[(String, ast::Expr)]) -> Result<BoundExpr> {
        let mut fields = properties
            .iter()
            .map(|(name, expression)| Ok((name.clone(), self.bind_expr(expression)?)))
            .collect::<Result<Vec<_>>>()?;
        fields.reverse();
        let ty = LogicalType::Struct(
            fields
                .iter()
                .map(|(name, expression)| (name.clone(), expression.ty()))
                .collect(),
        );
        Ok(BoundExpr::Struct { fields, ty })
    }

    fn any_label_predicates(&self, var: VarId, labels: &[String], predicates: &mut Vec<BoundExpr>) {
        if self.vars[var.0 as usize].is_recursive() {
            return;
        }
        if labels.is_empty() || !self.is_any_var(var) {
            return;
        }
        let info = &self.vars[var.0 as usize];
        let label = info
            .property("label")
            .expect("ANY variables carry the hidden label column");
        let label_expr = || BoundExpr::Property {
            var,
            prop: label.name.clone(),
            ty: label.ty.clone(),
        };
        if info.is_node() {
            predicates.extend(labels.iter().map(|name| BoundExpr::Call {
                name: "list_contains".to_string(),
                args: vec![
                    label_expr(),
                    BoundExpr::Literal(Value::String(name.clone())),
                ],
                ty: LogicalType::Bool,
            }));
        } else {
            let mut choices = labels.iter().map(|name| BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args: vec![
                    label_expr(),
                    BoundExpr::Literal(Value::String(name.clone())),
                ],
                ty: LogicalType::Bool,
            });
            if let Some(first) = choices.next() {
                predicates.push(choices.fold(first, |left, right| BoundExpr::Scalar {
                    op: ScalarOp::Or,
                    args: vec![left, right],
                    ty: LogicalType::Bool,
                }));
            }
        }
    }

    fn inline_predicates(
        &self,
        var: VarId,
        properties: &[(String, ast::Expr)],
        predicates: &mut Vec<BoundExpr>,
    ) -> Result<()> {
        for (k, e) in properties {
            let info = &self.vars[var.0 as usize];
            let (lhs, prop_ty) = if let Some(prop) = info.property(k) {
                (
                    BoundExpr::Property {
                        var,
                        prop: prop.name.clone(),
                        ty: prop.ty.clone(),
                    },
                    prop.ty.clone(),
                )
            } else if self.is_any_var(var) {
                let data = info
                    .property("data")
                    .expect("ANY variables carry the hidden data column");
                (
                    BoundExpr::ValueProperty {
                        value: Box::new(BoundExpr::Property {
                            var,
                            prop: data.name.clone(),
                            ty: data.ty.clone(),
                        }),
                        prop: k.clone(),
                        ty: LogicalType::Any,
                    },
                    LogicalType::Any,
                )
            } else {
                return Err(Error::binder(format!(
                    "Cannot find property {k} for {}.",
                    info.name
                )));
            };
            let rhs = self.coerce_to(self.bind_expr(e)?, &prop_ty);
            predicates.push(BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args: vec![lhs, rhs],
                ty: LogicalType::Bool,
            });
        }
        Ok(())
    }
}

/// Check that a value of type `src` may be assigned to a column of type `dst`
/// (exact match, an untyped NULL, or the INT64→DOUBLE widening). Prevents a
/// type-mismatched literal from being stored and then panicking on read-back.
/// Push `x` onto `v` only if not already present (small set semantics).
fn push_unique(v: &mut Vec<TableId>, x: TableId) {
    if !v.contains(&x) {
        v.push(x);
    }
}

/// Map the AST's recursive search mode to its bound mirror.
fn bind_recursive_mode(m: ast::RecursiveMode) -> RecursiveMode {
    match m {
        ast::RecursiveMode::All => RecursiveMode::All,
        ast::RecursiveMode::Shortest => RecursiveMode::Shortest,
        ast::RecursiveMode::AllShortest => RecursiveMode::AllShortest,
        ast::RecursiveMode::WShortest => RecursiveMode::WShortest,
        ast::RecursiveMode::AllWShortest => RecursiveMode::AllWShortest,
    }
}

/// Map the AST's path semantic to its bound mirror.
fn bind_path_semantic(s: ast::PathSemantic) -> PathSemantic {
    match s {
        ast::PathSemantic::Walk => PathSemantic::Walk,
        ast::PathSemantic::Trail => PathSemantic::Trail,
        ast::PathSemantic::Acyclic => PathSemantic::Acyclic,
    }
}

/// Reject a property/SET assignment whose value cannot implicitly cast to the
/// target column type, with the C++ oracle wording. `expr` is the rendered
/// assigned expression (`expr_name`). Mirrors C++
/// `ExpressionBinder::implicitCastIfNecessary` → `unsupportedImplicitCastException`
/// (expression_binder.cpp:103-108) — the same message the function/CASE/boolean
/// coercion paths emit, so every implicit-cast rejection reads identically.
/// The element type of a list literal — C++ `ListCreationFunction::bindFunc`.
/// The lattice combine runs over all elements first; when it fails, the fallback
/// depends on whether the elements span *mixed* type IDs: STRING if any element
/// is a STRING, else the FIRST concrete element's type (each element must then
/// implicitly cast to the result, so `[1, true]` is a bind-time rejection with
/// `expected INT64` while `[1, 'a', true]` unifies to STRING). An empty nested
/// list adopts another non-empty list element's type. If every nested list is
/// empty, or the only other elements are non-lists, its default `INT64[]` type
/// participates in the combine. (C++ also maps a combined type still containing
/// ANY to JSON — an extension type, out of scope here.)
fn list_literal_elem_type(elems: &[BoundExpr]) -> LogicalType {
    let has_non_empty_list = elems
        .iter()
        .any(|expr| matches!(expr, BoundExpr::List { elems, .. } if !elems.is_empty()));
    let tys: Vec<LogicalType> = elems
        .iter()
        .filter(|expr| {
            !has_non_empty_list
                || !matches!(expr, BoundExpr::List { elems, .. } if elems.is_empty())
        })
        .map(BoundExpr::ty)
        .collect();
    let mut distinct: Vec<String> = Vec::new();
    for t in tys.iter().filter(|t| !matches!(t, LogicalType::Any)) {
        let k = type_id_key(t);
        if !distinct.contains(&k) {
            distinct.push(k);
        }
    }
    match common_type_strict(tys.iter().cloned()) {
        Some(LogicalType::Any) => LogicalType::Int64,
        Some(t) => t,
        None if distinct.len() > 1 => {
            if distinct.iter().any(|k| k == "STRING") {
                LogicalType::String
            } else {
                tys.iter()
                    .find(|t| !matches!(t, LogicalType::Any))
                    .cloned()
                    .unwrap_or(LogicalType::Int64)
            }
        }
        None => LogicalType::Int64,
    }
}

/// A type's identity at the granularity of C++ `LogicalTypeID`: integer widths
/// are distinct IDs, container types collapse to their constructor.
fn type_id_key(t: &LogicalType) -> String {
    use LogicalType::*;
    match t {
        List(_) => "LIST".to_string(),
        Array(_, _) => "ARRAY".to_string(),
        Struct(_) => "STRUCT".to_string(),
        Map(_, _) => "MAP".to_string(),
        Union(_) => "UNION".to_string(),
        Decimal(_, _) => "DECIMAL".to_string(),
        Node(_) => "NODE".to_string(),
        Rel(_) => "REL".to_string(),
        other => other.name(),
    }
}

/// A `union_value(tag := v)` element adopts a combined UNION type's member
/// names positionally (the C++ list-literal implicit cast retags
/// `union_value(b := 7)` into `UNION(a INT64)`).
fn adopt_union_member_names(value: &mut BoundExpr, dst: &LogicalType) {
    if let (BoundExpr::Call { name, ty, .. }, LogicalType::Union(dm)) = (&mut *value, dst) {
        if name == "union_value" {
            if let LogicalType::Union(m) = ty {
                if m.len() == dm.len() {
                    *ty = dst.clone();
                }
            }
        }
    }
}

/// A struct *literal* assigned to a struct column adopts the column's field
/// names positionally (C++ binds `{revenue: X, locaton: Y}` into
/// `STRUCT(revenue INT16, location STRING[])` by position — oracle-verified;
/// only arity must match, names are the target's).
fn adopt_struct_field_names(value: &mut BoundExpr, dst: &LogicalType) {
    if let (BoundExpr::Struct { fields, ty }, LogicalType::Struct(dfields)) = (&mut *value, dst) {
        if fields.len() == dfields.len() {
            for ((name, fval), (dname, dty)) in fields.iter_mut().zip(dfields) {
                *name = dname.clone();
                // Nested struct literals adopt recursively (the C++ positional
                // cast descends — `stock: {price: ..., volumn: ...}`).
                adopt_struct_field_names(fval, dty);
            }
            *ty = LogicalType::Struct(fields.iter().map(|(n, v)| (n.clone(), v.ty())).collect());
        }
    }
}

/// The C++ `canCastStatically` escape for LITERAL shapes whose declared type
/// under-determines them: a NULL literal casts to anything; an empty list
/// literal to any list type (no element can fail — so `CREATE (:T {strs: []})`
/// into `STRING[]` works though empty lists default to INT64[]); a struct
/// literal casts field-wise, so `{first: NULL, userid: NULL}` (NULL fields
/// defaulting to STRING) still assigns into `STRUCT(first STRING, userid INT64)`.
fn statically_castable(value: &BoundExpr, dst: &LogicalType) -> bool {
    if matches!(value, BoundExpr::Literal(Value::Null)) {
        return true;
    }
    match (value, dst) {
        (BoundExpr::List { elems, .. }, LogicalType::List(_)) if elems.is_empty() => true,
        (BoundExpr::Struct { fields, .. }, LogicalType::Struct(dfields)) => {
            fields.len() == dfields.len()
                && fields.iter().zip(dfields).all(|((_, fv), (_, dt))| {
                    assignable(&fv.ty(), dt) || statically_castable(fv, dt)
                })
        }
        _ => false,
    }
}

fn assignable_or_err(value: &BoundExpr, dst: &LogicalType, expr: &str) -> Result<()> {
    let src = value.ty();
    if assignable(&src, dst) || statically_castable(value, dst) {
        Ok(())
    } else {
        Err(Error::binder(format!(
            "Expression {expr} has data type {src} but expected {dst}. \
             Implicit cast is not supported."
        )))
    }
}

/// Whether a value of type `src` may be implicitly cast to `dst` on assignment
/// (the bind-time gate; [`coerce_to`] then wraps the actual cast, which
/// [`koko_function::cast_value`] performs element-wise for nested types).
fn assignable(src: &LogicalType, dst: &LogicalType) -> bool {
    // The bind-time implicit-cast gate lives in koko-common (shared with the
    // exec-time union casts): C++ `CastFunction::hasImplicitCast`.
    koko_common::types::implicitly_castable(src, dst)
}

/// Rewrite `… OPTIONAL MATCH p MATCH q …` so the required MATCH starts a new
/// implicit `WITH *` part. Sequential Cypher semantics: the optional's output
/// (with NULLs) is the required MATCH's input; planning both in one part would
/// instead run the required match FIRST.
fn split_required_after_optional(q: &ast::SingleQuery) -> ast::SingleQuery {
    fn star_with() -> ast::WithClause {
        ast::WithClause {
            projection: ast::ReturnClause {
                distinct: false,
                items: vec![ast::ProjectionItem::Star],
                order_by: Vec::new(),
                skip: None,
                limit: None,
            },
            where_clause: None,
        }
    }
    fn split_reading(
        reading: &[ast::ReadingClause],
        new_parts: &mut Vec<ast::QueryPart>,
    ) -> Vec<ast::ReadingClause> {
        let mut current: Vec<ast::ReadingClause> = Vec::new();
        let mut seen_optional = false;
        for rc in reading {
            if matches!(rc, ast::ReadingClause::Match(m) if !m.optional) && seen_optional {
                new_parts.push(ast::QueryPart {
                    reading: std::mem::take(&mut current),
                    updating: Vec::new(),
                    with: star_with(),
                });
                seen_optional = false;
            }
            if matches!(rc, ast::ReadingClause::Match(m) if m.optional) {
                seen_optional = true;
            }
            current.push(rc.clone());
        }
        current
    }
    let mut new_parts = Vec::new();
    for qp in &q.parts {
        let tail = split_reading(&qp.reading, &mut new_parts);
        new_parts.push(ast::QueryPart {
            reading: tail,
            updating: qp.updating.clone(),
            with: qp.with.clone(),
        });
    }
    let reading = split_reading(&q.reading, &mut new_parts);
    ast::SingleQuery {
        parts: new_parts,
        reading,
        updating: q.updating.clone(),
        ret: q.ret.clone(),
    }
}

fn combine_and(mut preds: Vec<BoundExpr>) -> Option<BoundExpr> {
    match preds.len() {
        0 => None,
        1 => Some(preds.pop().unwrap()),
        _ => Some(BoundExpr::Scalar {
            op: ScalarOp::And,
            args: preds,
            ty: LogicalType::Bool,
        }),
    }
}

/// Extract the property names from a recursive-lambda projection list (each entry
/// is a `param.prop` access); non-property entries are ignored.
fn projection_names(exprs: &[ast::Expr]) -> Result<Vec<String>> {
    exprs
        .iter()
        .map(|e| match e {
            ast::Expr::Property { name, .. } => Ok(name.clone()),
            // Only property accesses may be projected on a recursive rel —
            // a bare variable (or anything else) is the C++ binder error.
            other => Err(Error::binder(format!(
                "Unsupported projection item {} on recursive rel.",
                expr_name(other)
            ))),
        })
        .collect()
}

fn scalar_op_name(op: ScalarOp) -> &'static str {
    match op {
        ScalarOp::Add => "+",
        ScalarOp::Sub => "-",
        ScalarOp::Mul => "*",
        ScalarOp::Div => "/",
        ScalarOp::Mod => "%",
        ScalarOp::Neg => "-",
        ScalarOp::Eq => "EQUALS",
        ScalarOp::Ne => "NOT_EQUALS",
        ScalarOp::Lt => "LESS_THAN",
        ScalarOp::Le => "LESS_THAN_EQUALS",
        ScalarOp::Gt => "GREATER_THAN",
        ScalarOp::Ge => "GREATER_THAN_EQUALS",
        ScalarOp::And => "AND",
        ScalarOp::Or => "OR",
        ScalarOp::Xor => "XOR",
        ScalarOp::Not => "NOT",
        ScalarOp::IsNull => "IS_NULL",
        ScalarOp::IsNotNull => "IS_NOT_NULL",
    }
}

/// Flatten a bound predicate into its top-level `AND` conjuncts.
fn split_and(e: BoundExpr) -> Vec<BoundExpr> {
    match e {
        BoundExpr::Scalar {
            op: ScalarOp::And,
            args,
            ..
        } => args.into_iter().flat_map(split_and).collect(),
        other => vec![other],
    }
}

/// Whether a bound expression references the lambda parameter `name` anywhere.
fn mentions_lambda(e: &BoundExpr, name: &str) -> bool {
    match e {
        BoundExpr::LambdaVar { name: n, .. } => n == name,
        BoundExpr::ValueProperty { value, .. } => mentions_lambda(value, name),
        BoundExpr::Cast { expr, .. } => mentions_lambda(expr, name),
        BoundExpr::Scalar { args, .. }
        | BoundExpr::Call { args, .. }
        | BoundExpr::List { elems: args, .. } => args.iter().any(|a| mentions_lambda(a, name)),
        BoundExpr::Struct { fields, .. } => fields.iter().any(|(_, v)| mentions_lambda(v, name)),
        BoundExpr::ListLambda { list, body, .. } => {
            mentions_lambda(list, name) || mentions_lambda(body, name)
        }
        BoundExpr::Aggregate { arg, .. } => arg.as_ref().is_some_and(|a| mentions_lambda(a, name)),
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            operand.as_ref().is_some_and(|o| mentions_lambda(o, name))
                || branches
                    .iter()
                    .any(|(c, r)| mentions_lambda(c, name) || mentions_lambda(r, name))
                || else_.as_ref().is_some_and(|e| mentions_lambda(e, name))
        }
        _ => false,
    }
}

fn cmp_op(op: ast::CmpOp) -> ScalarOp {
    match op {
        ast::CmpOp::Eq => ScalarOp::Eq,
        ast::CmpOp::Ne => ScalarOp::Ne,
        ast::CmpOp::Lt => ScalarOp::Lt,
        ast::CmpOp::Le => ScalarOp::Le,
        ast::CmpOp::Gt => ScalarOp::Gt,
        ast::CmpOp::Ge => ScalarOp::Ge,
    }
}

fn arith_op(op: ast::ArithOp) -> ScalarOp {
    match op {
        ast::ArithOp::Add => ScalarOp::Add,
        ast::ArithOp::Sub => ScalarOp::Sub,
        ast::ArithOp::Mul => ScalarOp::Mul,
        ast::ArithOp::Div => ScalarOp::Div,
        ast::ArithOp::Mod => ScalarOp::Mod,
    }
}

/// A best-effort default output-column name for an unaliased projection item.
/// (Column names are cosmetic in P0 — not compared by the `.test` runner unless
/// `-CHECK_COLUMN_NAMES` is set.)
/// Reject duplicate field names in a STRUCT value (a `{…}` literal or
/// `struct_pack`), matching C++ (`Found duplicate field {name} in STRUCT.`).
/// Case-sensitive, like the STRUCT/UNION *type* declaration check.
fn check_struct_field_dups<T>(fields: &[(String, T)]) -> Result<()> {
    for (i, (name, _)) in fields.iter().enumerate() {
        if fields[..i].iter().any(|(n, _)| n == name) {
            return Err(Error::binder(format!(
                "Found duplicate field {name} in STRUCT."
            )));
        }
    }
    Ok(())
}

/// Whether a bound SKIP/LIMIT expression is constant: free of variable,
/// property, aggregate and subquery references (parameters were substituted
/// with their literals at bind).
fn skip_limit_constant(e: &BoundExpr) -> bool {
    use BoundExpr::*;
    match e {
        Literal(_) => true,
        Cast { expr, .. } => skip_limit_constant(expr),
        Scalar { args, .. } | Call { args, .. } => args.iter().all(skip_limit_constant),
        List { elems, .. } => elems.iter().all(skip_limit_constant),
        Struct { fields, .. } => fields.iter().all(|(_, v)| skip_limit_constant(v)),
        Case {
            operand,
            branches,
            else_,
            ..
        } => {
            operand.as_deref().is_none_or(skip_limit_constant)
                && branches
                    .iter()
                    .all(|(c, r)| skip_limit_constant(c) && skip_limit_constant(r))
                && else_.as_deref().is_none_or(skip_limit_constant)
        }
        _ => false,
    }
}

/// The error for a function name with no scalar binding: a name that exists in
/// the oracle catalog as a table/copy entry gets the C++ entry-kind error (note
/// the trailing space), anything else the Catalog does-not-exist (name
/// uppercased — C++ function lookup is case-insensitive).
fn unknown_function_error(name: &str) -> Error {
    let upper = name.to_uppercase();
    let kind = koko_function::catalog_data::FUNCTION_CATALOG
        .iter()
        .find(|(n, _, _)| *n == upper)
        .map(|(_, k, _)| *k);
    let entry = match kind {
        Some("TABLE FUNCTION") => Some("TABLE_FUNCTION_ENTRY"),
        Some("STANDALONE TABLE FUNCTION") => Some("STANDALONE_TABLE_FUNCTION_ENTRY"),
        Some("COPY FUNCTION") => Some("COPY_FUNCTION_ENTRY"),
        _ => None,
    };
    // Known extension functions get the C++ INSTALL/LOAD hint.
    let extension = match upper.as_str() {
        n if n == "TO_JSON" || n.starts_with("JSON_") || n == "JSON" => Some("JSON"),
        _ => None,
    };
    match (entry, extension) {
        (Some(e), _) => Error::binder(format!(
            "{upper} is a {e}. Scalar function, aggregate function or macro was expected. "
        )),
        (None, Some(ext)) => Error::catalog(format!(
            "function {upper} is not defined. This function exists in the {ext} extension. \
             You can install and load the extension by running 'INSTALL {ext}; LOAD EXTENSION \
             {ext};'."
        )),
        (None, None) => Error::catalog(format!("function {upper} does not exist.")),
    }
}

/// The C++ `ExpressionType` name of an AST expression, for error messages.
fn ast_expr_kind(e: &ast::Expr) -> &'static str {
    match e {
        ast::Expr::Literal(_) => "LITERAL",
        ast::Expr::Variable(_) => "VARIABLE",
        ast::Expr::Property { .. } => "PROPERTY",
        ast::Expr::Parameter(_) => "PARAMETER",
        ast::Expr::Function { .. } => "FUNCTION",
        ast::Expr::Case { .. } => "CASE_ELSE",
        _ => "FUNCTION",
    }
}

/// The C++ conversion error for an integer literal beyond `u128`: the raw text
/// (minus included when negated) fails the widest integer cast, UINT128.
fn overflow_int_error(text: &str) -> Error {
    Error::conversion(format!(
        "Cast failed. Could not convert \"{text}\" to UINT128."
    ))
}

fn collect_hint_vars(t: &ast::JoinHint, out: &mut Vec<String>) {
    match t {
        ast::JoinHint::Var(v) => out.push(v.clone()),
        ast::JoinHint::Join(l, r) => {
            collect_hint_vars(l, out);
            collect_hint_vars(r, out);
        }
        ast::JoinHint::MultiJoin(l, names) => {
            collect_hint_vars(l, out);
            out.extend(names.iter().cloned());
        }
    }
}

/// Render a hint subtree for the cannot-resolve error: a leaf is `Scan(v)`,
/// a node-JOIN-its-rel pair collapses to `Scan(node,rel)` (a scan-with-extend
/// unit), everything else renders structurally as `JOIN(l,r)`.
fn render_hint(t: &ast::JoinHint, rels: &[(String, String, String, Vec<TableId>)]) -> String {
    match t {
        ast::JoinHint::Var(v) => format!("Scan({v})"),
        ast::JoinHint::Join(l, r) => {
            if let (ast::JoinHint::Var(a), ast::JoinHint::Var(b)) = (l.as_ref(), r.as_ref()) {
                let pairs = |n: &str, e: &str| {
                    rels.iter()
                        .any(|(rv, src, dst, _)| rv == e && (src == n || dst == n))
                };
                if pairs(a, b) || pairs(b, a) {
                    return format!("Scan({a},{b})");
                }
            }
            format!("JOIN({},{})", render_hint(l, rels), render_hint(r, rels))
        }
        ast::JoinHint::MultiJoin(l, names) => {
            format!("MULTI_JOIN({},{})", render_hint(l, rels), names.join(","))
        }
    }
}

/// C++ reports a nested-aggregation error by the projection item's OUTPUT
/// NAME — the alias when one is given (`… AS c` → `Expression c …`), else the
/// bound expression string (which `bind_expr` already produced). Rewrite the
/// message to the alias here, where the projection name is known.
fn rewrite_nested_agg(e: Error, alias: Option<&str>) -> Error {
    if let (Error::Binder(msg), Some(a)) = (&e, alias) {
        if msg.starts_with("Expression ") && msg.ends_with(" contains nested aggregation.") {
            return Error::binder(format!("Expression {a} contains nested aggregation."));
        }
    }
    e
}

fn expr_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::PatternComprehension { .. } => "PATTERN_COMPREHENSION".to_string(),
        ast::Expr::Literal(v) => v.to_result_string(),
        ast::Expr::OverflowInt(text) => text.clone(),
        ast::Expr::Variable(n) => n.clone(),
        ast::Expr::Property { base, name } => format!("{}.{}", expr_name(base), name),
        ast::Expr::Parameter(n) => format!("${n}"),
        ast::Expr::Star => "*".to_string(),
        ast::Expr::Subquery { kind, .. } => match kind {
            ast::SubqueryKind::Exists => "EXISTS { }".to_string(),
            ast::SubqueryKind::Count => "COUNT { }".to_string(),
        },
        ast::Expr::Function {
            name,
            distinct,
            args,
            arg_names,
        } => {
            // C++ renders `count(*)` as its bound form COUNT_STAR().
            if name.eq_ignore_ascii_case("count") && matches!(args.as_slice(), [ast::Expr::Star]) {
                return "COUNT_STAR()".to_string();
            }
            let inner = args
                .iter()
                .enumerate()
                .map(|(i, a)| match arg_names.get(i).and_then(|o| o.as_deref()) {
                    Some(nm) => format!("{nm} := {}", expr_name(a)),
                    None => expr_name(a),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let d = if *distinct { "DISTINCT " } else { "" };
            // C++ expression toString renders the function name uppercased,
            // through its alias canonicalization (date() is TO_DATE).
            let display = if name.eq_ignore_ascii_case("date") {
                "TO_DATE".to_string()
            } else {
                name.to_uppercase()
            };
            format!("{display}({d}{inner})")
        }
        // C++ renders bound binary expressions functionally: `+(a.age,2)`.
        ast::Expr::Comparison { op, lhs, rhs } => {
            format!("{}({},{})", cmp_symbol(*op), expr_name(lhs), expr_name(rhs))
        }
        ast::Expr::Arithmetic { op, lhs, rhs } => {
            format!(
                "{}({},{})",
                arith_symbol(*op),
                expr_name(lhs),
                expr_name(rhs)
            )
        }
        ast::Expr::And(ts) => ts.iter().map(expr_name).collect::<Vec<_>>().join(" AND "),
        ast::Expr::Or(ts) => ts.iter().map(expr_name).collect::<Vec<_>>().join(" OR "),
        ast::Expr::Xor(a, b) => format!("{} XOR {}", expr_name(a), expr_name(b)),
        ast::Expr::Not(a) => format!("NOT {}", expr_name(a)),
        ast::Expr::Negate(a) => format!("-{}", expr_name(a)),
        ast::Expr::IsNull(a) => format!("{} IS NULL", expr_name(a)),
        ast::Expr::IsNotNull(a) => format!("{} IS NOT NULL", expr_name(a)),
        ast::Expr::List(items) => {
            // C++ renders a list literal as its underlying `LIST_CREATION` call
            // (comma-separated, no spaces) in expression toString.
            format!(
                "LIST_CREATION({})",
                items.iter().map(expr_name).collect::<Vec<_>>().join(",")
            )
        }
        ast::Expr::Struct(fields) => {
            // C++ renders a struct literal as its underlying `STRUCT_PACK` call over the
            // field *values* (keys omitted), comma-separated with no spaces.
            format!(
                "STRUCT_PACK({})",
                fields
                    .iter()
                    .map(|(_, v)| expr_name(v))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        ast::Expr::Case {
            operand,
            when_thens,
            else_,
        } => {
            let mut s = String::from("CASE");
            if let Some(op) = operand {
                s.push_str(&format!(" {}", expr_name(op)));
            }
            for (c, r) in when_thens {
                s.push_str(&format!(" WHEN {} THEN {}", expr_name(c), expr_name(r)));
            }
            if let Some(e) = else_ {
                s.push_str(&format!(" ELSE {}", expr_name(e)));
            }
            s.push_str(" END");
            s
        }
        ast::Expr::Lambda { params, body } => {
            format!("{} -> {}", params.join(", "), expr_name(body))
        }
        ast::Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        } => {
            let mut s = format!("[{var} IN {}", expr_name(list));
            if let Some(p) = predicate {
                s.push_str(&format!(" WHERE {}", expr_name(p)));
            }
            if let Some(pr) = projection {
                s.push_str(&format!(" | {}", expr_name(pr)));
            }
            s.push(']');
            s
        }
    }
}

fn projection_has_aggregates(items: &[ProjItem]) -> bool {
    items.iter().any(|i| match i {
        ProjItem::Scalar { expr, .. } => expr.contains_aggregate(),
        ProjItem::Var { .. } => false,
    })
}

fn is_order_by_key_type_supported(ty: &LogicalType) -> bool {
    !matches!(
        ty,
        LogicalType::Node(_)
            | LogicalType::Rel(_)
            | LogicalType::RecursiveRel
            | LogicalType::InternalId
            | LogicalType::List(_)
            | LogicalType::Array(_, _)
            | LogicalType::Struct(_)
            | LogicalType::Map(_, _)
            | LogicalType::Union(_)
    )
}

fn validate_order_key_type(expr: &ast::Expr, ty: &LogicalType) -> Result<()> {
    if is_order_by_key_type_supported(ty) {
        return Ok(());
    }
    Err(Error::binder(format!(
        "Cannot order by {}. Order by {} is not supported.",
        order_expr_name(expr),
        ty
    )))
}

fn order_expr_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Function { name, args, .. }
            if name.eq_ignore_ascii_case("id") && args.len() == 1 =>
        {
            format!("{}._ID", expr_name(&args[0]))
        }
        _ => expr_name(e),
    }
}

fn bool_arg_or_err(expr: &ast::Expr, bound: &BoundExpr) -> Result<()> {
    let ty = bound.ty();
    if matches!(ty, LogicalType::Bool | LogicalType::Any) {
        return Ok(());
    }
    Err(Error::binder(format!(
        "Expression {} has data type {} but expected BOOL. Implicit cast is not supported.",
        expr_name(expr),
        ty.name()
    )))
}

/// Wrap `e` in a cast to `target` unless it already has that type (or `target`
/// is `Any`, or `e` is a NULL literal — NULL casts to NULL of any type).
/// Coerce a scalar call's arguments at the C++ signature's STRING positions
/// (audit §6.1 slice): every implicitly castable type gains a bind-time
/// CAST → STRING (`lower(123)` → `123`, `left(to_double(1.34), 8)` →
/// `1.340000`). BLOB and graph types stay uncoerced — C++ rejects those with
/// the overload table, and the existing type checks keep rejecting here.
/// Evaluate a literal-only bound expression (`date(2012)`, `CAST('x' AS T)`,
/// `[1,2]`), used to surface a constant operand's own eval error before a
/// comparison Type Mismatch like the C++ binder's constant folding. `None`
/// means "not a constant expression" (contains columns/vars/aggregates).
/// The C++ maximal common type for a same-named property across candidate
/// tables: equal stays; integer widths combine; mixed numerics promote to
/// DOUBLE; anything else unifies through STRING (each value casts at scan).
fn promote_property_type(a: &LogicalType, b: &LogicalType) -> LogicalType {
    use LogicalType as LT;
    if a == b {
        return a.clone();
    }
    let numeric = |t: &LT| {
        matches!(
            t,
            LT::Int(_) | LT::UInt128 | LT::Decimal(_, _) | LT::Float | LT::Double
        )
    };
    match (a, b) {
        (LT::Int(x), LT::Int(y)) => LT::Int(x.combine(*y)),
        _ if numeric(a) && numeric(b) => LT::Double,
        _ => LT::String,
    }
}

fn try_const_eval(e: &BoundExpr) -> Option<Result<Value>> {
    fn eval(e: &BoundExpr) -> Option<Result<Value>> {
        match e {
            BoundExpr::Literal(v) => Some(Ok(v.clone())),
            BoundExpr::Cast { expr, target } => match eval(expr)? {
                Ok(v) => {
                    // C++ resolves STRUCT→STRUCT casts against the DECLARED
                    // source type: a field-name mismatch reports the static
                    // shapes even when a field's value is NULL.
                    if let (LogicalType::Struct(sf), LogicalType::Struct(tf)) = (&expr.ty(), target)
                    {
                        let names_match = sf.len() == tf.len()
                            && sf
                                .iter()
                                .zip(tf)
                                .all(|((sn, _), (tn, _))| sn.eq_ignore_ascii_case(tn));
                        if !names_match {
                            return Some(Err(Error::conversion(format!(
                                "Unsupported casting function from {} to {}.",
                                expr.ty(),
                                target
                            ))));
                        }
                    }
                    Some(koko_function::cast_value(&v, target))
                }
                err => Some(err),
            },
            BoundExpr::Call { name, args, .. } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    match eval(a)? {
                        Ok(v) => vals.push(v),
                        err => return Some(err),
                    }
                }
                Some(koko_function::scalarfn::eval(name, &vals))
            }
            BoundExpr::List { elems, .. } => {
                let mut vals = Vec::with_capacity(elems.len());
                for a in elems {
                    match eval(a)? {
                        Ok(v) => vals.push(v),
                        err => return Some(err),
                    }
                }
                Some(Ok(Value::List(vals)))
            }
            _ => None,
        }
    }
    eval(e)
}

fn is_numeric_type(logical_type: &LogicalType) -> bool {
    matches!(
        logical_type,
        LogicalType::Int(_)
            | LogicalType::Serial
            | LogicalType::UInt128
            | LogicalType::Decimal(_, _)
            | LogicalType::Float
            | LogicalType::Double
    )
}

fn coerce_string_params(name: &str, mut bound: Vec<BoundExpr>) -> Vec<BoundExpr> {
    let Some(positions) = koko_function::scalarfn::string_coerce_positions(name) else {
        return bound;
    };
    for &i in positions {
        let Some(arg) = bound.get(i) else { continue };
        let coercible = !matches!(
            arg.ty(),
            LogicalType::String
                | LogicalType::Any
                | LogicalType::Blob
                | LogicalType::InternalId
                | LogicalType::Node(_)
                | LogicalType::Rel(_)
                | LogicalType::RecursiveRel
        );
        if coercible {
            let e = bound[i].clone();
            bound[i] = coerce_to(e, &LogicalType::String);
        }
    }
    bound
}

fn coerce_to(e: BoundExpr, target: &LogicalType) -> BoundExpr {
    if *target == LogicalType::Any || e.ty() == *target {
        return e;
    }
    if matches!(&e, BoundExpr::Literal(Value::Null)) {
        return e;
    }
    BoundExpr::Cast {
        expr: Box::new(e),
        target: target.clone(),
    }
}

/// The common supertype of a set of element types (NULL/`Any` ignored), using a
/// small numeric-promotion lattice (`INT + DOUBLE → DOUBLE`, widen int widths,
/// any FLOAT → FLOAT/DOUBLE). Falls back to the first concrete type on a mismatch.
fn common_type(types: impl IntoIterator<Item = LogicalType>) -> LogicalType {
    let mut acc = LogicalType::Any;
    for t in types {
        if t == LogicalType::Any {
            continue;
        }
        acc = match (&acc, &t) {
            (LogicalType::Any, _) => t,
            (a, b) if *a == *b => acc,
            (a, b) if a.is_numeric() && b.is_numeric() => {
                // DECIMAL + integer keeps DECIMAL, widened to hold an INT64 at
                // the same scale (audit V14: DECIMAL(10,2) + INT64 → DECIMAL(21,2),
                // oracle-verified); DECIMAL + float drops to DOUBLE.
                // Two DECIMALs keep DECIMAL: max integer digits + max scale
                // (DECIMAL(4,2) + DECIMAL(6,3) → DECIMAL(6,3), oracle).
                if let (LogicalType::Decimal(p1, s1), LogicalType::Decimal(p2, s2)) = (a, b) {
                    let scale = (*s1).max(*s2);
                    let int_digits = (p1 - s1).max(p2 - s2);
                    acc = LogicalType::Decimal((int_digits + scale).min(38), scale);
                    continue;
                }
                let dec = match (a, b) {
                    (LogicalType::Decimal(p, sc), o) | (o, LogicalType::Decimal(p, sc)) => {
                        Some(((*p, *sc), o.clone()))
                    }
                    _ => None,
                };
                if let Some(((p, sc), other_ty)) = dec {
                    if other_ty.int_kind().is_some() || other_ty == LogicalType::Serial {
                        LogicalType::Decimal(p.max(19 + sc).min(38), sc)
                    } else {
                        LogicalType::Double
                    }
                } else if *a == LogicalType::Double || *b == LogicalType::Double {
                    LogicalType::Double
                } else if *a == LogicalType::Float || *b == LogicalType::Float {
                    LogicalType::Float
                } else {
                    // Integer widths combine sign-aware (see IntKind::combine).
                    match (a.int_kind(), b.int_kind()) {
                        (Some(ka), Some(kb)) => LogicalType::Int(ka.combine(kb)),
                        _ => LogicalType::Int64,
                    }
                }
            }
            // Nested LUB: combine list element types (C++ `tryGetMaxLogicalType`
            // recurses into children), so `coalesce([1,2], [1.5])` is DOUBLE[], not
            // INT64[] — and each arg then implicitly casts to it.
            (LogicalType::List(a), LogicalType::List(b)) => {
                LogicalType::List(Box::new(common_type([(**a).clone(), (**b).clone()])))
            }
            // C++'s combine lattice (oracle-verified via list literals): STRING
            // loses to every concrete type (['a',1] targets INT64, blaming 'a');
            // BOOL loses to numerics ([1,true] targets INT64); DATE promotes
            // into any TIMESTAMP flavor ([date,ts] → TIMESTAMP[]).
            (LogicalType::String, _) => t,
            (_, LogicalType::String) => acc,
            (
                LogicalType::Date,
                LogicalType::Timestamp
                | LogicalType::TimestampNs
                | LogicalType::TimestampMs
                | LogicalType::TimestampSec
                | LogicalType::TimestampTz,
            ) => t,
            (
                LogicalType::Timestamp
                | LogicalType::TimestampNs
                | LogicalType::TimestampMs
                | LogicalType::TimestampSec
                | LogicalType::TimestampTz,
                LogicalType::Date,
            ) => acc,
            _ => acc, // incompatible — keep the first concrete type
        };
    }
    acc
}

/// Like [`common_type`], but `None` when a pair has NO max in C++'s lattice
/// (INT64×BOOL) — coalesce falls back to STRING exactly then, while a pair
/// with a max (INT64×STRING → INT64) instead bind-errors on the non-castable
/// argument (both oracle/corpus-verified).
fn common_type_strict(types: impl IntoIterator<Item = LogicalType>) -> Option<LogicalType> {
    let mut acc = LogicalType::Any;
    for t in types {
        if t == LogicalType::Any {
            continue;
        }
        if acc == LogicalType::Any || acc == t {
            acc = t;
            continue;
        }
        // Two lists combine element-wise (C++ tryCombineDataType recurses:
        // INT64[] × STRING[] → INT64[], since STRING loses to INT64).
        if let (LogicalType::List(a), LogicalType::List(b)) = (&acc, &t) {
            let elem = common_type_strict([(**a).clone(), (**b).clone()])?;
            acc = LogicalType::List(Box::new(elem));
            continue;
        }
        // Same-arity STRUCTs/UNIONs combine POSITIONALLY with the RIGHT side's
        // member names (oracle: [{a: 5, b: 3}, {c: 2, d: 4}] types as
        // STRUCT(c INT64, d INT64)[]; a left-fold over unions keeps the last).
        if let (LogicalType::Struct(a), LogicalType::Struct(b)) = (&acc, &t) {
            if a.len() != b.len() {
                return None;
            }
            let fields = a
                .iter()
                .zip(b)
                .map(|((_, at), (bn, bt))| {
                    common_type_strict([at.clone(), bt.clone()]).map(|ct| (bn.clone(), ct))
                })
                .collect::<Option<Vec<_>>>()?;
            acc = LogicalType::Struct(fields);
            continue;
        }
        if let (LogicalType::Union(a), LogicalType::Union(b)) = (&acc, &t) {
            if a.len() != b.len() {
                return None;
            }
            let members = a
                .iter()
                .zip(b)
                .map(|((_, at), (bn, bt))| {
                    common_type_strict([at.clone(), bt.clone()]).map(|ct| (bn.clone(), ct))
                })
                .collect::<Option<Vec<_>>>()?;
            acc = LogicalType::Union(members);
            continue;
        }
        let combined = common_type([acc.clone(), t.clone()]);
        // `common_type` keeps the first type on an incompatible pair; detect
        // that as "no max" unless the pair genuinely combines to `acc`.
        let has_max = combined != acc
            || t == acc
            || (acc.is_numeric() && t.is_numeric())
            || matches!(t, LogicalType::String)
            || (matches!(
                acc,
                LogicalType::Timestamp
                    | LogicalType::TimestampNs
                    | LogicalType::TimestampMs
                    | LogicalType::TimestampSec
                    | LogicalType::TimestampTz
            ) && t == LogicalType::Date);
        if !has_max {
            return None;
        }
        acc = combined;
    }
    Some(acc)
}

fn cmp_symbol(op: ast::CmpOp) -> &'static str {
    match op {
        ast::CmpOp::Eq => "=",
        ast::CmpOp::Ne => "<>",
        ast::CmpOp::Lt => "<",
        ast::CmpOp::Le => "<=",
        ast::CmpOp::Gt => ">",
        ast::CmpOp::Ge => ">=",
    }
}

fn arith_symbol(op: ast::ArithOp) -> &'static str {
    match op {
        ast::ArithOp::Add => "+",
        ast::ArithOp::Sub => "-",
        ast::ArithOp::Mul => "*",
        ast::ArithOp::Div => "/",
        ast::ArithOp::Mod => "%",
    }
}

/// Recognize the sequence value functions (case-insensitive).
fn sequence_fn(name: &str) -> Option<SequenceFn> {
    if name.eq_ignore_ascii_case("nextval") {
        Some(SequenceFn::NextVal)
    } else if name.eq_ignore_ascii_case("currval") {
        Some(SequenceFn::CurrVal)
    } else {
        None
    }
}

#[cfg(test)]
mod frontend_tests {
    use super::*;
    use koko_catalog::Catalog;
    use koko_parser::parse_statement;

    fn bind(sql: &str) -> Result<BoundStatement> {
        bind_with_config(sql, &SessionConfig::default())
    }

    fn bind_with_config(sql: &str, config: &SessionConfig) -> Result<BoundStatement> {
        let catalog = Catalog::new();
        let statement = parse_statement(sql)?;
        bind_statement(&catalog, &statement, &HashMap::new(), config)
    }

    #[test]
    fn binds_copy_to_query_schema_and_csv_options() {
        let BoundStatement::CopyTo(copy) =
            bind("COPY (RETURN 1 AS id, 'Ada' AS name) TO 'people.csv' (header=true, delim='|')")
                .unwrap()
        else {
            panic!("expected bound COPY TO")
        };
        assert_eq!(
            copy.columns,
            vec![
                ("id".to_string(), LogicalType::Int64),
                ("name".to_string(), LogicalType::String),
            ]
        );
        let BoundOutputOptions::Csv(options) = copy.options else {
            panic!("expected CSV options")
        };
        assert_eq!(options.header, Some(true));
        assert_eq!(options.delimiter, Some(b'|'));
    }

    #[test]
    fn binds_copy_to_parquet_compression_and_rejects_unsupported_formats() {
        let BoundStatement::CopyTo(copy) =
            bind("COPY (RETURN 1) TO 'out.parquet' (compression='zstd')").unwrap()
        else {
            panic!("expected bound COPY TO")
        };
        assert!(matches!(
            copy.options,
            BoundOutputOptions::Parquet {
                compression: BoundParquetCompression::Zstd
            }
        ));

        assert_eq!(
            bind("COPY (RETURN 1) TO 'out.npy'")
                .unwrap_err()
                .to_string(),
            "Runtime exception: Exporting query result to the 'npy' file is currently not supported."
        );
        assert_eq!(
            bind("COPY (RETURN 1) TO 'out.unknown'")
                .unwrap_err()
                .to_string(),
            "Runtime exception: Exporting query result to the 'unknown' file is currently not supported."
        );
        assert_eq!(
            bind("COPY (RETURN 1) TO 'out'").unwrap_err().to_string(),
            "Runtime exception: Exporting query result to the '' file is currently not supported."
        );
        assert_eq!(
            bind("COPY (RETURN 1) TO 'out.csv.gz'")
                .unwrap_err()
                .to_string(),
            "IO exception: Writing to compressed files is not supported yet."
        );
        assert_eq!(
            bind("COPY (RETURN 1) TO 'out.parquet' (compression=true)")
                .unwrap_err()
                .to_string(),
            "Runtime exception: Parquet compression option expects a string value, got: BOOL."
        );
        assert_eq!(
            bind("COPY (RETURN 1) TO 'out.parquet' (compression1='zstd')")
                .unwrap_err()
                .to_string(),
            "Runtime exception: Unrecognized parquet option: COMPRESSION1."
        );
        assert_eq!(
            bind("COPY (RETURN 1) TO 'out.parquet' (compression='lz4_raw1')")
                .unwrap_err()
                .to_string(),
            "Runtime exception: Unrecognized parquet compression option: lz4_raw1."
        );
    }

    #[test]
    fn binds_export_defaults_csv_options_and_schema_only() {
        let BoundStatement::ExportDatabase(default) = bind("EXPORT DATABASE 'snapshot'").unwrap()
        else {
            panic!("expected bound EXPORT DATABASE")
        };
        assert_eq!(default.options.format(), FileFormat::Parquet);
        assert!(!default.schema_only);

        let BoundStatement::ExportDatabase(csv) =
            bind("EXPORT DATABASE 'snapshot' (format='csv', header=true)").unwrap()
        else {
            panic!("expected bound EXPORT DATABASE")
        };
        assert_eq!(csv.options.format(), FileFormat::Csv);
        let BoundOutputOptions::Csv(csv_options) = csv.options else {
            panic!("expected CSV options")
        };
        assert_eq!(csv_options.header, Some(true));

        let BoundStatement::ExportDatabase(schema) =
            bind("EXPORT DATABASE 'snapshot' (schema_only=true)").unwrap()
        else {
            panic!("expected bound EXPORT DATABASE")
        };
        assert!(schema.schema_only);

        assert_eq!(
            bind("EXPORT DATABASE 'snapshot' (schema_only=true, format='csv')")
                .unwrap_err()
                .to_string(),
            "Binder exception: When 'SCHEMA_ONLY' option is set to true in export database, no other options are allowed."
        );
        assert_eq!(
            bind("EXPORT DATABASE 'snapshot' (schema_only='yes')")
                .unwrap_err()
                .to_string(),
            "Binder exception: The 'SCHEMA_ONLY' option must have a BOOL value."
        );
        assert_eq!(
            bind("EXPORT DATABASE 'snapshot' (format=false)")
                .unwrap_err()
                .to_string(),
            "Binder exception: The type of format option must be a string."
        );
        assert_eq!(
            bind("EXPORT DATABASE 'snapshot' (format='npy')")
                .unwrap_err()
                .to_string(),
            "Binder exception: Export database currently only supports csv and parquet files."
        );
        assert_eq!(
            bind("EXPORT DATABASE 'snapshot' (format='parquet', header=true)")
                .unwrap_err()
                .to_string(),
            "Binder exception: Only export to csv can have options."
        );
    }

    #[test]
    fn binds_import_resolved_paths_and_preserves_copy_to_prepared_metadata() {
        let config = SessionConfig {
            base_dir: std::path::PathBuf::from("/statement/base"),
            home_directory: Some(std::path::PathBuf::from("/session/home")),
            ..SessionConfig::default()
        };
        let BoundStatement::ImportDatabase(import) =
            bind_with_config("IMPORT DATABASE 'snapshot'", &config).unwrap()
        else {
            panic!("expected bound IMPORT DATABASE")
        };
        assert_eq!(import.path, "/statement/base/snapshot");

        let BoundStatement::CopyTo(copy) =
            bind_with_config("COPY (RETURN 1) TO '~/out.csv'", &config).unwrap()
        else {
            panic!("expected bound COPY TO")
        };
        assert_eq!(copy.path, "/session/home/out.csv");
        let catalog = Catalog::new();
        let statement = parse_statement("COPY (RETURN $value + 1 AS value) TO 'out.csv'").unwrap();
        let prepared = bind_statement_for_prepare(
            &catalog,
            &statement,
            &["value".to_string()],
            &HashMap::new(),
            &SessionConfig::default(),
        )
        .unwrap();
        assert!(matches!(prepared.statement, BoundStatement::CopyTo(_)));
        assert!(prepared.parameter_types.contains_key("value"));
    }
}
