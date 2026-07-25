//! `koko-loader` — bulk data loading from CSV (`COPY … FROM`).
//!
//! P0 had no way to get data in except `CREATE`. This crate adds CSV bulk load:
//! a node-table CSV is `col0,col1,…` in schema order; a relationship-table CSV
//! is `fromPK,toPK,prop0,…` (endpoints resolved by primary key). This is the
//! first P1 item because it brings the real `-DATASET CSV` corpus mechanism
//! online. Dialect auto-detection, parallel reads, and Parquet/NPY are later P1.

pub mod icebug;
pub mod npy;
pub mod parquet;

pub use npy::{
    NpyBatchReader, NpyColumnMetadata, NpyDType, NpyMetadata, inspect_npy, preflight_npy,
};
pub use parquet::{
    ParquetCompression, ParquetField, ParquetFileMetadata, ParquetFileWriter, ParquetReader,
    ParquetSchema, ParquetWriterOptions, inspect as inspect_parquet,
};

use koko_catalog::{Catalog, ColumnDefault, serial_sequence_name};
use koko_common::{
    DataChunk, Error, LogicalType, MemoryReservation, MemoryTracker, Result, TableId,
    VECTOR_CAPACITY, Value,
    csv_dialect::{CsvOptions, Dialect},
};
use koko_storage::{InMemStorage, StorageReadHandle, StorageWriteHandle};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread::JoinHandle;
#[cfg(test)]
static PARALLEL_WORKERS_STARTED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Parse one CSV cell into a [`Value`] of the given logical type using the default
/// CSV null-string configuration.
pub fn parse_cell(s: &str, ty: &LogicalType) -> Result<Value> {
    parse_cell_with_options(s, ty, &CsvOptions::default())
}

/// COPY reports storage constraint violations (duplicate / NULL primary key,
/// rel multiplicity) as `Copy exception`s with the same text the operator path
/// reports as `Runtime exception`s — mirroring C++'s bulk-copy `CopyException`.
fn copy_class(e: Error) -> Error {
    match e {
        Error::Runtime(m) => Error::Copy(m),
        other => other,
    }
}

fn warning_message(error: Error) -> String {
    match error {
        Error::Runtime(message) => message,
        other => other.to_string(),
    }
}

fn parse_cell_with_options(s: &str, ty: &LogicalType, options: &CsvOptions) -> Result<Value> {
    let normalized =
        koko_common::csv_dialect::normalize_unbraced_list(s, ty, options.list_unbraced);
    koko_function::parse_csv_cell(&normalized, ty, &options.null_strings)
}

/// The C++ ragged-row messages: a record with *more* fields than the table
/// (beyond one allowed trailing empty field — a trailing delimiter) is
/// "expected K values per row, but got more."; one with fewer reports the
/// exact count. `None` = the row is well-formed.
/// The CSV arity a COPY into `table` expects (endpoint keys + properties for a
/// rel; the input columns for a node), resolving an explicit column list with
/// the same unknown/duplicate binder errors as the copy itself.
pub fn copy_expected_arity(
    table: TableId,
    is_node: bool,
    columns: Option<&[String]>,
    catalog: &Catalog,
) -> Result<usize> {
    if is_node {
        let entry = catalog
            .node_table(table)
            .ok_or_else(|| Error::catalog("COPY into unknown node table".to_string()))?;
        match columns {
            Some(cols) => {
                resolve_listed_columns(cols, &entry.columns, &entry.name)?;
                Ok(cols.len())
            }
            None => Ok(entry
                .columns
                .iter()
                .filter(|c| {
                    !matches!(
                        &c.default,
                        ColumnDefault::NextVal(seq)
                            if *seq == serial_sequence_name(&entry.name, &c.name)
                    )
                })
                .count()),
        }
    } else {
        let rel = catalog
            .rel_table(table)
            .ok_or_else(|| Error::catalog("COPY into unknown rel table".to_string()))?;
        match columns {
            Some(cols) => {
                resolve_listed_columns(cols, &rel.columns, &rel.name)?;
                Ok(2 + cols.len())
            }
            None => Ok(2 + rel.columns.len()),
        }
    }
}

fn bom_only(path: &Path) -> bool {
    std::fs::read(path).is_ok_and(|bytes| bytes.as_slice() == [0xEF, 0xBB, 0xBF])
}

/// Validate every source file's first-record arity against `expected` BEFORE
/// any row is copied (the C++ bind-time sniff): a wider file is the Binder
/// "Number of columns mismatch."; a narrower one reports the row-width Copy
/// exception on its line 1 — so in a multi-file COPY the second file's bad
/// header errors before the first file inserts anything.
pub fn validate_copy_file_arity(expected: usize, path: &Path, options: &CsvOptions) -> Result<()> {
    if bom_only(path) {
        return Ok(());
    }
    if !options.ignore_errors {
        koko_common::csv_dialect::validate_file_structure(path, options)?;
    }
    // A zero-input COPY (serial-only table) counts physical rows whatever
    // their shape — no arity to enforce.
    if expected == 0 {
        return Ok(());
    }
    let dialect = detected_dialect(path, expected, options)?;
    let mut reader = koko_common::csv_dialect::open_reader(path, &dialect)?;
    // IGNORE_ERRORS makes row shape/quote/cast faults skippable. Resolving the
    // dialect and opening the reader above still preflights path/compression
    // failures before any source mutates storage.
    if options.ignore_errors {
        return Ok(());
    }
    let mut record = csv::StringRecord::new();
    let mut preflight_options = options.clone();
    preflight_options.parallel = false;
    if !koko_common::csv_dialect::read_prevalidated_record(
        &mut reader,
        &mut record,
        path,
        &preflight_options,
    )? {
        return Ok(());
    }
    loop {
        let mut actual = record.len();
        if actual == expected + 1 && record.get(actual - 1) == Some("") {
            actual = expected;
        }
        if actual > expected {
            return Err(Error::binder(format!(
                "Number of columns mismatch. Expected {expected} but got {actual}."
            )));
        }
        if actual < expected {
            let line = record
                .position()
                .map_or(1, |position| position.line() as usize);
            let inner = format!("expected {expected} values per row, but got {actual}.");
            return Err(wrap_row_error(path, line, &inner, None, options));
        }
        if !koko_common::csv_dialect::read_prevalidated_record(
            &mut reader,
            &mut record,
            path,
            &preflight_options,
        )? {
            return Ok(());
        }
    }
}

