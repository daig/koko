//! COPY format dispatch and typed batch insertion.

use koko_catalog::{Catalog, ColumnDefault};
use koko_common::warnings::WarningSink;
use koko_common::{
    DataChunk, Error, InternalId, LogicalType, MemoryTracker, QueryControl, Result, TableId,
    VECTOR_CAPACITY, Value,
};
use koko_ir::bound::BoundCopy;
use koko_storage::{SharedStorage, StorageReadHandle, StorageWriteHandle};
use std::path::{Path, PathBuf};

/// Explicit storage capabilities for one COPY operation.
pub(crate) struct CopyOperationContext<'a> {
    catalog: &'a Catalog,
    storage: &'a SharedStorage,
    memory: &'a MemoryTracker,
    read: StorageReadHandle,
    write: StorageWriteHandle,
    warnings: &'a WarningSink,
    worker_count: usize,
    control: QueryControl<'a>,
}

impl<'a> CopyOperationContext<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        catalog: &'a Catalog,
        storage: &'a SharedStorage,
        memory: &'a MemoryTracker,
        read: StorageReadHandle,
        write: StorageWriteHandle,
        warnings: &'a WarningSink,
        worker_count: usize,
        control: QueryControl<'a>,
    ) -> Self {
        Self {
            catalog,
            storage,
            memory,
            read,
            write,
            warnings,
            worker_count,
            control,
        }
    }

    fn catalog(&self) -> &Catalog {
        self.catalog
    }

    fn storage(&self) -> &SharedStorage {
        self.storage
    }

    fn memory(&self) -> &MemoryTracker {
        self.memory
    }

    fn read(&self) -> StorageReadHandle {
        self.read
    }

    fn write(&self) -> StorageWriteHandle {
        self.write
    }

    fn worker_count(&self) -> usize {
        self.worker_count
    }

    fn control(&self) -> QueryControl<'a> {
        self.control
    }

    fn find_endpoint(
        &self,
        table: TableId,
        key: &Value,
        read: StorageReadHandle,
    ) -> Result<InternalId> {
        let lookup_key = if let Some(table) = self.catalog.node_table(table) {
            koko_function::cast_value(key, table.primary_key_column().logical_type())?
        } else {
            key.clone()
        };
        self.storage
            .read()
            .find_node_by_pk(read, table, &lookup_key)
            .ok_or_else(|| {
                Error::copy(format!(
                    "Unable to find primary key value {}.",
                    lookup_key.to_result_string()
                ))
            })
    }
}

fn ignorable_copy_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Runtime(_) | Error::Copy(_) | Error::Conversion(_)
    )
}

/// Fold row-level batch insertion outcomes into COPY's count/warning/error contract.
fn account_copy_insert_results(
    results: Vec<Result<InternalId>>,
    ignore_errors: bool,
    context: &CopyOperationContext<'_>,
) -> Result<u64> {
    let mut inserted = 0;
    for result in results {
        match result {
            Ok(_) => inserted += 1,
            Err(error) if ignore_errors && ignorable_copy_error(&error) => {
                let message = match error {
                    Error::Runtime(message) | Error::Copy(message) => message,
                    other => other.to_string(),
                };
                context
                    .warnings
                    .push(message, String::new(), 0, String::new());
            }
            Err(Error::Runtime(message)) => return Err(Error::Copy(message)),
            Err(other) => return Err(other),
        }
    }
    Ok(inserted)
}

