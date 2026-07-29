//! Immutable public tooling views used by editors and command-line clients.
//!
//! These types deliberately belong to the `koko` facade. Callers do not need
//! a dependency on the parser or any private engine layer.
use crate::result::Column;
use crate::{QueryResult, Result, Value};

/// A half-open UTF-8 byte span in source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SourceSpan {
    start: usize,
    end: usize,
}

impl SourceSpan {
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    pub const fn start(&self) -> usize {
        self.start
    }

    pub const fn end(&self) -> usize {
        self.end
    }

    pub const fn len(&self) -> usize {
        self.end - self.start
    }

    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Coarse lexical classes for editor styling.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Keyword,
    Identifier,
    Parameter,
    String,
    Number,
    Comment,
    Punctuation,
    Operator,
}

/// One parser-backed lexical token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpan {
    kind: TokenKind,
    span: SourceSpan,
}

impl TokenSpan {
    pub const fn kind(&self) -> TokenKind {
        self.kind
    }

    pub const fn span(&self) -> SourceSpan {
        self.span
    }
}

/// Whole-buffer or per-statement syntax state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxStatus {
    Empty,
    Incomplete,
    Complete,
    Invalid,
}

/// Syntactic statement family. Binding remains authoritative for semantics.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementClass {
    Query,
    DataDefinition,
    Graph,
    Transaction,
    Setting,
    Copy,
    ImportExport,
    Explain,
    Profile,
}

/// Expected result family available without binding.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputClass {
    Rows,
    Status,
    Plan,
}

/// One non-empty logical statement in a source buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementAnalysis {
    span: SourceSpan,
    status: SyntaxStatus,
    class: Option<StatementClass>,
    output: Option<OutputClass>,
}

impl StatementAnalysis {
    pub const fn span(&self) -> SourceSpan {
        self.span
    }

    pub const fn status(&self) -> SyntaxStatus {
        self.status
    }

    pub const fn class(&self) -> Option<StatementClass> {
        self.class
    }

    pub const fn output_class(&self) -> Option<OutputClass> {
        self.output
    }
}

/// A source-located syntax diagnostic. The span is absent when the parser has
/// no structurally reliable location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxDiagnostic {
    message: String,
    span: Option<SourceSpan>,
}

impl SyntaxDiagnostic {
    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn span(&self) -> Option<SourceSpan> {
        self.span
    }
}

/// Completion family at the cursor.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorContextKind {
    Keyword,
    Graph,
    NodeLabel,
    RelationshipLabel,
    Variable,
    Property,
    Function,
    Parameter,
    Setting,
    Path,
}

/// Conservative cursor context produced without binding or execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorContext {
    kind: CursorContextKind,
    replacement: SourceSpan,
    prefix: String,
}

impl CursorContext {
    pub const fn kind(&self) -> CursorContextKind {
        self.kind
    }

    pub const fn replacement(&self) -> SourceSpan {
        self.replacement
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// Immutable syntax analysis of one source buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxAnalysis {
    status: SyntaxStatus,
    tokens: Vec<TokenSpan>,
    statements: Vec<StatementAnalysis>,
    diagnostic: Option<SyntaxDiagnostic>,
    cursor: Option<CursorContext>,
}

impl SyntaxAnalysis {
    pub const fn status(&self) -> SyntaxStatus {
        self.status
    }

    pub fn tokens(&self) -> &[TokenSpan] {
        &self.tokens
    }

    pub fn statements(&self) -> &[StatementAnalysis] {
        &self.statements
    }

    pub fn diagnostic(&self) -> Option<&SyntaxDiagnostic> {
        self.diagnostic.as_ref()
    }

    pub fn cursor_context(&self) -> Option<&CursorContext> {
        self.cursor.as_ref()
    }
}

/// Build a validated materialized tabular result for first-party tooling.
pub fn tabular_result(columns: Vec<Column>, rows: Vec<Vec<Value>>) -> Result<QueryResult> {
    QueryResult::from_rows(columns, rows)
}

/// Running library version used by first-party clients.
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Canonical Cypher keyword vocabulary for editor completion.
pub const fn cypher_keywords() -> &'static [&'static str] {
    koko_parser::tooling::CYPHER_KEYWORDS
}