/// Resolve an explicit COPY column list against `columns`, reporting unknown
/// then duplicate names with the C++ binder wording.
fn resolve_listed_columns(
    cols: &[String],
    columns: &[koko_catalog::Column],
    table_name: &str,
) -> Result<Vec<usize>> {
    let resolved: Vec<usize> = cols
        .iter()
        .map(|name| {
            columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    Error::binder(format!(
                        "Table {table_name} does not contain column {name}."
                    ))
                })
        })
        .collect::<Result<_>>()?;
    let mut seen = std::collections::HashSet::new();
    for (name, idx) in cols.iter().zip(&resolved) {
        if !seen.insert(*idx) {
            return Err(Error::binder(format!(
                "Detect duplicate column name {name} during COPY."
            )));
        }
    }
    Ok(resolved)
}

fn ragged_row_error(record: &csv::StringRecord, expected: usize) -> Option<String> {
    let mut len = record.len();
    if len == expected + 1 && record.get(len - 1) == Some("") {
        len = expected;
    }
    match len.cmp(&expected) {
        std::cmp::Ordering::Greater => {
            Some(format!("expected {expected} values per row, but got more."))
        }
        std::cmp::Ordering::Less => Some(format!(
            "expected {expected} values per row, but got {len}."
        )),
        std::cmp::Ordering::Equal => None,
    }
}

/// COPY-side shim over the shared C++-style row-error wrapper: resolve the
/// dialect the same way the reader did, then delegate to
/// [`koko_common::csv_dialect::wrap_row_error`].
fn wrap_row_error(
    path: &Path,
    line_no: usize,
    inner: &str,
    upto_field: Option<usize>,
    options: &CsvOptions,
) -> Error {
    let dialect = koko_common::csv_dialect::resolve_dialect(
        path,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        None,
    )
    .unwrap_or_default();
    koko_common::csv_dialect::wrap_row_error(path, line_no, inner, upto_field, &dialect)
}

/// Resolve the dialect once for serial or parallel readers.
fn detected_dialect(path: &Path, arity: usize, options: &CsvOptions) -> Result<Dialect> {
    koko_common::csv_dialect::resolve_dialect(
        path,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        Some(arity),
    )
}

/// Mutable execution resources owned by the calling query context.
pub struct CopyContext<'a> {
    pub catalog: &'a Catalog,
    pub storage: &'a mut InMemStorage,
    pub read: StorageReadHandle,
    pub write: StorageWriteHandle,
    pub warnings: &'a koko_common::warnings::WarningSink,
    /// Maximum parsing workers for this statement.
    pub worker_count: usize,
    /// Database-owned tracker bounding in-flight parsed batches.
    pub memory: MemoryTracker,
    /// Cooperative statement cancellation/deadline check.
    pub check: &'a dyn Fn() -> Result<()>,
}