/// Bulk-insert rows produced by a query source. Runtime execution of the source
/// precedes this adapter call, so COPY never calls back into runtime state.
pub(super) fn run_copy_from_rows(
    context: &mut CopyOperationContext<'_>,
    copy: &BoundCopy,
    rows: Vec<Vec<Value>>,
) -> Result<u64> {
    let rows_bytes = (rows.capacity() * std::mem::size_of::<Vec<Value>>()) as u64
        + rows
            .iter()
            .map(|row| {
                (row.capacity() * std::mem::size_of::<Value>()) as u64
                    + row
                        .iter()
                        .map(koko_common::value_payload_bytes)
                        .sum::<u64>()
            })
            .sum::<u64>();
    let _rows_memory = context.memory().try_reserve(rows_bytes)?;
    let ignore_errors = copy.options.ignore_errors;
    let mut count = 0u64;
    if copy.is_node {
        let entry = context
            .catalog()
            .node_table(copy.table)
            .ok_or_else(|| Error::catalog("COPY into unknown node table".to_string()))?;
        let table_name = entry.name().to_string();
        let col_types: Vec<LogicalType> = entry
            .columns()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect();
        let input_cols: Vec<usize> = match &copy.columns {
            Some(cols) => cols
                .iter()
                .map(|name| {
                    entry
                        .columns()
                        .iter()
                        .position(|column| column.name().eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            Error::binder(format!(
                                "Table {table_name} does not contain column {name}."
                            ))
                        })
                })
                .collect::<Result<_>>()?,
            // SERIAL columns are not query-fed — they take their sequence
            // defaults (matching the CSV path and the bind-time count).
            None => entry
                .columns()
                .iter()
                .enumerate()
                .filter(|(_, column)| !matches!(column.default(), Some(ColumnDefault::NextVal(_))))
                .map(|(i, _)| i)
                .collect(),
        };
        let default_cols: Vec<usize> = (0..col_types.len())
            .filter(|index| !input_cols.contains(index))
            .collect();
        for rows in rows.chunks(VECTOR_CAPACITY) {
            context.control().check()?;
            let mut batch = DataChunk::new(&col_types);
            for (position, row) in rows.iter().enumerate() {
                if row.len() != input_cols.len() {
                    return Err(Error::binder(format!(
                        "Number of columns mismatch. Expected {} but got {}.",
                        input_cols.len(),
                        row.len()
                    )));
                }
                let mut full = vec![Value::Null; col_types.len()];
                koko_loader::apply_column_defaults(
                    copy.table,
                    &default_cols,
                    &mut full,
                    context.catalog(),
                )?;
                for (value, &column_index) in row.iter().zip(&input_cols) {
                    full[column_index] =
                        koko_function::cast_value(value, &col_types[column_index])?;
                }
                for (column, value) in batch.columns.iter_mut().zip(&full) {
                    column.set_value(position, value);
                }
            }
            batch.set_flat(rows.len());
            count += account_copy_insert_results(
                context.storage().write().insert_node_batch(
                    context.write(),
                    copy.table,
                    &batch,
                    ignore_errors,
                ),
                ignore_errors,
                context,
            )?;
        }
    } else {
        let rel = context
            .catalog()
            .rel_table(copy.table)
            .ok_or_else(|| Error::catalog("COPY into unknown rel table".to_string()))?;
        let (_, from0, to0) = context
            .catalog()
            .rel_members(copy.table)
            .into_iter()
            .find(|(member, _, _)| *member == copy.table)
            .ok_or_else(|| Error::catalog("COPY relationship pair is missing".to_string()))?;
        let prop_types: Vec<LogicalType> = rel
            .columns()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect();
        let prop_cols: Vec<usize> = match &copy.columns {
            Some(cols) => cols
                .iter()
                .map(|name| {
                    rel.columns()
                        .iter()
                        .position(|column| column.name().eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            Error::binder(format!(
                                "Table {} does not contain column {name}.",
                                rel.name()
                            ))
                        })
                })
                .collect::<Result<_>>()?,
            None => (0..prop_types.len()).collect(),
        };
        let default_cols: Vec<usize> = (0..prop_types.len())
            .filter(|index| !prop_cols.contains(index))
            .collect();
        let mut batch_types = vec![LogicalType::InternalId, LogicalType::InternalId];
        batch_types.extend(prop_types.iter().cloned());
        let mut batch = DataChunk::new(&batch_types);
        let mut batch_len = 0;
        for row in &rows {
            if batch_len == 0 {
                context.control().check()?;
            }
            if row.len() != 2 + prop_cols.len() {
                return Err(Error::binder(format!(
                    "Number of columns mismatch. Expected {} but got {}.",
                    2 + prop_cols.len(),
                    row.len()
                )));
            }
            let endpoints = (
                context.find_endpoint(from0, &row[0], context.read()),
                context.find_endpoint(to0, &row[1], context.read()),
            );
            let (src, dst) = match endpoints {
                (Ok(src), Ok(dst)) => (src, dst),
                (Err(error), _) | (_, Err(error))
                    if ignore_errors && ignorable_copy_error(&error) =>
                {
                    if batch_len > 0 {
                        batch.set_flat(batch_len);
                        count += account_copy_insert_results(
                            context.storage().write().insert_rel_batch(
                                context.write(),
                                copy.table,
                                &batch,
                                true,
                            ),
                            true,
                            context,
                        )?;
                        batch = DataChunk::new(&batch_types);
                        batch_len = 0;
                    }
                    let message = match error {
                        Error::Runtime(message) | Error::Copy(message) => message,
                        other => other.to_string(),
                    };
                    context
                        .warnings
                        .push(message, String::new(), 0, String::new());
                    continue;
                }
                (Err(error), _) | (_, Err(error)) => return Err(error),
            };
            let mut props = vec![Value::Null; prop_types.len()];
            koko_loader::apply_column_defaults(
                copy.table,
                &default_cols,
                &mut props,
                context.catalog(),
            )?;
            for (value, &column_index) in row.iter().skip(2).zip(&prop_cols) {
                props[column_index] = koko_function::cast_value(value, &prop_types[column_index])?;
            }
            batch.columns[0].set_internal_id(batch_len, src);
            batch.columns[1].set_internal_id(batch_len, dst);
            for (column, value) in batch.columns[2..].iter_mut().zip(&props) {
                column.set_value(batch_len, value);
            }
            batch_len += 1;
            if batch_len == VECTOR_CAPACITY {
                batch.set_flat(batch_len);
                count += account_copy_insert_results(
                    context.storage().write().insert_rel_batch(
                        context.write(),
                        copy.table,
                        &batch,
                        ignore_errors,
                    ),
                    ignore_errors,
                    context,
                )?;
                batch = DataChunk::new(&batch_types);
                batch_len = 0;
            }
        }
        if batch_len > 0 {
            batch.set_flat(batch_len);
            count += account_copy_insert_results(
                context.storage().write().insert_rel_batch(
                    context.write(),
                    copy.table,
                    &batch,
                    ignore_errors,
                ),
                ignore_errors,
                context,
            )?;
        }
    }
    Ok(count)
}

