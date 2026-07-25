//! Native Rust Arrow interchange for query results and table imports.
//!
//! This module owns Arrow schema/value conversion. It deliberately does not expose
//! the Arrow C Data Interface: callers retain ownership of input [`RecordBatch`]es,
//! and returned batches own their Rust Arrow arrays. Imports validate every batch
//! before taking a storage savepoint, reserve tracked memory before conversion, and
//! roll every write in the call back on conversion or storage failure.

use super::QueryResult;
use arrow_array::builder::{BinaryBuilder, FixedSizeBinaryBuilder, StringBuilder};
use arrow_array::types::{
    Date32Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type,
    IntervalMonthDayNanoType, TimestampMicrosecondType, TimestampMillisecondType,
    TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{
    Array, ArrayRef, BinaryArray, BooleanArray, Date32Array, Decimal128Array, FixedSizeBinaryArray,
    FixedSizeListArray, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array,
    IntervalMonthDayNanoArray, ListArray, MapArray, PrimitiveArray, RecordBatch, StringArray,
    StructArray, TimestampMicrosecondArray, TimestampMillisecondArray, TimestampNanosecondArray,
    TimestampSecondArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array, UnionArray,
};
use arrow_buffer::{NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow_schema::{
    ArrowError, DataType, Field, Fields, IntervalUnit, Schema, TimeUnit, UnionFields, UnionMode,
};
use koko_catalog::{Catalog, NodeTable, RelTable, TableKind};
use koko_common::temporal::Interval;
use koko_common::types::IntKind;
use koko_common::vector::{ColumnData, DataChunk};
use koko_common::{Error, InternalId, LogicalType, MemoryTracker, Result, VECTOR_CAPACITY, Value};
use koko_storage::{SharedStorage, StorageWriteHandle};
use std::collections::HashMap;
use std::sync::Arc;

const LOGICAL_TYPE_METADATA: &str = "koko.logical_type";

/// Explicit catalog, storage, and memory capabilities for Arrow import.
pub(crate) struct ArrowImportContext<'a> {
    catalog: &'a Catalog,
    storage: &'a SharedStorage,
    memory: &'a MemoryTracker,
}

impl<'a> ArrowImportContext<'a> {
    pub(crate) fn new(
        catalog: &'a Catalog,
        storage: &'a SharedStorage,
        memory: &'a MemoryTracker,
    ) -> Self {
        Self {
            catalog,
            storage,
            memory,
        }
    }

    fn catalog(&self) -> &Catalog {
        self.catalog
    }

    fn storage(&self) -> &SharedStorage {
        self.storage
    }

    fn memory(&self) -> &MemoryTracker {
        self.memory
    }
}

fn arrow_error(context: &str, error: ArrowError) -> Error {
    Error::conversion(format!("{context}: {error}"))
}

fn unsupported(ty: &LogicalType) -> Error {
    Error::not_implemented(format!(
        "Arrow interchange does not support Koko logical type {ty}."
    ))
}

fn field_for(name: impl Into<String>, ty: &LogicalType, nullable: bool) -> Result<Field> {
    let mut metadata = HashMap::new();
    metadata.insert(LOGICAL_TYPE_METADATA.to_string(), ty.to_string());
    Ok(Field::new(name, logical_to_data_type(ty)?, nullable).with_metadata(metadata))
}

fn logical_to_data_type(ty: &LogicalType) -> Result<DataType> {
    Ok(match ty {
        LogicalType::Bool => DataType::Boolean,
        LogicalType::Int(kind) => match kind {
            IntKind::I8 => DataType::Int8,
            IntKind::I16 => DataType::Int16,
            IntKind::I32 => DataType::Int32,
            IntKind::I64 => DataType::Int64,
            IntKind::I128 => DataType::FixedSizeBinary(16),
            IntKind::U8 => DataType::UInt8,
            IntKind::U16 => DataType::UInt16,
            IntKind::U32 => DataType::UInt32,
            IntKind::U64 => DataType::UInt64,
        },
        LogicalType::Serial => DataType::Int64,
        LogicalType::UInt128 | LogicalType::Uuid => DataType::FixedSizeBinary(16),
        LogicalType::Decimal(precision, scale) => DataType::Decimal128(*precision, *scale as i8),
        LogicalType::Double => DataType::Float64,
        LogicalType::Float => DataType::Float32,
        LogicalType::String => DataType::Utf8,
        LogicalType::Json => DataType::Utf8,
        LogicalType::Blob => DataType::Binary,
        LogicalType::Date => DataType::Date32,
        LogicalType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        LogicalType::TimestampNs => DataType::Timestamp(TimeUnit::Nanosecond, None),
        LogicalType::TimestampMs => DataType::Timestamp(TimeUnit::Millisecond, None),
        LogicalType::TimestampSec => DataType::Timestamp(TimeUnit::Second, None),
        LogicalType::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        LogicalType::Interval => DataType::Interval(IntervalUnit::MonthDayNano),
        LogicalType::List(child) => DataType::List(Arc::new(field_for("item", child, true)?)),
        LogicalType::Array(child, length) => {
            let length = i32::try_from(*length).map_err(|_| {
                Error::conversion(format!(
                    "Arrow fixed-size list length {length} exceeds i32."
                ))
            })?;
            DataType::FixedSizeList(Arc::new(field_for("item", child, true)?), length)
        }
        LogicalType::Struct(members) => DataType::Struct(
            members
                .iter()
                .map(|(name, member)| field_for(name, member, true).map(Arc::new))
                .collect::<Result<Vec<_>>>()?
                .into(),
        ),
        LogicalType::Map(key, value) => {
            let entries: Fields = vec![
                Arc::new(field_for("key", key, false)?),
                Arc::new(field_for("value", value, true)?),
            ]
            .into();
            let entries = Field::new("entries", DataType::Struct(entries), false);
            DataType::Map(Arc::new(entries), false)
        }
        LogicalType::Union(members) => {
            if members.is_empty() {
                return Err(Error::conversion("Arrow cannot represent an empty UNION."));
            }
            let mut ids = Vec::with_capacity(members.len());
            let mut fields = Vec::with_capacity(members.len());
            for (index, (name, member)) in members.iter().enumerate() {
                ids.push(i8::try_from(index).map_err(|_| {
                    Error::conversion("Arrow UNION supports at most 128 Koko members.")
                })?);
                fields.push(Arc::new(field_for(name, member, true)?));
            }
            DataType::Union(UnionFields::new(ids, fields), UnionMode::Dense)
        }
        LogicalType::InternalId
        | LogicalType::Node(_)
        | LogicalType::Rel(_)
        | LogicalType::RecursiveRel
        | LogicalType::Any => return Err(unsupported(ty)),
    })
}

fn selected_values(chunk: &DataChunk, column: usize) -> Vec<Value> {
    chunk
        .sel
        .iter()
        .map(|position| chunk.columns[column].get_value(position))
        .collect()
}

fn expect_value<'a>(value: &'a Value, expected: &str) -> Result<&'a Value> {
    if value.is_null() {
        Ok(value)
    } else {
        let actual = value.logical_type();
        Err(Error::conversion(format!(
            "Arrow conversion expected {expected}, found {actual}."
        )))
    }
}

macro_rules! primitive_array {
    ($values:expr, Value::IntX { value, kind: $kind:path }, $array:ty, $cast:expr, $name:literal) => {{
        let mut converted = Vec::with_capacity($values.len());
        for value in $values {
            match value {
                Value::Null => converted.push(None),
                Value::IntX { value, kind: $kind } => converted.push(Some($cast(*value)?)),
                other => {
                    expect_value(other, $name)?;
                    unreachable!()
                }
            }
        }
        Arc::new(<$array>::from(converted)) as ArrayRef
    }};
    ($values:expr, $variant:path, $array:ty, $cast:expr, $name:literal) => {{
        let mut converted = Vec::with_capacity($values.len());
        for value in $values {
            match value {
                Value::Null => converted.push(None),
                $variant(value) => converted.push(Some($cast(*value)?)),
                other => {
                    expect_value(other, $name)?;
                    unreachable!()
                }
            }
        }
        Arc::new(<$array>::from(converted)) as ArrayRef
    }};
}