/// Bulk-load `path` (already resolved) into `table`. Returns the row count.
pub fn copy_from_csv(
    table: TableId,
    is_node: bool,
    columns: Option<&[String]>,
    path: &Path,
    options: &CsvOptions,
    context: CopyContext<'_>,
) -> Result<u64> {
    if bom_only(path) {
        return Ok(0);
    }
    if is_node {
        copy_node(
            table,
            columns,
            path,
            options,
            context.catalog,
            context.read,
            context.write,
            context.storage,
            context.warnings,
            context.worker_count,
            context.memory,
            context.check,
        )
    } else {
        copy_rel(
            table,
            columns,
            path,
            options,
            context.catalog,
            context.read,
            context.write,
            context.storage,
            context.warnings,
            context.worker_count,
            context.memory,
            context.check,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_node(
    table: TableId,
    columns: Option<&[String]>,
    path: &Path,
    options: &CsvOptions,
    catalog: &Catalog,
    _read: StorageReadHandle,
    write: StorageWriteHandle,
    storage: &mut InMemStorage,
    warnings: &koko_common::warnings::WarningSink,
    worker_count: usize,
    memory: MemoryTracker,
    check: &dyn Fn() -> Result<()>,
) -> Result<u64> {
    let entry = catalog
        .node_table(table)
        .ok_or_else(|| Error::catalog("COPY into unknown node table".to_string()))?;
    let table_name = entry.name.clone();
    let col_types: Vec<LogicalType> = entry.columns.iter().map(|c| c.ty.clone()).collect();
    // A partial column list restricts (and orders) the CSV input columns;
    // unlisted columns take their defaults.
    let input_cols: Vec<usize> = match columns {
        Some(cols) => {
            // Unknown columns report first, then the file-arity check (both
            // C++ binder errors, unlike the full form's row-level reporting).
            let resolved: Vec<usize> = cols
                .iter()
                .map(|name| {
                    entry
                        .columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            Error::binder(format!(
                                "Table {table_name} does not contain column {name}."
                            ))
                        })
                })
                .collect::<Result<_>>()?;
            let mut seen = std::collections::HashSet::new();
            for (name, idx) in cols.iter().zip(&resolved) {
                if !seen.insert(*idx) {
                    return Err(Error::binder(format!(
                        "Detect duplicate column name {name} during COPY."
                    )));
                }
            }
            resolved
        }
        None => entry
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                !matches!(
                    &c.default,
                    ColumnDefault::NextVal(seq) if *seq == serial_sequence_name(&table_name, &c.name)
                )
            })
            .map(|(i, _)| i)
            .collect(),
    };
    // "Number of columns mismatch."); a narrower one reports row-level. A
    // zero-input (serial-only) COPY counts rows whatever their shape.
    if let Some(actual) = koko_common::csv_dialect::sniffed_arity(
        &path.to_string_lossy(),
        options,
        Some(input_cols.len()),
    )
    .filter(|_| !input_cols.is_empty())
    {
        if actual > input_cols.len() + 1 {
            return Err(Error::binder(format!(
                "Number of columns mismatch. Expected {} but got {actual}.",
                input_cols.len()
            )));
        }
    }
    let default_cols: Vec<usize> = (0..col_types.len())
        .filter(|i| !input_cols.contains(i))
        .collect();
    let input_types: Vec<LogicalType> = input_cols.iter().map(|&i| col_types[i].clone()).collect();
    let input_names: Vec<String> = input_cols
        .iter()
        .map(|&i| entry.columns[i].name.clone())
        .collect();

    if input_types.is_empty() {
        return copy_node_zero_input(
            table,
            path,
            options,
            &col_types,
            &default_cols,
            catalog,
            write,
            storage,
            check,
        );
    }

    let diagnostic_dialect = detected_dialect(path, input_types.len(), options)?;
    let mut records = read_records(
        path,
        input_types.len(),
        options,
        warnings,
        worker_count,
        memory,
    )?;
    let mut inserter = NodeRecordInserter {
        table,
        path,
        col_types: &col_types,
        input_cols: &input_cols,
        default_cols: &default_cols,
        options,
        diagnostic_dialect,
        catalog,
        storage,
        write,
        warnings,
        check,
    };
    let mut count = 0u64;
    let mut batch = Vec::with_capacity(VECTOR_CAPACITY);
    let mut line = 1usize;
    // C++ order (audit W5): the header decision consumes row 1 first
    // (`header=` overrides the auto-detect; `header=false` keeps it as data),
    // then `skip=N` skips N data rows.
    let mut skipped = 0usize;
    if let Some(record) = records.next() {
        let record = record?;
        let is_header = match options.header {
            Some(h) => h,
            // Auto-detect the header only when AUTO_DETECT is on; with it off
            // (and no explicit HEADER), row 1 is DATA (C++ then cast-errors on
            // a header-shaped first row).
            None if options.auto_detect => {
                koko_common::csv_dialect::looks_like_header(&record, &input_names, &input_types)
            }
            None => false,
        };
        // An explicit `header` still validates the header row's field count
        // (the C++ reader errors on the header line itself — oracle-verified
        // "expected 3 values per row" on line 1).
        if is_header {
            if let Some(inner) = ragged_row_error(&record, input_cols.len()) {
                return Err(wrap_row_error(path, 1, &inner, None, options));
            }
        }
        if !is_header {
            if options.skip > 0 {
                skipped = 1;
            } else {
                match inserter.prepare(&record, line) {
                    Ok(prepared) => {
                        batch.push(prepared);
                        if batch.len() == VECTOR_CAPACITY {
                            count += inserter.flush(&mut batch)?;
                        }
                    }
                    // Parser-class errors are never skippable (C++ raises them
                    // through IGNORE_ERRORS).
                    Err(e)
                        if options.ignore_errors
                            && matches!(
                                e,
                                Error::Runtime(_) | Error::Copy(_) | Error::Conversion(_)
                            ) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        line += 1;
    }
    for record in records {
        let record = record?;
        line += 1;
        if skipped < options.skip {
            skipped += 1;
            continue;
        }
        match inserter.prepare(&record, line - 1) {
            Ok(prepared) => {
                batch.push(prepared);
                if batch.len() == VECTOR_CAPACITY {
                    count += inserter.flush(&mut batch)?;
                }
            }
            Err(e)
                if options.ignore_errors
                    && matches!(e, Error::Runtime(_) | Error::Copy(_) | Error::Conversion(_)) => {}
            Err(e) => return Err(e),
        }
    }
    count += inserter.flush(&mut batch)?;
    Ok(count)
}

fn read_records(
    path: &Path,
    arity: usize,
    options: &CsvOptions,
    warnings: &koko_common::warnings::WarningSink,
    worker_count: usize,
    memory: MemoryTracker,
) -> Result<RecordStream> {
    let dialect = detected_dialect(path, arity, options)?;
    // Unterminated/invalid records are isolated later under IGNORE_ERRORS.
    if !options.ignore_errors {
        koko_common::csv_dialect::validate_file_structure(path, options)?;
    }
    let compressed = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("gz") || ext.eq_ignore_ascii_case("gzip"));
    let parallel = options.parallel
        && worker_count > 1
        && !compressed
        && std::fs::metadata(path).is_ok_and(|metadata| metadata.len() >= 64 * 1024)
        && !koko_common::csv_dialect::has_quoted_newline(path, &dialect)?;
    if parallel {
        let ranges = split_record_ranges(path, worker_count)?;
        if ranges.len() > 1 {
            return Ok(RecordStream::Parallel(ParallelRecords::spawn(
                path, ranges, dialect, arity, memory,
            )?));
        }
    }
    let mut record_options = options.clone();
    if compressed {
        record_options.parallel = false;
    }
    Ok(RecordStream::Serial(Box::new(CheckedRecords {
        inner: koko_common::csv_dialect::open_reader(path, &dialect)?.into_records(),
        path: path.to_path_buf(),
        options: record_options,
        warnings: warnings.clone(),
        blank_lines: physical_blank_lines(path),
        arity,
        consumed: 0,
        pending: std::collections::VecDeque::new(),
    })))
}

enum RecordStream {
    Serial(Box<CheckedRecords>),
    Parallel(ParallelRecords),
}

impl Iterator for RecordStream {
    type Item = Result<csv::StringRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Serial(records) => records.next(),
            Self::Parallel(records) => records.next(),
        }
    }
}

struct TrackedRecordBatch {
    records: std::collections::VecDeque<csv::StringRecord>,
    _reservation: MemoryReservation,
}

enum WorkerMessage {
    Batch(TrackedRecordBatch),
    Error(Error),
    Done,
}

struct ParallelRecords {
    receivers: Vec<Receiver<WorkerMessage>>,
    handles: Vec<Option<JoinHandle<()>>>,
    worker: usize,
    batch: Option<TrackedRecordBatch>,
}

impl ParallelRecords {
    fn spawn(
        path: &Path,
        ranges: Vec<(u64, u64)>,
        dialect: Dialect,
        arity: usize,
        memory: MemoryTracker,
    ) -> Result<Self> {
        let mut receivers = Vec::with_capacity(ranges.len());
        let mut handles = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            let (sender, receiver) = sync_channel(1);
            let path = path.to_path_buf();
            let memory = memory.clone();
            let handle = std::thread::Builder::new()
                .spawn(move || {
                    parse_record_range(&path, start, end, dialect, arity, &memory, &sender)
                })
                .map_err(|error| Error::Io(error.to_string()))?;
            receivers.push(receiver);
            handles.push(Some(handle));
        }
        Ok(Self {
            receivers,
            handles,
            worker: 0,
            batch: None,
        })
    }
}

impl Iterator for ParallelRecords {
    type Item = Result<csv::StringRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(batch) = &mut self.batch {
                if let Some(record) = batch.records.pop_front() {
                    return Some(Ok(record));
                }
                self.batch = None;
            }
            let receiver = self.receivers.get(self.worker)?;
            match receiver.recv() {
                Ok(WorkerMessage::Batch(batch)) => self.batch = Some(batch),
                Ok(WorkerMessage::Error(error)) => return Some(Err(error)),
                Ok(WorkerMessage::Done) => {
                    let worker_panicked = self.handles[self.worker]
                        .take()
                        .is_some_and(|handle| handle.join().is_err());
                    if worker_panicked {
                        return Some(Err(Error::runtime("CSV parsing worker panicked.")));
                    }
                    self.worker += 1;
                }
                Err(_) => return Some(Err(Error::runtime("CSV parsing worker stopped."))),
            }
        }
    }
}

