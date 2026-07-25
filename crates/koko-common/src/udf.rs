//! Safe Rust scalar user-defined functions shared across binding and execution.

use crate::{Error, LogicalType, Result, Value};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

/// Whether SQL/Cypher NULL arguments bypass a scalar callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScalarUdfNullPolicy {
    /// Return NULL without invoking the callback when any argument is NULL.
    #[default]
    Propagate,
    /// Invoke the callback and pass NULL values through unchanged.
    Call,
}

/// Type-erased safe Rust callback accepted by a scalar UDF.
pub type ScalarUdfCallback = dyn Fn(&[Value]) -> Result<Value> + Send + Sync + 'static;

/// One immutable scalar-UDF overload. Bound and compiled expressions retain an
/// `Arc` to this value, so registry changes cannot retarget an in-flight query.
#[derive(Clone)]
pub struct ScalarUdf {
    pub name: String,
    pub parameter_types: Vec<LogicalType>,
    pub result_type: LogicalType,
    pub null_policy: ScalarUdfNullPolicy,
    callback: Arc<ScalarUdfCallback>,
}

impl PartialEq for ScalarUdf {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.parameter_types == other.parameter_types
            && self.result_type == other.result_type
            && self.null_policy == other.null_policy
            && Arc::ptr_eq(&self.callback, &other.callback)
    }
}

impl fmt::Debug for ScalarUdf {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScalarUdf")
            .field("name", &self.name)
            .field("parameter_types", &self.parameter_types)
            .field("result_type", &self.result_type)
            .field("null_policy", &self.null_policy)
            .finish_non_exhaustive()
    }
}

impl ScalarUdf {
    pub fn new<F>(
        name: String,
        parameter_types: Vec<LogicalType>,
        result_type: LogicalType,
        null_policy: ScalarUdfNullPolicy,
        callback: F,
    ) -> Self
    where
        F: Fn(&[Value]) -> Result<Value> + Send + Sync + 'static,
    {
        Self {
            name,
            parameter_types,
            result_type,
            null_policy,
            callback: Arc::new(callback),
        }
    }

    /// Invoke the callback across the panic boundary and enforce its declared
    /// output contract. NULL is valid for every declared result type.
    pub fn invoke(&self, arguments: &[Value]) -> Result<Value> {
        if self.null_policy == ScalarUdfNullPolicy::Propagate
            && arguments.iter().any(Value::is_null)
        {
            return Ok(Value::Null);
        }
        let result =
            catch_unwind(AssertUnwindSafe(|| (self.callback)(arguments))).map_err(|panic| {
                let message = if let Some(message) = panic.downcast_ref::<&str>() {
                    (*message).to_string()
                } else if let Some(message) = panic.downcast_ref::<String>() {
                    message.clone()
                } else {
                    "non-string panic payload".to_string()
                };
                Error::runtime(format!("Scalar function {} panicked: {message}", self.name))
            })??;
        if result.is_null()
            || self.result_type == LogicalType::Any
            || result.logical_type() == self.result_type
        {
            Ok(result)
        } else {
            Err(Error::runtime(format!(
                "Scalar function {} returned {}, expected {}.",
                self.name,
                result.logical_type(),
                self.result_type
            )))
        }
    }
}
