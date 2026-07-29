//! Borrowed cells and typed value conversion.

use koko_common::{Error, IntKind, InternalId, Interval, LogicalType, Result, Value};

/// Borrowed physical cell payload.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum CellValue<'a> {
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
pub struct Cell<'a> {
    pub(super) logical_type: &'a LogicalType,
    pub(super) value: CellValue<'a>,
}

impl<'a> Cell<'a> {
    pub const fn logical_type(&self) -> &'a LogicalType {
        self.logical_type
    }

    pub const fn value(&self) -> CellValue<'a> {
        self.value
    }

    pub fn to_owned(self) -> Value {
        match self.value {
            CellValue::Null => Value::Null,
            CellValue::Bool(value) => Value::Bool(value),
            CellValue::Int {
                value,
                kind: IntKind::I64,
            } => Value::Int64(value as i64),
            CellValue::Int { value, kind } => Value::IntX { value, kind },
            CellValue::UInt128(value) => Value::UInt128(value),
            CellValue::Decimal {
                value,
                precision,
                scale,
            } => Value::Decimal {
                value,
                precision,
                scale,
            },
            CellValue::Double(value) => Value::Double(value),
            CellValue::Float(value) => Value::Float(value),
            CellValue::String(value) => Value::String(value.to_string()),
            CellValue::Date(value) => Value::Date(value),
            CellValue::Timestamp(value) => Value::Timestamp(value),
            CellValue::TimestampTz(value) => Value::TimestampTz(value),
            CellValue::Interval(value) => Value::Interval(value),
            CellValue::Uuid(value) => Value::Uuid(value),
            CellValue::InternalId(value) => Value::InternalId(value),
            CellValue::Generic(value) => value.clone(),
        }
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
