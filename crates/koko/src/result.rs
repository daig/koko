//! Materialized query results, typed views, diagnostics, plans, and structured outcomes.

use crate::tooling::{PropertyDescriptor, SessionSnapshot, SyntaxDiagnostic, property_descriptor};
use koko_common::{
    DataChunk, Error, IntKind, InternalId, Interval, LogicalType, MemoryTracker, Result,
    VECTOR_CAPACITY, Value,
};
use std::time::Duration;

/// One warning retained for the statement that produced an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementWarning {
    query_id: u64,
    message: String,
    file_path: String,
    line_number: u64,
    skipped_line_or_record: String,
}

impl StatementWarning {
    pub const fn query_id(&self) -> u64 {
        self.query_id
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn file_path(&self) -> &str {
        &self.file_path
    }

    pub const fn line_number(&self) -> u64 {
        self.line_number
    }

    pub fn skipped_line_or_record(&self) -> &str {
        &self.skipped_line_or_record
    }
}

/// Statement-local diagnostics retained independently of warning history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatementDiagnostics {
    warnings: Vec<StatementWarning>,
    total_warning_count: u64,
}

impl StatementDiagnostics {
    pub fn warnings(&self) -> &[StatementWarning] {
        &self.warnings
    }

    pub const fn total_warning_count(&self) -> u64 {
        self.total_warning_count
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        (self.warnings.capacity() * std::mem::size_of::<StatementWarning>()) as u64
            + self
                .warnings
                .iter()
                .map(|warning| {
                    warning.message.capacity()
                        + warning.file_path.capacity()
                        + warning.skipped_line_or_record.capacity()
                })
                .sum::<usize>() as u64
    }
}

/// One pinned graph value type used by machine encoders after catalog changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphValueType {
    table_identity: u64,
    name: String,
    relationship: bool,
    properties: Vec<PropertyDescriptor>,
}

impl GraphValueType {
    pub const fn table_identity(&self) -> u64 {
        self.table_identity
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn is_relationship(&self) -> bool {
        self.relationship
    }

    pub fn properties(&self) -> &[PropertyDescriptor] {
        &self.properties
    }
}

/// Immutable type/catalog context captured with a materialized result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultTypeContext {
    catalog_revision: u64,
    graph_values: Vec<GraphValueType>,
}

impl ResultTypeContext {
    pub const fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }

    pub fn graph_values(&self) -> &[GraphValueType] {
        &self.graph_values
    }

    pub fn graph_value(&self, table_identity: u64) -> Option<&GraphValueType> {
        self.graph_values
            .iter()
            .find(|value| value.table_identity == table_identity)
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        (self.graph_values.capacity() * std::mem::size_of::<GraphValueType>()) as u64
            + self
                .graph_values
                .iter()
                .map(|value| {
                    value.name.capacity()
                        + value.properties.capacity() * std::mem::size_of::<PropertyDescriptor>()
                        + value
                            .properties
                            .iter()
                            .map(PropertyDescriptor::allocated_bytes)
                            .sum::<usize>()
                })
                .sum::<usize>() as u64
    }
}

/// One engine-owned plan node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanNode {
    operator: String,
    detail: Vec<(String, String)>,
    children: Vec<PlanNode>,
}

impl PlanNode {
    pub fn operator(&self) -> &str {
        &self.operator
    }

    pub fn detail(&self) -> &[(String, String)] {
        &self.detail
    }

    pub fn children(&self) -> &[PlanNode] {
        &self.children
    }

    fn allocated_bytes(&self) -> u64 {
        (self.operator.capacity()
            + self.detail.capacity() * std::mem::size_of::<(String, String)>()
            + self
                .detail
                .iter()
                .map(|(key, value)| key.capacity() + value.capacity())
                .sum::<usize>()
            + self.children.capacity() * std::mem::size_of::<PlanNode>()) as u64
            + self.children.iter().map(Self::allocated_bytes).sum::<u64>()
    }
}

/// Structural Rust planner presentation for EXPLAIN or PROFILE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPresentation {
    profiled: bool,
    roots: Vec<PlanNode>,
    execution_time: Option<std::time::Duration>,
}

impl PlanPresentation {
    pub const fn is_profile(&self) -> bool {
        self.profiled
    }

