//! Engine-authoritative snapshots mapped into ordinary typed results.

use crate::bootstrap::{AutoToggle, Format, NullDisplay, RowLimit, Settings, WidthLimit};
use crate::parameter::{ParameterOrigin, ParameterStore};
use crate::worker::{WorkerError, tooling_result};
use koko::config::MemoryUsage;
use koko::tooling::{CatalogSnapshot, FunctionKind, GraphKind, SessionSnapshot, TransactionMode};
use koko::value::IntKind;
use koko::{LogicalType, QueryResult, Value};

pub fn status_result(
    session: &SessionSnapshot,
    memory: MemoryUsage,
    parameters: &ParameterStore,
    settings: &Settings,
    output: &str,
) -> Result<QueryResult, WorkerError> {
    let graph_kind = match session.graph().kind() {
        GraphKind::Typed => "typed",
        GraphKind::Any => "ANY",
        _ => "unknown",
    };
    let transaction = match session.transaction() {
        TransactionMode::None => "none",
        TransactionMode::ReadOnly => "read-only",
        TransactionMode::ReadWrite => "read-write",
        _ => "unknown",
    };
    let timeout = session.timeout().map_or_else(
        || "none".to_string(),
        |timeout| format!("{} ms", timeout.as_millis()),
    );
    let memory_limit = memory.limit.map_or_else(|| "none".to_string(), human_bytes);
    let rows = match settings.rows.value() {
        RowLimit::Rows(value) => value.to_string(),
        RowLimit::All => "all".to_string(),
    };
    let width = match settings.width.value() {
        WidthLimit::Auto => "auto".to_string(),
        WidthLimit::Columns(value) => value.to_string(),
    };
    let values = [
        ("database", "in-memory".to_string()),
        (
            "graph",
            format!("{} ({graph_kind})", session.graph().name()),
        ),
        ("transaction", transaction.to_string()),
        ("parameters", parameters.entries().len().to_string()),
        ("timeout", timeout),
        ("workers", session.workers().to_string()),
        ("memory limit", memory_limit),
        ("format", format_name(*settings.format.value()).to_string()),
        ("rows / width", format!("{rows} / {width}")),
        ("timing", on_off(*settings.timing.value()).to_string()),
        (
            "progress",
            auto_toggle(*settings.progress.value()).to_string(),
        ),
        (
            "highlight",
            auto_toggle(*settings.highlight.value()).to_string(),
        ),
        (
            "completion",
            on_off(*settings.completion.value()).to_string(),
        ),
        ("history", on_off(*settings.history.value()).to_string()),
        (
            "NULL display",
            match settings.null_display.value() {
                NullDisplay::Literal => "literal",
                NullDisplay::Empty => "empty",
            }
            .to_string(),
        ),
        ("output", output.to_string()),
    ];
    tooling_result(
        &["setting", "value"],
        vec![LogicalType::String, LogicalType::String],
        values
            .into_iter()
            .map(|(name, value)| vec![Value::String(name.to_string()), Value::String(value)])
            .collect(),
    )
}

pub fn graphs_result(catalog: &CatalogSnapshot) -> Result<QueryResult, WorkerError> {
    tooling_result(
        &["selected", "graph", "kind", "identity"],
        vec![
            LogicalType::Bool,
            LogicalType::String,
            LogicalType::String,
            LogicalType::Int(IntKind::U64),
        ],
        catalog
            .graphs()
            .iter()
            .map(|graph| {
                vec![
                    Value::Bool(graph.identity() == catalog.selected_graph()),
                    Value::String(graph.name().to_string()),
                    Value::String(
                        match graph.kind() {
                            GraphKind::Typed => "typed",
                            GraphKind::Any => "ANY",
                            _ => "unknown",
                        }
                        .to_string(),
                    ),
                    Value::make_int(graph.identity().value() as i128, IntKind::U64),
                ]
            })
            .collect(),
    )
}

pub fn functions_result(
    catalog: &CatalogSnapshot,
    pattern: Option<&str>,
) -> Result<QueryResult, WorkerError> {
    let pattern = pattern.map(str::to_ascii_lowercase);
    tooling_result(
        &["name", "kind", "signature", "return_type"],
        vec![
            LogicalType::String,
            LogicalType::String,
            LogicalType::String,
            LogicalType::String,
        ],
        catalog
            .functions()
            .iter()
            .filter(|function| {
                pattern
                    .as_ref()
                    .is_none_or(|pattern| function.name().to_ascii_lowercase().contains(pattern))
            })
            .map(|function| {
                vec![
                    Value::String(function.name().to_string()),
                    Value::String(function_kind(function.kind()).to_string()),
                    Value::String(function.signature().to_string()),
                    Value::String(function.return_type().to_string()),
                ]
            })
            .collect(),
    )
}