/// Analyze Cypher through the canonical `koko-parser` tooling service.
pub fn analyze_cypher(source: &str, cursor: Option<usize>) -> SyntaxAnalysis {
    let analysis = koko_parser::tooling::analyze(source, cursor);
    SyntaxAnalysis {
        status: map_status(analysis.status),
        tokens: analysis
            .tokens
            .into_iter()
            .map(|token| TokenSpan {
                kind: match token.kind {
                    koko_parser::tooling::ToolTokenKind::Keyword => TokenKind::Keyword,
                    koko_parser::tooling::ToolTokenKind::Identifier => TokenKind::Identifier,
                    koko_parser::tooling::ToolTokenKind::Parameter => TokenKind::Parameter,
                    koko_parser::tooling::ToolTokenKind::String => TokenKind::String,
                    koko_parser::tooling::ToolTokenKind::Number => TokenKind::Number,
                    koko_parser::tooling::ToolTokenKind::Comment => TokenKind::Comment,
                    koko_parser::tooling::ToolTokenKind::Punctuation => TokenKind::Punctuation,
                    koko_parser::tooling::ToolTokenKind::Operator => TokenKind::Operator,
                },
                span: map_span(token.span),
            })
            .collect(),
        statements: analysis
            .statements
            .into_iter()
            .map(|statement| StatementAnalysis {
                span: map_span(statement.span),
                status: map_status(statement.status),
                class: statement.class.map(|class| match class {
                    koko_parser::tooling::ToolStatementClass::Query => StatementClass::Query,
                    koko_parser::tooling::ToolStatementClass::DataDefinition => {
                        StatementClass::DataDefinition
                    }
                    koko_parser::tooling::ToolStatementClass::Graph => StatementClass::Graph,
                    koko_parser::tooling::ToolStatementClass::Transaction => {
                        StatementClass::Transaction
                    }
                    koko_parser::tooling::ToolStatementClass::Setting => StatementClass::Setting,
                    koko_parser::tooling::ToolStatementClass::Copy => StatementClass::Copy,
                    koko_parser::tooling::ToolStatementClass::ImportExport => {
                        StatementClass::ImportExport
                    }
                    koko_parser::tooling::ToolStatementClass::Explain => StatementClass::Explain,
                    koko_parser::tooling::ToolStatementClass::Profile => StatementClass::Profile,
                }),
                output: statement.output.map(|output| match output {
                    koko_parser::tooling::ToolOutputClass::Rows => OutputClass::Rows,
                    koko_parser::tooling::ToolOutputClass::Status => OutputClass::Status,
                    koko_parser::tooling::ToolOutputClass::Plan => OutputClass::Plan,
                }),
            })
            .collect(),
        diagnostic: analysis.diagnostic.map(|diagnostic| SyntaxDiagnostic {
            message: diagnostic.message,
            span: diagnostic.span.map(map_span),
        }),
        cursor: analysis.cursor.map(|cursor| CursorContext {
            kind: match cursor.kind {
                koko_parser::tooling::ToolCursorKind::Keyword => CursorContextKind::Keyword,
                koko_parser::tooling::ToolCursorKind::Graph => CursorContextKind::Graph,
                koko_parser::tooling::ToolCursorKind::NodeLabel => CursorContextKind::NodeLabel,
                koko_parser::tooling::ToolCursorKind::RelationshipLabel => {
                    CursorContextKind::RelationshipLabel
                }
                koko_parser::tooling::ToolCursorKind::Variable => CursorContextKind::Variable,
                koko_parser::tooling::ToolCursorKind::Property => CursorContextKind::Property,
                koko_parser::tooling::ToolCursorKind::Function => CursorContextKind::Function,
                koko_parser::tooling::ToolCursorKind::Parameter => CursorContextKind::Parameter,
                koko_parser::tooling::ToolCursorKind::Setting => CursorContextKind::Setting,
                koko_parser::tooling::ToolCursorKind::Path => CursorContextKind::Path,
            },
            replacement: map_span(cursor.replacement),
            prefix: cursor.prefix,
        }),
    }
}

