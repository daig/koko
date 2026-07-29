//! Structured warnings and failures retained around engine errors.

use crate::tooling::SyntaxDiagnostic;
use koko_common::Error;

/// One warning retained for the statement that produced an outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    query_id: u64,
    message: String,
    file_path: String,
    line_number: u64,
    skipped_line_or_record: String,
}

impl Warning {
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
pub struct Diagnostics {
    warnings: Vec<Warning>,
    total_warning_count: u64,
}

impl Diagnostics {
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    pub const fn total_warning_count(&self) -> u64 {
        self.total_warning_count
    }

    pub(crate) fn from_engine(
        warnings: Vec<koko_common::warnings::Warning>,
        total_warning_count: u64,
    ) -> Self {
        Self {
            warnings: warnings
                .into_iter()
                .map(|warning| Warning {
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

    pub(crate) fn allocated_bytes(&self) -> u64 {
        (self.warnings.capacity() * std::mem::size_of::<Warning>()) as u64
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

/// Stable engine failure family.
#[non_exhaustive]
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
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    Explicit,
    Deadline,
}

/// Rich failure metadata preserving the unchanged engine error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    error: Error,
    kind: FailureKind,
    interrupt_reason: Option<InterruptReason>,
    diagnostic: Option<SyntaxDiagnostic>,
}

impl Failure {
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

    pub fn into_error(self) -> Error {
        self.error
    }

    pub(crate) fn new(
        error: Error,
        kind: FailureKind,
        interrupt_reason: Option<InterruptReason>,
        diagnostic: Option<SyntaxDiagnostic>,
    ) -> Self {
        Self {
            error,
            kind,
            interrupt_reason,
            diagnostic,
        }
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
        _ => FailureKind::Runtime,
    }
}