pub fn parameters_result(
    parameters: &ParameterStore,
    include_values: bool,
) -> Result<QueryResult, WorkerError> {
    let mut names = vec!["name", "logical_type", "source"];
    let mut types = vec![
        LogicalType::String,
        LogicalType::String,
        LogicalType::String,
    ];
    if include_values {
        names.push("value");
        types.push(LogicalType::Any);
    }
    let rows = parameters
        .entries()
        .map(|entry| {
            let mut row = vec![
                Value::String(entry.name().to_string()),
                Value::String(entry.logical_type().to_string()),
                Value::String(
                    match entry.origin() {
                        ParameterOrigin::File(_) => "file",
                        ParameterOrigin::CommandLine => "command-line",
                        ParameterOrigin::Interactive => "interactive",
                    }
                    .to_string(),
                ),
            ];
            if include_values {
                row.push(entry.value().clone());
            }
            row
        })
        .collect();
    tooling_result(&names, types, rows)
}

pub fn describe_result(
    catalog: &CatalogSnapshot,
    target: &str,
) -> Result<QueryResult, WorkerError> {
    let object = target.rsplit_once('.').map_or(target, |(_, object)| object);
    let mut rows = Vec::new();
    for table in catalog
        .node_tables()
        .iter()
        .filter(|table| table.name().eq_ignore_ascii_case(object))
    {
        for property in table.properties() {
            rows.push(vec![
                Value::String(table.name().to_string()),
                Value::String("node table".to_string()),
                Value::String(property.name().to_string()),
                Value::String(property.type_text().to_string()),
                Value::String(
                    if property.is_primary_key() {
                        "primary key"
                    } else {
                        property.default_text()
                    }
                    .to_string(),
                ),
            ]);
        }
    }
    for table in catalog
        .relationship_tables()
        .iter()
        .filter(|table| table.name().eq_ignore_ascii_case(object))
    {
        for endpoint in table.endpoints() {
            rows.push(vec![
                Value::String(table.name().to_string()),
                Value::String("relationship table".to_string()),
                Value::String("FROM/TO".to_string()),
                Value::String(format!("{} -> {}", endpoint.from(), endpoint.to())),
                Value::String(table.storage_direction().to_string()),
            ]);
        }
        for property in table.properties() {
            rows.push(vec![
                Value::String(table.name().to_string()),
                Value::String("relationship table".to_string()),
                Value::String(property.name().to_string()),
                Value::String(property.type_text().to_string()),
                Value::String(property.default_text().to_string()),
            ]);
        }
    }
    for index in catalog
        .indexes()
        .iter()
        .filter(|index| index.name().eq_ignore_ascii_case(object))
    {
        rows.push(vec![
            Value::String(index.name().to_string()),
            Value::String("index".to_string()),
            Value::String(index.table().to_string()),
            Value::String(index.index_type().to_string()),
            Value::String(index.properties().join(", ")),
        ]);
    }
    for item in catalog
        .macros()
        .iter()
        .filter(|item| item.name().eq_ignore_ascii_case(object))
    {
        rows.push(vec![
            Value::String(item.name().to_string()),
            Value::String("macro".to_string()),
            Value::String(item.signature().to_string()),
            Value::String("ANY".to_string()),
            Value::String(item.body().to_string()),
        ]);
    }
    tooling_result(
        &["object", "kind", "member", "type", "details"],
        vec![LogicalType::String; 5],
        rows,
    )
}

fn function_kind(kind: FunctionKind) -> &'static str {
    match kind {
        FunctionKind::Scalar => "scalar",
        FunctionKind::Aggregate => "aggregate",
        FunctionKind::Table => "table",
        FunctionKind::Macro => "macro",
        FunctionKind::ConnectionLocal => "connection-local",
        _ => "function",
    }
}

fn format_name(format: Format) -> &'static str {
    match format {
        Format::Auto => "auto",
        Format::Box => "box",
        Format::Table => "table",
        Format::Csv => "csv",
        Format::Tsv => "tsv",
        Format::Json => "json",
        Format::JsonLines => "jsonl",
        Format::Markdown => "markdown",
        Format::Line => "line",
        Format::Trash => "trash",
    }
}

fn auto_toggle(value: AutoToggle) -> &'static str {
    match value {
        AutoToggle::Auto => "auto",
        AutoToggle::On => "on",
        AutoToggle::Off => "off",
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn human_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}