    pub fn roots(&self) -> &[PlanNode] {
        &self.roots
    }

    pub const fn execution_time(&self) -> Option<std::time::Duration> {
        self.execution_time
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        (self.roots.capacity() * std::mem::size_of::<PlanNode>()) as u64
            + self
                .roots
                .iter()
                .map(PlanNode::allocated_bytes)
                .sum::<u64>()
    }
}

/// Stable engine failure family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Parser,
    Binder,
    Catalog,
    Transaction,
    Runtime,
    ImportExport,
    Memory,
    Interrupt,
    Configuration,
    Io,
    InternalPanic,
}

/// Cooperative interruption cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    Explicit,
    Deadline,
}

/// Rich failure metadata preserving the unchanged engine error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementFailure {
    error: Error,
    kind: FailureKind,
    interrupt_reason: Option<InterruptReason>,
    diagnostic: Option<SyntaxDiagnostic>,
}

impl StatementFailure {
    pub const fn error(&self) -> &Error {
        &self.error
    }

    pub const fn kind(&self) -> FailureKind {
        self.kind
    }

    pub const fn interrupt_reason(&self) -> Option<InterruptReason> {
        self.interrupt_reason
    }

    pub const fn diagnostic(&self) -> Option<&SyntaxDiagnostic> {
        self.diagnostic.as_ref()
    }
}

/// One metadata-preserving execution outcome.
#[derive(Debug)]
pub struct StatementOutcome {
    result: Option<QueryResult>,
    failure: Option<StatementFailure>,
    session_before: Option<SessionSnapshot>,
    session_after: Option<SessionSnapshot>,
}

impl StatementOutcome {
    pub fn result(&self) -> Option<&QueryResult> {
        self.result.as_ref()
    }

    pub const fn failure(&self) -> Option<&StatementFailure> {
        self.failure.as_ref()
    }

    pub const fn session_before(&self) -> Option<&SessionSnapshot> {
        self.session_before.as_ref()
    }

    pub const fn session_after(&self) -> Option<&SessionSnapshot> {
        self.session_after.as_ref()
    }

    pub fn into_result(self) -> Result<QueryResult> {
        match (self.result, self.failure) {
            (Some(result), None) => Ok(result),
            (None, Some(failure)) => Err(failure.error),
            _ => Err(Error::runtime("invalid structured statement outcome")),
        }
    }
}

pub(crate) fn statement_diagnostics(
    warnings: Vec<koko_common::warnings::Warning>,
    total_warning_count: u64,
) -> StatementDiagnostics {
    StatementDiagnostics {
        warnings: warnings
            .into_iter()
            .map(|warning| StatementWarning {
                query_id: warning.query_id,
                message: warning.message,
                file_path: warning.file_path,
                line_number: warning.line_number,
                skipped_line_or_record: warning.skipped_line_or_record,
            })
            .collect(),
        total_warning_count,
    }
}

pub(crate) fn capture_result_type_context(
    catalog: &koko_catalog::Catalog,
    catalog_revision: u64,
) -> ResultTypeContext {
    let mut graph_values = Vec::new();
    for identity in catalog.node_table_ids() {
        if catalog.is_any_node_table(identity) {
            continue;
        }
        let table = catalog.node_table(identity).expect("listed node table");
        graph_values.push(GraphValueType {
            table_identity: identity.0,
            name: table.name.clone(),
            relationship: false,
            properties: table
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| property_descriptor(column, index == table.primary_key))
                .collect(),
        });
    }
    for identity in catalog.rel_table_ids() {
        if catalog.is_any_rel_table(identity) {
            continue;
        }
        let table = catalog
            .rel_table(identity)
            .expect("listed relationship table");
        graph_values.push(GraphValueType {
            table_identity: identity.0,
            name: table.name.clone(),
            relationship: true,
            properties: table
                .columns
                .iter()
                .map(|column| property_descriptor(column, false))
                .collect(),
        });
    }
    ResultTypeContext {
        catalog_revision,
        graph_values,
    }
}

