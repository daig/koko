//! Human table, box, Markdown, line, and structural-plan renderers.

use crate::bootstrap::Format;
use crate::presentation::PresentationError;
use koko::result::{Cell, CellValue, PlanNode, ResultTypeContext};
use koko::value::{format_date, format_decimal, format_interval, format_timestamp, format_uuid};
use koko::{LogicalType, QueryResult};
use std::io::Write;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

#[derive(Debug, Clone)]
pub struct HumanOptions {
    pub format: Format,
    pub row_limit: Option<usize>,
    pub max_width: Option<usize>,
    pub null_display: String,
}

pub fn write_result(
    writer: &mut impl Write,
    result: &QueryResult,
    options: &HumanOptions,
) -> Result<usize, PresentationError> {
    if let Some(plan) = result.plan() {
        return write_plan(writer, plan.roots(), 0);
    }
    match options.format {
        Format::Markdown => write_markdown(writer, result, options),
        Format::Line => write_line(writer, result, options),
        Format::Table | Format::Box => write_table(writer, result, options),
        _ => Ok(0),
    }
}

pub fn cell_text(
    cell: Cell<'_>,
    _context: &ResultTypeContext,
    null_display: &str,
) -> Result<String, PresentationError> {
    let value = match cell.value() {
        CellValue::Null => null_display.to_string(),
        CellValue::Bool(value) => value.to_string(),
        CellValue::Int { value, .. } => value.to_string(),
        CellValue::UInt128(value) => value.to_string(),
        CellValue::Decimal { value, scale, .. } => format_decimal(value, scale),
        CellValue::Double(value) => format!("{value:.6}"),
        CellValue::Float(value) => format!("{value:.6}"),
        CellValue::String("") => "''".to_string(),
        CellValue::String(value) => escape_controls(value),
        CellValue::Date(value) => format_date(value),
        CellValue::Timestamp(value) => format_timestamp(value),
        CellValue::TimestampTz(value) => format!("{}+00", format_timestamp(value)),
        CellValue::Interval(value) => format_interval(&value),
        CellValue::Uuid(value) => format_uuid(value),
        CellValue::InternalId(value) => value.to_string(),
        CellValue::Generic(value) => escape_controls(&value.to_result_string()),
        _ => escape_controls(&cell.to_owned().to_result_string()),
    };
    Ok(value)
}

fn write_table(
    writer: &mut impl Write,
    result: &QueryResult,
    options: &HumanOptions,
) -> Result<usize, PresentationError> {
    let column_count = result.columns().len();
    let total_rows = result.len();
    if column_count == 0 {
        return Ok(0);
    }
    let displayed_rows = displayed_count(total_rows, options.row_limit);
    let available = options.max_width.unwrap_or(usize::MAX);
    let mut columns: Vec<Option<usize>> = (0..column_count).map(Some).collect();
    let mut widths = measure_widths(result, options, &columns)?;
    while table_width(&widths) > available
        && columns.iter().filter(|column| column.is_some()).count() > 2
    {
        let midpoint = columns.len() / 2;
        let remove_at = columns
            .iter()
            .enumerate()
            .filter(|(index, column)| {
                *index != 0 && *index + 1 != columns.len() && column.is_some()
            })
            .min_by_key(|(index, _)| index.abs_diff(midpoint))
            .map(|(index, _)| index)
            .expect("more than two visible columns includes a middle column");
        columns.remove(remove_at);
        widths.remove(remove_at);
        if !columns.contains(&None) {
            columns.insert(remove_at, None);
            widths.insert(remove_at, 1);
        }
    }
    while table_width(&widths) > available {
        let Some((index, _)) = widths
            .iter()
            .enumerate()
            .filter(|(index, width)| columns[*index].is_some() && **width > 3)
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[index] -= 1;
    }

    let box_style = options.format == Format::Box;
    border(
        writer,
        &widths,
        if box_style {
            ('┌', '┬', '┐')
        } else {
            ('+', '+', '+')
        },
    )?;
    write_cells(
        writer,
        &columns,
        &widths,
        |column| result.columns()[column].name().to_string(),
        |_| false,
        box_style,
    )?;
    write_cells(
        writer,
        &columns,
        &widths,
        |column| result.columns()[column].logical_type().to_string(),
        |_| false,
        box_style,
    )?;
    border(
        writer,
        &widths,
        if box_style {
            ('├', '┼', '┤')
        } else {
            ('+', '+', '+')
        },
    )?;

    for (row_index, row) in result.rows().enumerate() {
        if row_is_omission_start(row_index, total_rows, options.row_limit) {
            write_cells(
                writer,
                &columns,
                &widths,
                |_| "…".to_string(),
                |_| false,
                box_style,
            )?;
        }
        if !display_row(row_index, total_rows, options.row_limit) {
            continue;
        }
        write_cells(
            writer,
            &columns,
            &widths,
            |column| {
                cell_text(
                    row.cell(column).expect("measured query column"),
                    result.type_context(),
                    &options.null_display,
                )
                .unwrap_or_else(|error| format!("<render error: {error}>"))
            },
            |column| is_numeric(result.columns()[column].logical_type()),
            box_style,
        )?;
    }
    border(
        writer,
        &widths,
        if box_style {
            ('└', '┴', '┘')
        } else {
            ('+', '+', '+')
        },
    )?;
    Ok(displayed_rows)
}

