//! COPY TO and logical database interchange.
//!
//! The portable database format is deliberately executable and inspectable:
//! `schema.cypher`, `copy.cypher`, `index.cypher`, plus one data file per node
//! table and relationship-group member. There is no second native database
//! representation hidden behind this module.

use super::QueryResult;
use crate::macros::MacroRegistry;
use crate::{
    ColumnSchema, DataChunk, Error, InternalId, LogicalType, MemoryTracker, Result, Value,
};
use koko_binder::{BoundExportDatabase, BoundOutputOptions, BoundParquetCompression};
use koko_catalog::{Catalog, Column, ColumnDefault, NodeTable, RelTable, serial_sequence_name};
use koko_common::VECTOR_CAPACITY;
use koko_common::file_resolver::{FileFormat, FileResolverConfig, resolve_files};
use koko_parser::ast::{GraphKind, LoadOptVal, Statement};
use koko_parser::parse_statement;
use koko_processor::QueryControl;
use koko_storage::StorageReadHandle;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(0);

/// Immutable graph resources required by logical interchange encoding.
pub(crate) struct InterchangeReadContext<'a> {
    catalog: &'a Catalog,
    storage: &'a koko_storage::SharedStorage,
    macros: &'a MacroRegistry,
    memory: &'a MemoryTracker,
}

impl<'a> InterchangeReadContext<'a> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        storage: &'a koko_storage::SharedStorage,
        macros: &'a MacroRegistry,
        memory: &'a MemoryTracker,
    ) -> Self {
        Self {
            catalog,
            storage,
            macros,
            memory,
        }
    }

    fn catalog(&self) -> &Catalog {
        self.catalog
    }

    fn storage(&self) -> &koko_storage::SharedStorage {
        self.storage
    }

    fn macros(&self) -> &MacroRegistry {
        self.macros
    }

    fn memory(&self) -> &MemoryTracker {
        self.memory
    }
}

/// Statement-scoped read and cancellation capabilities for logical export.
pub(crate) struct InterchangeExportContext<'a> {
    read: StorageReadHandle,
    control: QueryControl<'a>,
}

impl<'a> InterchangeExportContext<'a> {
    pub(crate) fn new(read: StorageReadHandle, control: QueryControl<'a>) -> Self {
        Self { read, control }
    }

    fn read(&self) -> StorageReadHandle {
        self.read
    }

    fn control(&self) -> QueryControl<'a> {
        self.control
    }
}

/// Owned shallow snapshot used by a database-wide export.
pub(crate) struct InterchangeSnapshot {
    catalog: Arc<Catalog>,
    storage: Arc<koko_storage::SharedStorage>,
    macros: Arc<MacroRegistry>,
    memory: MemoryTracker,
}

impl InterchangeSnapshot {
    pub(crate) fn new(
        catalog: Arc<Catalog>,
        storage: Arc<koko_storage::SharedStorage>,
        macros: Arc<MacroRegistry>,
        memory: MemoryTracker,
    ) -> Self {
        Self {
            catalog,
            storage,
            macros,
            memory,
        }
    }

    fn context(&self) -> InterchangeReadContext<'_> {
        InterchangeReadContext::new(&self.catalog, &self.storage, &self.macros, &self.memory)
    }
}

pub(super) const DATABASE_IMAGE_HEADER: &str = "KOKO_LOGICAL_DATABASE\t1";

pub(super) struct ExportGraph {
    pub name: String,
    pub data: InterchangeSnapshot,
}

pub(super) struct GraphImage {
    pub name: String,
    pub kind: GraphKind,
    pub root: PathBuf,
    pub schema: Vec<Statement>,
    pub copy: Vec<Statement>,
    pub index: Vec<Statement>,
}

impl GraphImage {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn data_statements(&self) -> impl Iterator<Item = &Statement> {
        self.schema.iter().chain(&self.copy)
    }

    pub(crate) fn index_statements(&self) -> impl Iterator<Item = &Statement> {
        self.index.iter()
    }

    pub(crate) fn apply_error(&self, error: Error) -> Error {
        match error {
            Error::Interrupt | Error::BufferManager => error,
            other => Error::runtime(format!(
                "Import database failed while staging graph {}: {other}",
                self.name
            )),
        }
    }
}

pub(super) struct DatabaseImage {
    pub graphs: Vec<GraphImage>,
}

