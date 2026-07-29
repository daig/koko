use koko_catalog::Catalog;
use koko_common::{Error, IntKind, MemoryUsage, Result, TableId, TableStats, Value};
use koko_ir::bound::BoundTableFunc;

const DATABASE_NAME: &str = "main(graph)";
const DATABASE_VERSION: &str = "0.17.0";

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

/// Runtime-owned capabilities used while producing table-function rows.
pub trait TableFunctionRuntime: Sync {
    fn current_setting(&self, key: &str) -> Value;
    fn warning_rows(&self) -> Vec<Vec<Value>>;
    fn show_table_rows(&self) -> Option<Vec<Vec<Value>>> {
        None
    }
    fn clear_warnings(&self);
    fn macro_rows(&self) -> Vec<Vec<Value>>;
    fn memory_usage(&self) -> MemoryUsage;
    fn table_stats(&self, _table: TableId) -> Option<TableStats> {
        None
    }
}

/// Produce rows for a table function previously validated and bound by the binder.
pub fn produce_table_function_rows(
    catalog: &Catalog,
    function: BoundTableFunc,
    argument: Option<&str>,
    runtime: &dyn TableFunctionRuntime,
) -> Result<Vec<Vec<Value>>> {
    let rows = match function {
        BoundTableFunc::ShowSequences => catalog
            .sequences_sorted()
            .iter()
            .map(|sequence| {
                vec![
                    Value::String(sequence.name().to_string()),
                    Value::String(DATABASE_NAME.to_string()),
                    Value::Int64(sequence.display_val()),
                    Value::Int64(sequence.increment()),
                    Value::Int64(sequence.min()),
                    Value::Int64(sequence.max()),
                    Value::Bool(sequence.cycle()),
                ]
            })
            .collect(),
        BoundTableFunc::ShowTables => {
            if let Some(rows) = runtime.show_table_rows() {
                return Ok(rows);
            }
            let mut rows = Vec::new();
            for table in catalog.node_table_ids() {
                let entry = catalog.node_table(table).expect("listed node table");
                rows.push(table_row(
                    table.0,
                    entry.name(),
                    "NODE",
                    catalog.table_comment(table),
                ));
            }
            for table in catalog.rel_table_ids() {
                let entry = catalog.rel_table(table).expect("listed relationship table");
                rows.push(table_row(
                    table.0 + entry.pairs().len() as u64,
                    entry.name(),
                    "REL",
                    catalog.table_comment(table),
                ));
            }
            rows
        }
        BoundTableFunc::TableInfo => {
            let table = table_info_target(catalog, argument)?;
            if let Some(entry) = catalog.node_table(table) {
                entry
                    .columns()
                    .iter()
                    .map(|column| {
                        vec![
                            Value::Int64(column.column_id().0 as i64),
                            Value::String(column.name().to_string()),
                            Value::String(column.type_text().to_string()),
                            Value::String(column.default_text().to_string()),
                            Value::Bool(column.column_id().0 as usize == entry.primary_key_index()),
                        ]
                    })
                    .collect()
            } else {
                let entry = catalog
                    .rel_table(table)
                    .expect("validated relationship table");
                entry
                    .columns()
                    .iter()
                    .map(|column| {
                        vec![
                            Value::Int64(column.column_id().0 as i64 + 1),
                            Value::String(column.name().to_string()),
                            Value::String(column.type_text().to_string()),
                            Value::String(column.default_text().to_string()),
                            Value::String(entry.storage_direction().as_str().to_string()),
                        ]
                    })
                    .collect()
            }
        }
        BoundTableFunc::ShowMacros => runtime.macro_rows(),
        BoundTableFunc::ShowFunctions => koko_function::catalog_data::FUNCTION_CATALOG
            .iter()
            .map(|entry| {
                vec![
                    Value::String(entry.name.to_string()),
                    Value::String(entry.kind.as_str().to_string()),
                    Value::String(entry.signature.to_string()),
                ]
            })
            .collect(),
        BoundTableFunc::DbVersion => vec![vec![Value::String(DATABASE_VERSION.to_string())]],
        BoundTableFunc::CacheArrayColumn => Vec::new(),
        BoundTableFunc::ClearWarnings => {
            runtime.clear_warnings();
            Vec::new()
        }
        BoundTableFunc::ShowOfficialExtensions => OFFICIAL_EXTENSIONS
            .iter()
            .map(|(name, description)| {
                vec![
                    Value::String((*name).to_string()),
                    Value::String((*description).to_string()),
                ]
            })
            .collect(),
        BoundTableFunc::ShowIndexes => catalog
            .indexes()
            .into_iter()
            .map(|index| {
                let table_name = catalog
                    .node_table(index.table_id())
                    .expect("index table exists")
                    .name()
                    .to_string();
                let properties = index
                    .property_names()
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect();
                let property_definition = index
                    .property_names()
                    .iter()
                    .map(|name| format!("n.`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                vec![
                    Value::String(table_name.clone()),
                    Value::String(index.name().to_string()),
                    Value::String(index.index_type().name().to_string()),
                    Value::List(properties),
                    Value::Bool(true),
                    Value::String(format!(
                        "CREATE {} INDEX `{}` FOR (n:`{}`) ON ({});",
                        index.index_type().name(),
                        index.name(),
                        table_name,
                        property_definition
                    )),
                ]
            })
            .collect(),
        BoundTableFunc::ShowWarnings => runtime.warning_rows(),
        BoundTableFunc::ShowConnection => {
            let table = show_connection_target(catalog, argument)?;
            let entry = catalog
                .rel_table(table)
                .expect("validated relationship table");
            let primary_key = |table: TableId| {
                catalog
                    .node_table(table)
                    .map(|node| node.primary_key_column().name().to_string())
                    .unwrap_or_default()
            };
            entry
                .pairs()
                .iter()
                .map(|pair| {
                    vec![
                        Value::String(
                            catalog
                                .node_table(pair.from)
                                .map(|table| table.name().to_string())
                                .unwrap_or_default(),
                        ),
                        Value::String(
                            catalog
                                .node_table(pair.to)
                                .map(|table| table.name().to_string())
                                .unwrap_or_default(),
                        ),
                        Value::String(primary_key(pair.from)),
                        Value::String(primary_key(pair.to)),
                    ]
                })
                .collect()
        }
        BoundTableFunc::StorageInfo => {
            existing_table_target(catalog, argument)?;
            Vec::new()
        }
        BoundTableFunc::StatsInfo => {
            let table = existing_table_target(catalog, argument)?;
            let entry = catalog.node_table(table).ok_or_else(|| {
                Error::binder(format!(
                    "Stats from a non-node table {} is not supported yet!",
                    argument.unwrap_or_default()
                ))
            })?;
            let stats = runtime
                .table_stats(table)
                .unwrap_or_else(|| TableStats::with_columns(entry.columns().len()));
            let mut row = Vec::with_capacity(entry.columns().len() + 1);
            row.push(Value::Int64(stats.num_tuples() as i64));
            row.extend((0..entry.columns().len()).map(|column| {
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
            let key = argument.unwrap_or_default().to_ascii_lowercase();
            vec![vec![Value::String(
                runtime.current_setting(&key).to_result_string(),
            )]]
        }
        BoundTableFunc::BmInfo => {
            let usage = runtime.memory_usage();
            let unsigned = |value: u64| Value::IntX {
                value: value as i128,
                kind: IntKind::U64,
            };
            vec![vec![
                unsigned(usage.limit.unwrap_or(0)),
                unsigned(usage.current),
            ]]
        }
        BoundTableFunc::ShowLoadedExtensions => Vec::new(),
    };
    Ok(rows)
}

fn table_row(id: u64, name: &str, kind: &str, comment: &str) -> Vec<Value> {
    vec![
        Value::Int64(id as i64),
        Value::String(name.to_string()),
        Value::String(kind.to_string()),
        Value::String(DATABASE_NAME.to_string()),
        Value::String(comment.to_string()),
    ]
}

fn table_info_target(catalog: &Catalog, argument: Option<&str>) -> Result<TableId> {
    let name = argument
        .ok_or_else(|| Error::binder("TABLE_INFO requires a table name argument.".to_string()))?;
    catalog
        .table_id(name)
        .ok_or_else(|| Error::catalog(format!("{name} does not exist in catalog.")))
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
