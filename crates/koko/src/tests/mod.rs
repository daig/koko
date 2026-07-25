//! Cross-module facade contracts with private crate access.

use super::*;
use crate::runtime::{ACTIVE_TRANSACTION_MSG, READ_ONLY_WRITE_MSG};
use koko_common::VECTOR_CAPACITY;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

mod concurrency;
mod execution;
mod graphs;
mod interchange;
mod transactions;
mod udfs;

impl QueryResult {
    fn get_row_i64(&self, row: usize, col: usize) -> i64 {
        self.value(row, col).unwrap().as_i64().unwrap()
    }
}

/// Helper: a query's rendered rows, sorted for order-independent comparison.
fn sorted_rows(c: &Connection, q: &str) -> Vec<String> {
    let mut rows = c.query(q).unwrap().to_result_strings();
    rows.sort();
    rows
}

/// Write a CSV to a unique temp file; returns its forward-slash path for
/// embedding in a quoted `LOAD FROM "<path>"`.
fn temp_csv(tag: &str, contents: &str) -> (std::path::PathBuf, String) {
    let path = std::env::temp_dir().join(format!("koko_load_{tag}.csv"));
    std::fs::write(&path, contents).unwrap();
    let q = path.to_string_lossy().replace('\\', "/");
    (path, q)
}

fn external_dataset_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("KOKO_DATASET_DIR") {
        return PathBuf::from(path);
    }
    let workspace_parent = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace has a parent directory");
    let mut candidates = std::fs::read_dir(workspace_parent)
        .expect("workspace parent is readable")
        .filter_map(|entry| {
            let dataset = entry.ok()?.path().join("dataset");
            dataset
                .join("npy-2d/id_int64.npy")
                .is_file()
                .then_some(dataset)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .expect("set KOKO_DATASET_DIR to run tests that use the historical external NPY fixtures")
}

fn interchange_temp_path(tag: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tag = Path::new(tag);
    let stem = tag
        .file_stem()
        .and_then(|part| part.to_str())
        .unwrap_or("data");
    let suffix = tag.extension().and_then(|part| part.to_str());
    let unique = format!(
        "koko-interchange-{stem}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    std::env::temp_dir().join(match suffix {
        Some(suffix) => format!("{unique}.{suffix}"),
        None => unique,
    })
}

fn write_parquet_rows(path: &Path, fields: Vec<koko_loader::ParquetField>, rows: &[Vec<Value>]) {
    let schema = koko_loader::ParquetSchema::new(fields).unwrap();
    let types = schema.types();
    let mut writer = koko_loader::ParquetFileWriter::create(
        path,
        schema,
        koko_loader::ParquetWriterOptions::default(),
    )
    .unwrap();
    for rows in rows.chunks(VECTOR_CAPACITY) {
        let mut chunk = DataChunk::new(&types);
        for (position, row) in rows.iter().enumerate() {
            for (column, value) in chunk.columns.iter_mut().zip(row) {
                column.set_value(position, value);
            }
        }
        chunk.set_flat(rows.len());
        writer.write_chunk(&chunk).unwrap();
    }
    assert_eq!(writer.finish().unwrap(), rows.len() as u64);
}
