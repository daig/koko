//! Primary-key index representation and lookup keys.

use koko_common::Value;

/// Hashable representation of every supported primary-key value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum PkKey {
    Int(i128),
    UInt(u128),
    Str(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Uuid(u128),
    Bits(u64),
    Dec(i128, u8),
}

impl PkKey {
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Bool(value) => Some(Self::Bool(*value)),
            Value::String(value) => Some(Self::Str(value.clone())),
            Value::Blob(value) => Some(Self::Bytes(value.clone())),
            Value::Uuid(value) => Some(Self::Uuid(*value)),
            Value::Date(value) => Some(Self::Int(*value as i128)),
            Value::Timestamp(value) | Value::TimestampTz(value) => Some(Self::Int(*value as i128)),
            Value::Float(value) => Some(Self::Bits((*value as f64 + 0.0).to_bits())),
            Value::Double(value) => Some(Self::Bits((*value + 0.0).to_bits())),
            Value::Decimal { value, scale, .. } => Some(Self::Dec(*value, *scale)),
            _ => value
                .as_int128()
                .map(Self::Int)
                .or_else(|| value.as_u128().map(Self::UInt)),
        }
    }
}