pub(crate) fn plan_presentation(
    plan: &koko_planner::RegularPlan,
    profiled: bool,
) -> PlanPresentation {
    let roots = plan
        .operands
        .iter()
        .enumerate()
        .map(|(operand_index, operand)| PlanNode {
            operator: "UnionOperand".to_string(),
            detail: vec![("index".to_string(), (operand_index + 1).to_string())],
            children: operand
                .parts
                .iter()
                .enumerate()
                .map(|(part_index, part)| {
                    let mut children = vec![plan_op_node(&part.root)];
                    children.extend(part.update_ops.iter().map(update_node));
                    PlanNode {
                        operator: "QueryPart".to_string(),
                        detail: vec![("index".to_string(), (part_index + 1).to_string())],
                        children,
                    }
                })
                .collect(),
        })
        .collect();
    PlanPresentation {
        profiled,
        roots,
        execution_time: None,
    }
}

pub(crate) fn set_plan_execution_time(
    plan: &mut PlanPresentation,
    execution_time: std::time::Duration,
) {
    plan.execution_time = Some(execution_time);
}

pub(crate) fn success_outcome(
    result: QueryResult,
    session_before: Option<SessionSnapshot>,
    session_after: Option<SessionSnapshot>,
) -> StatementOutcome {
    StatementOutcome {
        result: Some(result),
        failure: None,
        session_before,
        session_after,
    }
}

pub(crate) fn failure_outcome(
    error: Error,
    kind: FailureKind,
    interrupt_reason: Option<InterruptReason>,
    diagnostic: Option<SyntaxDiagnostic>,
    session_before: Option<SessionSnapshot>,
    session_after: Option<SessionSnapshot>,
) -> StatementOutcome {
    StatementOutcome {
        result: None,
        failure: Some(StatementFailure {
            error,
            kind,
            interrupt_reason,
            diagnostic,
        }),
        session_before,
        session_after,
    }
}

pub(crate) fn failure_kind(error: &Error) -> FailureKind {
    match error {
        Error::Parser(_) => FailureKind::Parser,
        Error::Binder(_) => FailureKind::Binder,
        Error::Catalog(_) => FailureKind::Catalog,
        Error::Transaction(_) => FailureKind::Transaction,
        Error::BufferManager => FailureKind::Memory,
        Error::Interrupt => FailureKind::Interrupt,
        Error::Configuration(_) => FailureKind::Configuration,
        Error::Io(_) => FailureKind::Io,
        Error::Copy(_) => FailureKind::ImportExport,
        Error::Runtime(_)
        | Error::Conversion(_)
        | Error::Overflow(_)
        | Error::NotImplemented(_)
        | Error::Raw(_) => FailureKind::Runtime,
    }
}

fn leaf(operator: &str) -> PlanNode {
    PlanNode {
        operator: operator.to_string(),
        detail: Vec::new(),
        children: Vec::new(),
    }
}

fn unary(operator: &str, input: &koko_planner::PlanOp) -> PlanNode {
    PlanNode {
        operator: operator.to_string(),
        detail: Vec::new(),
        children: vec![plan_op_node(input)],
    }
}

fn binary(operator: &str, left: &koko_planner::PlanOp, right: &koko_planner::PlanOp) -> PlanNode {
    PlanNode {
        operator: operator.to_string(),
        detail: Vec::new(),
        children: vec![plan_op_node(left), plan_op_node(right)],
    }
}

