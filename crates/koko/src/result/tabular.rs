//! Validated private tabular storage and borrowed result traversal.

use super::cell::{Cell, CellValue, FromValue};
use super::plan::{PlanPresentation, ResultTypeContext, set_plan_execution_time};
use super::{Column, QuerySummary, ResultKind};
use crate::diagnostics::Diagnostics;
use koko_common::{
    DataChunk, Error, IntKind, LogicalType, MemoryTracker, Result, VECTOR_CAPACITY, Value,
};
use std::time::Duration;

struct TabularData {
    columns: Vec<Column>,
    batches: Vec<DataChunk>,
    batch_offsets: Vec<usize>,
    len: usize,
}

impl TabularData {
    fn new(columns: Vec<Column>, mut batches: Vec<DataChunk>) -> Result<Self> {
        batches.retain(|batch| !batch.is_empty());
        let mut batch_offsets = Vec::with_capacity(batches.len());
        let mut len = 0usize;
        for (batch_index, batch) in batches.iter().enumerate() {
            if batch.columns.len() != columns.len() {
                return Err(Error::runtime(format!(
                    "internal result batch {} has {} columns, expected {}",
                    batch_index + 1,
                    batch.columns.len(),
                    columns.len()
                )));
            }
            if batch.size() > VECTOR_CAPACITY {
                return Err(Error::runtime(format!(
                    "internal result batch {} has {} rows, exceeds vector capacity {}",
                    batch_index + 1,
                    batch.size(),
                    VECTOR_CAPACITY
                )));
            }
            for position in batch.sel.iter() {
                if position >= VECTOR_CAPACITY {
                    return Err(Error::runtime(format!(
                        "internal result batch {} selects position {} beyond vector capacity {}",
                        batch_index + 1,
                        position,
                        VECTOR_CAPACITY
                    )));
                }
                if batch.multiplicity(position) != 1 {
                    return Err(Error::runtime(format!(
                        "internal result batch {} retains factorized rows",
                        batch_index + 1
                    )));
                }
            }
            for (column_index, (vector, column)) in batch.columns.iter().zip(&columns).enumerate() {
                if vector.logical_type != column.logical_type {
                    return Err(Error::runtime(format!(
                        "internal result batch {} column {} has type {}, expected {}",
                        batch_index + 1,
                        column_index,
                        vector.logical_type,
                        column.logical_type
                    )));
                }
            }
            batch_offsets.push(len);
            len = len
                .checked_add(batch.size())
                .ok_or_else(|| Error::runtime("internal result row count overflow"))?;
        }
        Ok(Self {
            columns,
            batches,
            batch_offsets,
            len,
        })
    }

    fn allocated_bytes(&self) -> u64 {
        (self.columns.capacity() * std::mem::size_of::<Column>()) as u64
            + self
                .columns
                .iter()
                .map(|column| column.name.capacity() as u64)
                .sum::<u64>()
            + (self.batches.capacity() * std::mem::size_of::<DataChunk>()) as u64
            + self
                .batches
                .iter()
                .map(DataChunk::allocated_bytes)
                .sum::<u64>()
            + (self.batch_offsets.capacity() * std::mem::size_of::<usize>()) as u64
    }
}

enum Presentation {
    Rows(TabularData),
    Status {
        message: Option<String>,
    },
    Explain(PlanPresentation),
    Profile {
        rows: TabularData,
        plan: PlanPresentation,
    },
}

/// An eagerly materialized query result backed by private typed vector batches.
pub struct QueryResult {
    presentation: Presentation,
    diagnostics: Diagnostics,
    type_context: ResultTypeContext,
    summary: QuerySummary,
    memory: Option<koko_common::MemoryReservation>,
}

impl Default for QueryResult {
    fn default() -> Self {
        Self::status(None)
    }
}

impl std::fmt::Debug for QueryResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueryResult")
            .field("kind", &self.kind())
            .field("len", &self.len())
            .field("width", &self.width())
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

mod column_index {
    use super::{Column, Error, Result, result_column_bounds};

    pub trait Sealed {
        fn resolve(self, columns: &[Column]) -> Result<usize>;
    }

    impl Sealed for usize {
        fn resolve(self, columns: &[Column]) -> Result<usize> {
            if self >= columns.len() {
                return Err(result_column_bounds(self, columns.len()));
            }
            Ok(self)
        }
    }