impl Drop for ParallelRecords {
    fn drop(&mut self) {
        self.receivers.clear();
        for handle in &mut self.handles {
            if let Some(handle) = handle.take() {
                let _ = handle.join();
            }
        }
    }
}

fn parse_record_range(
    path: &Path,
    start: u64,
    end: u64,
    dialect: Dialect,
    arity: usize,
    memory: &MemoryTracker,
    sender: &SyncSender<WorkerMessage>,
) {
    #[cfg(test)]
    PARALLEL_WORKERS_STARTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let result = (|| -> Result<()> {
        let mut file = std::fs::File::open(path)
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        file.seek(SeekFrom::Start(start))
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        let mut input = BufReader::new(file.take(end.saturating_sub(start)));
        let mut line = Vec::new();
        let mut records = Vec::with_capacity(VECTOR_CAPACITY);
        let mut reservation = memory.try_reserve(0)?;
        let mut line_reservation = memory.try_reserve(0)?;
        let mut first_line = start == 0;
        loop {
            line.clear();
            let bytes = read_tracked_line(&mut input, &mut line, &mut line_reservation)?;
            if bytes == 0 {
                break;
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            let leading_file_line = first_line;
            first_line = false;
            if leading_file_line && line.starts_with(&[0xEF, 0xBB, 0xBF]) {
                line.drain(..3);
            }
            if line.is_empty() && (arity != 1 || leading_file_line) {
                continue;
            }
            // Parsing duplicates the raw line into StringRecord storage. Keep this reservation at
            // the reused Vec's high-water capacity instead of issuing two atomics for every row.
            line_reservation.resize((line.capacity() as u64).saturating_mul(2))?;
            let record = if line.is_empty() {
                csv::StringRecord::from(vec![""])
            } else {
                let mut reader = koko_common::csv_dialect::reader_from(line.as_slice(), &dialect);
                match reader.records().next() {
                    Some(Ok(record)) => record,
                    Some(Err(error)) => {
                        return Err(match error.kind() {
                            csv::ErrorKind::Utf8 { .. } => {
                                Error::copy("Invalid UTF8-encoded string.")
                            }
                            _ => Error::Io(error.to_string()),
                        });
                    }
                    None => csv::StringRecord::from(vec![""]),
                }
            };
            let bytes = record.as_byte_record().as_slice().len() as u64
                + std::mem::size_of::<csv::StringRecord>() as u64;
            reservation.resize(reservation.bytes().saturating_add(bytes))?;
            records.push(record);
            if records.len() == VECTOR_CAPACITY {
                let batch = TrackedRecordBatch {
                    records: std::collections::VecDeque::from(records),
                    _reservation: reservation,
                };
                if sender.send(WorkerMessage::Batch(batch)).is_err() {
                    return Ok(());
                }
                records = Vec::with_capacity(VECTOR_CAPACITY);
                reservation = memory.try_reserve(0)?;
            }
        }
        if !records.is_empty()
            && sender
                .send(WorkerMessage::Batch(TrackedRecordBatch {
                    records: std::collections::VecDeque::from(records),
                    _reservation: reservation,
                }))
                .is_err()
        {
            return Ok(());
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = sender.send(WorkerMessage::Error(error));
    }
    let _ = sender.send(WorkerMessage::Done);
}
fn read_tracked_line<R: BufRead>(
    reader: &mut R,
    line: &mut Vec<u8>,
    reservation: &mut MemoryReservation,
) -> Result<usize> {
    let mut total = 0usize;
    loop {
        let (take, done) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(total);
            }
            match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index + 1, true),
                None => (available.len(), false),
            }
        };
        let required = (line.len() + take) as u64;
        if required > reservation.bytes() {
            reservation.resize(required)?;
        }
        {
            let available = reader.fill_buf()?;
            line.extend_from_slice(&available[..take]);
        }
        reader.consume(take);
        total += take;
        if done {
            return Ok(total);
        }
    }
}

fn split_record_ranges(path: &Path, worker_count: usize) -> Result<Vec<(u64, u64)>> {
    let length = std::fs::metadata(path)
        .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?
        .len();
    let mut first = std::fs::File::open(path)
        .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
    let mut bom = [0u8; 3];
    let bom_len = if first.read(&mut bom).unwrap_or(0) == 3 && bom == [0xEF, 0xBB, 0xBF] {
        3
    } else {
        0
    };
    let workers = worker_count
        .max(1)
        .min(length.saturating_sub(bom_len).div_ceil(64 * 1024) as usize)
        .max(1);
    let mut boundaries = vec![bom_len];
    for worker in 1..workers {
        let target = bom_len + (length - bom_len) * worker as u64 / workers as u64;
        let mut file = std::fs::File::open(path)
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        file.seek(SeekFrom::Start(target.saturating_sub(1)))
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
        let mut reader = BufReader::new(file);
        let mut skipped = Vec::new();
        let previous = reader
            .fill_buf()
            .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?
            .first()
            .copied();
        let boundary = if previous == Some(b'\n') {
            target
        } else {
            reader
                .read_until(b'\n', &mut skipped)
                .map_err(|error| Error::Io(format!("{}: {error}", path.display())))?;
            target.saturating_sub(1) + skipped.len() as u64
        };
        if boundary > *boundaries.last().expect("initial boundary") && boundary < length {
            boundaries.push(boundary);
        }
    }
    boundaries.push(length);
    Ok(boundaries
        .windows(2)
        .filter_map(|pair| (pair[0] < pair[1]).then_some((pair[0], pair[1])))
        .collect())
}

/// Per physical line of `path`, whether it is a blank line (empty after the
/// line terminator / a leading BOM). Used to re-inject the interior blank
/// records the `csv` crate drops.
fn physical_blank_lines(path: &Path) -> Vec<bool> {
    koko_common::csv_dialect::blank_physical_lines(path).unwrap_or_default()
}

struct CheckedRecords {
    inner: csv::StringRecordsIntoIter<std::fs::File>,
    path: PathBuf,
    options: CsvOptions,
    warnings: koko_common::warnings::WarningSink,
    /// Per physical line, whether it is blank (the `csv` crate skips these
    /// without counting them; C++ reads each as a single-empty-field record —
    /// the `alice\n\nbob` null-PK case). Indexed by the count of physical
    /// lines consumed so far.
    blank_lines: Vec<bool>,
    arity: usize,
    /// How many physical lines (blank or record) have been consumed.
    consumed: usize,
    /// Blank records queued for the interior lines before the next record,
    /// plus the real record that follows them (emitted in order).
    pending: std::collections::VecDeque<csv::StringRecord>,
}