fn plan_op_node(operator: &koko_planner::PlanOp) -> PlanNode {
    use koko_planner::PlanOp;
    match operator {
        PlanOp::SingleRow => leaf("SingleRow"),
        PlanOp::InputScan => leaf("InputScan"),
        PlanOp::ScanTableFunc { .. } => leaf("TableFunctionScan"),
        PlanOp::LoadScan { format, paths, .. } => PlanNode {
            operator: "LoadScan".to_string(),
            detail: vec![
                ("format".to_string(), format!("{format:?}")),
                ("files".to_string(), paths.len().to_string()),
            ],
            children: Vec::new(),
        },
        PlanOp::ScanNode(scan) => PlanNode {
            operator: "NodeScan".to_string(),
            detail: vec![("tables".to_string(), scan.tables.len().to_string())],
            children: Vec::new(),
        },
        PlanOp::IndexScan(scan) => scan
            .input
            .as_deref()
            .map_or_else(|| leaf("IndexScan"), |input| unary("IndexScan", input)),
        PlanOp::Extend(extend) => unary("Extend", &extend.input),
        PlanOp::VarLengthExtend(extend) => unary("VariableLengthExtend", &extend.input),
        PlanOp::ProjectPath(path) => unary("ProjectPath", &path.input),
        PlanOp::CrossProduct { left, right, .. } => binary("CrossProduct", left, right),
        PlanOp::HashJoin {
            probe, build, kind, ..
        } => PlanNode {
            operator: "HashJoin".to_string(),
            detail: vec![("kind".to_string(), format!("{kind:?}"))],
            children: vec![plan_op_node(probe), plan_op_node(build)],
        },
        PlanOp::Filter { input, .. } => unary("Filter", input),
        PlanOp::Unwind { input, .. } => unary("Unwind", input),
        PlanOp::Optional { input, pattern, .. } => binary("Optional", input, pattern),
        PlanOp::Subquery {
            input,
            pattern,
            kind,
            ..
        } => PlanNode {
            operator: "Subquery".to_string(),
            detail: vec![("kind".to_string(), format!("{kind:?}"))],
            children: vec![plan_op_node(input), plan_op_node(pattern)],
        },
        PlanOp::SequenceCall { input, func, .. } => PlanNode {
            operator: "SequenceCall".to_string(),
            detail: vec![("function".to_string(), format!("{func:?}"))],
            children: vec![plan_op_node(input)],
        },
        PlanOp::MaterializeValues { input, items } => PlanNode {
            operator: "MaterializeValues".to_string(),
            detail: vec![("values".to_string(), items.len().to_string())],
            children: vec![plan_op_node(input)],
        },
    }
}

fn update_node(update: &koko_planner::UpdateOp) -> PlanNode {
    let operator = match update {
        koko_planner::UpdateOp::Create(_) => "Create",
        koko_planner::UpdateOp::Set(_) => "Set",
        koko_planner::UpdateOp::Delete(_) => "Delete",
        koko_planner::UpdateOp::Merge(_) => "Merge",
    };
    leaf(operator)
}

/// Timings for one successful statement.
///
/// Direct execution includes parsing in `compiling_time`; prepared execution
/// excludes its cached parse but includes metadata refresh, binding, and planning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuerySummary {
    compiling_time: Duration,
    execution_time: Duration,
}

impl QuerySummary {
    pub const fn compiling_time(&self) -> Duration {
        self.compiling_time
    }

    pub const fn compilation_time(&self) -> Duration {
        self.compiling_time
    }

    pub const fn execution_time(&self) -> Duration {
        self.execution_time
    }

    pub fn compiling_time_ms(&self) -> f64 {
        self.compiling_time.as_secs_f64() * 1_000.0
    }

    pub fn execution_time_ms(&self) -> f64 {
        self.execution_time.as_secs_f64() * 1_000.0
    }
}

/// One output column's exact bound name and logical type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    name: String,
    logical_type: LogicalType,
}

impl ColumnSchema {
    pub(crate) fn new(name: String, logical_type: LogicalType) -> Self {
        Self { name, logical_type }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn logical_type(&self) -> &LogicalType {
        &self.logical_type
    }
}

/// Structural result family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryResultKind {
    Rows,
    #[default]
    Status,
    Explain,
    Profile,
}

/// Borrowed physical cell payload.
#[derive(Debug, Clone, Copy)]
pub enum CellValueRef<'a> {
    Null,
    Bool(bool),
    Int {
        value: i128,
        kind: IntKind,
    },
    UInt128(u128),
    Decimal {
        value: i128,
        precision: u8,
        scale: u8,
    },
    Double(f64),
    Float(f32),
    String(&'a str),
    Date(i32),
    Timestamp(i64),
    TimestampTz(i64),
    Interval(Interval),
    Uuid(u128),
    InternalId(InternalId),
    Generic(&'a Value),
}

/// One bounds-checked result cell carrying its declared logical type.
#[derive(Debug, Clone, Copy)]
pub struct CellRef<'a> {
    logical_type: &'a LogicalType,
    value: CellValueRef<'a>,
}

impl<'a> CellRef<'a> {
    pub const fn logical_type(&self) -> &'a LogicalType {
        self.logical_type
    }

    pub const fn value(&self) -> CellValueRef<'a> {
        self.value
    }
}