fn encode_name(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(name.len() * 2);
    for byte in name.bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_name(encoded: &str) -> Result<String> {
    if encoded.len() & 1 != 0 {
        return Err(Error::runtime(
            "Import database failed: malformed graph name in manifest.",
        ));
    }
    let value = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    };
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let Some(high) = value(pair[0]) else {
            return Err(Error::runtime(
                "Import database failed: malformed graph name in manifest.",
            ));
        };
        let Some(low) = value(pair[1]) else {
            return Err(Error::runtime(
                "Import database failed: malformed graph name in manifest.",
            ));
        };
        decoded.push((high << 4) | low);
    }
    String::from_utf8(decoded)
        .map_err(|_| Error::runtime("Import database failed: graph name in manifest is not UTF-8."))
}

fn diagnostic_path(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    let display = normalized.to_string_lossy().into_owned();
    display
        .find("${")
        .map_or(display.clone(), |start| display[start..].to_string())
}

pub(super) fn write_query_result(
    path: &Path,
    options: &BoundOutputOptions,
    result: &QueryResult,
    memory: &MemoryTracker,
    control: koko_processor::QueryControl<'_>,
) -> Result<u64> {
    let temp = temp_path(path);
    let write_result = match options {
        BoundOutputOptions::Csv(options) => write_csv(&temp, options, result, memory, control),
        BoundOutputOptions::Parquet { compression } => {
            write_parquet(&temp, *compression, result, memory, control)
        }
    };
    let rows = match write_result {
        Ok(rows) => rows,
        Err(error) => {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
    };
    if path.is_dir() {
        let _ = fs::remove_file(&temp);
        return Err(Error::Io(format!(
            "Cannot overwrite directory {} with query output.",
            diagnostic_path(path)
        )));
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.into());
    }
    Ok(rows)
}

fn temp_path(path: &Path) -> PathBuf {
    let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("result");
    path.with_file_name(format!(".{name}.koko-{}-{id}.tmp", std::process::id()))
}

fn create_new(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(Into::into)
}

fn write_csv(
    path: &Path,
    options: &koko_common::csv_dialect::CsvOptions,
    result: &QueryResult,
    memory: &MemoryTracker,
    control: koko_processor::QueryControl<'_>,
) -> Result<u64> {
    let delimiter = options.delimiter.unwrap_or(b',');
    let quote = options.quote.unwrap_or(b'"');
    let escape = options.escape.unwrap_or(quote);
    let null = options.null_strings.first().cloned().unwrap_or_default();
    let _writer_memory = memory.try_reserve(8 * 1024)?;
    let mut output = BufWriter::new(create_new(path)?);
    if options.header.unwrap_or(false) {
        write_csv_record(
            &mut output,
            result.column_names().iter().map(String::as_str),
            delimiter,
            quote,
            escape,
            &null,
            None,
        )?;
    }
    let mut rows = 0u64;
    for batch in result.batches() {
        control.check()?;
        for position in batch.sel.iter() {
            for _ in 0..batch.multiplicity(position) {
                // One row can own the batch's entire variable-width payload.
                // Reserve twice the full batch before cloning values: one copy
                // for the row and one for its rendered CSV fields.
                let row_bytes = batch
                    .allocated_bytes()
                    .saturating_mul(2)
                    .saturating_add((batch.columns.len() as u64).saturating_mul(64));
                let _row_memory = memory.try_reserve(row_bytes)?;
                let values: Vec<Value> = batch
                    .columns
                    .iter()
                    .map(|column| column.get_value(position))
                    .collect();
                write_csv_record(
                    &mut output,
                    values.iter().map(Value::to_csv_string),
                    delimiter,
                    quote,
                    escape,
                    &null,
                    Some((&values, result.schema())),
                )?;
                rows = rows.saturating_add(1);
            }
        }
    }
    output.flush()?;
    Ok(rows)
}

fn write_csv_record<I, S>(
    output: &mut dyn Write,
    fields: I,
    delimiter: u8,
    quote: u8,
    escape: u8,
    null: &str,
    typed: Option<(&[Value], &[ColumnSchema])>,
) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut values = typed.map(|(values, schema)| values.iter().zip(schema));
    for (index, field) in fields.into_iter().enumerate() {
        if index != 0 {
            output.write_all(&[delimiter])?;
        }
        let (text, force_quote) = if let Some(iter) = values.as_mut() {
            let (value, column) = iter.next().expect("typed CSV fields are aligned");
            let text = if value.is_null() {
                null
            } else {
                field.as_ref()
            };
            let nested = matches!(
                column.logical_type(),
                LogicalType::List(_)
                    | LogicalType::Array(_, _)
                    | LogicalType::Struct(_)
                    | LogicalType::Map(_, _)
                    | LogicalType::Union(_)
            );
            (text, nested || (!value.is_null() && text == null))
        } else {
            (field.as_ref(), false)
        };
        write_csv_field(
            output,
            text.as_bytes(),
            delimiter,
            quote,
            escape,
            force_quote,
        )?;
    }
    output.write_all(b"\n")?;
    Ok(())
}