fn fixed_binary_array(values: &[Value], ty: &LogicalType) -> Result<ArrayRef> {
    let mut builder = FixedSizeBinaryBuilder::with_capacity(values.len(), 16);
    for value in values {
        match (ty, value) {
            (_, Value::Null) => builder.append_null(),
            (
                LogicalType::Int(IntKind::I128),
                Value::IntX {
                    value,
                    kind: IntKind::I128,
                },
            ) => {
                builder
                    .append_value(value.to_be_bytes())
                    .map_err(|e| arrow_error("building INT128 Arrow array", e))?;
            }
            (LogicalType::UInt128, Value::UInt128(value))
            | (LogicalType::Uuid, Value::Uuid(value)) => {
                builder
                    .append_value(value.to_be_bytes())
                    .map_err(|e| arrow_error("building 128-bit Arrow array", e))?;
            }
            (_, other) => {
                return Err(Error::conversion(format!(
                    "Arrow conversion expected {ty}, found {}.",
                    other.logical_type()
                )));
            }
        }
    }
    Ok(Arc::new(builder.finish()))
}

fn build_array(ty: &LogicalType, values: &[Value]) -> Result<ArrayRef> {
    Ok(match ty {
        LogicalType::Bool => primitive_array!(
            values,
            Value::Bool,
            BooleanArray,
            |v: bool| Ok::<_, Error>(v),
            "BOOL"
        ),
        LogicalType::Int(IntKind::I8) => primitive_array!(
            values,
            Value::IntX {
                value,
                kind: IntKind::I8
            },
            Int8Array,
            |v: i128| i8::try_from(v)
                .map_err(|_| Error::overflow("INT8 Arrow conversion overflow.")),
            "INT8"
        ),
        LogicalType::Int(IntKind::I16) => primitive_array!(
            values,
            Value::IntX {
                value,
                kind: IntKind::I16
            },
            Int16Array,
            |v: i128| i16::try_from(v)
                .map_err(|_| Error::overflow("INT16 Arrow conversion overflow.")),
            "INT16"
        ),
        LogicalType::Int(IntKind::I32) => primitive_array!(
            values,
            Value::IntX {
                value,
                kind: IntKind::I32
            },
            Int32Array,
            |v: i128| i32::try_from(v)
                .map_err(|_| Error::overflow("INT32 Arrow conversion overflow.")),
            "INT32"
        ),
        LogicalType::Int(IntKind::I64) | LogicalType::Serial => {
            let mut converted = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::Null => converted.push(None),
                    Value::Int64(value) => converted.push(Some(*value)),
                    Value::IntX {
                        value,
                        kind: IntKind::I64,
                    } => converted
                        .push(Some(i64::try_from(*value).map_err(|_| {
                            Error::overflow("INT64 Arrow conversion overflow.")
                        })?)),
                    other => {
                        return Err(Error::conversion(format!(
                            "Arrow conversion expected INT64, found {}.",
                            other.logical_type()
                        )));
                    }
                }
            }
            Arc::new(Int64Array::from(converted))
        }
        LogicalType::Int(IntKind::I128) | LogicalType::UInt128 | LogicalType::Uuid => {
            fixed_binary_array(values, ty)?
        }
        LogicalType::Int(IntKind::U8) => primitive_array!(
            values,
            Value::IntX {
                value,
                kind: IntKind::U8
            },
            UInt8Array,
            |v: i128| u8::try_from(v)
                .map_err(|_| Error::overflow("UINT8 Arrow conversion overflow.")),
            "UINT8"
        ),
        LogicalType::Int(IntKind::U16) => primitive_array!(
            values,
            Value::IntX {
                value,
                kind: IntKind::U16
            },
            UInt16Array,
            |v: i128| u16::try_from(v)
                .map_err(|_| Error::overflow("UINT16 Arrow conversion overflow.")),
            "UINT16"
        ),
        LogicalType::Int(IntKind::U32) => primitive_array!(
            values,
            Value::IntX {
                value,
                kind: IntKind::U32
            },
            UInt32Array,
            |v: i128| u32::try_from(v)
                .map_err(|_| Error::overflow("UINT32 Arrow conversion overflow.")),
            "UINT32"
        ),
        LogicalType::Int(IntKind::U64) => primitive_array!(
            values,
            Value::IntX {
                value,
                kind: IntKind::U64
            },
            UInt64Array,
            |v: i128| u64::try_from(v)
                .map_err(|_| Error::overflow("UINT64 Arrow conversion overflow.")),
            "UINT64"
        ),
        LogicalType::Decimal(precision, scale) => {
            let mut converted = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::Null => converted.push(None),
                    Value::Decimal {
                        value,
                        precision: p,
                        scale: s,
                    } if p == precision && s == scale => converted.push(Some(*value)),
                    other => {
                        return Err(Error::conversion(format!(
                            "Arrow conversion expected {ty}, found {}.",
                            other.logical_type()
                        )));
                    }
                }
            }
            Arc::new(
                Decimal128Array::from(converted)
                    .with_precision_and_scale(*precision, *scale as i8)
                    .map_err(|e| arrow_error("building DECIMAL Arrow array", e))?,
            )
        }
        LogicalType::Double => primitive_array!(
            values,
            Value::Double,
            Float64Array,
            |v: f64| Ok::<_, Error>(v),
            "DOUBLE"
        ),
        LogicalType::Float => primitive_array!(
            values,
            Value::Float,
            Float32Array,
            |v: f32| Ok::<_, Error>(v),
            "FLOAT"
        ),
        LogicalType::String => {
            let mut builder = StringBuilder::with_capacity(
                values.len(),
                values
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.len(),
                        _ => 0,
                    })
                    .sum(),
            );
            for value in values {
                match value {
                    Value::Null => builder.append_null(),
                    Value::String(value) => builder.append_value(value),
                    other => {
                        return Err(Error::conversion(format!(
                            "Arrow conversion expected STRING, found {}.",
                            other.logical_type()
                        )));
                    }
                }
            }
            Arc::new(builder.finish())
        }
        LogicalType::Json => {
            let rendered: Vec<Option<String>> = values
                .iter()
                .map(|value| match value {
                    Value::Null => Ok(None),
                    Value::Json(value) => Ok(Some(value.render())),
                    other => Err(Error::conversion(format!(
                        "Arrow conversion expected JSON, found {}.",
                        other.logical_type()
                    ))),
                })
                .collect::<Result<_>>()?;
            Arc::new(StringArray::from(rendered))
        }
        LogicalType::Blob => {
            let mut builder = BinaryBuilder::with_capacity(
                values.len(),
                values
                    .iter()
                    .map(|v| match v {
                        Value::Blob(b) => b.len(),
                        _ => 0,
                    })
                    .sum(),
            );
            for value in values {
                match value {
                    Value::Null => builder.append_null(),
                    Value::Blob(value) => builder.append_value(value),
                    other => {
                        return Err(Error::conversion(format!(
                            "Arrow conversion expected BLOB, found {}.",
                            other.logical_type()
                        )));
                    }
                }
            }
            Arc::new(builder.finish())
        }
        LogicalType::Date => primitive_array!(
            values,
            Value::Date,
            Date32Array,
            |v: i32| Ok::<_, Error>(v),
            "DATE"
        ),
        LogicalType::Timestamp => primitive_array!(
            values,
            Value::Timestamp,
            TimestampMicrosecondArray,
            |v: i64| Ok::<_, Error>(v),
            "TIMESTAMP"
        ),
        LogicalType::TimestampNs => primitive_array!(
            values,
            Value::Timestamp,
            TimestampNanosecondArray,
            |v: i64| v
                .checked_mul(1_000)
                .ok_or_else(|| Error::overflow("TIMESTAMP_NS Arrow conversion overflow.")),
            "TIMESTAMP_NS"
        ),
        LogicalType::TimestampMs => primitive_array!(
            values,
            Value::Timestamp,
            TimestampMillisecondArray,
            |v: i64| Ok::<_, Error>(v.div_euclid(1_000)),
            "TIMESTAMP_MS"
        ),
        LogicalType::TimestampSec => primitive_array!(
            values,
            Value::Timestamp,
            TimestampSecondArray,
            |v: i64| Ok::<_, Error>(v.div_euclid(1_000_000)),
            "TIMESTAMP_SEC"
        ),
        LogicalType::TimestampTz => {
            let array = primitive_array!(
                values,
                Value::TimestampTz,
                TimestampMicrosecondArray,
                |v: i64| Ok::<_, Error>(v),
                "TIMESTAMP_TZ"
            );
            let primitive = array
                .as_any()
                .downcast_ref::<TimestampMicrosecondArray>()
                .expect("constructed timestamp")
                .clone()
                .with_timezone("UTC");
            Arc::new(primitive)
        }
        LogicalType::Interval => {
            let mut converted = Vec::with_capacity(values.len());
            for value in values {
                match value {
                    Value::Null => converted.push(None),
                    Value::Interval(Interval {
                        months,
                        days,
                        micros,
                    }) => {
                        let nanos = micros.checked_mul(1_000).ok_or_else(|| {
                            Error::overflow("INTERVAL Arrow nanosecond conversion overflow.")
                        })?;
                        converted.push(Some(IntervalMonthDayNanoType::make_value(
                            *months, *days, nanos,
                        )));
                    }
                    other => {
                        return Err(Error::conversion(format!(
                            "Arrow conversion expected INTERVAL, found {}.",
                            other.logical_type()
                        )));
                    }
                }
            }
            Arc::new(IntervalMonthDayNanoArray::from(converted))
        }
        LogicalType::List(child) => build_list_array(child, values)?,
        LogicalType::Array(child, length) => build_fixed_list_array(child, *length, values)?,
        LogicalType::Struct(members) => build_struct_array(members, values)?,
        LogicalType::Map(key, value) => build_map_array(key, value, values)?,
        LogicalType::Union(members) => build_union_array(members, values)?,
        LogicalType::InternalId
        | LogicalType::Node(_)
        | LogicalType::Rel(_)
        | LogicalType::RecursiveRel
        | LogicalType::Any => return Err(unsupported(ty)),
    })
}