impl Iterator for CheckedRecords {
    type Item = Result<csv::StringRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        // Drain any queued blank / held records first.
        if let Some(rec) = self.pending.pop_front() {
            return Some(Ok(rec));
        }
        loop {
            // Queue the blank physical lines standing before the next record.
            while self
                .blank_lines
                .get(self.consumed)
                .copied()
                .unwrap_or(false)
            {
                if self.arity == 1 && self.consumed > 0 {
                    self.pending.push_back(csv::StringRecord::from(vec![""]));
                }
                self.consumed += 1;
            }
            // No more records: any queued trailing blanks are NOT records (a
            // trailing newline never yields one), so drop them via `?`.
            let record = self.inner.next()?;
            let record = match record.map_err(|e| match e.kind() {
                // Invalid UTF-8 is a Copy exception with the file/line/record
                // context (the record text renders lossily).
                csv::ErrorKind::Utf8 { pos, .. } => {
                    let line = pos.as_ref().map(|p| p.line() as usize).unwrap_or(1);
                    wrap_row_error(
                        &self.path,
                        line,
                        "Invalid UTF8-encoded string.",
                        None,
                        &self.options,
                    )
                }
                _ => Error::Io(e.to_string()),
            }) {
                Ok(r) => r,
                Err(e) => return Some(Err(e)),
            };
            // The parallel-reader quoted-newline rule: under IGNORE_ERRORS the
            // row records a warning and is skipped; otherwise it is fatal.
            if let Some((inner, line, fragment)) =
                koko_common::csv_dialect::quoted_newline_parts(&self.path, &record, &self.options)
            {
                if self.options.ignore_errors {
                    self.warnings.push(
                        inner,
                        self.path.to_string_lossy().into_owned(),
                        line,
                        fragment,
                    );
                    continue;
                }
                return Some(Err(koko_common::csv_dialect::validate_record(
                    &self.path,
                    &record,
                    &self.options,
                )
                .expect_err("violation detected above")));
            }
            // Interior blank lines the `csv` crate skipped between the last
            // record and this one are single-empty-field records (C++ reads
            // them so — a 1-column table then hits the NULL-PK constraint).
            // Interior blank physical lines the `csv` crate silently dropped
            // (it doesn't even count them in positions) are single-empty-field
            // records in C++ — a 1-column table then hits the NULL-PK
            // constraint (`alice\n\nbob`). Emit the blanks queued for the
            // lines before this record, then the record.
            self.consumed += 1; // this record consumed one non-blank line
            if !self.pending.is_empty() {
                self.pending.push_back(record);
                return Some(Ok(self.pending.pop_front().expect("queued above")));
            }
            return Some(Ok(record));
        }
    }
}

struct PreparedNode {
    values: Vec<Value>,
    line: usize,
    text: String,
}

struct NodeRecordInserter<'a, 's> {
    table: TableId,
    path: &'a Path,
    col_types: &'a [LogicalType],
    input_cols: &'a [usize],
    default_cols: &'a [usize],
    options: &'a CsvOptions,
    diagnostic_dialect: Dialect,
    catalog: &'a Catalog,
    storage: &'s mut InMemStorage,
    write: StorageWriteHandle,
    warnings: &'a koko_common::warnings::WarningSink,
    check: &'a dyn Fn() -> Result<()>,
}

impl NodeRecordInserter<'_, '_> {
    fn prepare(&self, record: &csv::StringRecord, line: usize) -> Result<PreparedNode> {
        let warn = |inner: &str| {
            if inner.starts_with("Parser exception: ") {
                return;
            }
            self.warnings.push(
                inner.to_string(),
                self.path.to_string_lossy().into_owned(),
                line as u64,
                record_text(record, self.options),
            );
        };
        if self.options.ignore_errors {
            if let Some((inner, line, fragment)) =
                koko_common::csv_dialect::invalid_record_parts_with_dialect(
                    self.path,
                    record,
                    self.options,
                    self.diagnostic_dialect,
                )
            {
                self.warnings.push(
                    inner.clone(),
                    self.path.to_string_lossy().into_owned(),
                    line,
                    fragment,
                );
                return Err(Error::copy(inner));
            }
        }
        if let Some(inner) = ragged_row_error(record, self.input_cols.len()) {
            if self.options.ignore_errors {
                warn(&inner);
            }
            return Err(wrap_row_error(self.path, line, &inner, None, self.options));
        }
        let mut values = vec![Value::Null; self.col_types.len()];
        let mut physical_line = None;
        for (i, &column) in self.input_cols.iter().enumerate() {
            let raw = record.get(i).unwrap_or("");
            let restored;
            let raw = if raw.contains('\'') {
                let line = physical_line.get_or_insert_with(|| {
                    koko_common::csv_dialect::physical_record_text(self.path, record)
                });
                if line.contains("\\'") {
                    restored = raw.replace('\'', "\\'");
                    restored.as_str()
                } else {
                    raw
                }
            } else {
                raw
            };
            values[column] = parse_cell_with_options(raw, &self.col_types[column], self.options)
                .map_err(|error| {
                    if self.options.ignore_errors {
                        let delimiter = self.options.delimiter.unwrap_or(b',') as char;
                        let mut fragment = record
                            .iter()
                            .take(i + 1)
                            .collect::<Vec<_>>()
                            .join(&delimiter.to_string());
                        if i + 1 < record.len() {
                            fragment.push_str("...");
                        }
                        self.warnings.push(
                            error.to_string(),
                            self.path.to_string_lossy().into_owned(),
                            line as u64,
                            fragment,
                        );
                    }
                    wrap_row_error(self.path, line, &error.to_string(), Some(i), self.options)
                })?;
        }
        apply_column_defaults(self.table, self.default_cols, &mut values, self.catalog)?;
        if let Some(table) = self.catalog.node_table(self.table) {
            if values
                .get(table.primary_key)
                .is_some_and(|value| value.is_null())
            {
                let message =
                    "Found NULL, which violates the non-null constraint of the primary key column.";
                if self.options.ignore_errors {
                    warn(message);
                }
                return Err(Error::copy(message.to_string()));
            }
        }
        Ok(PreparedNode {
            values,
            line,
            text: record_text(record, self.options),
        })
    }

