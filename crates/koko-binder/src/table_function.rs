use koko_catalog::Catalog;
use koko_common::{Error, IntKind, LogicalType, Result, TableId};
use koko_function::BuiltinTableFunction;
use koko_parser::ast;

/// The canonical uppercase name used in table-function diagnostics.
pub(crate) const fn display_name(function: BuiltinTableFunction) -> &'static str {
    function.canonical_name()
}

/// The name of a source query consisting of exactly one function call.
pub(crate) fn source_name(query: &ast::RegularQuery) -> Option<String> {
    if !query.union_all.is_empty() {
        return None;
    }
    let [single] = query.singles.as_slice() else {
        return None;
    };
    if !single.parts.is_empty() || !single.updating.is_empty() {
        return None;
    }
    match single.reading.as_slice() {
        [ast::ReadingClause::Call(call)] => Some(call.name.to_ascii_uppercase()),
        _ => None,
    }
}

/// Validate a table function's arguments and return its output schema.
pub fn schema(
    catalog: &Catalog,
    function: BuiltinTableFunction,
    arguments: &[String],
) -> Result<Vec<(String, LogicalType)>> {
    let argument = arguments.first().map(String::as_str);
    let extra_arguments = arguments.get(1..).unwrap_or_default();
    let string = str::to_string;
    let schema = match function {
        BuiltinTableFunction::ShowTables => vec![
            (string("id"), LogicalType::Int64),
            (string("name"), LogicalType::String),
            (string("type"), LogicalType::String),
            (string("database name"), LogicalType::String),
            (string("comment"), LogicalType::String),
        ],
        BuiltinTableFunction::ShowSequences => vec![
            (string("name"), LogicalType::String),
            (string("database name"), LogicalType::String),
            (string("start value"), LogicalType::Int64),
            (string("increment"), LogicalType::Int64),
            (string("min value"), LogicalType::Int64),
            (string("max value"), LogicalType::Int64),
            (string("cycle"), LogicalType::Bool),
        ],
        BuiltinTableFunction::TableInfo => {
            let table = table_info_target(catalog, argument)?;
            let mut schema = vec![
                (string("property id"), LogicalType::Int64),
                (string("name"), LogicalType::String),
                (string("type"), LogicalType::String),
                (string("default expression"), LogicalType::String),
            ];
            if catalog.node_table(table).is_some() {
                schema.push((string("primary key"), LogicalType::Bool));
            } else {
                schema.push((string("storage_direction"), LogicalType::String));
            }
            schema
        }
        BuiltinTableFunction::ShowMacros => vec![
            (string("name"), LogicalType::String),
            (string("definition"), LogicalType::String),
        ],
        BuiltinTableFunction::ShowFunctions => vec![
            (string("name"), LogicalType::String),
            (string("type"), LogicalType::String),
            (string("signature"), LogicalType::String),
        ],
        BuiltinTableFunction::DbVersion => vec![(string("version"), LogicalType::String)],
        BuiltinTableFunction::ShowOfficialExtensions => vec![
            (string("name"), LogicalType::String),
            (string("description"), LogicalType::String),
        ],
        BuiltinTableFunction::ClearWarnings => Vec::new(),
        BuiltinTableFunction::CacheArrayColumn => {
            let table_name = argument.unwrap_or_default();
            let column_name = extra_arguments
                .first()
                .map(String::as_str)
                .unwrap_or_default();
            let table = catalog
                .table_id(table_name)
                .ok_or_else(|| Error::binder(format!("Table {table_name} does not exist!")))?;
            let logical_type = catalog
                .node_table(table)
                .and_then(|entry| {
                    entry
                        .column(column_name)
                        .map(|column| column.logical_type().clone())
                })
                .ok_or_else(|| {
                    Error::binder(format!(
                        "Column {column_name} does not exist in table {table_name}."
                    ))
                })?;
            if !matches!(logical_type, LogicalType::Array(_, _)) {
                return Err(Error::binder(format!(
                    "Column {column_name} is not of the expected type ARRAY."
                )));
            }
            Vec::new()
        }
        BuiltinTableFunction::ShowIndexes => vec![
            (string("table_name"), LogicalType::String),
            (string("index_name"), LogicalType::String),
            (string("index_type"), LogicalType::String),
            (
                string("property_names"),
                LogicalType::List(Box::new(LogicalType::String)),
            ),
            (string("extension_loaded"), LogicalType::Bool),
            (string("index_definition"), LogicalType::String),
        ],
        BuiltinTableFunction::ShowWarnings => vec![
            (string("query_id"), LogicalType::Int(IntKind::U64)),
            (string("message"), LogicalType::String),
            (string("file_path"), LogicalType::String),
            (string("line_number"), LogicalType::Int(IntKind::U64)),
            (string("skipped_line_or_record"), LogicalType::String),
        ],
        BuiltinTableFunction::ShowConnection => {
            show_connection_target(catalog, argument)?;
            vec![
                (string("source table name"), LogicalType::String),
                (string("destination table name"), LogicalType::String),
                (string("source table primary key"), LogicalType::String),
                (string("destination table primary key"), LogicalType::String),
            ]
        }
        BuiltinTableFunction::StorageInfo => {
            existing_table_target(catalog, argument)?;
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
            .map(|(name, logical_type)| (string(name), logical_type))
            .collect()
        }
        BuiltinTableFunction::StatsInfo => {
            let table = existing_table_target(catalog, argument)?;
            let entry = catalog.node_table(table).ok_or_else(|| {
                Error::binder(format!(
                    "Stats from a non-node table {} is not supported yet!",
                    argument.unwrap_or_default()
                ))
            })?;
            let mut schema = Vec::with_capacity(entry.columns().len() + 1);
            schema.push((string("cardinality"), LogicalType::Int64));
            schema.extend(entry.columns().iter().map(|property| {
                (
                    format!("{}_distinct_count", property.name()),
                    LogicalType::Int64,
                )
            }));
            schema
        }
        BuiltinTableFunction::CurrentSetting => vec![(
            argument.unwrap_or_default().to_string(),
            LogicalType::String,
        )],
        BuiltinTableFunction::BmInfo => vec![
            (string("mem_limit"), LogicalType::Int(IntKind::U64)),
            (string("mem_usage"), LogicalType::Int(IntKind::U64)),
        ],
        BuiltinTableFunction::ShowLoadedExtensions => vec![
            (string("extension name"), LogicalType::String),
            (string("extension source"), LogicalType::String),
            (string("extension path"), LogicalType::String),
        ],
    };
    Ok(schema)
}

fn show_connection_target(catalog: &Catalog, argument: Option<&str>) -> Result<TableId> {
    argument
        .and_then(|name| catalog.table_id(name))
        .filter(|table| catalog.rel_table(*table).is_some())
        .ok_or_else(|| {
            Error::binder("Show connection can only be called on a rel table!".to_string())
        })
}

fn existing_table_target(catalog: &Catalog, argument: Option<&str>) -> Result<TableId> {
    let name = argument.unwrap_or_default();
    catalog
        .table_id(name)
        .ok_or_else(|| Error::binder(format!("Table {name} does not exist!")))
}

fn table_info_target(catalog: &Catalog, argument: Option<&str>) -> Result<TableId> {
    let name = argument
        .ok_or_else(|| Error::binder("TABLE_INFO requires a table name argument.".to_string()))?;
    catalog
        .table_id(name)
        .ok_or_else(|| Error::catalog(format!("{name} does not exist in catalog.")))
}