fn validity_buffer(valid: Vec<bool>) -> Option<NullBuffer> {
    (!valid.iter().all(|value| *value)).then(|| NullBuffer::from(valid))
}

fn build_list_array(child: &LogicalType, values: &[Value]) -> Result<ArrayRef> {
    let mut offsets = Vec::with_capacity(values.len() + 1);
    let mut children = Vec::new();
    let mut valid = Vec::with_capacity(values.len());
    offsets.push(0i32);
    for value in values {
        match value {
            Value::Null => valid.push(false),
            Value::List(items) => {
                valid.push(true);
                children.extend(items.iter().cloned());
            }
            other => {
                return Err(Error::conversion(format!(
                    "Arrow conversion expected LIST, found {}.",
                    other.logical_type()
                )));
            }
        }
        offsets.push(
            i32::try_from(children.len())
                .map_err(|_| Error::conversion("Arrow LIST child count exceeds i32."))?,
        );
    }
    let children = build_array(child, &children)?;
    let array = ListArray::try_new(
        Arc::new(field_for("item", child, true)?),
        OffsetBuffer::new(ScalarBuffer::from(offsets)),
        children,
        validity_buffer(valid),
    )
    .map_err(|e| arrow_error("building LIST Arrow array", e))?;
    Ok(Arc::new(array))
}

fn build_fixed_list_array(child: &LogicalType, length: u64, values: &[Value]) -> Result<ArrayRef> {
    let length_i32 =
        i32::try_from(length).map_err(|_| Error::conversion("Arrow ARRAY length exceeds i32."))?;
    let length = usize::try_from(length)
        .map_err(|_| Error::conversion("Arrow ARRAY length exceeds usize."))?;
    let mut children = Vec::with_capacity(values.len().saturating_mul(length));
    let mut valid = Vec::with_capacity(values.len());
    for value in values {
        match value {
            Value::Null => {
                valid.push(false);
                children.extend(std::iter::repeat_n(Value::Null, length));
            }
            Value::List(items) if items.len() == length => {
                valid.push(true);
                children.extend(items.iter().cloned());
            }
            Value::List(items) => {
                return Err(Error::conversion(format!(
                    "ARRAY expects {length} elements, found {}.",
                    items.len()
                )));
            }
            other => {
                return Err(Error::conversion(format!(
                    "Arrow conversion expected ARRAY, found {}.",
                    other.logical_type()
                )));
            }
        }
    }
    let array = FixedSizeListArray::try_new(
        Arc::new(field_for("item", child, true)?),
        length_i32,
        build_array(child, &children)?,
        validity_buffer(valid),
    )
    .map_err(|e| arrow_error("building ARRAY Arrow array", e))?;
    Ok(Arc::new(array))
}

fn build_struct_array(members: &[(String, LogicalType)], values: &[Value]) -> Result<ArrayRef> {
    let mut columns: Vec<Vec<Value>> = members
        .iter()
        .map(|_| Vec::with_capacity(values.len()))
        .collect();
    let mut valid = Vec::with_capacity(values.len());
    for value in values {
        match value {
            Value::Null => {
                valid.push(false);
                for column in &mut columns {
                    column.push(Value::Null);
                }
            }
            Value::Struct(fields) => {
                valid.push(true);
                for (index, (name, _)) in members.iter().enumerate() {
                    let value = fields
                        .iter()
                        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                        .ok_or_else(|| {
                            Error::conversion(format!("STRUCT value is missing field `{name}`."))
                        })?;
                    columns[index].push(value);
                }
            }
            other => {
                return Err(Error::conversion(format!(
                    "Arrow conversion expected STRUCT, found {}.",
                    other.logical_type()
                )));
            }
        }
    }
    let fields: Fields = members
        .iter()
        .map(|(name, ty)| field_for(name, ty, true).map(Arc::new))
        .collect::<Result<Vec<_>>>()?
        .into();
    let arrays = members
        .iter()
        .zip(columns)
        .map(|((_, ty), values)| build_array(ty, &values))
        .collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(
        StructArray::try_new(fields, arrays, validity_buffer(valid))
            .map_err(|e| arrow_error("building STRUCT Arrow array", e))?,
    ))
}

fn build_map_array(key: &LogicalType, value: &LogicalType, values: &[Value]) -> Result<ArrayRef> {
    let mut offsets = Vec::with_capacity(values.len() + 1);
    let mut keys = Vec::new();
    let mut vals = Vec::new();
    let mut valid = Vec::with_capacity(values.len());
    offsets.push(0i32);
    for item in values {
        match item {
            Value::Null => valid.push(false),
            Value::Map(entries) => {
                valid.push(true);
                for (map_key, map_value) in entries {
                    if map_key.is_null() {
                        return Err(Error::conversion("Arrow MAP keys cannot be NULL."));
                    }
                    keys.push(map_key.clone());
                    vals.push(map_value.clone());
                }
            }
            other => {
                return Err(Error::conversion(format!(
                    "Arrow conversion expected MAP, found {}.",
                    other.logical_type()
                )));
            }
        }
        offsets.push(
            i32::try_from(keys.len())
                .map_err(|_| Error::conversion("Arrow MAP entry count exceeds i32."))?,
        );
    }
    let fields: Fields = vec![
        Arc::new(field_for("key", key, false)?),
        Arc::new(field_for("value", value, true)?),
    ]
    .into();
    let entries = StructArray::try_new(
        fields.clone(),
        vec![build_array(key, &keys)?, build_array(value, &vals)?],
        None,
    )
    .map_err(|e| arrow_error("building MAP entries", e))?;
    let entry_field = Arc::new(Field::new("entries", DataType::Struct(fields), false));
    Ok(Arc::new(
        MapArray::try_new(
            entry_field,
            OffsetBuffer::new(ScalarBuffer::from(offsets)),
            entries,
            validity_buffer(valid),
            false,
        )
        .map_err(|e| arrow_error("building MAP Arrow array", e))?,
    ))
}

fn build_union_array(members: &[(String, LogicalType)], values: &[Value]) -> Result<ArrayRef> {
    if members.is_empty() {
        return Err(Error::conversion("Arrow cannot represent an empty UNION."));
    }
    let mut type_ids = Vec::with_capacity(values.len());
    let mut offsets = Vec::with_capacity(values.len());
    let mut children: Vec<Vec<Value>> = members.iter().map(|_| Vec::new()).collect();
    for item in values {
        let (tag, payload) = match item {
            Value::Null => (0usize, Value::Null),
            Value::Union {
                variants,
                tag,
                value,
            } if variants == members && *tag < members.len() => (*tag, value.as_ref().clone()),
            Value::Union { .. } => {
                return Err(Error::conversion(
                    "UNION value members do not match the result schema.",
                ));
            }
            other => {
                return Err(Error::conversion(format!(
                    "Arrow conversion expected UNION, found {}.",
                    other.logical_type()
                )));
            }
        };
        type_ids
            .push(i8::try_from(tag).map_err(|_| Error::conversion("Arrow UNION tag exceeds i8."))?);
        offsets.push(
            i32::try_from(children[tag].len())
                .map_err(|_| Error::conversion("Arrow UNION child offset exceeds i32."))?,
        );
        children[tag].push(payload);
    }
    let fields = UnionFields::new(
        (0..members.len())
            .map(|index| index as i8)
            .collect::<Vec<_>>(),
        members
            .iter()
            .map(|(name, ty)| field_for(name, ty, true).map(Arc::new))
            .collect::<Result<Vec<_>>>()?,
    );
    let arrays = members
        .iter()
        .zip(children)
        .map(|((_, ty), values)| build_array(ty, &values))
        .collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(
        UnionArray::try_new(
            fields,
            ScalarBuffer::from(type_ids),
            Some(ScalarBuffer::from(offsets)),
            arrays,
        )
        .map_err(|e| arrow_error("building UNION Arrow array", e))?,
    ))
}