    impl Sealed for &str {
        fn resolve(self, columns: &[Column]) -> Result<usize> {
            let mut matches = columns
                .iter()
                .enumerate()
                .filter_map(|(index, column)| (column.name == self).then_some(index));
            let index = matches
                .next()
                .ok_or_else(|| Error::runtime(format!("result column `{self}` does not exist")))?;
            if matches.next().is_some() {
                return Err(Error::runtime(format!(
                    "result column name `{self}` is ambiguous"
                )));
            }
            Ok(index)
        }
    }
}

/// A sealed result-column selector, implemented for `usize` and `&str`.
pub trait ColumnIndex: column_index::Sealed {}

impl ColumnIndex for usize {}
impl ColumnIndex for &str {}

impl QueryResult {
    pub(crate) fn from_exec(result: koko_processor::ExecResult) -> Result<Self> {
        if result.column_names.len() != result.column_types.len() {
            return Err(Error::runtime(format!(
                "internal result has {} column names but {} column types",
                result.column_names.len(),
                result.column_types.len()
            )));
        }
        let columns = result
            .column_names
            .into_iter()
            .zip(result.column_types)
            .map(|(name, logical_type)| Column::new(name, logical_type))
            .collect();
        Self::from_tabular(TabularData::new(columns, result.batches)?)
    }

    pub(crate) fn from_batches(columns: Vec<Column>, batches: Vec<DataChunk>) -> Result<Self> {
        Self::from_tabular(TabularData::new(columns, batches)?)
    }

    pub(crate) fn from_typed_rows(
        column_names: Vec<String>,
        column_types: Vec<LogicalType>,
        rows: Vec<Vec<Value>>,
    ) -> Result<Self> {
        if column_names.len() != column_types.len() {
            return Err(Error::conversion(format!(
                "tooling result has {} names but {} types",
                column_names.len(),
                column_types.len()
            )));
        }
        let columns = column_names
            .into_iter()
            .zip(column_types)
            .map(|(name, logical_type)| Column::new(name, logical_type))
            .collect();
        Self::from_rows(columns, rows)
    }

    pub(crate) fn from_rows(columns: Vec<Column>, rows: Vec<Vec<Value>>) -> Result<Self> {
        if let Some((index, row)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.len() != columns.len())
        {
            return Err(Error::conversion(format!(
                "tooling result row {} has {} values but {} columns",
                index + 1,
                row.len(),
                columns.len()
            )));
        }
        for (row_index, row) in rows.iter().enumerate() {
            for (column_index, (value, column)) in row.iter().zip(&columns).enumerate() {
                let value_type = value.logical_type();
                let compatible = value.is_null()
                    || column.logical_type == LogicalType::Any
                    || value_type == column.logical_type
                    || match (&column.logical_type, &value_type, value) {
                        (LogicalType::Serial, LogicalType::Int(_), _) => true,
                        (
                            LogicalType::TimestampNs
                            | LogicalType::TimestampMs
                            | LogicalType::TimestampSec,
                            LogicalType::Timestamp,
                            _,
                        ) => true,
                        (
                            LogicalType::Array(expected, len),
                            LogicalType::List(actual),
                            Value::List(values),
                        ) => {
                            expected == actual
                                && u64::try_from(values.len())
                                    .is_ok_and(|actual_len| actual_len == *len)
                        }
                        _ => false,
                    };
                if !compatible {
                    return Err(Error::conversion(format!(
                        "tooling result row {} column {} has type {}, expected {}",
                        row_index + 1,
                        column_index + 1,
                        value_type,
                        column.logical_type
                    )));
                }
            }
        }
        let column_types: Vec<_> = columns
            .iter()
            .map(|column| column.logical_type.clone())
            .collect();
        let mut batches = Vec::new();
        let mut batch = DataChunk::new(&column_types);
        let mut len = 0;
        for row in rows {
            for (vector, value) in batch.columns.iter_mut().zip(row) {
                vector.set_value_owned(len, value);
            }
            len += 1;
            if len == VECTOR_CAPACITY {
                batch.set_flat(len);
                batches.push(batch);
                batch = DataChunk::new(&column_types);
                len = 0;
            }
        }
        if len != 0 {
            batch.set_flat(len);
            batches.push(batch);
        }
        Self::from_batches(columns, batches)
    }

    fn from_tabular(rows: TabularData) -> Result<Self> {
        Ok(Self {
            presentation: Presentation::Rows(rows),
            diagnostics: Diagnostics::default(),
            type_context: ResultTypeContext::default(),
            summary: QuerySummary::default(),
            memory: None,
        })
    }

