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

mod expression;
mod query;
mod statement;

use expression::{LambdaBinding, PendingSubquery, PreparedParameterState};
use statement::bind_storage_direction;

use crate::config::SessionConfig;
use crate::load_options::{
    bool_value as opt_bool, char_value as opt_char, int_value as opt_int, option_type_name,
    string_list_value as opt_string_list, string_value as opt_string,
    validate_file_format as validate_file_format_option,
};
use crate::table_function::{schema as table_func_schema, source_name as table_func_source_name};

use koko_catalog::Catalog;
use koko_common::{
    Error, LogicalType, RelStorageDirection, Result, TableId, Value,
    file_resolver::{FileFormat, FileResolverConfig, resolve_files},
};
use koko_function::{
    AggOp, BuiltinFunction, BuiltinScalar, ScalarOp, resolve_builtin, resolve_builtin_scalar,
};
use koko_ir::bound::*;
use koko_parser::{ast, expr_to_cypher};
use std::collections::{HashMap, HashSet};

/// A preparation-only binding plus the binder-inferred type of each symbolic parameter.
pub struct PreparedBinding {
    pub statement: BoundStatement,
    pub parameter_types: HashMap<String, LogicalType>,
}

/// Bind a parsed statement against the catalog, substituting concrete parameter values.
pub(crate) fn bind_statement(
    catalog: &Catalog,
    stmt: &ast::Statement,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
) -> Result<BoundStatement> {
    bind_statement_with_state(catalog, stmt, params, config, None)
}

/// Bind for preparation with symbolic parameters. `initial_types` contains
/// caller-supplied seed types; absent entries remain `Any` until constrained by
/// the regular binder's assignment, comparison, function, or boolean rules.
pub(crate) fn bind_statement_for_prepare(
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
    let mut state = PreparedParameterState { types, error: None };
    let values = HashMap::new();
    let statement = bind_statement_with_state(catalog, stmt, &values, config, Some(&mut state))?;
    if let Some(error) = state.error {
        return Err(error);
    }
    Ok(PreparedBinding {
        statement,
        parameter_types: state.types,
    })
}

fn bind_statement_with_state(
    catalog: &Catalog,
    stmt: &ast::Statement,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
    state: Option<&mut PreparedParameterState>,
) -> Result<BoundStatement> {
    match stmt {
        ast::Statement::Query(query) => bind_regular_query(catalog, query, params, config, state),
        ast::Statement::CreateTableAs(create) => {
            bind_create_table_as(catalog, create, params, config, state)
        }
        other => Binder::with_state(catalog, params, config, state).bind_nonquery(other),
    }
}