impl QueryResult {
    /// Convert this result to native Rust Arrow batches, one [`RecordBatch`] per
    /// existing [`DataChunk`]. An empty result produces one zero-row batch so its
    /// exact schema remains available to downstream writers. No row matrix is
    /// materialized; each bounded result column is converted once and Arrow fields
    /// carry `koko.logical_type`. Graph entities, paths, internal ids, and
    /// unresolved columns return an explicit unsupported-type error rather than
    /// being stringified.
    pub fn to_arrow_record_batches(&self) -> Result<Vec<RecordBatch>> {
        let fields = self
            .schema()
            .iter()
            .map(|column| field_for(column.name(), column.logical_type(), true).map(Arc::new))
            .collect::<Result<Vec<_>>>()?;
        let schema = Arc::new(Schema::new(fields));
        if self.batches().is_empty() {
            return Ok(vec![RecordBatch::new_empty(schema)]);
        }
        self.batches()
            .iter()
            .map(|batch| {
                let arrays = self
                    .schema()
                    .iter()
                    .enumerate()
                    .map(|(index, column)| {
                        let values = selected_values(batch, index);
                        build_array(column.logical_type(), &values)
                    })
                    .collect::<Result<Vec<_>>>()?;
                RecordBatch::try_new(Arc::clone(&schema), arrays)
                    .map_err(|e| arrow_error("building Arrow record batch", e))
            })
            .collect()
    }
}

#[derive(Clone)]
enum ImportTarget {
    Node(NodeTable),
    Rel(RelTable, Vec<NodeTable>, Vec<NodeTable>),
}

impl ImportTarget {
    fn resolve(catalog: &koko_catalog::Catalog, table: &str) -> Result<Self> {
        let id = catalog
            .table_id(table)
            .ok_or_else(|| Error::binder(format!("Table `{table}` does not exist.")))?;
        match catalog.table_kind(id) {
            Some(TableKind::Node) => Ok(Self::Node(
                catalog
                    .node_table(id)
                    .expect("catalog kind is node")
                    .clone(),
            )),
            Some(TableKind::Rel) => {
                let rel = catalog.rel_table(id).expect("catalog kind is rel").clone();
                let from = rel
                    .pairs
                    .iter()
                    .map(|(id, _)| {
                        catalog
                            .node_table(*id)
                            .expect("relationship endpoint exists")
                            .clone()
                    })
                    .collect();
                let to = rel
                    .pairs
                    .iter()
                    .map(|(_, id)| {
                        catalog
                            .node_table(*id)
                            .expect("relationship endpoint exists")
                            .clone()
                    })
                    .collect();
                Ok(Self::Rel(rel, from, to))
            }
            None => Err(Error::binder(format!("Table `{table}` does not exist."))),
        }
    }

    fn expected(&self) -> Result<Vec<(&str, &LogicalType, bool)>> {
        match self {
            ImportTarget::Node(table) => Ok(table
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    (column.name.as_str(), &column.ty, index == table.primary_key)
                })
                .collect()),
            ImportTarget::Rel(table, from, to) => {
                let first_from = &from[0].columns[from[0].primary_key].ty;
                let first_to = &to[0].columns[to[0].primary_key].ty;
                if from
                    .iter()
                    .any(|node| &node.columns[node.primary_key].ty != first_from)
                    || to
                        .iter()
                        .any(|node| &node.columns[node.primary_key].ty != first_to)
                {
                    return Err(Error::binder(format!(
                        "Relationship group `{}` has endpoint primary keys with incompatible logical types.",
                        table.name
                    )));
                }
                let mut fields = Vec::with_capacity(table.columns.len() + 2);
                fields.push(("from", first_from, true));
                fields.push(("to", first_to, true));
                fields.extend(
                    table
                        .columns
                        .iter()
                        .map(|column| (column.name.as_str(), &column.ty, false)),
                );
                Ok(fields)
            }
        }
    }
}

fn metadata_compatible(field: &Field, expected: &LogicalType) -> bool {
    field
        .metadata()
        .get(LOGICAL_TYPE_METADATA)
        .is_none_or(|actual| actual.eq_ignore_ascii_case(&expected.to_string()))
}

fn data_type_compatible(actual: &DataType, expected: &LogicalType) -> bool {
    match (actual, expected) {
        (DataType::Boolean, LogicalType::Bool)
        | (DataType::Int8, LogicalType::Int(IntKind::I8))
        | (DataType::Int16, LogicalType::Int(IntKind::I16))
        | (DataType::Int32, LogicalType::Int(IntKind::I32))
        | (DataType::Int64, LogicalType::Int(IntKind::I64) | LogicalType::Serial)
        | (DataType::UInt8, LogicalType::Int(IntKind::U8))
        | (DataType::UInt16, LogicalType::Int(IntKind::U16))
        | (DataType::UInt32, LogicalType::Int(IntKind::U32))
        | (DataType::UInt64, LogicalType::Int(IntKind::U64))
        | (
            DataType::FixedSizeBinary(16),
            LogicalType::Int(IntKind::I128) | LogicalType::UInt128 | LogicalType::Uuid,
        )
        | (DataType::Float64, LogicalType::Double)
        | (DataType::Float32, LogicalType::Float)
        | (DataType::Utf8, LogicalType::String | LogicalType::Json)
        | (DataType::Binary, LogicalType::Blob)
        | (DataType::Date32, LogicalType::Date)
        | (DataType::Interval(IntervalUnit::MonthDayNano), LogicalType::Interval) => true,
        (DataType::Decimal128(p, s), LogicalType::Decimal(ep, es)) => *p == *ep && *s == *es as i8,
        (DataType::Timestamp(TimeUnit::Microsecond, None), LogicalType::Timestamp) => true,
        (DataType::Timestamp(TimeUnit::Nanosecond, None), LogicalType::TimestampNs) => true,
        (DataType::Timestamp(TimeUnit::Millisecond, None), LogicalType::TimestampMs) => true,
        (DataType::Timestamp(TimeUnit::Second, None), LogicalType::TimestampSec) => true,
        (DataType::Timestamp(TimeUnit::Microsecond, Some(_)), LogicalType::TimestampTz) => true,
        (DataType::List(field), LogicalType::List(child)) => {
            metadata_compatible(field, child) && data_type_compatible(field.data_type(), child)
        }
        (DataType::FixedSizeList(field, length), LogicalType::Array(child, expected_length)) => {
            *length >= 0
                && *length as u64 == *expected_length
                && metadata_compatible(field, child)
                && data_type_compatible(field.data_type(), child)
        }
        (DataType::Struct(fields), LogicalType::Struct(members)) => {
            fields.len() == members.len()
                && fields.iter().zip(members).all(|(field, (name, ty))| {
                    field.name().eq_ignore_ascii_case(name)
                        && metadata_compatible(field, ty)
                        && data_type_compatible(field.data_type(), ty)
                })
        }
        (DataType::Map(entries, _), LogicalType::Map(key, value)) => match entries.data_type() {
            DataType::Struct(fields) if fields.len() == 2 => {
                data_type_compatible(fields[0].data_type(), key)
                    && data_type_compatible(fields[1].data_type(), value)
            }
            _ => false,
        },
        (DataType::Union(fields, UnionMode::Dense), LogicalType::Union(members)) => {
            fields.len() == members.len()
                && fields.iter().zip(members).all(|((_, field), (name, ty))| {
                    field.name().eq_ignore_ascii_case(name)
                        && metadata_compatible(field, ty)
                        && data_type_compatible(field.data_type(), ty)
                })
        }
        _ => false,
    }
}

