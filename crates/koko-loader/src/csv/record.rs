use super::*;

/// Parse one CSV cell into a [`Value`] of the given logical type using the default
/// CSV null-string configuration.
pub fn parse_cell(s: &str, ty: &LogicalType) -> Result<Value> {
    parse_cell_with_options(s, ty, &CsvOptions::default())
}

pub(super) fn parse_cell_with_options(
    s: &str,
    ty: &LogicalType,
    options: &CsvOptions,
) -> Result<Value> {
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
                resolve_listed_columns(cols, entry.columns(), entry.name())?;
                Ok(cols.len())
            }
            None => Ok(entry
                .columns()
                .iter()
                .filter(|column| !column.is_serial())
                .count()),
        }
    } else {
        let rel = catalog
            .rel_table(table)
            .ok_or_else(|| Error::catalog("COPY into unknown rel table".to_string()))?;
        match columns {
            Some(cols) => {
                resolve_listed_columns(cols, rel.columns(), rel.name())?;
                Ok(2 + cols.len())
            }
            None => Ok(2 + rel.columns().len()),
        }
    }
}

pub(super) fn bom_only(path: &Path) -> bool {
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
pub(super) fn resolve_listed_columns(
    cols: &[String],
    columns: &[koko_catalog::Column],
    table_name: &str,
) -> Result<Vec<usize>> {
    let resolved: Vec<usize> = cols
        .iter()
        .map(|name| {
            columns
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
    Ok(resolved)
}

pub(super) fn ragged_row_error(record: &csv::StringRecord, expected: usize) -> Option<String> {
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
pub(super) fn wrap_row_error(
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
pub(super) fn detected_dialect(path: &Path, arity: usize, options: &CsvOptions) -> Result<Dialect> {
    koko_common::csv_dialect::resolve_dialect(
        path,
        options.delimiter,
        options.quote,
        options.escape,
        options.auto_detect,
        Some(arity),
    )
}

/// A record's raw text (fields rejoined with the effective delimiter) for
/// warning reports.
pub(super) fn record_text(record: &csv::StringRecord, options: &CsvOptions) -> String {
    let delimiter = options.delimiter.unwrap_or(b',') as char;
    record
        .iter()
        .collect::<Vec<_>>()
        .join(&delimiter.to_string())
}