/// A materialized query result backed by typed vector batches.
#[derive(Debug, Default)]
pub struct QueryResult {
    column_names: Vec<String>,
    schema: Vec<ColumnSchema>,
    batches: Vec<DataChunk>,
    batch_offsets: Vec<usize>,
    num_rows: usize,
    summary: QuerySummary,
    kind: QueryResultKind,
    diagnostics: StatementDiagnostics,
    type_context: ResultTypeContext,
    status_message: Option<String>,
    plan: Option<PlanPresentation>,
    memory: Option<koko_common::MemoryReservation>,
}

impl QueryResult {
    pub(crate) fn from_exec(result: koko_processor::ExecResult) -> Self {
        let schema = result
            .column_names
            .iter()
            .cloned()
            .zip(result.column_types)
            .map(|(name, logical_type)| ColumnSchema { name, logical_type })
            .collect();
        Self::from_batches(result.column_names, schema, result.batches)
    }

    pub(crate) fn from_batches(
        column_names: Vec<String>,
        schema: Vec<ColumnSchema>,
        batches: Vec<DataChunk>,
    ) -> Self {
        let mut num_rows = 0;
        let batch_offsets = batches
            .iter()
            .map(|batch| {
                let offset = num_rows;
                num_rows += batch.size();
                offset
            })
            .collect();
        Self {
            column_names,
            schema,
            batches,
            batch_offsets,
            num_rows,
            summary: QuerySummary::default(),
            kind: QueryResultKind::Rows,
            diagnostics: StatementDiagnostics::default(),
            type_context: ResultTypeContext::default(),
            status_message: None,
            plan: None,
            memory: None,
        }
    }

    /// A single-row, single-column informational result (e.g. DDL messages).
    pub(crate) fn message(message: String) -> Self {
        let mut result = Self::from_typed_rows(
            vec!["result".to_string()],
            vec![LogicalType::String],
            vec![vec![Value::String(message.clone())]],
        );
        result.kind = QueryResultKind::Status;
        result.status_message = Some(message);
        result
    }

    pub(crate) fn from_typed_rows(
        column_names: Vec<String>,
        column_types: Vec<LogicalType>,
        rows: Vec<Vec<Value>>,
    ) -> Self {
        debug_assert_eq!(column_names.len(), column_types.len());
        let schema = column_names
            .iter()
            .cloned()
            .zip(column_types.iter().cloned())
            .map(|(name, logical_type)| ColumnSchema { name, logical_type })
            .collect();
        let mut batches = Vec::new();
        let mut batch = DataChunk::new(&column_types);
        let mut len = 0;
        for row in rows {
            debug_assert_eq!(row.len(), column_types.len());
            for (column, value) in batch.columns.iter_mut().zip(row) {
                column.set_value_owned(len, value);
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
        Self::from_batches(column_names, schema, batches)
    }

    /// Build a typed, columnar result from trusted first-party tooling rows.
    ///
    /// This does not execute Cypher. It lets embedded clients present immutable
    /// catalog/session metadata through the same result traversal and machine
    /// codec as query results.
    pub fn from_tooling_rows(
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
        if let Some((index, row)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.len() != column_types.len())
        {
            return Err(Error::conversion(format!(
                "tooling result row {} has {} values but {} columns",
                index + 1,
                row.len(),
                column_types.len()
            )));
        }
        Ok(Self::from_typed_rows(column_names, column_types, rows))
    }

    pub const fn result_kind(&self) -> QueryResultKind {
        self.kind
    }

    pub const fn statement_diagnostics(&self) -> &StatementDiagnostics {
        &self.diagnostics
    }

    pub const fn type_context(&self) -> &ResultTypeContext {
        &self.type_context
    }

    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_deref()
    }

    pub const fn plan(&self) -> Option<&PlanPresentation> {
        self.plan.as_ref()
    }

    pub fn cell(&self, row: usize, column: usize) -> Result<CellRef<'_>> {
        if column >= self.num_columns() {
            return Err(result_column_bounds(column, self.num_columns()));
        }
        let (vector, position) = self.vector_position(row, column)?;
        let logical_type = &self.schema[column].logical_type;
        let value = if vector.nulls.is_null(position) {
            CellValueRef::Null
        } else {
            match &vector.data {
                koko_common::ColumnData::Bool(values) => CellValueRef::Bool(values[position]),
                koko_common::ColumnData::Int64(values) => CellValueRef::Int {
                    value: values[position] as i128,
                    kind: IntKind::I64,
                },
                koko_common::ColumnData::Int128(values) => CellValueRef::Int {
                    value: values[position],
                    kind: match logical_type {
                        LogicalType::Int(kind) => *kind,
                        _ => IntKind::I128,
                    },
                },
                koko_common::ColumnData::UInt128(values) => CellValueRef::UInt128(values[position]),
                koko_common::ColumnData::Double(values) => CellValueRef::Double(values[position]),
                koko_common::ColumnData::Float(values) => CellValueRef::Float(values[position]),
                koko_common::ColumnData::Date(values) => CellValueRef::Date(values[position]),
                koko_common::ColumnData::Timestamp(values) => {
                    if *logical_type == LogicalType::TimestampTz {
                        CellValueRef::TimestampTz(values[position])
                    } else {
                        CellValueRef::Timestamp(values[position])
                    }
                }
                koko_common::ColumnData::Interval(values) => {
                    CellValueRef::Interval(values[position])
                }
                koko_common::ColumnData::Uuid(values) => CellValueRef::Uuid(values[position]),
                koko_common::ColumnData::Decimal(values) => {
                    let (precision, scale) = match logical_type {
                        LogicalType::Decimal(precision, scale) => (*precision, *scale),
                        _ => (38, 0),
                    };
                    CellValueRef::Decimal {
                        value: values[position],
                        precision,
                        scale,
                    }
                }
                koko_common::ColumnData::Str(values) => CellValueRef::String(&values[position]),
                koko_common::ColumnData::InternalId(values) => {
                    CellValueRef::InternalId(values[position])
                }
                koko_common::ColumnData::Generic(values) => {
                    CellValueRef::Generic(&values[position])
                }
            }
        };
        Ok(CellRef {
            logical_type,
            value,
        })
    }