    fn flush(&mut self, batch: &mut Vec<PreparedNode>) -> Result<u64> {
        if batch.is_empty() {
            return Ok(0);
        }
        (self.check)()?;
        let mut rows = DataChunk::new(self.col_types);
        for (position, prepared) in batch.iter_mut().enumerate() {
            for (column, value) in prepared.values.drain(..).enumerate() {
                rows.columns[column].set_value_owned(position, value);
            }
        }
        rows.set_flat(batch.len());
        let results = self.storage.insert_node_batch(
            self.write,
            self.table,
            &rows,
            self.options.ignore_errors,
        );
        let mut inserted = 0;
        for (prepared, result) in batch.iter().zip(results) {
            match result {
                Ok(_) => inserted += 1,
                Err(error)
                    if self.options.ignore_errors
                        && matches!(
                            error,
                            Error::Runtime(_) | Error::Copy(_) | Error::Conversion(_)
                        ) =>
                {
                    self.warnings.push(
                        warning_message(error),
                        self.path.to_string_lossy().into_owned(),
                        prepared.line as u64,
                        prepared.text.clone(),
                    )
                }
                Err(error) => return Err(copy_class(error)),
            }
        }
        batch.clear();
        Ok(inserted)
    }
}

/// A record's raw text (fields rejoined with the effective delimiter) for
/// warning reports.
fn record_text(record: &csv::StringRecord, options: &CsvOptions) -> String {
    let delimiter = options.delimiter.unwrap_or(b',') as char;
    record
        .iter()
        .collect::<Vec<_>>()
        .join(&delimiter.to_string())
}

struct PreparedRel {
    src: koko_common::InternalId,
    dst: koko_common::InternalId,
    props: Vec<Value>,
    line: usize,
    text: String,
}

struct RelRecordInserter<'a, 's> {
    table: TableId,
    path: &'a Path,
    from_table: TableId,
    to_table: TableId,
    from_pk_ty: &'a LogicalType,
    to_pk_ty: &'a LogicalType,
    all_prop_types: &'a [LogicalType],
    prop_types: &'a [LogicalType],
    prop_cols: &'a [usize],
    default_cols: &'a [usize],
    num_props: usize,
    options: &'a CsvOptions,
    diagnostic_dialect: Dialect,
    catalog: &'a Catalog,
    storage: &'s mut InMemStorage,
    read: StorageReadHandle,
    write: StorageWriteHandle,
    warnings: &'a koko_common::warnings::WarningSink,
    check: &'a dyn Fn() -> Result<()>,
}

impl RelRecordInserter<'_, '_> {
    fn warn(&self, record: &csv::StringRecord, line: usize, inner: &str) {
        if inner.starts_with("Parser exception: ") {
            return;
        }
        self.warnings.push(
            inner.to_string(),
            self.path.to_string_lossy().into_owned(),
            line as u64,
            record_text(record, self.options),
        );
    }

    fn prepare(&self, record: &csv::StringRecord, line: usize) -> Result<PreparedRel> {
        if self.options.ignore_errors {
            if let Some((inner, warning_line, fragment)) =
                koko_common::csv_dialect::invalid_record_parts_with_dialect(
                    self.path,
                    record,
                    self.options,
                    self.diagnostic_dialect,
                )
            {
                self.warnings.push(
                    inner.clone(),
                    self.path.to_string_lossy().into_owned(),
                    warning_line,
                    fragment,
                );
                return Err(Error::copy(inner));
            }
        }
        let expected = 2 + self.prop_types.len();
        if let Some(inner) = ragged_row_error(record, expected) {
            if self.options.ignore_errors {
                self.warn(record, line, &inner);
            }
            return Err(wrap_row_error(self.path, line, &inner, None, self.options));
        }
        let (path, options) = (self.path, self.options);
        let wrap = |i: usize, error: Error| {
            wrap_row_error(path, line, &error.to_string(), Some(i), options)
        };
        let from_pk =
            parse_cell_with_options(record.get(0).unwrap_or(""), self.from_pk_ty, self.options)
                .map_err(|error| {
                    if self.options.ignore_errors {
                        self.warn_through(record, line, &error.to_string(), 0);
                    }
                    wrap(0, error)
                })?;
        let to_pk =
            parse_cell_with_options(record.get(1).unwrap_or(""), self.to_pk_ty, self.options)
                .map_err(|error| {
                    if self.options.ignore_errors {
                        self.warn_through(record, line, &error.to_string(), 1);
                    }
                    wrap(1, error)
                })?;
        if from_pk.is_null() || to_pk.is_null() {
            let message =
                "Found NULL, which violates the non-null constraint of the primary key column.";
            if self.options.ignore_errors {
                self.warn(record, line, message);
            }
            return Err(Error::copy(message.to_string()));
        }
        let src = self
            .storage
            .find_node_by_pk(self.read, self.from_table, &from_pk)
            .ok_or_else(|| {
                let message = format!(
                    "Unable to find primary key value {}.",
                    from_pk.to_result_string()
                );
                if self.options.ignore_errors {
                    self.warn(record, line, &message);
                }
                Error::copy(message)
            })?;
        let dst = self
            .storage
            .find_node_by_pk(self.read, self.to_table, &to_pk)
            .ok_or_else(|| {
                let message = format!(
                    "Unable to find primary key value {}.",
                    to_pk.to_result_string()
                );
                if self.options.ignore_errors {
                    self.warn(record, line, &message);
                }
                Error::copy(message)
            })?;
        let mut props = vec![Value::Null; self.num_props];
        apply_column_defaults(self.table, self.default_cols, &mut props, self.catalog)?;
        for (i, (ty, &column)) in self.prop_types.iter().zip(self.prop_cols).enumerate() {
            props[column] =
                parse_cell_with_options(record.get(2 + i).unwrap_or(""), ty, self.options)
                    .map_err(|error| {
                        if self.options.ignore_errors {
                            self.warn_through(record, line, &error.to_string(), 2 + i);
                        }
                        wrap(2 + i, error)
                    })?;
        }
        Ok(PreparedRel {
            src,
            dst,
            props,
            line,
            text: record_text(record, self.options),
        })
    }