fn write_markdown(
    writer: &mut impl Write,
    result: &QueryResult,
    options: &HumanOptions,
) -> Result<usize, PresentationError> {
    writer.write_all(b"|")?;
    for column in result.columns() {
        write!(writer, " {} |", markdown_escape(column.name()))?;
    }
    writer.write_all(b"\n|")?;
    for _ in result.columns() {
        writer.write_all(b" --- |")?;
    }
    writer.write_all(b"\n")?;
    let total_rows = result.len();
    for (row_index, row) in result.rows().enumerate() {
        if !display_row(row_index, total_rows, options.row_limit) {
            continue;
        }
        writer.write_all(b"|")?;
        for column in 0..row.len() {
            let value = cell_text(
                row.cell(column)?,
                result.type_context(),
                &options.null_display,
            )?;
            write!(writer, " {} |", markdown_escape(&value))?;
        }
        writer.write_all(b"\n")?;
    }
    Ok(displayed_count(total_rows, options.row_limit))
}

fn write_line(
    writer: &mut impl Write,
    result: &QueryResult,
    options: &HumanOptions,
) -> Result<usize, PresentationError> {
    let total_rows = result.len();
    for (row_index, row) in result.rows().enumerate() {
        if !display_row(row_index, total_rows, options.row_limit) {
            continue;
        }
        for column in 0..row.len() {
            let value = cell_text(
                row.cell(column)?,
                result.type_context(),
                &options.null_display,
            )?;
            writeln!(writer, "{} = {value}", result.columns()[column].name())?;
        }
    }
    Ok(displayed_count(total_rows, options.row_limit))
}

fn write_plan(
    writer: &mut impl Write,
    nodes: &[PlanNode],
    depth: usize,
) -> Result<usize, PresentationError> {
    let mut count = 0;
    for node in nodes {
        for _ in 0..depth {
            writer.write_all(b"  ")?;
        }
        if depth != 0 {
            writer.write_all("└─ ".as_bytes())?;
        }
        writer.write_all(node.operator().as_bytes())?;
        if !node.detail().is_empty() {
            writer.write_all(b" [")?;
            for (index, (key, value)) in node.detail().iter().enumerate() {
                if index != 0 {
                    writer.write_all(b", ")?;
                }
                write!(
                    writer,
                    "{}={}",
                    escape_controls(key),
                    escape_controls(value)
                )?;
            }
            writer.write_all(b"]")?;
        }
        writer.write_all(b"\n")?;
        count += 1 + write_plan(writer, node.children(), depth + 1)?;
    }
    Ok(count)
}