fn write_csv_field(
    output: &mut dyn Write,
    field: &[u8],
    delimiter: u8,
    quote: u8,
    escape: u8,
    force_quote: bool,
) -> Result<()> {
    let quoted = force_quote
        || field
            .iter()
            .any(|byte| matches!(*byte, b'\n' | b'\r') || *byte == delimiter || *byte == quote);
    if !quoted {
        output.write_all(field)?;
        return Ok(());
    }
    output.write_all(&[quote])?;
    for &byte in field {
        if byte == quote || byte == escape {
            output.write_all(&[escape])?;
        }
        output.write_all(&[byte])?;
    }
    output.write_all(&[quote])?;
    Ok(())
}

fn write_parquet(
    path: &Path,
    compression: BoundParquetCompression,
    result: &QueryResult,
    memory: &MemoryTracker,
    control: koko_processor::QueryControl<'_>,
) -> Result<u64> {
    let batch_bytes = result
        .batches()
        .iter()
        .map(DataChunk::allocated_bytes)
        .max()
        .unwrap_or(0);
    let writer_bytes = 64_u64
        .saturating_mul(1024)
        .saturating_add(batch_bytes.saturating_mul(2))
        .saturating_add((result.schema().len() as u64).saturating_mul(256));
    let _writer_memory = memory.try_reserve(writer_bytes)?;
    use koko_loader::parquet::{
        ParquetCompression, ParquetField, ParquetFileWriter, ParquetSchema, ParquetWriterOptions,
    };

    let mut names = HashSet::with_capacity(result.schema().len());
    let fields = result
        .schema()
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let base = if column.name().is_empty() {
                format!("column{index}")
            } else {
                column.name().to_string()
            };
            let mut name = base.clone();
            let mut suffix = 1usize;
            while !names.insert(name.to_ascii_lowercase()) {
                name = format!("{base}_{suffix}");
                suffix += 1;
            }
            ParquetField::new(name, column.logical_type().clone(), true)
        })
        .collect();
    let schema = ParquetSchema { fields };
    let compression = match compression {
        BoundParquetCompression::Uncompressed => ParquetCompression::Uncompressed,
        BoundParquetCompression::Snappy => ParquetCompression::Snappy,
        BoundParquetCompression::Zstd => ParquetCompression::Zstd,
        BoundParquetCompression::Gzip => ParquetCompression::Gzip,
        BoundParquetCompression::Lz4Raw => ParquetCompression::Lz4Raw,
    };
    let mut writer = ParquetFileWriter::create(path, schema, ParquetWriterOptions { compression })?;
    for batch in result.batches() {
        control.check()?;
        writer.write_chunk(batch)?;
    }
    writer.finish()
}

