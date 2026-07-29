//! Typed, bounded readers for `LOAD FROM` sources.

use crate::{npy, parquet};
use koko_common::{
    DataChunk, Error, LogicalType, MemoryReservation, MemoryTracker, QueryControl, Result,
    VECTOR_CAPACITY, Value,
    csv_dialect::{self, CsvOptions, Dialect},
    file_resolver::FileFormat,
    warnings::WarningSink,
};
use koko_function::{cast_value, parse_csv_cell};
use std::path::{Path, PathBuf};

/// A decoded source batch whose temporary memory remains charged until consumed.
pub struct LoadBatch {
    pub columns: DataChunk,
    _reservation: MemoryReservation,
}

enum ColumnarReader {
    Parquet(parquet::ParquetReader),
    Npy(npy::NpyBatchReader),
}

impl Iterator for ColumnarReader {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Parquet(reader) => reader.next(),
            Self::Npy(reader) => reader.next(),
        }
    }
}

/// Stateful multi-file `LOAD FROM` decoder.
pub struct LoadScan {
    paths: Vec<String>,
    column_names: Vec<String>,
    column_types: Vec<LogicalType>,
    options: CsvOptions,
    format: FileFormat,
    bare: bool,
    file_index: usize,
    csv_reader: Option<csv::Reader<std::fs::File>>,
    csv_dialect: Option<Dialect>,
    columnar_reader: Option<ColumnarReader>,
    done: bool,
}

impl LoadScan {
    pub fn new(
        paths: &[String],
        column_names: &[String],
        column_types: Vec<LogicalType>,
        options: &CsvOptions,
        format: FileFormat,
        bare: bool,
    ) -> Self {
        Self {
            paths: paths.to_vec(),
            column_names: column_names.to_vec(),
            column_types,
            options: options.clone(),
            format,
            bare,
            file_index: 0,
            csv_reader: None,
            csv_dialect: None,
            columnar_reader: None,
            done: false,
        }
    }

    pub fn next_batch(
        &mut self,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
        warnings: &WarningSink,
    ) -> Result<Option<LoadBatch>> {
        let columns = if self.format == FileFormat::Csv {
            self.next_csv_batch(control, warnings)?
        } else {
            self.next_columnar_batch(control, warnings)?
        };
        let Some(columns) = columns else {
            return Ok(None);
        };
        let reservation = memory.try_reserve(columns.allocated_bytes())?;
        Ok(Some(LoadBatch {
            columns,
            _reservation: reservation,
        }))
    }

