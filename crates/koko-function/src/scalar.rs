//! Typed builtin-scalar identity and generated descriptor lookup.

pub use crate::catalog_data::{
    BUILTIN_DESCRIPTORS, BuiltinDescriptor, BuiltinFunction, BuiltinScalar, CastTarget,
    CatalogTypeId, DigestAlgorithm, FunctionCatalogEntry, FunctionCatalogKind, OverloadDescriptor,
    RoundMode, resolve_builtin, resolve_builtin_scalar,
};
use koko_common::{IntKind, LogicalType};

impl CastTarget {
    /// The concrete result type produced by a typed cast alias.
    pub fn logical_type(self) -> LogicalType {
        match self {
            Self::Int8 => LogicalType::Int(IntKind::I8),
            Self::Int16 => LogicalType::Int(IntKind::I16),
            Self::Int32 => LogicalType::Int(IntKind::I32),
            Self::Int64 => LogicalType::Int64,
            Self::Int128 => LogicalType::Int(IntKind::I128),
            Self::UInt8 => LogicalType::Int(IntKind::U8),
            Self::UInt16 => LogicalType::Int(IntKind::U16),
            Self::UInt32 => LogicalType::Int(IntKind::U32),
            Self::UInt64 => LogicalType::Int(IntKind::U64),
            Self::UInt128 => LogicalType::UInt128,
            Self::Serial => LogicalType::Serial,
            Self::Double => LogicalType::Double,
            Self::Float => LogicalType::Float,
            Self::Bool => LogicalType::Bool,
            Self::String => LogicalType::String,
            Self::Blob => LogicalType::Blob,
            Self::Uuid => LogicalType::Uuid,
            Self::Date => LogicalType::Date,
        }
    }
}

impl CatalogTypeId {
    /// Representative runtime type used by overload compatibility checks.
    pub fn logical_type(self) -> Option<LogicalType> {
        Some(match self {
            Self::Any => LogicalType::Any,
            Self::Bool => LogicalType::Bool,
            Self::Int8 => LogicalType::Int(IntKind::I8),
            Self::Int16 => LogicalType::Int(IntKind::I16),
            Self::Int32 => LogicalType::Int(IntKind::I32),
            Self::Int64 => LogicalType::Int64,
            Self::Int128 => LogicalType::Int(IntKind::I128),
            Self::UInt8 => LogicalType::Int(IntKind::U8),
            Self::UInt16 => LogicalType::Int(IntKind::U16),
            Self::UInt32 => LogicalType::Int(IntKind::U32),
            Self::UInt64 => LogicalType::Int(IntKind::U64),
            Self::UInt128 => LogicalType::UInt128,
            Self::Serial => LogicalType::Serial,
            Self::Decimal => LogicalType::Decimal(0, 0),
            Self::Double => LogicalType::Double,
            Self::Float => LogicalType::Float,
            Self::String => LogicalType::String,
            Self::Date => LogicalType::Date,
            Self::Timestamp => LogicalType::Timestamp,
            Self::TimestampNs => LogicalType::TimestampNs,
            Self::TimestampMs => LogicalType::TimestampMs,
            Self::TimestampSec => LogicalType::TimestampSec,
            Self::TimestampTz => LogicalType::TimestampTz,
            Self::Interval => LogicalType::Interval,
            Self::Uuid => LogicalType::Uuid,
            Self::Blob => LogicalType::Blob,
            Self::List => LogicalType::List(Box::new(LogicalType::Any)),
            Self::Array => LogicalType::Array(Box::new(LogicalType::Any), 0),
            Self::Struct => LogicalType::Struct(Vec::new()),
            Self::Map => LogicalType::Map(Box::new(LogicalType::Any), Box::new(LogicalType::Any)),
            Self::Union => LogicalType::Union(Vec::new()),
            Self::Node => LogicalType::Node(koko_common::TableId(0)),
            Self::Rel => LogicalType::Rel(koko_common::TableId(0)),
            Self::RecursiveRel => LogicalType::RecursiveRel,
            Self::InternalId => LogicalType::InternalId,
            Self::Json => return None,
        })
    }
}