pub(super) fn export_database_image(
    graphs: &[ExportGraph],
    export: &BoundExportDatabase,
    query: &InterchangeExportContext<'_>,
) -> Result<()> {
    let root = Path::new(&export.path);
    if root.exists() {
        return Err(Error::runtime(format!(
            "Directory {} already exists.",
            diagnostic_path(root)
        )));
    }
    fs::create_dir_all(root)?;
    let result = (|| {
        let mut ordered: Vec<&ExportGraph> = graphs.iter().collect();
        ordered.sort_by(|left, right| {
            let left_main = left.name.eq_ignore_ascii_case("main");
            let right_main = right.name.eq_ignore_ascii_case("main");
            right_main
                .cmp(&left_main)
                .then_with(|| {
                    left.name
                        .bytes()
                        .map(|byte| byte.to_ascii_lowercase())
                        .cmp(right.name.bytes().map(|byte| byte.to_ascii_lowercase()))
                })
                .then_with(|| left.name.cmp(&right.name))
        });
        let mut manifest = String::from(DATABASE_IMAGE_HEADER);
        manifest.push('\n');
        let mut named_index = 0usize;
        for graph in ordered {
            query.control().check()?;
            let is_main = graph.name.eq_ignore_ascii_case("main");
            let relative = if is_main {
                ".".to_string()
            } else {
                let path = format!("graphs/{named_index:06}");
                named_index += 1;
                path
            };
            let data = graph.data.context();
            let kind = if data.catalog().any_tables().is_some() {
                "A"
            } else {
                "T"
            };
            manifest.push_str(&format!(
                "GRAPH\t{kind}\t{}\t{relative}\n",
                encode_name(&graph.name)
            ));
            let graph_root = root.join(&relative);
            fs::create_dir_all(&graph_root)?;
            export_graph_image(&data, export, query, &graph_root)?;
        }
        fs::write(root.join("manifest.koko"), manifest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(root);
    }
    result
}

fn export_graph_image(
    database: &InterchangeReadContext<'_>,
    export: &BoundExportDatabase,
    query: &InterchangeExportContext<'_>,
    root: &Path,
) -> Result<()> {
    fs::write(root.join("schema.cypher"), schema_script(database))?;
    fs::write(root.join("index.cypher"), index_script(database))?;
    if export.schema_only {
        fs::write(root.join("copy.cypher"), "")?;
        return Ok(());
    }

    let extension = match export.options.format() {
        FileFormat::Csv => "csv",
        FileFormat::Parquet => "parquet",
        FileFormat::Npy => unreachable!("NPY is not an output format"),
    };
    let mut copy = String::new();
    let mut file_names = HashSet::new();
    let mut nodes: Vec<NodeTable> = database
        .catalog()
        .node_table_ids()
        .into_iter()
        .filter_map(|id| database.catalog().node_table(id).cloned())
        .collect();
    nodes.sort_by(|left, right| left.name.cmp(&right.name));
    for node in nodes {
        query.control().check()?;
        let file_name = format!("{}.{}", node.name, extension);
        admit_export_file(&file_name, &mut file_names)?;
        copy.push_str(&copy_statement(
            &node.name,
            &node.columns,
            &file_name,
            &export.options,
            None,
        ));
        let mut result = node_result(database, &node, query)?;
        result.track_memory(database.memory())?;
        write_query_result(
            &root.join(file_name),
            &export.options,
            &result,
            database.memory(),
            query.control(),
        )?;
    }

    let mut rels: Vec<RelTable> = database
        .catalog()
        .rel_table_ids()
        .into_iter()
        .filter_map(|id| database.catalog().rel_table(id).cloned())
        .collect();
    rels.sort_by(|left, right| left.name.cmp(&right.name));
    for rel in rels {
        query.control().check()?;
        for ((from, to), member) in rel.pairs.iter().zip(&rel.member_ids) {
            let from_table = database
                .catalog()
                .node_table(*from)
                .expect("relationship FROM table exists");
            let to_table = database
                .catalog()
                .node_table(*to)
                .expect("relationship TO table exists");
            let file_name = format!(
                "{}_{}_{}.{}",
                rel.name, from_table.name, to_table.name, extension
            );
            admit_export_file(&file_name, &mut file_names)?;
            copy.push_str(&copy_statement(
                &rel.name,
                &rel.columns,
                &file_name,
                &export.options,
                Some((&from_table.name, &to_table.name)),
            ));
            let mut result = rel_result(database, &rel, *member, from_table, to_table, query)?;
            result.track_memory(database.memory())?;
            write_query_result(
                &root.join(file_name),
                &export.options,
                &result,
                database.memory(),
                query.control(),
            )?;
        }
    }
    fs::write(root.join("copy.cypher"), copy)?;
    Ok(())
}

fn admit_export_file(file_name: &str, names: &mut HashSet<String>) -> Result<()> {
    if Path::new(file_name).components().count() != 1
        || file_name
            .bytes()
            .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}'))
        || !names.insert(file_name.to_ascii_lowercase())
    {
        return Err(Error::runtime(format!(
            "Cannot export database because data file name {file_name} is invalid or collides."
        )));
    }
    Ok(())
}

fn index_script(database: &InterchangeReadContext<'_>) -> String {
    let mut indexes = database.catalog().indexes();
    indexes.sort_by(|left, right| {
        let left_table = database.catalog().table_name(left.table_id).unwrap_or("");
        let right_table = database.catalog().table_name(right.table_id).unwrap_or("");
        left_table
            .to_ascii_lowercase()
            .cmp(&right_table.to_ascii_lowercase())
            .then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            })
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut output = String::new();
    for index in indexes {
        let table = database
            .catalog()
            .table_name(index.table_id)
            .expect("indexed table exists");
        let properties = index
            .property_names
            .iter()
            .map(|property| format!("n.{}", quote_ident(property)))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "CREATE {} INDEX {} FOR (n:{}) ON ({});\n",
            index.index_type.name(),
            quote_ident(&index.name),
            quote_ident(table),
            properties
        ));
    }
    output
}