/// Bind `CREATE … AS <query>`: bind the inner query, infer its result schema (the
/// first column is the primary key for a node table), and validate.
fn bind_create_table_as(
    catalog: &Catalog,
    c: &ast::CreateTableAs,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
    state: Option<&mut PreparedParameterState>,
) -> Result<BoundStatement> {
    let BoundStatement::Query(query) =
        bind_regular_query(catalog, &c.query, params, config, state)?
    else {
        unreachable!("bind_regular_query yields a Query");
    };
    let columns: Vec<BoundColumn> = query
        .result_columns()
        .iter()
        .cloned()
        .map(|(name, logical_type)| BoundColumn {
            type_text: logical_type.to_string(),
            name,
            logical_type,
            generation: BoundColumnGeneration::None,
        })
        .collect();
    if columns.is_empty() {
        return Err(Error::binder("Subquery returns no columns".to_string()));
    }
    if c.is_node {
        // The first result column becomes the primary key.
        let pk_ty = &columns[0].logical_type;
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
            .map(|table| table.id())
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
/// namespace, then one canonical result schema is resolved across all operands.
fn bind_regular_query(
    catalog: &Catalog,
    rq: &ast::RegularQuery,
    params: &HashMap<String, Value>,
    config: &SessionConfig,
    mut state: Option<&mut PreparedParameterState>,
) -> Result<BoundStatement> {
    let mut operands = Vec::with_capacity(rq.singles.len());
    for single in &rq.singles {
        operands.push(
            Binder::with_state(catalog, params, config, state.as_deref_mut()).bind_query(single)?,
        );
    }

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
    }

    if let Some(parameters) = state.as_deref() {
        refresh_direct_parameter_types(&mut operands, parameters);
    }
    let operand_columns: Vec<_> = operands.iter().map(classify_union_columns).collect();
    let result_columns = resolve_union_columns(&operand_columns)?;
    if let Some(parameters) = state {
        constrain_direct_union_parameters(&mut operands, &result_columns, parameters);
    }

    // Plain UNION (no ALL boundary) deduplicates; UNION ALL and a lone operand do not.
    let distinct = !rq.union_all.is_empty() && rq.union_all.iter().all(|&a| !a);
    Ok(BoundStatement::Query(Box::new(BoundRegularQuery::new(
        operands,
        distinct,
        result_columns,
    ))))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UnionTypeProvenance {
    UntypedNull,
    UnknownParameter(String),
    Concrete(LogicalType),
    DynamicAny,
}

fn classify_union_columns(query: &BoundQuery) -> Vec<(String, UnionTypeProvenance)> {
    let columns = query.result_columns();
    let Some(projection) = query.parts.last().and_then(|part| part.projection.as_ref()) else {
        return Vec::new();
    };
    debug_assert_eq!(projection.items.len(), columns.len());
    projection
        .items
        .iter()
        .zip(columns)
        .map(|(item, (name, logical_type))| {
            let provenance = match item {
                ProjItem::Scalar {
                    expr: BoundExpr::Literal(Value::Null),
                    ..
                } => UnionTypeProvenance::UntypedNull,
                ProjItem::Scalar {
                    expr: BoundExpr::Parameter { name, ty },
                    ..
                } if *ty == LogicalType::Any => UnionTypeProvenance::UnknownParameter(name.clone()),
                _ if logical_type == LogicalType::Any => UnionTypeProvenance::DynamicAny,
                _ => UnionTypeProvenance::Concrete(logical_type),
            };
            (name, provenance)
        })
        .collect()
}

fn resolve_union_columns(
    operands: &[Vec<(String, UnionTypeProvenance)>],
) -> Result<Vec<(String, LogicalType)>> {
    let Some(first) = operands.first() else {
        return Ok(Vec::new());
    };
    for columns in &operands[1..] {
        if columns.len() != first.len() {
            return Err(Error::binder(
                "The number of columns to union/union all must be the same.".to_string(),
            ));
        }
    }

    (0..first.len())
        .map(|column| {
            let sources: Vec<_> = operands.iter().map(|columns| &columns[column].1).collect();
            let logical_type = if sources
                .iter()
                .any(|source| matches!(source, UnionTypeProvenance::DynamicAny))
            {
                LogicalType::Any
            } else if let Some(anchor) = sources.iter().find_map(|source| match source {
                UnionTypeProvenance::Concrete(logical_type) => Some(logical_type),
                _ => None,
            }) {
                for (operand, source) in sources.iter().enumerate() {
                    if let UnionTypeProvenance::Concrete(logical_type) = source
                        && *logical_type != *anchor
                    {
                        let name = &operands[operand][column].0;
                        return Err(Error::binder(format!(
                            "{name} has data type {logical_type} but {anchor} was expected."
                        )));
                    }
                }
                anchor.clone()
            } else {
                LogicalType::Any
            };
            Ok((first[column].0.clone(), logical_type))
        })
        .collect()
}

fn refresh_direct_parameter_types(operands: &mut [BoundQuery], state: &PreparedParameterState) {
    for query in operands {
        let Some(projection) = query
            .parts
            .last_mut()
            .and_then(|part| part.projection.as_mut())
        else {
            continue;
        };
        for item in &mut projection.items {
            let ProjItem::Scalar {
                expr: BoundExpr::Parameter { name, ty },
                ..
            } = item
            else {
                continue;
            };
            if *ty == LogicalType::Any
                && let Some(inferred) = state
                    .types
                    .get(name)
                    .filter(|inferred| **inferred != LogicalType::Any)
            {
                *ty = inferred.clone();
            }
        }
    }
}

fn constrain_direct_union_parameters(
    operands: &mut [BoundQuery],
    result_columns: &[(String, LogicalType)],
    state: &mut PreparedParameterState,
) {
    for query in operands {
        let Some(projection) = query
            .parts
            .last_mut()
            .and_then(|part| part.projection.as_mut())
        else {
            continue;
        };
        for (item, (_, target)) in projection.items.iter_mut().zip(result_columns) {
            let ProjItem::Scalar {
                expr: BoundExpr::Parameter { name, ty },
                ..
            } = item
            else {
                continue;
            };
            if *ty != LogicalType::Any || *target == LogicalType::Any {
                continue;
            }
            let current = state.types.get(name).cloned().unwrap_or(LogicalType::Any);
            if current == LogicalType::Any || current == *target {
                state.types.insert(name.clone(), target.clone());
                *ty = target.clone();
            } else if state.error.is_none() {
                state.error = Some(Error::binder(format!(
                    "Parameter ${name} has conflicting type constraints {current} and {target}."
                )));
            }
        }
    }
}

pub fn bound_table_func(f: ast::TableFunc) -> BoundTableFunc {
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

/// Bind a standalone-`CALL` config value against the option's declared input type
/// (C++ `bindStandaloneCall`): a floating-point value never implicitly casts into
/// an integral option (a bespoke check ahead of the general gate — the numeric
/// catch-all in `hasImplicitCast` would otherwise admit it), then the assignment
/// implicit-cast gate applies. Returns the bound expr coerced to `dst`; the caller
/// constant-folds it (out-of-range values overflow there, e.g. `timeout=-1`).
pub(crate) fn bind_config_value(
    catalog: &Catalog,
    expr: &ast::Expr,
    config: &SessionConfig,
    dst: &LogicalType,
) -> Result<BoundExpr> {
    let params = HashMap::new();
    let bound = Binder::with_state(catalog, &params, config, None).bind_expr(expr)?;
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

struct Binder<'catalog, 'bind> {
    catalog: &'catalog Catalog,
    /// Provided values for query parameters (`$name`), substituted at bind time.
    params: &'catalog HashMap<String, Value>,
    prepared_parameters: Option<&'bind mut PreparedParameterState>,
    session_config: SessionConfig,
    /// The max recursive depth for variable-length patterns (session config).
    max_recursive_depth: u32,
    /// `disable_map_key_check` (session config; `true` = C++ default, no check).
    disable_map_key_check: bool,
    vars: Vec<VarInfo>,
    scope: HashMap<String, VarId>,
    anon: u32,
    /// Lambda parameters currently in scope, innermost last.
    lambda_params: Vec<LambdaBinding>,
    next_lambda_id: u32,
    lambda_names: Vec<String>,
    /// Next subquery id and subqueries captured during expression binding.
    subquery_count: usize,
    pending_subqueries: Vec<PendingSubquery>,
    /// `nextval`/`currval` calls staged during this part's expression binding,
    /// drained into the part's `sequence_calls` (ids are per-part indices).
    pending_sequence_calls: Vec<BoundSequenceCall>,
}

impl<'catalog, 'bind> Binder<'catalog, 'bind> {
    fn with_state(
        catalog: &'catalog Catalog,
        params: &'catalog HashMap<String, Value>,
        config: &SessionConfig,
        prepared_parameters: Option<&'bind mut PreparedParameterState>,
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
            lambda_params: Vec::new(),
            next_lambda_id: 0,
            lambda_names: Vec::new(),
            subquery_count: 0,
            pending_subqueries: Vec::new(),
            pending_sequence_calls: Vec::new(),
        }
    }

    // ---- DDL ----

    // ---- MERGE ----

    // ---- projection ----

    // ---- expressions ----

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
                    let label = entry.name().to_string();
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
            .and_then(|icebug| icebug.load_error())
        {
            return Err(Error::runtime(error.to_string()));
        }
        Ok(())
    }

    /// Resolve a MATCH node pattern's labels to its candidate table set: every
    /// node table when unlabeled (`()`/`(a)`), or the named tables for a labeled
    /// / multi-label (`(a:A:B)`) pattern. The matched node's actual table is
    /// recovered at runtime from its internal id.
    fn resolve_node_tables(&self, labels: &[String]) -> Result<Vec<TableId>> {
        if let Some(tables) = self.catalog.any_tables() {
            return Ok(vec![tables.nodes()]);
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
                Some(table) if !tables.contains(&table.id()) => {
                    self.ensure_table_readable(table.id())?;
                    tables.push(table.id());
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
            return Ok(tables.nodes());
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
                self.ensure_table_readable(t.id())?;
                Ok(t.id())
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
            for column in self.catalog.node_table(t).unwrap().columns() {
                match props
                    .iter_mut()
                    .find(|property| property.name.eq_ignore_ascii_case(column.name()))
                {
                    // A same-named property whose type differs across the
                    // candidate tables scans as the promoted common type
                    // (INT64+DOUBLE → DOUBLE, else STRING); the scan casts
                    // each table's raw value up (`promote_prop`).
                    Some(property) if &property.ty != column.logical_type() => {
                        property.ty = promote_property_type(&property.ty, column.logical_type());
                    }
                    Some(_) => {}
                    None => props.push(PropInfo {
                        name: column.name().to_string(),
                        column_id: column.column_id().0,
                        ty: column.logical_type().clone(),
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
            return Ok(vec![tables.edges()]);
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
                Some(table) if !v.contains(&table.id()) => v.push(table.id()),
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
            for column in self.catalog.rel_table(t).unwrap().columns() {
                match props
                    .iter_mut()
                    .find(|property| property.name.eq_ignore_ascii_case(column.name()))
                {
                    // Heterogeneous same-named property → the promoted common
                    // type (see node_props_union).
                    Some(property) if &property.ty != column.logical_type() => {
                        property.ty = promote_property_type(&property.ty, column.logical_type());
                    }
                    Some(_) => {}
                    None => props.push(PropInfo {
                        name: column.name().to_string(),
                        column_id: column.column_id().0,
                        ty: column.logical_type().clone(),
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
        let label = self
            .catalog
            .node_table(narrowed[0])
            .unwrap()
            .name()
            .to_string();
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
            .columns()
            .iter()
            .map(|column| PropInfo {
                name: column.name().to_string(),
                column_id: column.column_id().0,
                ty: column.logical_type().clone(),
            })
            .collect()
    }

    fn rel_props(&self, table: TableId) -> Vec<PropInfo> {
        self.catalog
            .rel_table(table)
            .unwrap()
            .columns()
            .iter()
            .map(|column| PropInfo {
                name: column.name().to_string(),
                column_id: column.column_id().0,
                ty: column.logical_type().clone(),
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
                function: BuiltinScalar::ListContains,
                called_name: "list_contains".to_string(),
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
    if let (BoundExpr::Call { function, ty, .. }, LogicalType::Union(dm)) = (&mut *value, dst) {
        if *function == BuiltinScalar::UnionValue {
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

/// Whether a bound expression references the lambda parameter `id` anywhere.
fn mentions_lambda(e: &BoundExpr, id: LambdaVarId) -> bool {
    match e {
        BoundExpr::LambdaVar { id: candidate, .. } => *candidate == id,
        BoundExpr::ValueProperty { value, .. } => mentions_lambda(value, id),
        BoundExpr::Cast { expr, .. } => mentions_lambda(expr, id),
        BoundExpr::Scalar { args, .. }
        | BoundExpr::Call { args, .. }
        | BoundExpr::List { elems: args, .. } => args.iter().any(|arg| mentions_lambda(arg, id)),
        BoundExpr::Struct { fields, .. } => {
            fields.iter().any(|(_, value)| mentions_lambda(value, id))
        }
        BoundExpr::ListLambda { list, body, .. } => {
            mentions_lambda(list, id) || mentions_lambda(body, id)
        }
        BoundExpr::Aggregate { arg, .. } => {
            arg.as_ref().is_some_and(|arg| mentions_lambda(arg, id))
        }
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|operand| mentions_lambda(operand, id))
                || branches.iter().any(|(condition, result)| {
                    mentions_lambda(condition, id) || mentions_lambda(result, id)
                })
                || else_
                    .as_ref()
                    .is_some_and(|else_expr| mentions_lambda(else_expr, id))
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

/// Reject duplicate field names in a STRUCT literal or `struct_pack`.
/// Field names are case-sensitive, matching STRUCT and UNION type declarations.
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
    let entry = match koko_function::resolve_builtin(name).map(|descriptor| descriptor.catalog_kind)
    {
        Some(koko_function::FunctionCatalogKind::Table) => Some("TABLE_FUNCTION_ENTRY"),
        Some(koko_function::FunctionCatalogKind::StandaloneTable) => {
            Some("STANDALONE_TABLE_FUNCTION_ENTRY")
        }
        Some(koko_function::FunctionCatalogKind::Copy) => Some("COPY_FUNCTION_ENTRY"),
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
            BoundExpr::Call {
                function,
                called_name,
                args,
                ..
            } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    match eval(a)? {
                        Ok(v) => vals.push(v),
                        err => return Some(err),
                    }
                }
                Some(koko_function::scalarfn::eval_with_context(
                    *function,
                    called_name,
                    &vals,
                    &koko_function::oracle_hash::RandomState::default(),
                ))
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
                koko_common::types::common_numeric_type([a, b])
                    .expect("a pair of numeric types has a common numeric type")
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
    match resolve_builtin(name).map(|descriptor| descriptor.function) {
        Some(BuiltinFunction::Scalar(BuiltinScalar::Nextval)) => Some(SequenceFn::NextVal),
        Some(BuiltinFunction::Scalar(BuiltinScalar::Currval)) => Some(SequenceFn::CurrVal),
        _ => None,
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
    #[test]
    fn union_type_provenance_resolves_each_column_order_independently() {
        let column = |provenance| vec![("x".to_string(), provenance)];
        let int = UnionTypeProvenance::Concrete(LogicalType::Int64);

        for operands in [
            vec![
                column(UnionTypeProvenance::UntypedNull),
                column(int.clone()),
            ],
            vec![
                column(int.clone()),
                column(UnionTypeProvenance::UntypedNull),
            ],
            vec![
                column(UnionTypeProvenance::UnknownParameter("value".to_string())),
                column(int.clone()),
            ],
        ] {
            assert_eq!(
                resolve_union_columns(&operands).unwrap(),
                vec![("x".to_string(), LogicalType::Int64)]
            );
        }

        assert_eq!(
            resolve_union_columns(&[
                column(UnionTypeProvenance::UntypedNull),
                column(UnionTypeProvenance::UnknownParameter("value".to_string())),
            ])
            .unwrap(),
            vec![("x".to_string(), LogicalType::Any)]
        );
        assert_eq!(
            resolve_union_columns(&[
                column(UnionTypeProvenance::Concrete(LogicalType::Int64)),
                column(UnionTypeProvenance::Concrete(LogicalType::String)),
                column(UnionTypeProvenance::DynamicAny),
            ])
            .unwrap(),
            vec![("x".to_string(), LogicalType::Any)]
        );

        let error = resolve_union_columns(&[
            column(UnionTypeProvenance::Concrete(LogicalType::Int64)),
            column(UnionTypeProvenance::Concrete(LogicalType::String)),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Binder exception: x has data type STRING but INT64 was expected."
        );

        let nested_any = LogicalType::List(Box::new(LogicalType::Any));
        let nested_int = LogicalType::List(Box::new(LogicalType::Int64));
        assert_eq!(
            resolve_union_columns(&[
                column(UnionTypeProvenance::Concrete(nested_any)),
                column(UnionTypeProvenance::Concrete(nested_int)),
            ])
            .unwrap_err()
            .to_string(),
            "Binder exception: x has data type INT64[] but ANY[] was expected."
        );
    }

    #[test]
    fn union_binding_publishes_canonical_columns_and_infers_direct_parameters() {
        let BoundStatement::Query(query) =
            bind("RETURN null AS first UNION ALL RETURN 1 AS second").unwrap()
        else {
            panic!("expected query")
        };
        assert_eq!(
            query.result_columns(),
            &[("first".to_string(), LogicalType::Int64)]
        );

        let BoundStatement::Query(dynamic) = bind(
            "RETURN 1 AS x UNION ALL \
             RETURN union_extract(union_value(a := 2), 'a') AS x UNION ALL \
             RETURN 'three' AS x",
        )
        .unwrap() else {
            panic!("expected query")
        };
        assert_eq!(
            dynamic.result_columns(),
            &[("x".to_string(), LogicalType::Any)]
        );

        let catalog = Catalog::new();
        for cypher in [
            "RETURN $value AS x UNION ALL RETURN 1 AS x",
            "RETURN 1 AS x UNION ALL RETURN $value AS x",
        ] {
            let statement = parse_statement(cypher).unwrap();
            let prepared = bind_statement_for_prepare(
                &catalog,
                &statement,
                &["value".to_string()],
                &HashMap::new(),
                &SessionConfig::default(),
            )
            .unwrap();
            assert_eq!(
                prepared.parameter_types.get("value"),
                Some(&LogicalType::Int64)
            );
            let BoundStatement::Query(query) = prepared.statement else {
                panic!("expected query")
            };
            assert_eq!(
                query.result_columns(),
                &[("x".to_string(), LogicalType::Int64)]
            );
        }

        let statement =
            parse_statement("RETURN $value AS a, $value AS b UNION ALL RETURN 1 AS a, 's' AS b")
                .unwrap();
        let error = match bind_statement_for_prepare(
            &catalog,
            &statement,
            &["value".to_string()],
            &HashMap::new(),
            &SessionConfig::default(),
        ) {
            Ok(_) => panic!("conflicting direct parameter constraints must fail"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Binder exception: Parameter $value has conflicting type constraints INT64 and STRING."
        );
    }
}