const fn map_span(span: koko_parser::tooling::ToolSpan) -> SourceSpan {
    SourceSpan::new(span.start, span.end)
}

const fn map_status(status: koko_parser::tooling::ToolSyntaxStatus) -> SyntaxStatus {
    match status {
        koko_parser::tooling::ToolSyntaxStatus::Empty => SyntaxStatus::Empty,
        koko_parser::tooling::ToolSyntaxStatus::Incomplete => SyntaxStatus::Incomplete,
        koko_parser::tooling::ToolSyntaxStatus::Complete => SyntaxStatus::Complete,
        koko_parser::tooling::ToolSyntaxStatus::Invalid => SyntaxStatus::Invalid,
    }
}

/// Stable database-local graph identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GraphIdentity(pub(crate) u64);

impl GraphIdentity {
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// Schema mode of a graph.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphKind {
    Typed,
    Any,
}

/// Authoritative explicit transaction state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionMode {
    None,
    ReadOnly,
    ReadWrite,
}

/// One graph-registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDescriptor {
    pub(crate) identity: GraphIdentity,
    pub(crate) name: String,
    pub(crate) kind: GraphKind,
}

impl GraphDescriptor {
    pub const fn identity(&self) -> GraphIdentity {
        self.identity
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn kind(&self) -> GraphKind {
        self.kind
    }
}

/// Immutable connection session state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub(crate) revision: u64,
    pub(crate) graph: GraphDescriptor,
    pub(crate) transaction: TransactionMode,
    pub(crate) timeout: Option<std::time::Duration>,
    pub(crate) workers: usize,
    pub(crate) catalog_revision: u64,
    pub(crate) graph_registry_revision: u64,
}

impl SessionSnapshot {
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn graph(&self) -> &GraphDescriptor {
        &self.graph
    }

    pub const fn transaction(&self) -> TransactionMode {
        self.transaction
    }

    pub const fn timeout(&self) -> Option<std::time::Duration> {
        self.timeout
    }

    pub const fn workers(&self) -> usize {
        self.workers
    }

    pub const fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }

    pub const fn graph_registry_revision(&self) -> u64 {
        self.graph_registry_revision
    }
}

/// One table property.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyDescriptor {
    pub(crate) name: String,
    pub(crate) logical_type: super::LogicalType,
    pub(crate) type_text: String,
    pub(crate) primary_key: bool,
    pub(crate) default_text: String,
}

impl PropertyDescriptor {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn logical_type(&self) -> &super::LogicalType {
        &self.logical_type
    }

    pub fn type_text(&self) -> &str {
        &self.type_text
    }

    pub const fn is_primary_key(&self) -> bool {
        self.primary_key
    }

    pub fn default_text(&self) -> &str {
        &self.default_text
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.name.capacity() + self.type_text.capacity() + self.default_text.capacity()
    }
}

/// One relationship FROM/TO endpoint pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointDescriptor {
    pub(crate) from: String,
    pub(crate) to: String,
}

impl EndpointDescriptor {
    pub fn from(&self) -> &str {
        &self.from
    }

    pub fn to(&self) -> &str {
        &self.to
    }
}

/// Visible node-table metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeTableDescriptor {
    pub(crate) identity: u64,
    pub(crate) name: String,
    pub(crate) properties: Vec<PropertyDescriptor>,
    pub(crate) comment: Option<String>,
}