fn copy_input_types(
    context: &CopyOperationContext<'_>,
    copy: &BoundCopy,
) -> Result<Vec<LogicalType>> {
    if copy.is_node {
        let table = context
            .catalog()
            .node_table(copy.table)
            .ok_or_else(|| Error::catalog("COPY into unknown node table".to_string()))?;
        let indices: Vec<usize> = match &copy.columns {
            Some(columns) => columns
                .iter()
                .map(|name| {
                    table
                        .columns()
                        .iter()
                        .position(|column| column.name().eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            Error::binder(format!(
                                "Table {} does not contain column {name}.",
                                table.name()
                            ))
                        })
                })
                .collect::<Result<_>>()?,
            None => table
                .columns()
                .iter()
                .enumerate()
                .filter(|(_, column)| !matches!(column.default(), Some(ColumnDefault::NextVal(_))))
                .map(|(index, _)| index)
                .collect(),
        };
        return Ok(indices
            .into_iter()
            .map(|index| table.columns()[index].logical_type().clone())
            .collect());
    }

    let rel = context
        .catalog()
        .rel_table(copy.table)
        .ok_or_else(|| Error::catalog("COPY into unknown rel table".to_string()))?;
    let (_, from, to) = context
        .catalog()
        .rel_members(copy.table)
        .into_iter()
        .find(|(member, _, _)| *member == copy.table)
        .ok_or_else(|| Error::catalog("COPY relationship pair is missing".to_string()))?;
    let mut types = Vec::with_capacity(rel.columns().len() + 2);
    types.push(
        context
            .catalog()
            .node_table(from)
            .expect("relationship source table exists")
            .primary_key_column()
            .logical_type()
            .clone(),
    );
    types.push(
        context
            .catalog()
            .node_table(to)
            .expect("relationship destination table exists")
            .primary_key_column()
            .logical_type()
            .clone(),
    );
    match &copy.columns {
        Some(columns) => {
            for name in columns {
                let column = rel.column(name).ok_or_else(|| {
                    Error::binder(format!(
                        "Table {} does not contain column {name}.",
                        rel.name()
                    ))
                })?;
                types.push(column.logical_type().clone());
            }
        }
        None => types.extend(
            rel.columns()
                .iter()
                .map(|column| column.logical_type().clone()),
        ),
    }
    Ok(types)
}

fn warn_columnar_row(context: &CopyOperationContext<'_>, path: &str, row: usize, error: &Error) {
    let message = match error {
        Error::Runtime(message) | Error::Copy(message) | Error::Conversion(message) => {
            message.clone()
        }
        other => other.to_string(),
    };
    context
        .warnings
        .push(message, path.to_string(), row as u64 + 1, String::new());
}