fn field_mapping(
    batch: &RecordBatch,
    expected: &[(&str, &LogicalType, bool)],
) -> Result<Vec<usize>> {
    if batch.num_rows() > VECTOR_CAPACITY {
        return Err(Error::conversion(format!(
            "Arrow batch has {} rows; the maximum is VECTOR_CAPACITY ({VECTOR_CAPACITY}).",
            batch.num_rows()
        )));
    }
    let mut names = HashMap::with_capacity(batch.num_columns());
    for (index, field) in batch.schema().fields().iter().enumerate() {
        let folded = field.name().to_ascii_lowercase();
        if names.insert(folded, index).is_some() {
            return Err(Error::binder(format!(
                "Arrow schema contains duplicate field name `{}` (case-insensitive).",
                field.name()
            )));
        }
    }
    if names.len() != expected.len() {
        return Err(Error::binder(format!(
            "Arrow schema has {} fields, but target table requires {}.",
            names.len(),
            expected.len()
        )));
    }
    let mut mapping = Vec::with_capacity(expected.len());
    for (name, ty, non_null) in expected {
        let index = *names.get(&name.to_ascii_lowercase()).ok_or_else(|| {
            Error::binder(format!("Arrow schema is missing required field `{name}`."))
        })?;
        let schema = batch.schema();
        let field = schema.field(index);
        if !metadata_compatible(field, ty) || !data_type_compatible(field.data_type(), ty) {
            return Err(Error::binder(format!(
                "Arrow field `{}` has type {}, but target column `{name}` requires {ty}.",
                field.name(),
                field.data_type()
            )));
        }
        if *non_null && batch.column(index).null_count() != 0 {
            return Err(Error::runtime(format!(
                "Arrow field `{name}` contains NULL, but the target column is non-nullable."
            )));
        }
        for row in 0..batch.num_rows() {
            validate_arrow_value(batch.column(index).as_ref(), row, ty)?;
        }
        mapping.push(index);
    }
    Ok(mapping)
}