impl NodeTableDescriptor {
    pub const fn identity(&self) -> u64 {
        self.identity
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn properties(&self) -> &[PropertyDescriptor] {
        &self.properties
    }

    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

/// Visible relationship-table metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipTableDescriptor {
    pub(crate) identity: u64,
    pub(crate) name: String,
    pub(crate) properties: Vec<PropertyDescriptor>,
    pub(crate) endpoints: Vec<EndpointDescriptor>,
    pub(crate) storage_direction: String,
    pub(crate) comment: Option<String>,
}

impl RelationshipTableDescriptor {
    pub const fn identity(&self) -> u64 {
        self.identity
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn properties(&self) -> &[PropertyDescriptor] {
        &self.properties
    }

    pub fn endpoints(&self) -> &[EndpointDescriptor] {
        &self.endpoints
    }

    pub fn storage_direction(&self) -> &str {
        &self.storage_direction
    }

    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

/// One visible in-memory index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescriptor {
    pub(crate) name: String,
    pub(crate) table: String,
    pub(crate) index_type: String,
    pub(crate) properties: Vec<String>,
}

impl IndexDescriptor {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn index_type(&self) -> &str {
        &self.index_type
    }

    pub fn properties(&self) -> &[String] {
        &self.properties
    }
}

/// One scalar macro definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDescriptor {
    pub(crate) name: String,
    pub(crate) signature: String,
    pub(crate) body: String,
}

impl MacroDescriptor {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    pub fn body(&self) -> &str {
        &self.body
    }
}

/// Function catalog family.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    Scalar,
    Aggregate,
    Table,
    Macro,
    ConnectionLocal,
}

/// One function overload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDescriptor {
    pub(crate) name: String,
    pub(crate) kind: FunctionKind,
    pub(crate) signature: String,
    pub(crate) return_type: String,
}

impl FunctionDescriptor {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn kind(&self) -> FunctionKind {
        self.kind
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    pub fn return_type(&self) -> &str {
        &self.return_type
    }
}

/// One recognized connection setting.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingDescriptor {
    pub(crate) name: String,
    pub(crate) logical_type: super::LogicalType,
    pub(crate) current_value: super::Value,
    pub(crate) accepted_values: Vec<String>,
}

impl SettingDescriptor {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn logical_type(&self) -> &super::LogicalType {
        &self.logical_type
    }

    pub const fn current_value(&self) -> &super::Value {
        &self.current_value
    }

    pub fn accepted_values(&self) -> &[String] {
        &self.accepted_values
    }
}

/// Coherent owned metadata for the selected transaction/catalog view.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSnapshot {
    pub(crate) graph_registry_revision: u64,
    pub(crate) catalog_revision: u64,
    pub(crate) function_revision: u64,
    pub(crate) selected_graph: GraphIdentity,
    pub(crate) graphs: Vec<GraphDescriptor>,
    pub(crate) node_tables: Vec<NodeTableDescriptor>,
    pub(crate) relationship_tables: Vec<RelationshipTableDescriptor>,
    pub(crate) indexes: Vec<IndexDescriptor>,
    pub(crate) macros: Vec<MacroDescriptor>,
    pub(crate) functions: Vec<FunctionDescriptor>,
    pub(crate) settings: Vec<SettingDescriptor>,
    pub(crate) schema_script: String,
}

impl CatalogSnapshot {
    pub const fn graph_registry_revision(&self) -> u64 {
        self.graph_registry_revision
    }

