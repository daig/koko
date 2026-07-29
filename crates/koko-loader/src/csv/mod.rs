//! CSV COPY orchestration and protocol-specific implementation.

mod node;
mod protocol;
mod record;
mod relation;

use node::*;
use protocol::*;
use record::*;
use relation::*;

use koko_catalog::{Catalog, ColumnDefault};
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

pub use record::{copy_expected_arity, parse_cell, validate_copy_file_arity};

/// COPY reports storage constraint violations (duplicate / NULL primary key,
/// rel multiplicity) as `Copy exception`s with the same text the operator path
/// reports as `Runtime exception`s — mirroring C++'s bulk-copy `CopyException`.
pub(super) fn copy_class(e: Error) -> Error {
    match e {
        Error::Runtime(m) => Error::Copy(m),
        other => other,
    }
}

pub(super) fn warning_message(error: Error) -> String {
    match error {
        Error::Runtime(message) => message,
        other => other.to_string(),
    }
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
pub fn apply_column_defaults(
    table: TableId,
    cols: &[usize],
    values: &mut [Value],
    catalog: &Catalog,
) -> Result<()> {
    for &col in cols {
        values[col] = match catalog.column_default(table, col) {
            Some(ColumnDefault::Constant(value)) => value,
            Some(ColumnDefault::NextVal(sequence)) => {
                Value::Int64(catalog.sequence_next_val(&sequence)?)
            }
            None => continue,
        };
    }
    Ok(())
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