    fn status(message: Option<String>) -> Self {
        Self {
            presentation: Presentation::Status { message },
            diagnostics: Diagnostics::default(),
            type_context: ResultTypeContext::default(),
            summary: QuerySummary::default(),
            memory: None,
        }
    }

    pub(crate) fn message(message: String) -> Self {
        Self::status(Some(message))
    }

    pub(crate) fn explain(type_context: ResultTypeContext, plan: PlanPresentation) -> Self {
        Self {
            presentation: Presentation::Explain(plan),
            diagnostics: Diagnostics::default(),
            type_context,
            summary: QuerySummary::default(),
            memory: None,
        }
    }

    pub(crate) fn into_profile(
        mut self,
        type_context: ResultTypeContext,
        plan: PlanPresentation,
    ) -> Result<Self> {
        let presentation = std::mem::replace(
            &mut self.presentation,
            Presentation::Status { message: None },
        );
        let Presentation::Rows(rows) = presentation else {
            return Err(Error::runtime(
                "internal PROFILE result does not contain row storage",
            ));
        };
        self.presentation = Presentation::Profile { rows, plan };
        self.type_context = type_context;
        Ok(self)
    }

    pub const fn kind(&self) -> ResultKind {
        match self.presentation {
            Presentation::Rows(_) => ResultKind::Rows,
            Presentation::Status { .. } => ResultKind::Status,
            Presentation::Explain(_) => ResultKind::Explain,
            Presentation::Profile { .. } => ResultKind::Profile,
        }
    }

    pub const fn diagnostics(&self) -> &Diagnostics {
        &self.diagnostics
    }

    pub const fn type_context(&self) -> &ResultTypeContext {
        &self.type_context
    }

    pub fn status_message(&self) -> Option<&str> {
        match &self.presentation {
            Presentation::Status { message } => message.as_deref(),
            _ => None,
        }
    }

    pub const fn plan(&self) -> Option<&PlanPresentation> {
        match &self.presentation {
            Presentation::Explain(plan) | Presentation::Profile { plan, .. } => Some(plan),
            Presentation::Rows(_) | Presentation::Status { .. } => None,
        }
    }

    pub const fn summary(&self) -> &QuerySummary {
        &self.summary
    }

    pub fn len(&self) -> usize {
        self.tabular().map_or(0, |rows| rows.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn width(&self) -> usize {
        self.tabular().map_or(0, |rows| rows.columns.len())
    }

    pub fn columns(&self) -> &[Column] {
        self.tabular().map_or(&[], |rows| rows.columns.as_slice())
    }

    pub fn row(&self, index: usize) -> Option<Row<'_>> {
        (index < self.len()).then_some(Row {
            result: self,
            index,
        })
    }

    pub fn rows(&self) -> Rows<'_> {
        Rows {
            result: self,
            range: 0..self.len(),
        }
    }