fn downcast<'a, T: 'static>(array: &'a dyn Array, expected: &str) -> Result<&'a T> {
    array
        .as_any()
        .downcast_ref::<T>()
        .ok_or_else(|| Error::conversion(format!("Arrow array is not physical type {expected}.")))
}

fn primitive_value<T, F>(array: &dyn Array, row: usize, expected: &str, convert: F) -> Result<Value>
where
    T: arrow_array::types::ArrowPrimitiveType,
    F: FnOnce(T::Native) -> Result<Value>,
{
    convert(downcast::<PrimitiveArray<T>>(array, expected)?.value(row))
}

fn union_tag(array: &UnionArray, type_id: i8) -> Result<usize> {
    let DataType::Union(fields, UnionMode::Dense) = array.data_type() else {
        return Err(Error::conversion("Arrow array is not a dense UNION."));
    };
    fields
        .iter()
        .enumerate()
        .find_map(|(tag, (candidate, _))| (candidate == type_id).then_some(tag))
        .ok_or_else(|| Error::conversion(format!("Arrow UNION has unknown type id {type_id}.")))
}

fn validate_arrow_value(array: &dyn Array, row: usize, ty: &LogicalType) -> Result<()> {
    if array.is_null(row) {
        return Ok(());
    }
    match ty {
        LogicalType::TimestampMs => {
            let value =
                downcast::<TimestampMillisecondArray>(array, "Timestamp(Millisecond)")?.value(row);
            value
                .checked_mul(1_000)
                .ok_or_else(|| Error::overflow("TIMESTAMP_MS import overflow."))?;
        }
        LogicalType::TimestampSec => {
            let value = downcast::<TimestampSecondArray>(array, "Timestamp(Second)")?.value(row);
            value
                .checked_mul(1_000_000)
                .ok_or_else(|| Error::overflow("TIMESTAMP_SEC import overflow."))?;
        }
        LogicalType::Interval => {
            let value =
                downcast::<IntervalMonthDayNanoArray>(array, "Interval(MonthDayNano)")?.value(row);
            if value.nanoseconds % 1_000 != 0 {
                return Err(Error::conversion(
                    "Arrow INTERVAL nanoseconds are not exactly representable as Koko microseconds.",
                ));
            }
        }
        LogicalType::List(child) => {
            let values = downcast::<ListArray>(array, "List")?.value(row);
            for index in 0..values.len() {
                validate_arrow_value(values.as_ref(), index, child)?;
            }
        }
        LogicalType::Array(child, _) => {
            let values = downcast::<FixedSizeListArray>(array, "FixedSizeList")?.value(row);
            for index in 0..values.len() {
                validate_arrow_value(values.as_ref(), index, child)?;
            }
        }
        LogicalType::Struct(members) => {
            let array = downcast::<StructArray>(array, "Struct")?;
            for (index, (_, member)) in members.iter().enumerate() {
                validate_arrow_value(array.column(index).as_ref(), row, member)?;
            }
        }
        LogicalType::Map(key, value) => {
            let entries = downcast::<MapArray>(array, "Map")?.value(row);
            for index in 0..entries.len() {
                if entries.column(0).is_null(index) {
                    return Err(Error::conversion("Arrow MAP keys cannot be NULL."));
                }
                validate_arrow_value(entries.column(0).as_ref(), index, key)?;
                validate_arrow_value(entries.column(1).as_ref(), index, value)?;
            }
        }
        LogicalType::Union(members) => {
            let array = downcast::<UnionArray>(array, "DenseUnion")?;
            let type_id = array.type_id(row);
            let tag = union_tag(array, type_id)?;
            validate_arrow_value(
                array.child(type_id).as_ref(),
                array.value_offset(row),
                &members[tag].1,
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn array_value(array: &dyn Array, row: usize, ty: &LogicalType) -> Result<Value> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    Ok(match ty {
        LogicalType::Bool => Value::Bool(downcast::<BooleanArray>(array, "Boolean")?.value(row)),
        LogicalType::Int(IntKind::I8) => primitive_value::<Int8Type, _>(array, row, "Int8", |v| {
            Ok(Value::IntX {
                value: v as i128,
                kind: IntKind::I8,
            })
        })?,
        LogicalType::Int(IntKind::I16) => {
            primitive_value::<Int16Type, _>(array, row, "Int16", |v| {
                Ok(Value::IntX {
                    value: v as i128,
                    kind: IntKind::I16,
                })
            })?
        }
        LogicalType::Int(IntKind::I32) => {
            primitive_value::<Int32Type, _>(array, row, "Int32", |v| {
                Ok(Value::IntX {
                    value: v as i128,
                    kind: IntKind::I32,
                })
            })?
        }
        LogicalType::Int(IntKind::I64) | LogicalType::Serial => {
            primitive_value::<Int64Type, _>(array, row, "Int64", |v| Ok(Value::Int64(v)))?
        }
        LogicalType::Int(IntKind::I128) => {
            let bytes: [u8; 16] = downcast::<FixedSizeBinaryArray>(array, "FixedSizeBinary(16)")?
                .value(row)
                .try_into()
                .expect("validated fixed width");
            Value::IntX {
                value: i128::from_be_bytes(bytes),
                kind: IntKind::I128,
            }
        }
        LogicalType::Int(IntKind::U8) => {
            primitive_value::<UInt8Type, _>(array, row, "UInt8", |v| {
                Ok(Value::IntX {
                    value: v as i128,
                    kind: IntKind::U8,
                })
            })?
        }
        LogicalType::Int(IntKind::U16) => {
            primitive_value::<UInt16Type, _>(array, row, "UInt16", |v| {
                Ok(Value::IntX {
                    value: v as i128,
                    kind: IntKind::U16,
                })
            })?
        }
        LogicalType::Int(IntKind::U32) => {
            primitive_value::<UInt32Type, _>(array, row, "UInt32", |v| {
                Ok(Value::IntX {
                    value: v as i128,
                    kind: IntKind::U32,
                })
            })?
        }
        LogicalType::Int(IntKind::U64) => {
            primitive_value::<UInt64Type, _>(array, row, "UInt64", |v| {
                Ok(Value::IntX {
                    value: v as i128,
                    kind: IntKind::U64,
                })
            })?
        }
        LogicalType::UInt128 | LogicalType::Uuid => {
            let bytes: [u8; 16] = downcast::<FixedSizeBinaryArray>(array, "FixedSizeBinary(16)")?
                .value(row)
                .try_into()
                .expect("validated fixed width");
            let value = u128::from_be_bytes(bytes);
            if matches!(ty, LogicalType::Uuid) {
                Value::Uuid(value)
            } else {
                Value::UInt128(value)
            }
        }
        LogicalType::Decimal(precision, scale) => Value::Decimal {
            value: downcast::<Decimal128Array>(array, "Decimal128")?.value(row),
            precision: *precision,
            scale: *scale,
        },
        LogicalType::Double => {
            primitive_value::<Float64Type, _>(array, row, "Float64", |v| Ok(Value::Double(v)))?
        }
        LogicalType::Float => {
            primitive_value::<Float32Type, _>(array, row, "Float32", |v| Ok(Value::Float(v)))?
        }
        LogicalType::String => Value::String(
            downcast::<StringArray>(array, "Utf8")?
                .value(row)
                .to_string(),
        ),
        LogicalType::Json => Value::Json(koko_common::JsonValue::parse(
            downcast::<StringArray>(array, "Utf8")?.value(row),
        )?),
        LogicalType::Blob => Value::Blob(
            downcast::<BinaryArray>(array, "Binary")?
                .value(row)
                .to_vec(),
        ),
        LogicalType::Date => {
            primitive_value::<Date32Type, _>(array, row, "Date32", |v| Ok(Value::Date(v)))?
        }
        LogicalType::Timestamp => primitive_value::<TimestampMicrosecondType, _>(
            array,
            row,
            "Timestamp(Microsecond)",
            |v| Ok(Value::Timestamp(v)),
        )?,
        LogicalType::TimestampNs => primitive_value::<TimestampNanosecondType, _>(
            array,
            row,
            "Timestamp(Nanosecond)",
            |v| Ok(Value::Timestamp(v.div_euclid(1_000))),
        )?,
        LogicalType::TimestampMs => primitive_value::<TimestampMillisecondType, _>(
            array,
            row,
            "Timestamp(Millisecond)",
            |v| {
                v.checked_mul(1_000)
                    .map(Value::Timestamp)
                    .ok_or_else(|| Error::overflow("TIMESTAMP_MS import overflow."))
            },
        )?,
        LogicalType::TimestampSec => {
            primitive_value::<TimestampSecondType, _>(array, row, "Timestamp(Second)", |v| {
                v.checked_mul(1_000_000)
                    .map(Value::Timestamp)
                    .ok_or_else(|| Error::overflow("TIMESTAMP_SEC import overflow."))
            })?
        }
        LogicalType::TimestampTz => primitive_value::<TimestampMicrosecondType, _>(
            array,
            row,
            "Timestamp(Microsecond, timezone)",
            |v| Ok(Value::TimestampTz(v)),
        )?,
        LogicalType::Interval => primitive_value::<IntervalMonthDayNanoType, _>(
            array,
            row,
            "Interval(MonthDayNano)",
            |v| {
                if v.nanoseconds % 1_000 != 0 {
                    return Err(Error::conversion(
                        "Arrow INTERVAL nanoseconds are not exactly representable as Koko microseconds.",
                    ));
                }
                Ok(Value::Interval(Interval {
                    months: v.months,
                    days: v.days,
                    micros: v.nanoseconds / 1_000,
                }))
            },
        )?,
        LogicalType::List(child) => {
            let array = downcast::<ListArray>(array, "List")?;
            let values = array.value(row);
            let mut result = Vec::with_capacity(values.len());
            for index in 0..values.len() {
                result.push(array_value(values.as_ref(), index, child)?);
            }
            Value::List(result)
        }
        LogicalType::Array(child, _) => {
            let array = downcast::<FixedSizeListArray>(array, "FixedSizeList")?;
            let values = array.value(row);
            let mut result = Vec::with_capacity(values.len());
            for index in 0..values.len() {
                result.push(array_value(values.as_ref(), index, child)?);
            }
            Value::List(result)
        }
        LogicalType::Struct(members) => {
            let array = downcast::<StructArray>(array, "Struct")?;
            let mut result = Vec::with_capacity(members.len());
            for (index, (name, member)) in members.iter().enumerate() {
                result.push((
                    name.clone(),
                    array_value(array.column(index).as_ref(), row, member)?,
                ));
            }
            Value::Struct(result)
        }
        LogicalType::Map(key, value) => {
            let array = downcast::<MapArray>(array, "Map")?;
            let entries = array.value(row);
            let keys = entries.column(0);
            let values = entries.column(1);
            let mut result = Vec::with_capacity(entries.len());
            for index in 0..entries.len() {
                let key = array_value(keys.as_ref(), index, key)?;
                if key.is_null() {
                    return Err(Error::conversion("Arrow MAP keys cannot be NULL."));
                }
                result.push((key, array_value(values.as_ref(), index, value)?));
            }
            Value::Map(result)
        }
        LogicalType::Union(members) => {
            let array = downcast::<UnionArray>(array, "DenseUnion")?;
            let type_id = array.type_id(row);
            let tag = union_tag(array, type_id)?;
            let offset = array.value_offset(row);
            let payload = array_value(array.child(type_id).as_ref(), offset, &members[tag].1)?;
            if payload.is_null() {
                Value::Null
            } else {
                Value::Union {
                    variants: members.to_vec(),
                    tag,
                    value: Box::new(payload),
                }
            }
        }
        LogicalType::InternalId
        | LogicalType::Node(_)
        | LogicalType::Rel(_)
        | LogicalType::RecursiveRel
        | LogicalType::Any => return Err(unsupported(ty)),
    })
}

fn conversion_reservation(
    context: &ArrowImportContext<'_>,
    batch: &RecordBatch,
    types: &[LogicalType],
) -> Result<koko_common::MemoryReservation> {
    let fixed = types
        .iter()
        .map(|ty| ColumnData::allocation_bytes(ty.physical_type()))
        .sum::<u64>();
    let arrow_bytes = batch
        .columns()
        .iter()
        .map(|array| array.get_array_memory_size() as u64)
        .sum::<u64>();
    context
        .memory()
        .try_reserve(fixed.saturating_add(arrow_bytes))
}

fn fill_chunk(batch: &RecordBatch, mapping: &[usize], types: &[LogicalType]) -> Result<DataChunk> {
    let mut chunk = DataChunk::new(types);
    for (column, (&source, ty)) in mapping.iter().zip(types).enumerate() {
        let array = batch.column(source);
        for row in 0..batch.num_rows() {
            chunk.columns[column].set_value_owned(row, array_value(array.as_ref(), row, ty)?);
        }
    }
    chunk.set_flat(batch.num_rows());
    Ok(chunk)
}

fn import_node(
    context: &ArrowImportContext<'_>,
    write: koko_storage::StorageWriteHandle,
    table: &NodeTable,
    batches: &[RecordBatch],
    mappings: &[Vec<usize>],
) -> Result<u64> {
    let types = table
        .columns
        .iter()
        .map(|column| column.ty.clone())
        .collect::<Vec<_>>();
    let mut inserted = 0u64;
    for (batch, mapping) in batches.iter().zip(mappings) {
        let _memory = conversion_reservation(context, batch, &types)?;
        let chunk = fill_chunk(batch, mapping, &types)?;
        for result in context
            .storage()
            .insert_node_batch(write, table.id, &chunk, false)
        {
            result?;
            inserted += 1;
        }
    }
    Ok(inserted)
}

fn import_rel(
    context: &ArrowImportContext<'_>,
    write: koko_storage::StorageWriteHandle,
    table: &RelTable,
    from_tables: &[NodeTable],
    to_tables: &[NodeTable],
    batches: &[RecordBatch],
    mappings: &[Vec<usize>],
) -> Result<u64> {
    let prop_types = table
        .columns
        .iter()
        .map(|column| column.ty.clone())
        .collect::<Vec<_>>();
    let chunk_types = std::iter::once(LogicalType::InternalId)
        .chain(std::iter::once(LogicalType::InternalId))
        .chain(prop_types.iter().cloned())
        .collect::<Vec<_>>();
    let target = ImportTarget::Rel(table.clone(), from_tables.to_vec(), to_tables.to_vec());
    let expected = target.expected()?;
    let from_ty = expected[0].1;
    let to_ty = expected[1].1;
    let mut inserted = 0u64;
    for (batch, mapping) in batches.iter().zip(mappings) {
        let _memory = conversion_reservation(context, batch, &chunk_types)?;
        let routing_bytes = (batch.num_rows() as u64)
            .saturating_mul(std::mem::size_of::<(usize, InternalId, InternalId)>() as u64)
            .saturating_mul(2)
            .saturating_add(
                (table.pairs.len() * std::mem::size_of::<Vec<(usize, InternalId, InternalId)>>())
                    as u64,
            );
        let _routing_memory = context.memory().try_reserve(routing_bytes)?;
        let from_array = batch.column(mapping[0]);
        let to_array = batch.column(mapping[1]);
        let mut routed: Vec<Vec<(usize, InternalId, InternalId)>> =
            table.pairs.iter().map(|_| Vec::new()).collect();
        for row in 0..batch.num_rows() {
            let from_pk = array_value(from_array.as_ref(), row, from_ty)?;
            let to_pk = array_value(to_array.as_ref(), row, to_ty)?;
            let mut route = None;
            for pair in 0..table.pairs.len() {
                let src =
                    context
                        .storage()
                        .find_node_by_pk(write.read(), from_tables[pair].id, &from_pk);
                let dst =
                    context
                        .storage()
                        .find_node_by_pk(write.read(), to_tables[pair].id, &to_pk);
                if let (Some(src), Some(dst)) = (src, dst) {
                    if route.is_some() {
                        return Err(Error::runtime(format!(
                            "Relationship row {row} resolves to more than one endpoint pair in group `{}`.",
                            table.name
                        )));
                    }
                    route = Some((pair, src, dst));
                }
            }
            let (pair, src, dst) = route.ok_or_else(|| {
                Error::runtime(format!(
                    "Relationship row {row} cannot resolve `from`/`to` primary keys in table `{}`.",
                    table.name
                ))
            })?;
            routed[pair].push((row, src, dst));
        }
        for (pair, rows) in routed.iter().enumerate() {
            if rows.is_empty() {
                continue;
            }
            let mut chunk = DataChunk::new(&chunk_types);
            for (out, (row, src, dst)) in rows.iter().enumerate() {
                chunk.columns[0].set_internal_id(out, *src);
                chunk.columns[1].set_internal_id(out, *dst);
                for (property, ty) in prop_types.iter().enumerate() {
                    let value =
                        array_value(batch.column(mapping[property + 2]).as_ref(), *row, ty)?;
                    chunk.columns[property + 2].set_value_owned(out, value);
                }
            }
            chunk.set_flat(rows.len());
            for result in
                context
                    .storage()
                    .insert_rel_batch(write, table.member_ids[pair], &chunk, false)
            {
                result?;
                inserted += 1;
            }
        }
    }
    Ok(inserted)
}

fn preflight(target: &ImportTarget, batches: &[RecordBatch]) -> Result<Vec<Vec<usize>>> {
    let expected = target.expected()?;
    batches
        .iter()
        .map(|batch| field_mapping(batch, &expected))
        .collect()
}

pub(crate) struct ArrowImportPlan {
    target: ImportTarget,
    mappings: Vec<Vec<usize>>,
}

pub(crate) fn prepare_import(
    context: &ArrowImportContext<'_>,
    table: &str,
    batches: &[RecordBatch],
) -> Result<Option<ArrowImportPlan>> {
    let target = ImportTarget::resolve(context.catalog(), table)?;
    let mappings = preflight(&target, batches)?;
    if batches.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ArrowImportPlan { target, mappings }))
    }
}

pub(crate) fn apply_import(
    context: &ArrowImportContext<'_>,
    write: StorageWriteHandle,
    plan: ArrowImportPlan,
    batches: &[RecordBatch],
) -> Result<u64> {
    match &plan.target {
        ImportTarget::Node(table) => import_node(context, write, table, batches, &plan.mappings),
        ImportTarget::Rel(table, from, to) => {
            import_rel(context, write, table, from, to, batches, &plan.mappings)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Database, DatabaseConfig};

    fn record_batch(fields: Vec<Field>, arrays: Vec<ArrayRef>) -> RecordBatch {
        RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays).unwrap()
    }

    fn node_batch(ids: Vec<Option<i64>>, names: Vec<Option<&str>>) -> RecordBatch {
        record_batch(
            vec![
                Field::new("id", DataType::Int64, true),
                Field::new("name", DataType::Utf8, true),
            ],
            vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(names)),
            ],
        )
    }

    #[test]
    fn empty_query_result_keeps_exact_arrow_schema() {
        let result = QueryResult::from_typed_rows(
            vec!["id".into(), "name".into()],
            vec![LogicalType::Int64, LogicalType::String],
            Vec::new(),
        );
        let batches = result.to_arrow_record_batches().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 0);
        assert_eq!(batches[0].schema().field(0).data_type(), &DataType::Int64);
        assert_eq!(
            batches[0].schema().field(1).metadata()[LOGICAL_TYPE_METADATA],
            "STRING"
        );
    }

    #[test]
    fn primitive_temporal_and_fixed_array_types_round_trip() {
        let types = vec![
            LogicalType::Bool,
            LogicalType::Int(IntKind::I8),
            LogicalType::Int(IntKind::I16),
            LogicalType::Int(IntKind::I32),
            LogicalType::Int64,
            LogicalType::Int(IntKind::I128),
            LogicalType::Int(IntKind::U8),
            LogicalType::Int(IntKind::U16),
            LogicalType::Int(IntKind::U32),
            LogicalType::Int(IntKind::U64),
            LogicalType::UInt128,
            LogicalType::Decimal(12, 3),
            LogicalType::Float,
            LogicalType::Double,
            LogicalType::String,
            LogicalType::Blob,
            LogicalType::Uuid,
            LogicalType::Date,
            LogicalType::Timestamp,
            LogicalType::TimestampNs,
            LogicalType::TimestampMs,
            LogicalType::TimestampSec,
            LogicalType::TimestampTz,
            LogicalType::Interval,
            LogicalType::Array(Box::new(LogicalType::String), 2),
        ];
        let values = vec![
            Value::Bool(true),
            Value::IntX {
                value: -8,
                kind: IntKind::I8,
            },
            Value::IntX {
                value: -16,
                kind: IntKind::I16,
            },
            Value::IntX {
                value: -32,
                kind: IntKind::I32,
            },
            Value::Int64(-64),
            Value::IntX {
                value: i128::MIN + 1,
                kind: IntKind::I128,
            },
            Value::IntX {
                value: 8,
                kind: IntKind::U8,
            },
            Value::IntX {
                value: 16,
                kind: IntKind::U16,
            },
            Value::IntX {
                value: 32,
                kind: IntKind::U32,
            },
            Value::IntX {
                value: 64,
                kind: IntKind::U64,
            },
            Value::UInt128(u128::MAX - 1),
            Value::Decimal {
                value: 12_345,
                precision: 12,
                scale: 3,
            },
            Value::Float(1.25),
            Value::Double(-2.5),
            Value::String("hello".into()),
            Value::Blob(vec![0, 1, 255]),
            Value::Uuid(0x1234),
            Value::Date(20_000),
            Value::Timestamp(2_000_000),
            Value::Timestamp(2_000_000),
            Value::Timestamp(2_000_000),
            Value::Timestamp(2_000_000),
            Value::TimestampTz(2_000_000),
            Value::Interval(Interval {
                months: 2,
                days: -3,
                micros: 4_000,
            }),
            Value::List(vec![Value::String("a".into()), Value::String("b".into())]),
        ];
        let result = QueryResult::from_typed_rows(
            (0..types.len()).map(|index| format!("c{index}")).collect(),
            types.clone(),
            vec![values.clone(), vec![Value::Null; types.len()]],
        );
        let batches = result.to_arrow_record_batches().unwrap();
        assert_eq!(batches.len(), 1);
        for (index, ty) in types.iter().enumerate() {
            assert_eq!(
                array_value(batches[0].column(index).as_ref(), 0, ty).unwrap(),
                values[index]
            );
            assert_eq!(
                array_value(batches[0].column(index).as_ref(), 1, ty).unwrap(),
                Value::Null
            );
        }
    }

    #[test]
    fn query_result_arrow_preserves_schema_nulls_nested_and_batches() {
        let nested = LogicalType::Struct(vec![
            (
                "items".into(),
                LogicalType::List(Box::new(LogicalType::Int64)),
            ),
            (
                "attrs".into(),
                LogicalType::Map(Box::new(LogicalType::String), Box::new(LogicalType::Int64)),
            ),
        ]);
        let union = LogicalType::Union(vec![
            ("number".into(), LogicalType::Int64),
            ("text".into(), LogicalType::String),
        ]);
        let mut rows = Vec::with_capacity(VECTOR_CAPACITY + 1);
        rows.push(vec![
            Value::Int64(1),
            Value::Null,
            Value::Struct(vec![
                (
                    "items".into(),
                    Value::List(vec![Value::Int64(2), Value::Null]),
                ),
                (
                    "attrs".into(),
                    Value::Map(vec![(Value::String("x".into()), Value::Int64(3))]),
                ),
            ]),
            Value::Union {
                variants: match &union {
                    LogicalType::Union(members) => members.clone(),
                    _ => unreachable!(),
                },
                tag: 1,
                value: Box::new(Value::String("u".into())),
            },
        ]);
        rows.extend((1..VECTOR_CAPACITY).map(|index| {
            vec![
                Value::Int64(index as i64 + 1),
                Value::Bool(index % 2 == 0),
                Value::Null,
                Value::Null,
            ]
        }));
        rows.push(vec![
            Value::Int64(9_999),
            Value::Bool(true),
            Value::Null,
            Value::Null,
        ]);
        let result = QueryResult::from_typed_rows(
            vec!["id".into(), "flag".into(), "nested".into(), "choice".into()],
            vec![
                LogicalType::Int64,
                LogicalType::Bool,
                nested.clone(),
                union.clone(),
            ],
            rows,
        );

        let batches = result.to_arrow_record_batches().unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].num_rows(), VECTOR_CAPACITY);
        assert_eq!(batches[1].num_rows(), 1);
        for (index, ty) in [
            LogicalType::Int64,
            LogicalType::Bool,
            nested.clone(),
            union.clone(),
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                batches[0].schema().field(index).metadata()[LOGICAL_TYPE_METADATA],
                ty.to_string()
            );
        }
        assert!(batches[0].column(1).is_null(0));
        assert_eq!(
            array_value(batches[0].column(2).as_ref(), 0, &nested).unwrap(),
            Value::Struct(vec![
                (
                    "items".into(),
                    Value::List(vec![Value::Int64(2), Value::Null]),
                ),
                (
                    "attrs".into(),
                    Value::Map(vec![(Value::String("x".into()), Value::Int64(3))]),
                ),
            ])
        );
        assert_eq!(
            array_value(batches[0].column(3).as_ref(), 0, &union)
                .unwrap()
                .to_result_string(),
            "u"
        );
        assert_eq!(
            array_value(batches[1].column(0).as_ref(), 0, &LogicalType::Int64).unwrap(),
            Value::Int64(9_999)
        );
        let database = Database::in_memory();
        let connection = database.connect();
        connection
            .query(
                "CREATE NODE TABLE RoundTrip(id INT64, flag BOOL, nested STRUCT(items INT64[], attrs MAP(STRING, INT64)), choice UNION(number INT64, text STRING), PRIMARY KEY(id))",
            )
            .unwrap();
        assert_eq!(
            connection.import_arrow("roundtrip", &batches).unwrap(),
            (VECTOR_CAPACITY + 1) as u64
        );
        assert_eq!(
            connection
                .query("MATCH (n:RoundTrip) WHERE n.id = 1 RETURN n.nested, n.choice")
                .unwrap()
                .to_result_strings(),
            vec!["{items: [2,], attrs: {x=3}}|u"]
        );
    }

    #[test]
    fn arrow_node_import_validates_names_types_and_nulls() {
        let db = Database::in_memory();
        let connection = db.connect();
        connection
            .query("CREATE NODE TABLE N(id INT64, name STRING, PRIMARY KEY(id))")
            .unwrap();

        let batch = node_batch(vec![Some(1), Some(2)], vec![Some("a"), None]);
        assert_eq!(connection.import_arrow("n", &[batch]).unwrap(), 2);
        assert_eq!(
            connection
                .query("MATCH (n:N) RETURN n.id, n.name ORDER BY n.id")
                .unwrap()
                .to_result_strings(),
            vec!["1|a", "2|"]
        );

        let wrong_type = record_batch(
            vec![
                Field::new("id", DataType::Utf8, false),
                Field::new("name", DataType::Utf8, true),
            ],
            vec![
                Arc::new(StringArray::from(vec![Some("3")])),
                Arc::new(StringArray::from(vec![Some("c")])),
            ],
        );
        assert!(matches!(
            connection.import_arrow("N", &[wrong_type]),
            Err(Error::Binder(_))
        ));

        let duplicate_name = record_batch(
            vec![
                Field::new("id", DataType::Int64, false),
                Field::new("ID", DataType::Int64, false),
            ],
            vec![
                Arc::new(Int64Array::from(vec![Some(3)])),
                Arc::new(Int64Array::from(vec![Some(4)])),
            ],
        );
        assert!(connection.import_arrow("N", &[duplicate_name]).is_err());

        let null_pk = node_batch(vec![None], vec![Some("bad")]);
        assert!(matches!(
            connection.import_arrow("N", &[null_pk]),
            Err(Error::Runtime(_))
        ));
        assert_eq!(
            connection
                .query("MATCH (n:N) RETURN count(*)")
                .unwrap()
                .to_result_strings(),
            vec!["2"]
        );
    }

    #[test]
    fn arrow_relationship_group_routes_by_endpoint_primary_keys() {
        let db = Database::in_memory();
        let connection = db.connect();
        connection
            .query("CREATE NODE TABLE Person(name STRING, PRIMARY KEY(name))")
            .unwrap();
        connection
            .query("CREATE NODE TABLE City(name STRING, PRIMARY KEY(name))")
            .unwrap();
        connection
            .query(
                "CREATE REL TABLE Likes(FROM Person TO Person, FROM Person TO City, weight INT64)",
            )
            .unwrap();
        connection
            .query(
                "CREATE (:Person {name:'Alice'}), (:Person {name:'Bob'}), (:City {name:'London'})",
            )
            .unwrap();
        let batch = record_batch(
            vec![
                Field::new("FROM", DataType::Utf8, false),
                Field::new("to", DataType::Utf8, false),
                Field::new("Weight", DataType::Int64, true),
            ],
            vec![
                Arc::new(StringArray::from(vec![Some("Alice"), Some("Alice")])),
                Arc::new(StringArray::from(vec![Some("Bob"), Some("London")])),
                Arc::new(Int64Array::from(vec![Some(4), Some(7)])),
            ],
        );
        assert_eq!(connection.import_arrow("likes", &[batch]).unwrap(), 2);
        assert_eq!(
            connection
                .query("MATCH (:Person)-[r:Likes]->(n) RETURN n.name, r.weight ORDER BY r.weight")
                .unwrap()
                .to_result_strings(),
            vec!["Bob|4", "London|7"]
        );
    }

    #[test]
    fn arrow_import_rolls_back_all_batches_and_call_savepoint() {
        let db = Database::in_memory();
        let connection = db.connect();
        connection
            .query("CREATE NODE TABLE N(id INT64, name STRING, PRIMARY KEY(id))")
            .unwrap();
        let first = node_batch(vec![Some(1)], vec![Some("first")]);
        let duplicate = node_batch(vec![Some(1)], vec![Some("duplicate")]);
        assert!(connection.import_arrow("N", &[first, duplicate]).is_err());
        assert_eq!(
            connection
                .query("MATCH (n:N) RETURN count(*)")
                .unwrap()
                .to_result_strings(),
            vec!["0"]
        );

        let transaction = connection.transaction().unwrap();
        transaction
            .query("CREATE (:N {id: 10, name: 'kept'})")
            .unwrap();
        let first = node_batch(vec![Some(11)], vec![Some("rolled")]);
        let duplicate = node_batch(vec![Some(10)], vec![Some("duplicate")]);
        assert!(connection.import_arrow("N", &[first, duplicate]).is_err());
        assert_eq!(
            transaction
                .query("MATCH (n:N) RETURN n.id ORDER BY n.id")
                .unwrap()
                .to_result_strings(),
            vec!["10"]
        );
        transaction.commit().unwrap();
    }

    #[test]
    fn arrow_import_obeys_memory_limit_without_mutation() {
        let config = DatabaseConfig::new().with_memory_limit(60_000).unwrap();
        let db = Database::in_memory_with_config(config).unwrap();
        let connection = db.connect();
        connection
            .query("CREATE NODE TABLE N(id INT64, name STRING, PRIMARY KEY(id))")
            .unwrap();
        let batch = node_batch(vec![Some(1)], vec![Some("value")]);
        assert!(connection.import_arrow("N", &[batch]).is_err());
        assert_eq!(
            connection
                .query("MATCH (n:N) RETURN count(*)")
                .unwrap()
                .to_result_strings(),
            vec!["0"]
        );
    }

    #[test]
    fn arrow_export_rejects_graph_and_internal_values() {
        let db = Database::in_memory();
        let connection = db.connect();
        connection
            .query("CREATE NODE TABLE N(id INT64, PRIMARY KEY(id))")
            .unwrap();
        connection.query("CREATE (:N {id: 1})").unwrap();
        let graph = connection.query("MATCH (n:N) RETURN n").unwrap();
        assert!(matches!(
            graph.to_arrow_record_batches(),
            Err(Error::NotImplemented(_))
        ));

        let internal = QueryResult::from_typed_rows(
            vec!["id".into()],
            vec![LogicalType::InternalId],
            vec![vec![Value::InternalId(InternalId::new(
                koko_common::TableId(1),
                0,
            ))]],
        );
        assert!(matches!(
            internal.to_arrow_record_batches(),
            Err(Error::NotImplemented(_))
        ));
    }
}