fn copy_typed_batch(
    context: &mut CopyOperationContext<'_>,
    copy: &BoundCopy,
    path: &str,
    source: &DataChunk,
) -> Result<u64> {
    let ignore_errors = copy.options.ignore_errors;
    if copy.is_node {
        let table = context
            .catalog()
            .node_table(copy.table)
            .ok_or_else(|| Error::catalog("COPY into unknown node table".to_string()))?;
        let col_types: Vec<LogicalType> = table
            .columns()
            .iter()
            .map(|column| column.logical_type().clone())
            .collect();
        let input_cols: Vec<usize> = match &copy.columns {
            Some(columns) => columns
                .iter()
                .map(|name| {
                    table
                        .columns()
                        .iter()
                        .position(|column| column.name().eq_ignore_ascii_case(name))
                        .ok_or_else(|| {
                            Error::binder(format!(
                                "Table {} does not contain column {name}.",
                                table.name()
                            ))
                        })
                })
                .collect::<Result<_>>()?,
            None => table
                .columns()
                .iter()
                .enumerate()
                .filter(|(_, column)| !matches!(column.default(), Some(ColumnDefault::NextVal(_))))
                .map(|(index, _)| index)
                .collect(),
        };
        if source.columns.len() != input_cols.len() {
            return Err(Error::binder(format!(
                "Number of columns mismatch. Expected {} but got {}.",
                input_cols.len(),
                source.columns.len()
            )));
        }
        let default_cols: Vec<usize> = (0..col_types.len())
            .filter(|index| !input_cols.contains(index))
            .collect();
        let mut output = DataChunk::new(&col_types);
        let mut values = vec![Value::Null; col_types.len()];
        let mut output_row = 0usize;
        for (row, physical) in source.sel.iter().enumerate() {
            values.fill(Value::Null);
            koko_loader::apply_column_defaults(
                copy.table,
                &default_cols,
                &mut values,
                context.catalog(),
            )?;
            // Bind/evaluate every projected cast before choosing the row error.
            // C++ constructs the whole COPY projection and reports the last
            // incompatible projected column rather than short-circuiting the
            // first cast expression.
            let mut conversion = None;
            for (source_column, &target_column) in source.columns.iter().zip(&input_cols) {
                match koko_function::cast_value(
                    &source_column.get_value(physical),
                    &col_types[target_column],
                ) {
                    Ok(value) => values[target_column] = value,
                    Err(error) => conversion = Some(error),
                }
            }
            if let Some(error) = conversion {
                if !ignore_errors {
                    return Err(error);
                }
                warn_columnar_row(context, path, row, &error);
                continue;
            }
            for (column, value) in output.columns.iter_mut().zip(&values) {
                column.set_value(output_row, value);
            }
            output_row += 1;
        }
        if output_row == 0 {
            return Ok(0);
        }
        output.set_flat(output_row);
        return account_copy_insert_results(
            context.storage().write().insert_node_batch(
                context.write(),
                copy.table,
                &output,
                ignore_errors,
            ),
            ignore_errors,
            context,
        );
    }

    let rel = context
        .catalog()
        .rel_table(copy.table)
        .ok_or_else(|| Error::catalog("COPY into unknown rel table".to_string()))?;
    let (_, from, to) = context
        .catalog()
        .rel_members(copy.table)
        .into_iter()
        .find(|(member, _, _)| *member == copy.table)
        .ok_or_else(|| Error::catalog("COPY relationship pair is missing".to_string()))?;
    let prop_types: Vec<LogicalType> = rel
        .columns()
        .iter()
        .map(|column| column.logical_type().clone())
        .collect();
    let prop_cols: Vec<usize> = match &copy.columns {
        Some(columns) => columns
            .iter()
            .map(|name| {
                rel.columns()
                    .iter()
                    .position(|column| column.name().eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        Error::binder(format!(
                            "Table {} does not contain column {name}.",
                            rel.name()
                        ))
                    })
            })
            .collect::<Result<_>>()?,
        None => (0..prop_types.len()).collect(),
    };
    if source.columns.len() != prop_cols.len() + 2 {
        return Err(Error::binder(format!(
            "Number of columns mismatch. Expected {} but got {}.",
            prop_cols.len() + 2,
            source.columns.len()
        )));
    }
    let default_cols: Vec<usize> = (0..prop_types.len())
        .filter(|index| !prop_cols.contains(index))
        .collect();
    let mut output_types = vec![LogicalType::InternalId, LogicalType::InternalId];
    output_types.extend(prop_types.iter().cloned());
    let mut output = DataChunk::new(&output_types);
    let mut props = vec![Value::Null; prop_types.len()];
    let mut output_row = 0usize;
    for (row, physical) in source.sel.iter().enumerate() {
        let endpoints = (
            context.find_endpoint(from, &source.columns[0].get_value(physical), context.read()),
            context.find_endpoint(to, &source.columns[1].get_value(physical), context.read()),
        );
        let (src, dst) = match endpoints {
            (Ok(src), Ok(dst)) => (src, dst),
            (Err(error), _) | (_, Err(error)) if ignore_errors && ignorable_copy_error(&error) => {
                warn_columnar_row(context, path, row, &error);
                continue;
            }
            (Err(error), _) | (_, Err(error)) => return Err(error),
        };
        props.fill(Value::Null);
        koko_loader::apply_column_defaults(
            copy.table,
            &default_cols,
            &mut props,
            context.catalog(),
        )?;
        let mut conversion = None;
        for (source_column, &target_column) in source.columns[2..].iter().zip(&prop_cols) {
            match koko_function::cast_value(
                &source_column.get_value(physical),
                &prop_types[target_column],
            ) {
                Ok(value) => props[target_column] = value,
                Err(error) => conversion = Some(error),
            }
        }
        if let Some(error) = conversion {
            if !ignore_errors {
                return Err(error);
            }
            warn_columnar_row(context, path, row, &error);
            continue;
        }
        output.columns[0].set_internal_id(output_row, src);
        output.columns[1].set_internal_id(output_row, dst);
        for (column, value) in output.columns[2..].iter_mut().zip(&props) {
            column.set_value(output_row, value);
        }
        output_row += 1;
    }
    if output_row == 0 {
        return Ok(0);
    }
    output.set_flat(output_row);
    account_copy_insert_results(
        context.storage().write().insert_rel_batch(
            context.write(),
            copy.table,
            &output,
            ignore_errors,
        ),
        ignore_errors,
        context,
    )
}

