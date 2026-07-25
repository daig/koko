//! Streaming CSV, TSV, JSON, and JSON Lines protocols.

use crate::presentation::{PresentationError, StatementContext};
use crate::value_codec;
use koko::{
    CellValueRef, FailureKind, PlanNode, QueryResult, QueryResultKind, StatementFailure, Value,
};
use std::io::Write;

pub fn begin_json(writer: &mut impl Write) -> Result<(), PresentationError> {
    writer.write_all(br#"{"version":1,"results":["#)?;
    Ok(())
}

pub fn finish_json(
    writer: &mut impl Write,
    complete: bool,
    error: Option<(&StatementContext<'_>, &StatementFailure)>,
) -> Result<(), PresentationError> {
    writer.write_all(b"],\"complete\":")?;
    writer.write_all(if complete { b"true" } else { b"false" })?;
    if let Some((statement, failure)) = error {
        writer.write_all(b",\"error\":")?;
        write_error_object(writer, statement, failure)?;
    }
    writer.write_all(b"}\n")?;
    Ok(())
}

pub fn write_json_result(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    result: &QueryResult,
    first: &mut bool,
    timing: bool,
) -> Result<(), PresentationError> {
    if !*first {
        writer.write_all(b",")?;
    }
    *first = false;
    write_result_object(writer, statement, result, timing)?;
    Ok(())
}

pub fn write_jsonl_result(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    result: &QueryResult,
    timing: bool,
) -> Result<(), PresentationError> {
    if result.result_kind() == QueryResultKind::Status {
        return write_status_record(writer, statement, result, timing);
    }
    write_schema_record(writer, statement, result)?;
    for row in result.rows() {
        write_record_prefix(writer, statement, "row")?;
        writer.write_all(b",\"values\":[")?;
        for column in 0..row.len() {
            separator(writer, column)?;
            value_codec::write_cell(writer, row.cell(column)?, result.type_context())?;
        }
        writer.write_all(b"]}\n")?;
    }
    write_summary_record(writer, statement, result, timing)?;
    Ok(())
}

pub fn write_jsonl_error(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    failure: &StatementFailure,
) -> Result<(), PresentationError> {
    write_record_prefix(writer, statement, "error")?;
    writer.write_all(b",\"error\":")?;
    write_failure(writer, failure)?;
    write_source_fields(writer, statement)?;
    writer.write_all(b"}\n")?;
    Ok(())
}

pub fn write_delimited(
    writer: &mut impl Write,
    result: &QueryResult,
    delimiter: u8,
    header: bool,
    null_token: &str,
) -> Result<(), PresentationError> {
    if delimiter == b',' && !is_safe_null_token(null_token, delimiter) {
        return Err(PresentationError::InvalidNullToken(null_token.to_string()));
    }
    if header {
        for (index, column) in result.schema().iter().enumerate() {
            separator_byte(writer, index, delimiter)?;
            write_delimited_text(
                writer,
                column.name(),
                delimiter,
                null_token,
                delimiter == b'\t',
            )?;
        }
        writer.write_all(b"\n")?;
    }
    for row in result.rows() {
        for column in 0..row.len() {
            separator_byte(writer, column, delimiter)?;
            let cell = row.cell(column)?;
            match cell.value() {
                CellValueRef::Null => writer.write_all(null_token.as_bytes())?,
                CellValueRef::String(value) => {
                    write_delimited_text(writer, value, delimiter, null_token, delimiter == b'\t')?;
                }
                CellValueRef::Generic(Value::String(value)) => {
                    write_delimited_text(writer, value, delimiter, null_token, delimiter == b'\t')?;
                }
                _ => {
                    let value = crate::human::cell_text(cell, result.type_context(), "")?;
                    write_delimited_text(
                        writer,
                        &value,
                        delimiter,
                        null_token,
                        delimiter == b'\t',
                    )?;
                }
            }
        }
        writer.write_all(b"\n")?;
    }
    Ok(())
}

fn write_result_object(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    result: &QueryResult,
    timing: bool,
) -> Result<(), PresentationError> {
    writer.write_all(br#"{"statement":"#)?;
    write_usize(writer, statement.number)?;
    write_source_fields(writer, statement)?;
    writer.write_all(b",\"columns\":[")?;
    for (index, column) in result.schema().iter().enumerate() {
        separator(writer, index)?;
        writer.write_all(br#"{"name":"#)?;
        write_string(writer, column.name())?;
        writer.write_all(b",\"type\":")?;
        write_string(writer, &column.logical_type().to_string())?;
        writer.write_all(b"}")?;
    }
    writer.write_all(b"],\"rows\":[")?;
    for (row_index, row) in result.rows().enumerate() {
        separator(writer, row_index)?;
        writer.write_all(b"[")?;
        for column in 0..row.len() {
            separator(writer, column)?;
            value_codec::write_cell(writer, row.cell(column)?, result.type_context())?;
        }
        writer.write_all(b"]")?;
    }
    writer.write_all(b"],\"summary\":")?;
    write_summary_object(writer, result, timing)?;
    if let Some(status) = result.status_message() {
        writer.write_all(b",\"status\":")?;
        write_string(writer, status)?;
    }
    if let Some(plan) = result.plan() {
        writer.write_all(b",\"plan\":")?;
        write_plan(writer, plan.roots())?;
    }
    writer.write_all(b"}")?;
    Ok(())
}

fn write_schema_record(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    result: &QueryResult,
) -> Result<(), PresentationError> {
    write_record_prefix(writer, statement, "schema")?;
    writer.write_all(b",\"columns\":[")?;
    for (index, column) in result.schema().iter().enumerate() {
        separator(writer, index)?;
        writer.write_all(br#"{"name":"#)?;
        write_string(writer, column.name())?;
        writer.write_all(b",\"type\":")?;
        write_string(writer, &column.logical_type().to_string())?;
        writer.write_all(b"}")?;
    }
    writer.write_all(b"]")?;
    write_source_fields(writer, statement)?;
    writer.write_all(b"}\n")?;
    Ok(())
}

fn write_status_record(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    result: &QueryResult,
    timing: bool,
) -> Result<(), PresentationError> {
    write_record_prefix(writer, statement, "status")?;
    writer.write_all(b",\"message\":")?;
    write_string(writer, result.status_message().unwrap_or(""))?;
    writer.write_all(b",\"summary\":")?;
    write_summary_object(writer, result, timing)?;
    writer.write_all(b"}\n")?;
    Ok(())
}

fn write_record_prefix(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    record_type: &str,
) -> Result<(), PresentationError> {
    writer.write_all(br#"{"version":1,"result":"#)?;
    write_usize(writer, statement.result)?;
    writer.write_all(b",\"type\":")?;
    write_string(writer, record_type)?;
    writer.write_all(b",\"statement\":")?;
    write_usize(writer, statement.number)?;
    Ok(())
}

fn write_summary_record(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    result: &QueryResult,
    timing: bool,
) -> Result<(), PresentationError> {
    write_record_prefix(writer, statement, "summary")?;
    writer.write_all(b",\"summary\":")?;
    write_summary_object(writer, result, timing)?;
    if let Some(status) = result.status_message() {
        writer.write_all(b",\"status\":")?;
        write_string(writer, status)?;
    }
    if let Some(plan) = result.plan() {
        writer.write_all(b",\"plan\":")?;
        write_plan(writer, plan.roots())?;
    }
    writer.write_all(b"}\n")?;
    Ok(())
}

fn write_summary_object(
    writer: &mut impl Write,
    result: &QueryResult,
    timing: bool,
) -> Result<(), PresentationError> {
    writer.write_all(br#"{"rows":"#)?;
    write_usize(writer, result.num_rows())?;
    if timing {
        let summary = result.summary();
        writer.write_all(b",\"compiling_ms\":")?;
        write_f64(writer, summary.compiling_time_ms())?;
        writer.write_all(b",\"execution_ms\":")?;
        write_f64(writer, summary.execution_time_ms())?;
    }
    writer.write_all(b",\"warnings\":[")?;
    for (index, warning) in result.statement_diagnostics().warnings().iter().enumerate() {
        separator(writer, index)?;
        writer.write_all(br#"{"message":"#)?;
        write_string(writer, warning.message())?;
        writer.write_all(b"}")?;
    }
    writer.write_all(b"],\"total_warning_count\":")?;
    write_u64(writer, result.statement_diagnostics().total_warning_count())?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_error_object(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
    failure: &StatementFailure,
) -> Result<(), PresentationError> {
    writer.write_all(b"{")?;
    writer.write_all(br#""statement":"#)?;
    write_usize(writer, statement.number)?;
    writer.write_all(b",\"error\":")?;
    write_failure(writer, failure)?;
    write_source_fields(writer, statement)?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_failure(
    writer: &mut impl Write,
    failure: &StatementFailure,
) -> Result<(), PresentationError> {
    writer.write_all(br#"{"kind":"#)?;
    write_string(writer, failure_kind(failure))?;
    writer.write_all(b",\"message\":")?;
    let message = if failure.kind() == FailureKind::InternalPanic {
        "Internal error: query execution panicked.".to_string()
    } else {
        failure.error().to_string()
    };
    write_string(writer, &message)?;
    writer.write_all(b"}")?;
    Ok(())
}

fn failure_kind(failure: &StatementFailure) -> &'static str {
    match failure.kind() {
        FailureKind::Parser => "parser",
        FailureKind::Binder => "binder",
        FailureKind::Catalog => "catalog",
        FailureKind::Transaction => "transaction",
        FailureKind::Runtime => "runtime",
        FailureKind::ImportExport => "import_export",
        FailureKind::Memory => "memory",
        FailureKind::Interrupt
            if failure.interrupt_reason() == Some(koko::InterruptReason::Deadline) =>
        {
            "deadline"
        }
        FailureKind::Interrupt => "interrupt",
        FailureKind::Configuration => "configuration",
        FailureKind::Io => "io",
        FailureKind::InternalPanic => "internal",
    }
}

fn write_plan(writer: &mut impl Write, roots: &[PlanNode]) -> Result<(), PresentationError> {
    writer.write_all(b"[")?;
    for (index, node) in roots.iter().enumerate() {
        separator(writer, index)?;
        writer.write_all(br#"{"operator":"#)?;
        write_string(writer, node.operator())?;
        writer.write_all(b",\"details\":[")?;
        for (detail_index, (key, value)) in node.detail().iter().enumerate() {
            separator(writer, detail_index)?;
            writer.write_all(b"[")?;
            write_string(writer, key)?;
            writer.write_all(b",")?;
            write_string(writer, value)?;
            writer.write_all(b"]")?;
        }
        writer.write_all(b"],\"children\":")?;
        write_plan(writer, node.children())?;
        writer.write_all(b"}")?;
    }
    writer.write_all(b"]")?;
    Ok(())
}

fn write_source_fields(
    writer: &mut impl Write,
    statement: &StatementContext<'_>,
) -> Result<(), PresentationError> {
    if let Some(source) = statement.source {
        writer.write_all(b",\"source\":")?;
        write_string(writer, source)?;
    }
    if let Some(line) = statement.line {
        writer.write_all(b",\"line\":")?;
        write_u64(writer, line)?;
    }
    if let Some(column) = statement.column {
        writer.write_all(b",\"column\":")?;
        write_u64(writer, column)?;
    }
    Ok(())
}

fn write_delimited_text(
    writer: &mut impl Write,
    value: &str,
    delimiter: u8,
    null_token: &str,
    tsv: bool,
) -> Result<(), PresentationError> {
    if tsv {
        if value == null_token {
            writer.write_all(b"\\")?;
        }
        for byte in value.bytes() {
            match byte {
                b'\\' => writer.write_all(b"\\\\")?,
                b'\t' => writer.write_all(b"\\t")?,
                b'\n' => writer.write_all(b"\\n")?,
                b'\r' => writer.write_all(b"\\r")?,
                byte => writer.write_all(&[byte])?,
            }
        }
        return Ok(());
    }
    let quote = value.is_empty()
        || value == null_token
        || value
            .bytes()
            .any(|byte| byte == delimiter || matches!(byte, b'"' | b'\r' | b'\n'));
    if quote {
        writer.write_all(b"\"")?;
    }
    for byte in value.bytes() {
        if byte == b'"' {
            writer.write_all(b"\"\"")?;
        } else {
            writer.write_all(&[byte])?;
        }
    }
    if quote {
        writer.write_all(b"\"")?;
    }
    Ok(())
}

fn is_safe_null_token(value: &str, delimiter: u8) -> bool {
    !value.is_empty()
        && !value
            .bytes()
            .any(|byte| byte == delimiter || matches!(byte, b'"' | b'\r' | b'\n'))
}

fn separator(writer: &mut impl Write, index: usize) -> std::io::Result<()> {
    if index != 0 {
        writer.write_all(b",")?;
    }
    Ok(())
}

fn separator_byte(writer: &mut impl Write, index: usize, byte: u8) -> std::io::Result<()> {
    if index != 0 {
        writer.write_all(&[byte])?;
    }
    Ok(())
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<(), PresentationError> {
    serde_json::to_writer(writer, value)
        .map_err(|error| PresentationError::Io(std::io::Error::other(error)))
}

fn write_usize(writer: &mut impl Write, value: usize) -> std::io::Result<()> {
    write!(writer, "{value}")
}

fn write_u64(writer: &mut impl Write, value: u64) -> std::io::Result<()> {
    write!(writer, "{value}")
}

fn write_f64(writer: &mut impl Write, value: f64) -> std::io::Result<()> {
    if value.is_finite() {
        write!(writer, "{value}")
    } else {
        writer.write_all(b"null")
    }
}

pub fn is_row_producing(result: &QueryResult) -> bool {
    result.result_kind() == QueryResultKind::Rows
}
