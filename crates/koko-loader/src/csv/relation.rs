use super::*;

pub(super) struct PreparedRel {
    src: koko_common::InternalId,
    dst: koko_common::InternalId,
    props: Vec<Value>,
    line: usize,
    text: String,
}

pub(super) struct RelRecordInserter<'a, 's> {
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
pub(super) fn copy_rel(
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
    let num_props = rel.columns().len();
    let all_prop_types: Vec<_> = rel
        .columns()
        .iter()
        .map(|column| column.logical_type().clone())
        .collect();
    // A partial column list (`COPY r(a, b) FROM …`) restricts and orders the
    // CSV property columns; unlisted properties stay NULL.
    let prop_cols: Vec<usize> = match columns {
        Some(cols) => {
            let rel_name = rel.name().to_string();
            let resolved: Vec<usize> = cols
                .iter()
                .map(|name| {
                    rel.columns()
                        .iter()
                        .position(|column| column.name().eq_ignore_ascii_case(name))
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
        .map(|&i| rel.columns()[i].logical_type().clone())
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
        .logical_type()
        .clone();
    let to_pk_ty = catalog
        .node_table(to_table)
        .unwrap()
        .primary_key_column()
        .logical_type()
        .clone();

    let expected = 2 + prop_types.len();
    let diagnostic_dialect = detected_dialect(path, expected, options)?;
    let mut records = read_records(path, expected, options, warnings, worker_count, memory)?;
    // A relationship header (when present) is row 1 read as the FROM/TO endpoints
    // plus the property columns; detected schema-anchored (names match, or a cell
    // fails to parse as its declared type — e.g. an LDBC `:START_ID(..)` label).
    let mut hdr_names = vec!["from".to_string(), "to".to_string()];
    hdr_names.extend(
        prop_cols
            .iter()
            .map(|&i| rel.columns()[i].name().to_string()),
    );
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