fn run_columnar_copy(
    context: &mut CopyOperationContext<'_>,
    copy: &BoundCopy,
    format: koko_common::file_resolver::FileFormat,
) -> Result<u64> {
    let paths: Vec<String> = std::iter::once(copy.file_path.clone())
        .chain(copy.extra_files.iter().cloned())
        .collect();
    let mut total = 0u64;
    match format {
        koko_common::file_resolver::FileFormat::Parquet => {
            for path in &paths {
                let mut reader = koko_loader::parquet::ParquetReader::open(path)?;
                for batch in &mut reader {
                    context.control().check()?;
                    let batch = batch?;
                    let _memory = context.memory().try_reserve(batch.allocated_bytes())?;
                    total += copy_typed_batch(context, copy, path, &batch)?;
                }
            }
        }
        koko_common::file_resolver::FileFormat::Npy => {
            let target_types = copy_input_types(context, copy)?;
            let npy_paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            let metadata = koko_loader::npy::preflight_npy(&npy_paths, Some(&target_types))?;
            let mut reader = koko_loader::npy::NpyBatchReader::from_metadata(metadata)?;
            for batch in &mut reader {
                context.control().check()?;
                let batch = batch?;
                let _memory = context.memory().try_reserve(batch.allocated_bytes())?;
                total += copy_typed_batch(context, copy, &paths[0], &batch)?;
            }
        }
        koko_common::file_resolver::FileFormat::Csv => unreachable!("CSV has a dedicated loader"),
    }
    Ok(total)
}

/// Execute file-backed `COPY` over paths canonicalized by the central binder
/// resolver.
pub(super) fn run_copy(context: &mut CopyOperationContext<'_>, copy: &BoundCopy) -> Result<u64> {
    let format = copy
        .format
        .unwrap_or(koko_common::file_resolver::FileFormat::Csv);
    if format != koko_common::file_resolver::FileFormat::Csv {
        return run_columnar_copy(context, copy, format);
    }
    debug_assert!(!copy.by_column, "BY COLUMN format resolves to NPY");
    // Pre-validate every file's first-record arity (the C++ bind-time sniff),
    // so a later file's bad header errors before the first file copies.
    let expected = koko_loader::copy_expected_arity(
        copy.table,
        copy.is_node,
        copy.columns.as_deref(),
        context.catalog(),
    )?;
    for file in std::iter::once(&copy.file_path).chain(copy.extra_files.iter()) {
        context.control().check()?;
        koko_loader::validate_copy_file_arity(expected, Path::new(file), &copy.options)?;
    }
    let memory = context.memory().clone();
    let mut total = 0u64;
    let check_control = || context.control().check();
    let mut storage = context.storage().write();
    for file in std::iter::once(&copy.file_path).chain(copy.extra_files.iter()) {
        context.control().check()?;
        let path = Path::new(file);
        total += koko_loader::copy_from_csv(
            copy.table,
            copy.is_node,
            copy.columns.as_deref(),
            path,
            &copy.options,
            koko_loader::CopyContext {
                catalog: context.catalog(),
                read: context.read(),
                write: context.write(),
                storage: &mut storage,
                warnings: context.warnings,
                worker_count: context.worker_count(),
                memory: memory.clone(),
                check: &check_control,
            },
        )?;
    }
    Ok(total)
}