pub(super) fn schema_script(database: &InterchangeReadContext<'_>) -> String {
    let mut output = String::new();
    for (name, ty) in database.catalog().user_types_sorted() {
        output.push_str(&format!("CREATE TYPE {} AS {};\n", quote_ident(name), ty));
    }

    let mut nodes: Vec<&NodeTable> = database
        .catalog()
        .node_table_ids()
        .into_iter()
        .filter_map(|id| database.catalog().node_table(id))
        .collect();
    nodes.sort_by(|left, right| left.name.cmp(&right.name));
    let mut implicit_sequences = HashSet::new();
    for node in &nodes {
        for column in &node.columns {
            if column.type_text.eq_ignore_ascii_case("SERIAL") {
                implicit_sequences.insert(serial_sequence_name(&node.name, &column.name));
            }
        }
        if database.catalog().is_any_node_table(node.id) {
            continue;
        }
        output.push_str(&format!(
            "CREATE NODE TABLE {} ({}, PRIMARY KEY({}));\n",
            quote_ident(&node.name),
            column_definitions(&node.columns),
            quote_ident(&node.columns[node.primary_key].name)
        ));
    }

    let mut rels: Vec<&RelTable> = database
        .catalog()
        .rel_table_ids()
        .into_iter()
        .filter_map(|id| database.catalog().rel_table(id))
        .collect();
    rels.sort_by(|left, right| left.name.cmp(&right.name));
    for rel in &rels {
        for column in &rel.columns {
            if column.type_text.eq_ignore_ascii_case("SERIAL") {
                implicit_sequences.insert(serial_sequence_name(&rel.name, &column.name));
            }
        }
        if database.catalog().is_any_rel_table(rel.id) {
            continue;
        }
        let mut definitions: Vec<String> = rel
            .pairs
            .iter()
            .map(|(from, to)| {
                let from = &database
                    .catalog()
                    .node_table(*from)
                    .expect("FROM table exists")
                    .name;
                let to = &database
                    .catalog()
                    .node_table(*to)
                    .expect("TO table exists")
                    .name;
                format!("FROM {} TO {}", quote_ident(from), quote_ident(to))
            })
            .collect();
        if !rel.columns.is_empty() {
            definitions.push(column_definitions(&rel.columns));
        }
        let multiplicity = rel
            .member_ids
            .first()
            .map(|member| database.storage().rel_multiplicity(*member))
            .unwrap_or_default();
        let src = if multiplicity.dst_single {
            "ONE"
        } else {
            "MANY"
        };
        let dst = if multiplicity.src_single {
            "ONE"
        } else {
            "MANY"
        };
        definitions.push(format!("{src}_{dst}"));
        output.push_str(&format!(
            "CREATE REL TABLE {} ({}) WITH (storage_direction='{}');\n",
            quote_ident(&rel.name),
            definitions.join(", "),
            rel.storage_direction.as_str()
        ));
    }

    let state: HashMap<String, (i64, u64)> = database
        .catalog()
        .sequence_state()
        .into_iter()
        .map(|(name, current, usage)| (name.to_ascii_lowercase(), (current, usage)))
        .collect();
    for sequence in database.catalog().sequences_sorted() {
        if implicit_sequences.contains(&sequence.name) {
            continue;
        }
        let (current, usage) = state
            .get(&sequence.name.to_ascii_lowercase())
            .copied()
            .unwrap_or((sequence.start, 0));
        let start = if usage == 0 { sequence.start } else { current };
        output.push_str(&format!(
            "CREATE SEQUENCE {} START {} INCREMENT {} MINVALUE {} MAXVALUE {} {};\n",
            quote_ident(&sequence.name),
            start,
            sequence.increment,
            sequence.min,
            sequence.max,
            if sequence.cycle { "CYCLE" } else { "NO CYCLE" }
        ));
    }

    let mut macros: Vec<_> = database.macros().iter().collect();
    macros.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, definition) in macros {
        let mut parameters: Vec<String> = definition
            .positional
            .iter()
            .map(|parameter| quote_ident(parameter))
            .collect();
        parameters.extend(definition.defaults.iter().map(|(parameter, value)| {
            format!(
                "{} := {}",
                quote_ident(parameter),
                koko_parser::expr_to_cypher(value)
            )
        }));
        output.push_str(&format!(
            "CREATE MACRO {}({}) AS {};\n",
            quote_ident(name),
            parameters.join(", "),
            koko_parser::expr_to_cypher(&definition.body)
        ));
    }

    let mut sequence_restores: Vec<_> = database
        .catalog()
        .sequences_sorted()
        .into_iter()
        .filter_map(|sequence| {
            let (_, usage) = state
                .get(&sequence.name.to_ascii_lowercase())
                .copied()
                .unwrap_or((sequence.start, 0));
            if usage == 0 {
                return None;
            }
            // Explicit sequences were recreated at their current value above,
            // so one call restores their used state. Implicit SERIAL sequences
            // are recreated by CREATE TABLE at their original start and must
            // be advanced once per historical call.
            let calls = if implicit_sequences.contains(&sequence.name) {
                usage
            } else {
                1
            };
            Some((sequence.name.clone(), calls))
        })
        .collect();
    sequence_restores.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for (name, calls) in sequence_restores {
        for _ in 0..calls {
            output.push_str(&format!("RETURN nextval({});\n", quote_string(&name)));
        }
    }

    for table in nodes
        .into_iter()
        .filter(|table| !database.catalog().is_any_node_table(table.id))
        .filter_map(|table| table.comment.as_ref().map(|comment| (&table.name, comment)))
        .chain(
            rels.into_iter()
                .filter(|table| !database.catalog().is_any_rel_table(table.id))
                .filter_map(|table| table.comment.as_ref().map(|comment| (&table.name, comment))),
        )
    {
        output.push_str(&format!(
            "COMMENT ON TABLE {} IS {};\n",
            quote_ident(table.0),
            quote_string(table.1)
        ));
    }
    output
}

