//! Query inputs and metadata-preserving detailed execution outcomes.

use crate::diagnostics::{Failure, FailureKind, InterruptReason};
use crate::result::QueryResult;
use crate::tooling::{SessionSnapshot, SyntaxDiagnostic};
use crate::value::{LogicalType, Value};
use crate::{Error, Result};

/// One owned named query parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    name: String,
    value: Value,
    declared_type: Option<LogicalType>,
}

impl Parameter {
    pub fn new(name: impl Into<String>, value: impl Into<Value>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            declared_type: None,
        }
    }

    pub fn typed(
        name: impl Into<String>,
        value: impl Into<Value>,
        declared_type: LogicalType,
    ) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            declared_type: Some(declared_type),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn value(&self) -> &Value {
        &self.value
    }

    pub const fn declared_type(&self) -> Option<&LogicalType> {
        self.declared_type.as_ref()
    }

    pub(crate) fn into_parts(self) -> (String, Value, Option<LogicalType>) {
        (self.name, self.value, self.declared_type)
    }
}

#[derive(Debug)]
enum OutcomeState {
    Success(QueryResult),
    Failure(Failure),
}

/// One metadata-preserving execution outcome.
#[derive(Debug)]
pub struct Outcome {
    state: OutcomeState,
    session_before: Option<SessionSnapshot>,
    session_after: Option<SessionSnapshot>,
}

impl Outcome {
    pub const fn is_success(&self) -> bool {
        matches!(self.state, OutcomeState::Success(_))
    }

    pub const fn is_failure(&self) -> bool {
        matches!(self.state, OutcomeState::Failure(_))
    }

    pub const fn result(&self) -> Option<&QueryResult> {
        match &self.state {
            OutcomeState::Success(result) => Some(result),
            OutcomeState::Failure(_) => None,
        }
    }

    pub const fn failure(&self) -> Option<&Failure> {
        match &self.state {
            OutcomeState::Success(_) => None,
            OutcomeState::Failure(failure) => Some(failure),
        }
    }

    pub const fn session_before(&self) -> Option<&SessionSnapshot> {
        self.session_before.as_ref()
    }

    pub const fn session_after(&self) -> Option<&SessionSnapshot> {
        self.session_after.as_ref()
    }

    pub fn into_result(self) -> Result<QueryResult> {
        match self.state {
            OutcomeState::Success(result) => Ok(result),
            OutcomeState::Failure(failure) => Err(failure.into_error()),
        }
    }

    pub(crate) fn success(
        result: QueryResult,
        session_before: Option<SessionSnapshot>,
        session_after: Option<SessionSnapshot>,
    ) -> Self {
        Self {
            state: OutcomeState::Success(result),
            session_before,
            session_after,
        }
    }

    pub(crate) fn failed(
        error: Error,
        kind: FailureKind,
        interrupt_reason: Option<InterruptReason>,
        diagnostic: Option<SyntaxDiagnostic>,
        session_before: Option<SessionSnapshot>,
        session_after: Option<SessionSnapshot>,
    ) -> Self {
        Self {
            state: OutcomeState::Failure(Failure::new(error, kind, interrupt_reason, diagnostic)),
            session_before,
            session_after,
        }
    }
}
