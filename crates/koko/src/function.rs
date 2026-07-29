//! Connection-local native scalar function descriptors.

use crate::Result;
use crate::value::{LogicalType, Value};

/// Whether NULL arguments bypass a scalar callback.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NullPolicy {
    #[default]
    Propagate,
    Call,
}

impl From<NullPolicy> for koko_common::RegisteredScalarFunctionNullPolicy {
    fn from(policy: NullPolicy) -> Self {
        match policy {
            NullPolicy::Propagate => Self::Propagate,
            NullPolicy::Call => Self::Call,
        }
    }
}

/// An immutable native scalar function ready for registration.
#[derive(Debug, Clone)]
pub struct ScalarFunction {
    inner: koko_common::RegisteredScalarFunction,
}

impl ScalarFunction {
    pub fn new<F>(
        name: impl Into<String>,
        parameter_types: impl IntoIterator<Item = LogicalType>,
        result_type: LogicalType,
        callback: F,
    ) -> Self
    where
        F: Fn(&[Value]) -> Result<Value> + Send + Sync + 'static,
    {
        Self {
            inner: koko_common::RegisteredScalarFunction::new(
                name.into(),
                parameter_types.into_iter().collect(),
                result_type,
                koko_common::RegisteredScalarFunctionNullPolicy::Propagate,
                callback,
            ),
        }
    }

    pub fn with_null_policy(mut self, policy: NullPolicy) -> Self {
        self.inner.null_policy = policy.into();
        self
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn parameter_types(&self) -> &[LogicalType] {
        &self.inner.parameter_types
    }

    pub const fn result_type(&self) -> &LogicalType {
        &self.inner.result_type
    }

    pub const fn null_policy(&self) -> NullPolicy {
        match self.inner.null_policy {
            koko_common::RegisteredScalarFunctionNullPolicy::Propagate => NullPolicy::Propagate,
            koko_common::RegisteredScalarFunctionNullPolicy::Call => NullPolicy::Call,
        }
    }

    pub(crate) fn into_inner(self) -> koko_common::RegisteredScalarFunction {
        self.inner
    }
}