    pub fn column(&self, index: impl ColumnIndex) -> Result<ColumnView<'_>> {
        let index = column_index::Sealed::resolve(index, self.columns())?;
        Ok(ColumnView {
            result: self,
            index,
        })
    }

    pub fn typed_column<T: FromValue>(
        &self,
        index: impl ColumnIndex,
    ) -> Result<TypedColumnView<'_, T>> {
        self.column(index)?.typed()
    }

    pub fn cell(&self, row: usize, index: impl ColumnIndex) -> Result<Cell<'_>> {
        let column = column_index::Sealed::resolve(index, self.columns())?;
        self.cell_at(row, column)
    }

    pub fn value(&self, row: usize, index: impl ColumnIndex) -> Result<Value> {
        let column = column_index::Sealed::resolve(index, self.columns())?;
        self.value_at(row, column)
    }

    #[cfg(test)]
    pub(crate) fn rendered_rows(&self) -> Vec<String> {
        self.rows()
            .map(|row| {
                (0..row.len())
                    .map(|column| {
                        row.value(column)
                            .expect("validated result coordinate")
                            .to_result_string()
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    }

    fn tabular(&self) -> Option<&TabularData> {
        match &self.presentation {
            Presentation::Rows(rows) | Presentation::Profile { rows, .. } => Some(rows),
            Presentation::Status { .. } | Presentation::Explain(_) => None,
        }
    }

    fn vector_position(
        &self,
        row: usize,
        column: usize,
    ) -> Result<(&koko_common::ValueVector, usize)> {
        let tabular = self
            .tabular()
            .ok_or_else(|| Error::runtime("result does not contain tabular rows"))?;
        if row >= tabular.len {
            return Err(Error::runtime(format!(
                "result row index {row} is out of bounds for {} rows",
                tabular.len
            )));
        }
        let batch_index = tabular
            .batch_offsets
            .partition_point(|offset| *offset <= row)
            - 1;
        let batch = &tabular.batches[batch_index];
        let logical_position = row - tabular.batch_offsets[batch_index];
        let position = batch
            .sel
            .iter()
            .nth(logical_position)
            .expect("validated batch offsets match selections");
        Ok((&batch.columns[column], position))
    }

    fn cell_at(&self, row: usize, column: usize) -> Result<Cell<'_>> {
        let (vector, position) = self.vector_position(row, column)?;
        let logical_type = &self.columns()[column].logical_type;
        let value = if vector.nulls.is_null(position) {
            CellValue::Null
        } else {
            match &vector.data {
                koko_common::ColumnData::Bool(values) => CellValue::Bool(values[position]),
                koko_common::ColumnData::Int64(values) => CellValue::Int {
                    value: values[position] as i128,
                    kind: IntKind::I64,
                },
                koko_common::ColumnData::Int128(values) => CellValue::Int {
                    value: values[position],
                    kind: match logical_type {
                        LogicalType::Int(kind) => *kind,
                        _ => IntKind::I128,
                    },
                },
                koko_common::ColumnData::UInt128(values) => CellValue::UInt128(values[position]),
                koko_common::ColumnData::Double(values) => CellValue::Double(values[position]),
                koko_common::ColumnData::Float(values) => CellValue::Float(values[position]),
                koko_common::ColumnData::Date(values) => CellValue::Date(values[position]),
                koko_common::ColumnData::Timestamp(values) => {
                    if *logical_type == LogicalType::TimestampTz {
                        CellValue::TimestampTz(values[position])
                    } else {
                        CellValue::Timestamp(values[position])
                    }
                }
                koko_common::ColumnData::Interval(values) => CellValue::Interval(values[position]),
                koko_common::ColumnData::Uuid(values) => CellValue::Uuid(values[position]),
                koko_common::ColumnData::Decimal(values) => {
                    let (precision, scale) = match logical_type {
                        LogicalType::Decimal(precision, scale) => (*precision, *scale),
                        _ => (38, 0),
                    };
                    CellValue::Decimal {
                        value: values[position],
                        precision,
                        scale,
                    }
                }
                koko_common::ColumnData::Str(values) => CellValue::String(&values[position]),
                koko_common::ColumnData::InternalId(values) => {
                    CellValue::InternalId(values[position])
                }
                koko_common::ColumnData::Generic(values) => CellValue::Generic(&values[position]),
            }
        };
        Ok(Cell {
            logical_type,
            value,
        })
    }

    fn value_at(&self, row: usize, column: usize) -> Result<Value> {
        let (vector, position) = self.vector_position(row, column)?;
        Ok(vector.get_value(position))
    }

    pub(crate) fn batches(&self) -> &[DataChunk] {
        self.tabular().map_or(&[], |rows| rows.batches.as_slice())
    }

    pub(crate) fn set_type_context(&mut self, type_context: ResultTypeContext) {
        self.type_context = type_context;
    }

    pub(crate) fn set_diagnostics(&mut self, diagnostics: Diagnostics) {
        self.diagnostics = diagnostics;
    }

    pub(crate) fn set_summary(&mut self, compilation_time: Duration, execution_time: Duration) {
        self.summary = QuerySummary {
            compilation_time,
            execution_time,
        };
    }

    pub(crate) fn set_execution_time(&mut self, execution_time: Duration) {
        self.summary.execution_time = execution_time;
    }

    pub(crate) fn attach_plan_execution_time(&mut self) {
        match &mut self.presentation {
            Presentation::Explain(plan) | Presentation::Profile { plan, .. } => {
                set_plan_execution_time(plan, self.summary.execution_time());
            }
            Presentation::Rows(_) | Presentation::Status { .. } => {}
        }
    }

    fn allocated_bytes(&self) -> u64 {
        let presentation = match &self.presentation {
            Presentation::Rows(rows) => rows.allocated_bytes(),
            Presentation::Status { message } => message
                .as_ref()
                .map_or(0, |message| message.capacity() as u64),
            Presentation::Explain(plan) => plan.allocated_bytes(),
            Presentation::Profile { rows, plan } => rows.allocated_bytes() + plan.allocated_bytes(),
        };
        presentation + self.diagnostics.allocated_bytes() + self.type_context.allocated_bytes()
    }

    pub(crate) fn track_memory(&mut self, tracker: &MemoryTracker) -> Result<()> {
        self.memory = Some(tracker.try_reserve(self.allocated_bytes())?);
        Ok(())
    }
}
fn result_column_bounds(index: usize, width: usize) -> Error {
    Error::runtime(format!(
        "result column index {index} is out of bounds for {width} columns"
    ))
}

/// A bounds-checked borrowed view of one result column.
#[derive(Clone, Copy)]
pub struct ColumnView<'a> {
    result: &'a QueryResult,
    index: usize,
}

impl<'a> ColumnView<'a> {
    pub fn column(&self) -> &Column {
        &self.result.columns()[self.index]
    }

    pub fn len(&self) -> usize {
        self.result.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn cell(&self, row: usize) -> Result<Cell<'a>> {
        self.result.cell_at(row, self.index)
    }

    pub fn value(&self, row: usize) -> Result<Value> {
        self.result.value_at(row, self.index)
    }

    pub fn iter(&self) -> Cells<'a> {
        Cells {
            column: *self,
            range: 0..self.len(),
        }
    }

    fn typed<T: FromValue>(self) -> Result<TypedColumnView<'a, T>> {
        if !T::accepts(self.column().logical_type()) {
            return Err(Error::conversion(format!(
                "cannot view result column `{}` of type {} as {}",
                self.column().name(),
                self.column().logical_type(),
                T::type_name()
            )));
        }
        Ok(TypedColumnView {
            column: self,
            marker: std::marker::PhantomData,
        })
    }
}

