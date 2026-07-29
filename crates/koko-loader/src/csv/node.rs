use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn copy_node(
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
    let table_name = entry.name().to_string();
    let col_types: Vec<LogicalType> = entry
        .columns()
        .iter()
        .map(|column| column.logical_type().clone())
        .collect();
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
                        .columns()
                        .iter()
                        .position(|column| column.name().eq_ignore_ascii_case(name))
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
            .columns()
            .iter()
            .enumerate()
            .filter(|(_, column)| !column.is_serial())
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
        .map(|&i| entry.columns()[i].name().to_string())
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

pub(super) struct PreparedNode {
    values: Vec<Value>,
    line: usize,
    text: String,
}

pub(super) struct NodeRecordInserter<'a, 's> {
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
                .get(table.primary_key_index())
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

#[allow(clippy::too_many_arguments)]
pub(super) fn copy_node_zero_input(
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