    fn next_csv_batch(
        &mut self,
        control: QueryControl<'_>,
        warnings: &WarningSink,
    ) -> Result<Option<DataChunk>> {
        if self.done {
            return Ok(None);
        }
        let mut output = DataChunk::new(&self.column_types);
        let mut output_row = 0usize;
        loop {
            if self.csv_reader.is_none() {
                if self.file_index >= self.paths.len() {
                    self.done = true;
                    if output_row == 0 {
                        return Ok(None);
                    }
                    output.set_flat(output_row);
                    return Ok(Some(output));
                }
                let path = self.paths[self.file_index].as_str();
                let (mut reader, dialect) =
                    open_csv_reader(path, &self.options, self.column_types.len())?;
                self.csv_dialect = Some(dialect);
                let mut record = csv::StringRecord::new();
                let mut skipped = 0usize;
                match self.options.header {
                    Some(true) => {
                        read_csv(
                            &mut reader,
                            &mut record,
                            path,
                            &self.options,
                            dialect,
                            control,
                            warnings,
                        )?;
                    }
                    Some(false) => {}
                    None => {
                        if read_csv(
                            &mut reader,
                            &mut record,
                            path,
                            &self.options,
                            dialect,
                            control,
                            warnings,
                        )? {
                            let is_header = csv_dialect::looks_like_header(
                                &record,
                                &self.column_names,
                                &self.column_types,
                            );
                            if !is_header {
                                if self.options.skip > 0 {
                                    skipped = 1;
                                } else if let Err(error) = push_csv_row(
                                    &record,
                                    &self.column_types,
                                    self.bare,
                                    &self.options,
                                    warnings,
                                    path,
                                    &mut output,
                                    output_row,
                                ) {
                                    if !self.options.ignore_errors
                                        || matches!(error, Error::Parser(_))
                                    {
                                        return Err(error);
                                    }
                                } else {
                                    output_row += 1;
                                }
                            }
                        }
                    }
                }
                while skipped < self.options.skip {
                    if !read_csv(
                        &mut reader,
                        &mut record,
                        path,
                        &self.options,
                        dialect,
                        control,
                        warnings,
                    )? {
                        break;
                    }
                    skipped += 1;
                }
                self.csv_reader = Some(reader);
            }

            let path = self.paths[self.file_index].as_str();
            let reader = self.csv_reader.as_mut().expect("CSV reader initialized");
            let mut record = csv::StringRecord::new();
            loop {
                control.check()?;
                if output_row == VECTOR_CAPACITY {
                    output.set_flat(output_row);
                    return Ok(Some(output));
                }
                if !read_csv(
                    reader,
                    &mut record,
                    path,
                    &self.options,
                    self.csv_dialect.expect("CSV dialect resolved with reader"),
                    control,
                    warnings,
                )? {
                    self.csv_reader = None;
                    self.file_index += 1;
                    break;
                }
                match push_csv_row(
                    &record,
                    &self.column_types,
                    self.bare,
                    &self.options,
                    warnings,
                    path,
                    &mut output,
                    output_row,
                ) {
                    Ok(()) => output_row += 1,
                    Err(error)
                        if self.options.ignore_errors && !matches!(error, Error::Parser(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    fn next_columnar_batch(
        &mut self,
        control: QueryControl<'_>,
        warnings: &WarningSink,
    ) -> Result<Option<DataChunk>> {
        if self.done {
            return Ok(None);
        }
        loop {
            control.check()?;
            if self.columnar_reader.is_none() {
                self.columnar_reader = Some(match self.format {
                    FileFormat::Parquet => {
                        if self.file_index >= self.paths.len() {
                            self.done = true;
                            return Ok(None);
                        }
                        ColumnarReader::Parquet(parquet::ParquetReader::open(
                            &self.paths[self.file_index],
                        )?)
                    }
                    FileFormat::Npy => {
                        let paths = self.paths.iter().map(PathBuf::from).collect::<Vec<_>>();
                        let metadata = npy::preflight_npy(&paths, None)?;
                        ColumnarReader::Npy(npy::NpyBatchReader::from_metadata(metadata)?)
                    }
                    FileFormat::Csv => unreachable!("CSV has a dedicated reader"),
                });
            }
            match self
                .columnar_reader
                .as_mut()
                .expect("columnar reader initialized")
                .next()
            {
                Some(Ok(source)) => {
                    if source.columns.len() != self.column_types.len() {
                        return Err(Error::binder(format!(
                            "Number of columns mismatch. Expected {} but got {}.",
                            self.column_types.len(),
                            source.columns.len()
                        )));
                    }
                    let mut output = DataChunk::new(&self.column_types);
                    let mut output_row = 0usize;
                    let mut converted = Vec::with_capacity(self.column_types.len());
                    for (source_row, physical) in source.sel.iter().enumerate() {
                        converted.clear();
                        let conversion = source
                            .columns
                            .iter()
                            .zip(&self.column_types)
                            .try_for_each(|(column, target)| {
                                converted.push(cast_value(&column.get_value(physical), target)?);
                                Ok::<(), Error>(())
                            });
                        if let Err(error) = conversion {
                            if !self.options.ignore_errors {
                                return Err(error);
                            }
                            warnings.push(
                                error.to_string(),
                                self.paths[self.file_index.min(self.paths.len() - 1)].clone(),
                                (source_row + 1) as u64,
                                String::new(),
                            );
                            continue;
                        }
                        for (column, value) in converted.drain(..).enumerate() {
                            output.columns[column].set_value_owned(output_row, value);
                        }
                        output_row += 1;
                    }
                    if output_row > 0 {
                        output.set_flat(output_row);
                        return Ok(Some(output));
                    }
                }
                Some(Err(error)) => return Err(error),
                None => {
                    self.columnar_reader = None;
                    match self.format {
                        FileFormat::Parquet => self.file_index += 1,
                        FileFormat::Npy => {
                            self.done = true;
                            return Ok(None);
                        }
                        FileFormat::Csv => unreachable!("CSV has a dedicated reader"),
                    }
                }
            }
        }
    }
}

fn open_csv_reader(
    path: &str,
    options: &CsvOptions,
    arity: usize,
) -> Result<(csv::Reader<std::fs::File>, Dialect)> {
    let source = Path::new(path);
    let mut dialect = csv_dialect::resolve_dialect(
        source,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        Some(arity),
    )?;
    if arity == 1
        && options.quote.is_none()
        && std::fs::read(path).is_ok_and(|bytes| {
            bytes
                .split(|&byte| byte == b'\n')
                .skip(1)
                .any(|line| line.first() == Some(&b'"'))
        })
    {
        dialect.quote = Some(b'"');
    }
    if !options.ignore_errors {
        csv_dialect::validate_file_structure(source, options)?;
    }
    Ok((csv_dialect::open_reader(source, &dialect)?, dialect))
}

fn is_compressed_csv(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("gz") || extension.eq_ignore_ascii_case("gzip")
        })
}

fn read_csv(
    reader: &mut csv::Reader<std::fs::File>,
    record: &mut csv::StringRecord,
    path: &str,
    options: &CsvOptions,
    dialect: Dialect,
    control: QueryControl<'_>,
    warnings: &WarningSink,
) -> Result<bool> {
    control.check()?;
    let compressed = is_compressed_csv(path);
    let serial_options;
    let options = if compressed && options.parallel {
        serial_options = CsvOptions {
            parallel: false,
            ..options.clone()
        };
        &serial_options
    } else {
        options
    };
    let source = Path::new(path);
    loop {
        let has_record = csv_dialect::read_prevalidated_record(reader, record, source, options)?;
        if !has_record {
            return Ok(false);
        }
        let warning =
            csv_dialect::invalid_record_parts_with_dialect(source, record, options, dialect)
                .or_else(|| csv_dialect::quoted_newline_parts(source, record, options));
        let Some((message, line, mut fragment)) = warning else {
            return Ok(true);
        };
        if compressed {
            fragment.clear();
        }
        if !options.ignore_errors {
            return Err(Error::copy(format!(
                "Error in file {} on line {line}: {message} Line/record containing the error: \
                 '{fragment}'",
                source.display()
            )));
        }
        warnings.push(
            message,
            source.to_string_lossy().into_owned(),
            line,
            fragment,
        );
        record.clear();
    }
}

#[allow(clippy::too_many_arguments)]
fn push_csv_row(
    record: &csv::StringRecord,
    column_types: &[LogicalType],
    bare: bool,
    options: &CsvOptions,
    warnings: &WarningSink,
    path: &str,
    output: &mut DataChunk,
    output_row: usize,
) -> Result<()> {
    let source = Path::new(path);
    let line = record
        .position()
        .map(|position| position.line() as usize)
        .unwrap_or(0);
    let dialect = || {
        csv_dialect::resolve_dialect(
            source,
            options.delimiter,
            options.quote,
            options.escape,
            options.auto_detect,
            Some(column_types.len()),
        )
        .unwrap_or_default()
    };
    let expected = column_types.len();
    let mut actual = record.len();
    if actual == expected + 1 && record.get(actual - 1) == Some("") {
        actual = expected;
    }
    if actual != expected {
        let reported = if bare && expected == 1 { 0 } else { expected };
        let inner = if actual > expected {
            format!("expected {reported} values per row, but got more.")
        } else {
            format!("expected {reported} values per row, but got {actual}.")
        };
        if options.ignore_errors {
            warnings.push(
                inner.clone(),
                source.to_string_lossy().into_owned(),
                line as u64,
                if is_compressed_csv(path) {
                    String::new()
                } else {
                    csv_dialect::physical_line_text(source, line as u64)
                },
            );
        }
        return Err(csv_dialect::wrap_row_error(
            source,
            line,
            &inner,
            None,
            &dialect(),
        ));
    }

    for (column, target) in column_types.iter().enumerate() {
        let raw = record.get(column).unwrap_or("");
        let normalized = csv_dialect::normalize_unbraced_list(raw, target, options.list_unbraced);
        let mut value =
            parse_csv_cell(&normalized, target, &options.null_strings).map_err(|error| {
                if options.ignore_errors && !matches!(error, Error::Parser(_)) {
                    warnings.push(
                        error.to_string(),
                        source.to_string_lossy().into_owned(),
                        line as u64,
                        csv_dialect::physical_line_text(source, line as u64),
                    );
                }
                csv_dialect::wrap_row_error(
                    source,
                    line,
                    &error.to_string(),
                    Some(column),
                    &dialect(),
                )
            })?;
        if bare && matches!(target, LogicalType::String) {
            if let Value::String(text) = &value {
                if let Some(normalized) = koko_common::literal::normalize_list_literal_text(text) {
                    value = Value::String(normalized);
                }
            }
        }
        output.columns[column].set_value_owned(output_row, value);
    }
    Ok(())
}