/// Exact-size borrowed cell iterator for one column.
pub struct Cells<'a> {
    column: ColumnView<'a>,
    range: std::ops::Range<usize>,
}

impl<'a> Iterator for Cells<'a> {
    type Item = Cell<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.range
            .next()
            .map(|row| self.column.cell(row).expect("validated result coordinate"))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.range.size_hint()
    }
}

impl ExactSizeIterator for Cells<'_> {}

/// A schema-checked typed view of one result column.
#[derive(Clone, Copy)]
pub struct TypedColumnView<'a, T> {
    column: ColumnView<'a>,
    marker: std::marker::PhantomData<T>,
}

impl<'a, T: FromValue> TypedColumnView<'a, T> {
    pub fn column(&self) -> &Column {
        self.column.column()
    }

    pub fn len(&self) -> usize {
        self.column.len()
    }

    pub fn is_empty(&self) -> bool {
        self.column.is_empty()
    }

    pub fn get(&self, row: usize) -> Result<T> {
        let value = self.column.value(row)?;
        T::from_value(&value)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Result<T>> + '_ {
        (0..self.len()).map(|row| self.get(row))
    }
}

/// Exact-size iterator over borrowed result rows.
pub struct Rows<'a> {
    result: &'a QueryResult,
    range: std::ops::Range<usize>,
}

impl<'a> Iterator for Rows<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.range.next().map(|index| Row {
            result: self.result,
            index,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.range.size_hint()
    }
}

impl ExactSizeIterator for Rows<'_> {}

impl<'a> IntoIterator for &'a QueryResult {
    type Item = Row<'a>;
    type IntoIter = Rows<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows()
    }
}

impl std::fmt::Display for QueryResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(message) = self.status_message() {
            formatter.write_str(message)?;
            return formatter.write_str("\n");
        }
        for row in self {
            for column in 0..row.len() {
                if column != 0 {
                    formatter.write_str("|")?;
                }
                formatter.write_str(
                    &row.value(column)
                        .map_err(|_| std::fmt::Error)?
                        .to_result_string(),
                )?;
            }
            formatter.write_str("\n")?;
        }
        Ok(())
    }
}

/// A borrowed cursor over one result row.
#[derive(Clone, Copy)]
pub struct Row<'a> {
    result: &'a QueryResult,
    index: usize,
}

impl Row<'_> {
    pub fn len(&self) -> usize {
        self.result.width()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn cell(&self, index: impl ColumnIndex) -> Result<Cell<'_>> {
        self.result.cell(self.index, index)
    }

    pub fn value(&self, index: impl ColumnIndex) -> Result<Value> {
        self.result.value(self.index, index)
    }

    pub fn get<T: FromValue>(&self, index: impl ColumnIndex) -> Result<T> {
        let value = self.value(index)?;
        T::from_value(&value)
    }
}