    fn flush(&mut self, batch: &mut Vec<PreparedRel>) -> Result<u64> {
        if batch.is_empty() {
            return Ok(0);
        }
        (self.check)()?;
        let mut types = Vec::with_capacity(self.all_prop_types.len() + 2);
        types.extend([LogicalType::InternalId, LogicalType::InternalId]);
        types.extend(self.all_prop_types.iter().cloned());
        let mut rows = DataChunk::new(&types);
        for (position, prepared) in batch.iter_mut().enumerate() {
            rows.columns[0].set_internal_id(position, prepared.src);
            rows.columns[1].set_internal_id(position, prepared.dst);
            for (column, value) in prepared.props.drain(..).enumerate() {
                rows.columns[column + 2].set_value_owned(position, value);
            }
        }
        rows.set_flat(batch.len());
        let results = self.storage.insert_rel_batch(
            self.write,
            self.table,
            &rows,
            self.options.ignore_errors,
        );
        let mut inserted = 0;
        for (prepared, result) in batch.iter().zip(results) {
            match result {
                Ok(_) => inserted += 1,
                Err(error)
                    if self.options.ignore_errors
                        && matches!(
                            error,
                            Error::Runtime(_) | Error::Copy(_) | Error::Conversion(_)
                        ) =>
                {
                    self.warnings.push(
                        warning_message(error),
                        self.path.to_string_lossy().into_owned(),
                        prepared.line as u64,
                        prepared.text.clone(),
                    )
                }
                Err(error) => return Err(copy_class(error)),
            }
        }
        batch.clear();
        Ok(inserted)
    }