    /// Compilation and execution timings for this statement.
    pub const fn summary(&self) -> &QuerySummary {
        &self.summary
    }
    pub fn num_rows(&self) -> usize {
        self.num_rows
    }

    pub fn num_columns(&self) -> usize {
        self.schema.len()
    }

    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    pub fn schema(&self) -> &[ColumnSchema] {
        &self.schema
    }

    pub fn batches(&self) -> &[DataChunk] {
        &self.batches
    }

    pub(crate) fn configure_explain(
        &mut self,
        kind: QueryResultKind,
        type_context: ResultTypeContext,
        plan: PlanPresentation,
    ) {
        self.kind = kind;
        self.type_context = type_context;
        self.plan = Some(plan);
    }

    pub(crate) fn configure_status(&mut self, message: String) {
        self.kind = QueryResultKind::Status;
        self.status_message = Some(message);
    }

    pub(crate) fn set_type_context(&mut self, type_context: ResultTypeContext) {
        self.type_context = type_context;
    }

    pub(crate) fn set_diagnostics(&mut self, diagnostics: StatementDiagnostics) {
        self.diagnostics = diagnostics;
    }

    pub(crate) fn set_summary(&mut self, compiling_time: Duration, execution_time: Duration) {
        self.summary = QuerySummary {
            compiling_time,
            execution_time,
        };
    }

    pub(crate) fn set_execution_time(&mut self, execution_time: Duration) {
        self.summary.execution_time = execution_time;
    }

    pub(crate) fn attach_plan_execution_time(&mut self) {
        if let Some(plan) = self.plan.as_mut() {
            set_plan_execution_time(plan, self.summary.execution_time());
        }
    }