fn measure_widths(
    result: &QueryResult,
    options: &HumanOptions,
    columns: &[Option<usize>],
) -> Result<Vec<usize>, PresentationError> {
    let total_rows = result.len();
    let mut widths = columns
        .iter()
        .map(|column| {
            column.map_or(1, |column| {
                result.columns()[column]
                    .name()
                    .width()
                    .max(result.columns()[column].logical_type().to_string().width())
                    .max(1)
            })
        })
        .collect::<Vec<_>>();
    for (row_index, row) in result.rows().enumerate() {
        if !display_row(row_index, total_rows, options.row_limit) {
            continue;
        }
        for (position, column) in columns.iter().enumerate() {
            let Some(column) = column else { continue };
            let value = cell_text(
                row.cell(*column)?,
                result.type_context(),
                &options.null_display,
            )?;
            widths[position] = widths[position].max(value.width().min(40));
        }
    }
    Ok(widths)
}

fn write_cells(
    writer: &mut impl Write,
    columns: &[Option<usize>],
    widths: &[usize],
    mut value: impl FnMut(usize) -> String,
    mut right_align: impl FnMut(usize) -> bool,
    box_style: bool,
) -> Result<(), PresentationError> {
    writer.write_all(if box_style { "│".as_bytes() } else { b"|" })?;
    for (position, column) in columns.iter().enumerate() {
        writer.write_all(b" ")?;
        let text = column.map_or_else(|| "…".to_string(), &mut value);
        let text = truncate(&text, widths[position]);
        let padding = widths[position].saturating_sub(text.width());
        if column.is_some_and(&mut right_align) {
            write_spaces(writer, padding)?;
            writer.write_all(text.as_bytes())?;
        } else {
            writer.write_all(text.as_bytes())?;
            write_spaces(writer, padding)?;
        }
        writer.write_all(b" ")?;
        writer.write_all(if box_style { "│".as_bytes() } else { b"|" })?;
    }
    writer.write_all(b"\n")?;
    Ok(())
}

fn write_spaces(writer: &mut impl Write, count: usize) -> std::io::Result<()> {
    for _ in 0..count {
        writer.write_all(b" ")?;
    }
    Ok(())
}

fn border(
    writer: &mut impl Write,
    widths: &[usize],
    chars: (char, char, char),
) -> Result<(), PresentationError> {
    write!(writer, "{}", chars.0)?;
    for (index, width) in widths.iter().enumerate() {
        for _ in 0..(*width + 2) {
            writer.write_all(if chars.0 == '+' {
                b"-"
            } else {
                "─".as_bytes()
            })?;
        }
        write!(
            writer,
            "{}",
            if index + 1 == widths.len() {
                chars.2
            } else {
                chars.1
            }
        )?;
    }
    writer.write_all(b"\n")?;
    Ok(())
}

fn table_width(widths: &[usize]) -> usize {
    widths.iter().sum::<usize>() + widths.len() * 3 + 1
}

fn truncate(value: &str, width: usize) -> String {
    if value.width() <= width {
        return value.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut output = String::new();
    let mut output_width = 0;
    for grapheme in value.graphemes(true) {
        let grapheme_width = grapheme.width();
        if output_width + grapheme_width + 1 > width {
            break;
        }
        output.push_str(grapheme);
        output_width += grapheme_width;
    }
    output.push('…');
    output
}

fn escape_controls(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                std::fmt::Write::write_fmt(
                    &mut output,
                    format_args!("\\u{{{:x}}}", character as u32),
                )
                .expect("writing to a String cannot fail");
            }
            character => output.push(character),
        }
    }
    output
}

fn markdown_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

fn displayed_count(total: usize, limit: Option<usize>) -> usize {
    limit.map_or(total, |limit| total.min(limit))
}

fn display_row(index: usize, total: usize, limit: Option<usize>) -> bool {
    let Some(limit) = limit else { return true };
    if total <= limit {
        return true;
    }
    let head = limit.div_ceil(2);
    index < head || index >= total - (limit - head)
}

fn row_is_omission_start(index: usize, total: usize, limit: Option<usize>) -> bool {
    let Some(limit) = limit else { return false };
    total > limit && index == limit.div_ceil(2)
}

fn is_numeric(logical_type: &LogicalType) -> bool {
    matches!(
        logical_type,
        LogicalType::Int(_)
            | LogicalType::Serial
            | LogicalType::UInt128
            | LogicalType::Decimal(_, _)
            | LogicalType::Double
            | LogicalType::Float
    )
}