    fn warn_through(&self, record: &csv::StringRecord, line: usize, inner: &str, field: usize) {
        if inner.starts_with("Parser exception: ") {
            return;
        }
        let delimiter = self.options.delimiter.unwrap_or(b',') as char;
        let mut fragment = record
            .iter()
            .take(field + 1)
            .collect::<Vec<_>>()
            .join(&delimiter.to_string());
        if field + 1 < record.len() {
            fragment.push_str("...");
        }
        self.warnings.push(
            inner.to_string(),
            self.path.to_string_lossy().into_owned(),
            line as u64,
            fragment,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_node_zero_input(
    table: TableId,
    path: &Path,
    options: &CsvOptions,
    col_types: &[LogicalType],
    default_cols: &[usize],
    catalog: &Catalog,
    write: StorageWriteHandle,
    storage: &mut InMemStorage,
    check: &dyn Fn() -> Result<()>,
) -> Result<u64> {
    let file =
        std::fs::File::open(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = Vec::new();
    let mut physical_line = 0u64;
    let mut count = 0u64;
    let skip_header = options.header == Some(true);
    let mut batch = DataChunk::new(col_types);
    let mut batch_len = 0usize;

    loop {
        buf.clear();
        let read = std::io::BufRead::read_until(&mut reader, b'\n', &mut buf)
            .map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        physical_line += 1;
        if skip_header && physical_line == 1 {
            continue;
        }

        let mut values = vec![Value::Null; col_types.len()];
        apply_column_defaults(table, default_cols, &mut values, catalog)?;
        for (column, value) in values.into_iter().enumerate() {
            batch.columns[column].set_value_owned(batch_len, value);
        }
        batch_len += 1;
        if batch_len == VECTOR_CAPACITY {
            check()?;
            batch.set_flat(batch_len);
            for result in storage.insert_node_batch(write, table, &batch, false) {
                result?;
                count += 1;
            }
            batch = DataChunk::new(col_types);
            batch_len = 0;
        }
    }

    if batch_len != 0 {
        check()?;
        batch.set_flat(batch_len);
        for result in storage.insert_node_batch(write, table, &batch, false) {
            result?;
            count += 1;
        }
    }
    Ok(count)
}

pub fn apply_column_defaults(
    table: TableId,
    cols: &[usize],
    values: &mut [Value],
    catalog: &Catalog,
) -> Result<()> {
    for &col in cols {
        values[col] = match catalog.column_default(table, col) {
            ColumnDefault::Const(value) => value,
            ColumnDefault::NextVal(seq) => Value::Int64(catalog.sequence_next_val(&seq)?),
            ColumnDefault::None => continue,
        };
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_rel(
    table: TableId,
    columns: Option<&[String]>,
    path: &Path,
    options: &CsvOptions,
    catalog: &Catalog,
    read: StorageReadHandle,
    write: StorageWriteHandle,
    storage: &mut InMemStorage,
    warnings: &koko_common::warnings::WarningSink,
    worker_count: usize,
    memory: MemoryTracker,
    check: &dyn Fn() -> Result<()>,
) -> Result<u64> {
    let rel = catalog
        .rel_table(table)
        .ok_or_else(|| Error::catalog("COPY into unknown rel table".to_string()))?;
    let (_, from_table, to_table) = catalog
        .rel_members(table)
        .into_iter()
        .find(|(member, _, _)| *member == table)
        .ok_or_else(|| Error::catalog("COPY relationship pair is missing".to_string()))?;
    let num_props = rel.columns.len();
    let all_prop_types: Vec<_> = rel.columns.iter().map(|column| column.ty.clone()).collect();
    // A partial column list (`COPY r(a, b) FROM …`) restricts and orders the
    // CSV property columns; unlisted properties stay NULL.
    let prop_cols: Vec<usize> = match columns {
        Some(cols) => {
            let rel_name = rel.name.clone();
            let resolved: Vec<usize> = cols
                .iter()
                .map(|name| {
                    rel.columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            Error::binder(format!(
                                "Table {rel_name} does not contain column {name}."
                            ))
                        })
                })
                .collect::<Result<_>>()?;
            let mut seen = std::collections::HashSet::new();
            for (name, idx) in cols.iter().zip(&resolved) {
                if !seen.insert(*idx) {
                    return Err(Error::binder(format!(
                        "Detect duplicate column name {name} during COPY."
                    )));
                }
            }
            resolved
        }
        None => (0..num_props).collect(),
    };
    let prop_types: Vec<LogicalType> = prop_cols
        .iter()
        .map(|&i| rel.columns[i].ty.clone())
        .collect();
    // Like the node path: a WIDER file errors at bind, a narrower one
    // reports row-level.
    if let Some(actual) = koko_common::csv_dialect::sniffed_arity(
        &path.to_string_lossy(),
        options,
        Some(2 + prop_cols.len()),
    ) {
        if actual > 2 + prop_cols.len() {
            return Err(Error::binder(format!(
                "Number of columns mismatch. Expected {} but got {actual}.",
                2 + prop_cols.len()
            )));
        }
    }
    let from_pk_ty = catalog
        .node_table(from_table)
        .unwrap()
        .primary_key_column()
        .ty
        .clone();
    let to_pk_ty = catalog
        .node_table(to_table)
        .unwrap()
        .primary_key_column()
        .ty
        .clone();

    let expected = 2 + prop_types.len();
    let diagnostic_dialect = detected_dialect(path, expected, options)?;
    let mut records = read_records(path, expected, options, warnings, worker_count, memory)?;
    // A relationship header (when present) is row 1 read as the FROM/TO endpoints
    // plus the property columns; detected schema-anchored (names match, or a cell
    // fails to parse as its declared type — e.g. an LDBC `:START_ID(..)` label).
    let mut hdr_names = vec!["from".to_string(), "to".to_string()];
    hdr_names.extend(prop_cols.iter().map(|&i| rel.columns[i].name.clone()));
    let mut hdr_types = vec![from_pk_ty.clone(), to_pk_ty.clone()];
    hdr_types.extend(prop_types.iter().cloned());
    let mut count = 0u64;
    let default_cols: Vec<usize> = (0..num_props).filter(|i| !prop_cols.contains(i)).collect();
    let mut inserter = RelRecordInserter {
        table,
        path,
        from_table,
        to_table,
        from_pk_ty: &from_pk_ty,
        to_pk_ty: &to_pk_ty,
        all_prop_types: &all_prop_types,
        prop_types: &prop_types,
        prop_cols: &prop_cols,
        default_cols: &default_cols,
        num_props,
        options,
        diagnostic_dialect,
        catalog,
        read,
        write,
        storage,
        warnings,
        check,
    };
    let mut batch = Vec::with_capacity(VECTOR_CAPACITY);
    let mut line = 1usize;
    let mut skipped = 0usize;
    if let Some(record) = records.next() {
        let record = record?;
        let is_header = match options.header {
            Some(header) => header,
            None if options.auto_detect => {
                koko_common::csv_dialect::looks_like_header(&record, &hdr_names, &hdr_types)
            }
            None => false,
        };
        if !is_header {
            if options.skip > 0 {
                skipped = 1;
            } else {
                match inserter.prepare(&record, line) {
                    Ok(prepared) => {
                        batch.push(prepared);
                        if batch.len() == VECTOR_CAPACITY {
                            count += inserter.flush(&mut batch)?;
                        }
                    }
                    Err(e)
                        if options.ignore_errors
                            && matches!(
                                e,
                                Error::Runtime(_) | Error::Copy(_) | Error::Conversion(_)
                            ) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        line += 1;
    }
    for record in records {
        let record = record?;
        if skipped < options.skip {
            skipped += 1;
            line += 1;
            continue;
        }
        match inserter.prepare(&record, line) {
            Ok(prepared) => {
                batch.push(prepared);
                if batch.len() == VECTOR_CAPACITY {
                    count += inserter.flush(&mut batch)?;
                }
            }
            Err(e)
                if options.ignore_errors
                    && matches!(e, Error::Runtime(_) | Error::Copy(_) | Error::Conversion(_)) => {}
            Err(e) => return Err(e),
        }
        line += 1;
    }
    count += inserter.flush(&mut batch)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cells() {
        assert_eq!(parse_cell("", &LogicalType::Int64).unwrap(), Value::Null);
        assert_eq!(
            parse_cell("42", &LogicalType::Int64).unwrap(),
            Value::Int64(42)
        );
        assert_eq!(
            parse_cell("1.5", &LogicalType::Double).unwrap(),
            Value::Double(1.5)
        );
        assert_eq!(
            parse_cell("hi", &LogicalType::String).unwrap(),
            Value::String("hi".into())
        );
        assert_eq!(
            parse_cell("TRUE", &LogicalType::Bool).unwrap(),
            Value::Bool(true)
        );
        assert!(parse_cell("x", &LogicalType::Int64).is_err());
    }

    #[test]
    fn list_unbraced_normalizes_nested_values() {
        let options = CsvOptions {
            list_unbraced: true,
            ..CsvOptions::default()
        };
        let strings = LogicalType::List(Box::new(LogicalType::String));
        assert_eq!(
            parse_cell_with_options("a;b;c", &strings, &options).unwrap(),
            Value::List(vec![
                Value::String("a".into()),
                Value::String("b".into()),
                Value::String("c".into()),
            ])
        );
        let nested = LogicalType::List(Box::new(strings));
        assert_eq!(
            parse_cell_with_options("[a,b];x;[y,z]", &nested, &options)
                .unwrap()
                .to_result_string(),
            "[[a,b],[x],[y,z]]"
        );
    }

    #[test]
    fn seekable_parallel_records_equal_serial_and_use_multiple_workers() {
        use std::sync::atomic::Ordering;

        let path =
            std::env::temp_dir().join(format!("koko-parallel-csv-{}.csv", std::process::id()));
        let mut contents = String::new();
        for row in 0..12_000 {
            contents.push_str(&format!("{row},value-{row}\n"));
        }
        std::fs::write(&path, contents).unwrap();
        let warnings = koko_common::warnings::WarningRegistry::default().sink(1, u64::MAX);

        let serial_options = CsvOptions {
            parallel: false,
            header: Some(false),
            ..CsvOptions::default()
        };
        let serial: Vec<Vec<String>> = read_records(
            &path,
            2,
            &serial_options,
            &warnings,
            8,
            MemoryTracker::default(),
        )
        .unwrap()
        .map(|record| {
            record
                .unwrap()
                .iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();

        PARALLEL_WORKERS_STARTED.store(0, Ordering::SeqCst);
        let tracker = MemoryTracker::default();
        let parallel: Vec<Vec<String>> = read_records(
            &path,
            2,
            &CsvOptions {
                parallel: true,
                header: Some(false),
                ..CsvOptions::default()
            },
            &warnings,
            4,
            tracker.clone(),
        )
        .unwrap()
        .map(|record| {
            record
                .unwrap()
                .iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();

        assert_eq!(parallel, serial);
        assert!(PARALLEL_WORKERS_STARTED.load(Ordering::SeqCst) > 1);
        assert_eq!(tracker.usage().current, 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parallel_record_memory_limit_fails_without_leaking_reservations() {
        let path =
            std::env::temp_dir().join(format!("koko-parallel-memory-{}.csv", std::process::id()));
        let mut contents = "x".repeat(70 * 1024);
        contents.push_str("\nsmall\n");
        std::fs::write(&path, contents).unwrap();
        let warnings = koko_common::warnings::WarningRegistry::default().sink(1, u64::MAX);
        let tracker = MemoryTracker::new(Some(1024));
        let mut records = read_records(
            &path,
            1,
            &CsvOptions::default(),
            &warnings,
            4,
            tracker.clone(),
        )
        .unwrap();
        let error = records.next().unwrap().unwrap_err();
        assert!(matches!(error, Error::BufferManager));
        assert_eq!(
            error.to_string(),
            "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
        );
        drop(records);
        assert_eq!(tracker.usage().current, 0);
        let _ = std::fs::remove_file(path);
    }
}