    pub fn column(&self, index: usize) -> Result<ColumnView<'_>> {
        if index >= self.num_columns() {
            return Err(result_column_bounds(index, self.num_columns()));
        }
        Ok(ColumnView {
            result: self,
            index,
        })
    }

    pub fn column_by_name(&self, name: &str) -> Result<ColumnView<'_>> {
        let mut matches = self
            .column_names
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| (candidate == name).then_some(index));
        let index = matches
            .next()
            .ok_or_else(|| Error::runtime(format!("result column `{name}` does not exist")))?;
        if matches.next().is_some() {
            return Err(Error::runtime(format!(
                "result column name `{name}` is ambiguous"
            )));
        }
        self.column(index)
    }

    pub fn typed_column<T: FromValue>(&self, index: usize) -> Result<TypedColumnView<'_, T>> {
        self.column(index)?.typed()
    }

    pub fn typed_column_by_name<T: FromValue>(&self, name: &str) -> Result<TypedColumnView<'_, T>> {
        self.column_by_name(name)?.typed()
    }

    /// The value at `(row, column)`, with both indices checked.
    pub fn value(&self, row: usize, column: usize) -> Result<Value> {
        self.column(column)?.get(row)
    }

    pub fn value_by_name(&self, row: usize, name: &str) -> Result<Value> {
        self.column_by_name(name)?.get(row)
    }

    fn vector_position(
        &self,
        row: usize,
        column: usize,
    ) -> Result<(&koko_common::ValueVector, usize)> {
        if row >= self.num_rows {
            return Err(Error::runtime(format!(
                "result row index {row} is out of bounds for {} rows",
                self.num_rows
            )));
        }
        let batch_index = self.batch_offsets.partition_point(|offset| *offset <= row) - 1;
        let batch = &self.batches[batch_index];
        let logical_position = row - self.batch_offsets[batch_index];
        let position = batch
            .sel
            .iter()
            .nth(logical_position)
            .expect("batch offset metadata matches its selection");
        Ok((&batch.columns[column], position))
    }

    fn value_at(&self, row: usize, column: usize) -> Result<Value> {
        let (vector, position) = self.vector_position(row, column)?;
        Ok(vector.get_value(position))
    }

    /// Ergonomic row cursors over the typed result batches.
    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> {
        (0..self.num_rows).map(move |index| Row {
            result: self,
            index,
        })
    }

    /// Render each row as `|`-joined cells (the `.test` corpus result format).
    pub fn to_result_strings(&self) -> Vec<String> {
        (0..self.num_rows)
            .map(|row| {
                (0..self.num_columns())
                    .map(|column| {
                        self.value_at(row, column)
                            .expect("result coordinates are in bounds")
                            .to_result_string()
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    }
    fn allocated_bytes(&self) -> u64 {
        (self.column_names.capacity() * std::mem::size_of::<String>()) as u64
            + self
                .column_names
                .iter()
                .map(|name| name.capacity() as u64)
                .sum::<u64>()
            + (self.schema.capacity() * std::mem::size_of::<ColumnSchema>()) as u64
            + self
                .schema
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
            + self
                .status_message
                .as_ref()
                .map_or(0, |message| message.capacity() as u64)
            + self.diagnostics.allocated_bytes()
            + self.type_context.allocated_bytes()
            + self
                .plan
                .as_ref()
                .map_or(0, PlanPresentation::allocated_bytes)
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

/// A bounds-checked view of one result column.
#[derive(Clone, Copy)]
pub struct ColumnView<'a> {
    result: &'a QueryResult,
    index: usize,
}
impl<'a> ColumnView<'a> {
    pub fn schema(&self) -> &ColumnSchema {
        &self.result.schema[self.index]
    }

    pub fn len(&self) -> usize {
        self.result.num_rows
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, row: usize) -> Result<Value> {
        self.result.value_at(row, self.index)
    }

    pub fn cell(&self, row: usize) -> Result<CellRef<'a>> {
        self.result.cell(row, self.index)
    }

    pub fn typed<T: FromValue>(self) -> Result<TypedColumnView<'a, T>> {
        if !T::accepts(self.schema().logical_type()) {
            return Err(Error::conversion(format!(
                "cannot view result column `{}` of type {} as {}",
                self.schema().name(),
                self.schema().logical_type(),
                T::type_name()
            )));
        }
        Ok(TypedColumnView {
            column: self,
            marker: std::marker::PhantomData,
        })
    }
}

/// A schema-checked typed view of one result column.
#[derive(Clone, Copy)]
pub struct TypedColumnView<'a, T> {
    column: ColumnView<'a>,
    marker: std::marker::PhantomData<T>,
}

impl<T: FromValue> TypedColumnView<'_, T> {
    pub fn schema(&self) -> &ColumnSchema {
        self.column.schema()
    }

    pub fn len(&self) -> usize {
        self.column.len()
    }

    pub fn is_empty(&self) -> bool {
        self.column.is_empty()
    }

    pub fn get(&self, row: usize) -> Result<T> {
        let value = self.column.get(row)?;
        T::from_value(&value)
    }
}