fn column_definitions(columns: &[Column]) -> String {
    columns
        .iter()
        .map(|column| {
            let mut definition = format!("{} {}", quote_ident(&column.name), column.type_text);
            if !column.type_text.eq_ignore_ascii_case("SERIAL")
                && !matches!(column.default, ColumnDefault::None)
            {
                definition.push_str(" DEFAULT ");
                definition.push_str(&column.default_text);
            }
            definition
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn copy_statement(
    table: &str,
    columns: &[Column],
    file_name: &str,
    options: &BoundOutputOptions,
    endpoints: Option<(&str, &str)>,
) -> String {
    let columns = columns
        .iter()
        .map(|column| quote_ident(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut bound_options = match options {
        BoundOutputOptions::Csv(options) => csv_options_cypher(options),
        BoundOutputOptions::Parquet { .. } => Vec::new(),
    };
    if let Some((from, to)) = endpoints {
        bound_options.push(format!("FROM={}", quote_string(from)));
        bound_options.push(format!("TO={}", quote_string(to)));
    }
    let options = if bound_options.is_empty() {
        String::new()
    } else {
        format!(" ({})", bound_options.join(", "))
    };
    let columns = if columns.is_empty() {
        String::new()
    } else {
        format!(" ({columns})")
    };
    format!(
        "COPY {}{} FROM {}{};\n",
        quote_ident(table),
        columns,
        quote_string(file_name),
        options
    )
}

fn csv_options_cypher(options: &koko_common::csv_dialect::CsvOptions) -> Vec<String> {
    let delimiter = options.delimiter.unwrap_or(b',') as char;
    let quote = options.quote.unwrap_or(b'"') as char;
    let escape = options.escape.unwrap_or(quote as u8) as char;
    let nulls = options
        .null_strings
        .iter()
        .map(|value| quote_string(value))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        format!("HEADER={}", options.header.unwrap_or(false)),
        format!("DELIM={}", quote_string(&delimiter.to_string())),
        format!("QUOTE={}", quote_string(&quote.to_string())),
        format!("ESCAPE={}", quote_string(&escape.to_string())),
        "PARALLEL=false".to_string(),
        "AUTO_DETECT=false".to_string(),
        format!("NULL_STRINGS=[{nulls}]"),
        format!("LIST_UNBRACED={}", options.list_unbraced),
    ]
}

fn quote_ident(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn node_result(
    database: &InterchangeReadContext<'_>,
    table: &NodeTable,
    query: &InterchangeExportContext<'_>,
) -> Result<QueryResult> {
    let projected: Vec<usize> = (0..table.columns.len()).collect();
    let mut batches = Vec::new();
    let mut offset = 0u64;
    let count = database.storage().node_count(table.id);
    while offset < count {
        query.control().check()?;
        let mut batch = database.storage().scan_node_batch(
            query.read(),
            table.id,
            &projected,
            offset,
            VECTOR_CAPACITY,
        );
        offset = offset.saturating_add(VECTOR_CAPACITY as u64);
        if batch.is_empty() {
            continue;
        }
        batch.columns.remove(0);
        batches.push(batch);
    }
    Ok(result_from_columns(&table.columns, batches))
}

fn rel_result(
    database: &InterchangeReadContext<'_>,
    rel: &RelTable,
    member: koko_common::TableId,
    from: &NodeTable,
    to: &NodeTable,
    query: &InterchangeExportContext<'_>,
) -> Result<QueryResult> {
    let from_pk = from.primary_key_column();
    let to_pk = to.primary_key_column();
    let mut used: HashSet<String> = rel
        .columns
        .iter()
        .map(|column| column.name.to_ascii_lowercase())
        .collect();
    let mut endpoint_name = |base: &str| {
        let mut name = base.to_string();
        while !used.insert(name.to_ascii_lowercase()) {
            name.push('_');
        }
        name
    };
    let mut names = vec![endpoint_name("__from"), endpoint_name("__to")];
    names.extend(rel.columns.iter().map(|column| column.name.clone()));
    let mut types = vec![from_pk.ty.clone(), to_pk.ty.clone()];
    types.extend(rel.columns.iter().map(|column| column.ty.clone()));
    let projected: Vec<usize> = (0..rel.columns.len()).collect();
    let mut batches = Vec::new();
    let mut offset = 0u64;
    let count = database.storage().rel_count(member);
    while offset < count {
        query.control().check()?;
        let source = database.storage().scan_rel_batch(
            query.read(),
            member,
            &projected,
            offset,
            VECTOR_CAPACITY,
        );
        offset = offset.saturating_add(VECTOR_CAPACITY as u64);
        if source.is_empty() {
            continue;
        }
        let endpoint_ids: Vec<(InternalId, InternalId)> = source
            .sel
            .iter()
            .map(|position| {
                (
                    expect_internal_id(source.columns[1].get_value(position)),
                    expect_internal_id(source.columns[2].get_value(position)),
                )
            })
            .collect();
        let src_offsets: Vec<u64> = endpoint_ids.iter().map(|(src, _)| src.offset.0).collect();
        let dst_offsets: Vec<u64> = endpoint_ids.iter().map(|(_, dst)| dst.offset.0).collect();
        let src_properties = database.storage().node_properties_batch(
            query.read(),
            from.id,
            &src_offsets,
            &[from.primary_key],
        );
        let dst_properties = database.storage().node_properties_batch(
            query.read(),
            to.id,
            &dst_offsets,
            &[to.primary_key],
        );
        let mut batch = DataChunk::new(&types);
        for (row, position) in source.sel.iter().enumerate() {
            batch.columns[0].set_value_owned(row, src_properties[0].columns[0].get_value(row));
            batch.columns[1].set_value_owned(row, dst_properties[0].columns[0].get_value(row));
            for (target, source) in batch.columns[2..].iter_mut().zip(&source.columns[3..]) {
                target.set_value_owned(row, source.get_value(position));
            }
        }
        batch.set_flat(source.size());
        batches.push(batch);
    }
    let schema = names
        .iter()
        .cloned()
        .zip(types)
        .map(|(name, logical_type)| ColumnSchema::new(name, logical_type))
        .collect();
    Ok(QueryResult::from_batches(names, schema, batches))
}

fn expect_internal_id(value: Value) -> InternalId {
    match value {
        Value::InternalId(id) => id,
        other => panic!("storage relationship endpoint is not INTERNAL_ID: {other:?}"),
    }
}

fn result_from_columns(columns: &[Column], batches: Vec<DataChunk>) -> QueryResult {
    let names: Vec<_> = columns.iter().map(|column| column.name.clone()).collect();
    let schema = columns
        .iter()
        .map(|column| ColumnSchema::new(column.name.clone(), column.ty.clone()))
        .collect();
    QueryResult::from_batches(names, schema, batches)
}

pub(super) fn preflight_database_image(
    root: &Path,
    session: &koko_binder::SessionConfig,
) -> Result<DatabaseImage> {
    if !root.is_dir() {
        return Err(Error::binder(format!(
            "Directory {} does not exist.",
            diagnostic_path(root)
        )));
    }
    let manifest_path = root.join("manifest.koko");
    if !manifest_path.is_file()
        && root.join("schema.cypher").is_file()
        && root.join("copy.cypher").is_file()
    {
        return Ok(DatabaseImage {
            graphs: vec![preflight_graph_image(
                "main".to_string(),
                GraphKind::Typed,
                root.to_path_buf(),
                session,
            )?],
        });
    }
    if !manifest_path.is_file() {
        return Err(Error::binder(format!(
            "File {} does not exist.",
            diagnostic_path(&manifest_path)
        )));
    }
    let manifest = fs::read_to_string(&manifest_path)?;
    let mut lines = manifest.lines();
    if lines.next() != Some(DATABASE_IMAGE_HEADER) {
        return Err(Error::runtime(
            "Import database failed: unsupported or malformed logical database image.",
        ));
    }
    let mut names = HashSet::new();
    let mut paths = HashSet::new();
    let mut graphs = Vec::new();
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 4 || fields[0] != "GRAPH" {
            return Err(Error::runtime(
                "Import database failed: malformed graph manifest entry.",
            ));
        }
        let kind = match fields[1] {
            "T" => GraphKind::Typed,
            "A" => GraphKind::Any,
            _ => {
                return Err(Error::runtime(
                    "Import database failed: unknown graph kind in manifest.",
                ));
            }
        };
        let name = decode_name(fields[2])?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(Error::runtime(format!(
                "Import database failed: duplicate graph name {name}."
            )));
        }
        let relative = fields[3];
        if relative != "." {
            let mut components = Path::new(relative).components();
            let valid = matches!(
                (components.next(), components.next(), components.next()),
                (
                    Some(std::path::Component::Normal(prefix)),
                    Some(std::path::Component::Normal(directory)),
                    None
                ) if prefix == "graphs"
                    && directory.len() == 6
                    && directory.as_encoded_bytes().iter().all(u8::is_ascii_digit)
            );
            if !valid {
                return Err(Error::runtime(
                    "Import database failed: invalid graph image path.",
                ));
            }
        }
        if !paths.insert(relative.to_string()) {
            return Err(Error::runtime(format!(
                "Import database failed: duplicate graph image path {relative}."
            )));
        }
        let graph_root = root.join(relative);
        graphs.push(preflight_graph_image(name, kind, graph_root, session)?);
    }
    let main = graphs
        .iter()
        .filter(|graph| graph.name.eq_ignore_ascii_case("main"))
        .collect::<Vec<_>>();
    if main.len() != 1 || main[0].kind != GraphKind::Typed || main[0].root != root {
        return Err(Error::runtime(
            "Import database failed: the manifest must contain typed graph main at path '.'.",
        ));
    }
    if graphs.is_empty() {
        return Err(Error::runtime(
            "Import database failed: the graph manifest is empty.",
        ));
    }
    Ok(DatabaseImage { graphs })
}

fn preflight_graph_image(
    name: String,
    kind: GraphKind,
    root: PathBuf,
    session: &koko_binder::SessionConfig,
) -> Result<GraphImage> {
    if !root.is_dir() {
        return Err(Error::binder(format!(
            "Directory {} does not exist.",
            diagnostic_path(&root)
        )));
    }
    let schema_path = root.join("schema.cypher");
    let copy_path = root.join("copy.cypher");
    let index_path = root.join("index.cypher");
    for path in [&schema_path, &copy_path, &index_path] {
        if !path.is_file() {
            return Err(Error::binder(format!(
                "File {} does not exist.",
                diagnostic_path(path)
            )));
        }
    }
    let schema = read_and_parse_script(&schema_path)?;
    let copy = read_and_parse_script(&copy_path)?;
    let index = read_and_parse_script(&index_path)?;
    if copy
        .iter()
        .any(|statement| !matches!(statement, Statement::Copy(_)))
        || index
            .iter()
            .any(|statement| !matches!(statement, Statement::CreateIndex(_)))
    {
        return Err(Error::runtime(
            "Import database failed: copy/index scripts contain an invalid statement.",
        ));
    }
    preflight_copy_sources(&root, &copy, session)?;
    Ok(GraphImage {
        name,
        kind,
        root,
        schema,
        copy,
        index,
    })
}

/// Split portable image scripts into one non-comment statement per line.
pub(crate) fn split_statements(script: &str) -> Vec<String> {
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//") && !line.starts_with('#'))
        .map(|line| line.trim_end_matches(';').trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn read_and_parse_script(path: &Path) -> Result<Vec<Statement>> {
    let script = fs::read_to_string(path)?;
    split_statements(&script)
        .into_iter()
        .map(|statement| parse_statement(&statement))
        .collect()
}

fn preflight_copy_sources(
    root: &Path,
    statements: &[Statement],
    session: &koko_binder::SessionConfig,
) -> Result<()> {
    let config = FileResolverConfig {
        base_dir: root.to_path_buf(),
        home_directory: session.home_directory.clone(),
        file_search_path: session.file_search_path.clone(),
    };
    let canonical_root = fs::canonicalize(root)?;
    for statement in statements {
        let Statement::Copy(copy) = statement else {
            continue;
        };
        if copy.source_query.is_some() {
            continue;
        }
        let files: Vec<String> = std::iter::once(copy.file_path.clone())
            .chain(copy.extra_files.iter().cloned())
            .collect();
        if files.iter().any(|file| {
            Path::new(file).components().count() != 1
                || file
                    .bytes()
                    .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b']' | b'{' | b'}'))
        }) {
            return Err(Error::runtime(
                "Import database failed: a COPY source path is not portable.",
            ));
        }
        let explicit = copy.options.iter().find_map(|(name, value)| {
            if name.eq_ignore_ascii_case("file_format") {
                match value {
                    LoadOptVal::Str(value) => Some(value.as_str()),
                    _ => None,
                }
            } else {
                None
            }
        });
        let resolved = resolve_files(&files, &config, explicit)?;
        if resolved
            .iter()
            .any(|file| !file.path.starts_with(&canonical_root))
        {
            return Err(Error::runtime(
                "Import database failed: a COPY source escapes its graph image.",
            ));
        }
    }
    Ok(())
}