    pub const fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }

    pub const fn function_revision(&self) -> u64 {
        self.function_revision
    }

    pub const fn selected_graph(&self) -> GraphIdentity {
        self.selected_graph
    }

    pub fn graphs(&self) -> &[GraphDescriptor] {
        &self.graphs
    }

    pub fn node_tables(&self) -> &[NodeTableDescriptor] {
        &self.node_tables
    }

    pub fn relationship_tables(&self) -> &[RelationshipTableDescriptor] {
        &self.relationship_tables
    }

    pub fn indexes(&self) -> &[IndexDescriptor] {
        &self.indexes
    }

    pub fn macros(&self) -> &[MacroDescriptor] {
        &self.macros
    }

    pub fn functions(&self) -> &[FunctionDescriptor] {
        &self.functions
    }

    pub fn settings(&self) -> &[SettingDescriptor] {
        &self.settings
    }

    pub fn schema_script(&self) -> &str {
        &self.schema_script
    }

    /// Canonical executable schema statements in dependency order.
    ///
    /// The interchange renderer guarantees one complete statement per line;
    /// clients consume this structured iterator rather than parsing display
    /// output or issuing `SHOW` queries.
    pub fn schema_statements(&self) -> impl Iterator<Item = &str> {
        self.schema_script.lines().filter(|line| !line.is_empty())
    }

    /// Canonical statements belonging to one named schema object.
    pub fn schema_statements_for_object<'a>(&'a self, name: &str) -> Vec<&'a str> {
        self.schema_statements()
            .filter(|statement| schema_statement_matches(statement, name))
            .collect()
    }
}

fn schema_statement_matches(statement: &str, name: &str) -> bool {
    let Ok(statement) = koko_parser::parse_statement(statement) else {
        return false;
    };
    let equals = |candidate: &str| candidate.eq_ignore_ascii_case(name);
    match statement {
        koko_parser::ast::Statement::CreateNodeTable(item) => equals(&item.name),
        koko_parser::ast::Statement::CreateRelTable(item) => equals(&item.name),
        koko_parser::ast::Statement::CreateIndex(item) => equals(&item.name) || equals(&item.table),
        koko_parser::ast::Statement::CreateSequence(item) => equals(&item.name),
        koko_parser::ast::Statement::Comment(item) => equals(&item.table),
        koko_parser::ast::Statement::Alter(item) => equals(&item.table),
        koko_parser::ast::Statement::CreateType(item) => equals(&item.name),
        koko_parser::ast::Statement::CreateMacro(item) => equals(&item.name),
        _ => false,
    }
}

pub(crate) fn property_descriptor(
    column: &koko_catalog::Column,
    primary_key: bool,
) -> PropertyDescriptor {
    PropertyDescriptor {
        name: column.name().to_string(),
        logical_type: column.logical_type().clone(),
        type_text: column.type_text().to_string(),
        primary_key,
        default_text: column.default_text().to_string(),
    }
}

pub(crate) fn function_descriptors(
    macros: &[MacroDescriptor],
    scalar_udfs: &std::collections::HashMap<
        String,
        std::sync::Arc<koko_common::RegisteredScalarFunction>,
    >,
) -> Vec<FunctionDescriptor> {
    let mut functions: Vec<_> = koko_function::catalog_data::FUNCTION_CATALOG
        .iter()
        .map(|entry| FunctionDescriptor {
            name: entry.name.to_string(),
            kind: match entry.kind {
                koko_function::FunctionCatalogKind::Aggregate => FunctionKind::Aggregate,
                koko_function::FunctionCatalogKind::Table
                | koko_function::FunctionCatalogKind::StandaloneTable => FunctionKind::Table,
                _ => FunctionKind::Scalar,
            },
            signature: entry.signature.to_string(),
            return_type: entry
                .signature
                .rsplit_once(" -> ")
                .map_or("", |(_, result)| result)
                .to_string(),
        })
        .collect();
    functions.extend(macros.iter().map(|item| FunctionDescriptor {
        name: item.name.clone(),
        kind: FunctionKind::Macro,
        signature: item.signature.clone(),
        return_type: "ANY".to_string(),
    }));
    functions.extend(scalar_udfs.values().map(|function| {
        let arguments = function
            .parameter_types
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        FunctionDescriptor {
            name: function.name.clone(),
            kind: FunctionKind::ConnectionLocal,
            signature: format!("({arguments}) -> {}", function.result_type),
            return_type: function.result_type.to_string(),
        }
    }));
    functions.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.signature.cmp(&right.signature))
            .then_with(|| (left.kind as u8).cmp(&(right.kind as u8)))
    });
    functions
}