impl std::fmt::Display for QueryResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in self.to_result_strings() {
            writeln!(f, "{line}")?;
        }
        Ok(())
    }
}

/// A borrowed cursor over one result row.
pub struct Row<'a> {
    result: &'a QueryResult,
    index: usize,
}

impl Row<'_> {
    pub fn len(&self) -> usize {
        self.result.num_columns()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get_value(&self, column: usize) -> Result<Value> {
        self.result.value(self.index, column)
    }

    pub fn get_value_by_name(&self, name: &str) -> Result<Value> {
        self.result.value_by_name(self.index, name)
    }

    pub fn cell(&self, column: usize) -> Result<CellRef<'_>> {
        self.result.cell(self.index, column)
    }

    pub fn cell_by_name(&self, name: &str) -> Result<CellRef<'_>> {
        let column = self.result.column_by_name(name)?;
        column.cell(self.index)
    }

    /// Typed extraction: `row.get::<i64>(0)?`.
    pub fn get<T: FromValue>(&self, column: usize) -> Result<T> {
        let value = self.get_value(column)?;
        T::from_value(&value)
    }

    pub fn get_by_name<T: FromValue>(&self, name: &str) -> Result<T> {
        let value = self.get_value_by_name(name)?;
        T::from_value(&value)
    }
}

/// Typed extraction of a [`Value`] into a Rust type. Implementations may narrow
/// [`accepts`](FromValue::accepts) so typed column views reject incompatible schemas up front.
pub trait FromValue: Sized {
    fn from_value(value: &Value) -> Result<Self>;

    fn accepts(_logical_type: &LogicalType) -> bool {
        true
    }

    fn type_name() -> &'static str {
        std::any::type_name::<Self>()
    }
}

fn conv_err(v: &Value, target: &str) -> Error {
    Error::conversion(format!("cannot read {} as {target}", v.logical_type()))
}

impl FromValue for i64 {
    fn from_value(v: &Value) -> Result<Self> {
        v.as_i64().ok_or_else(|| conv_err(v, "INT64"))
    }

    fn accepts(logical_type: &LogicalType) -> bool {
        use koko_common::IntKind;
        matches!(
            logical_type,
            LogicalType::Int(
                IntKind::I8
                    | IntKind::I16
                    | IntKind::I32
                    | IntKind::I64
                    | IntKind::U8
                    | IntKind::U16
                    | IntKind::U32
            ) | LogicalType::Serial
        )
    }

    fn type_name() -> &'static str {
        "INT64"
    }
}
impl FromValue for f64 {
    fn from_value(v: &Value) -> Result<Self> {
        v.as_f64().ok_or_else(|| conv_err(v, "DOUBLE"))
    }

    fn accepts(logical_type: &LogicalType) -> bool {
        matches!(logical_type, LogicalType::Double | LogicalType::Float)
    }

    fn type_name() -> &'static str {
        "DOUBLE"
    }
}
impl FromValue for bool {
    fn from_value(v: &Value) -> Result<Self> {
        v.as_bool().ok_or_else(|| conv_err(v, "BOOL"))
    }

    fn accepts(logical_type: &LogicalType) -> bool {
        matches!(logical_type, LogicalType::Bool)
    }

    fn type_name() -> &'static str {
        "BOOL"
    }
}
impl FromValue for String {
    fn from_value(v: &Value) -> Result<Self> {
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| conv_err(v, "STRING"))
    }

    fn accepts(logical_type: &LogicalType) -> bool {
        matches!(logical_type, LogicalType::String)
    }

    fn type_name() -> &'static str {
        "STRING"
    }
}
impl FromValue for Value {
    fn from_value(v: &Value) -> Result<Self> {
        Ok(v.clone())
    }
}
/// Any value may be read as an `Option<T>`, mapping `Null` to `None`.
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: &Value) -> Result<Self> {
        if v.is_null() {
            Ok(None)
        } else {
            T::from_value(v).map(Some)
        }
    }

    fn accepts(logical_type: &LogicalType) -> bool {
        T::accepts(logical_type)
    }

    fn type_name() -> &'static str {
        T::type_name()
    }
}
