//! Normative typed JSON value boundary shared by parameters and machine output.

use base64::Engine as _;
use koko::result::{Cell, CellValue, ResultTypeContext};
use koko::value::{
    IntKind, InternalId, Interval, JsonValue, NodeValue, RecursiveRelValue, RelValue, TableId,
    format_date, format_decimal, format_timestamp, format_uuid, parse_date, parse_decimal,
    parse_timestamp,
};
use koko::{LogicalType, Value};
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use std::collections::HashSet;
use std::fmt;
use std::io::Write as _;

const MAX_EXACT_JSON_INTEGER: i128 = (1_i128 << 53) - 1;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CodecError {
    #[error("invalid JSON value: {0}")]
    Json(String),
    #[error("invalid tagged value `{tag}`: {message}")]
    Tagged { tag: String, message: String },
    #[error("duplicate parameter name `{0}` in parameters file")]
    DuplicateParameter(String),
    #[error("parameters file must contain one top-level JSON object")]
    ParameterObject,
}

/// Decode one native or tagged JSON value.
pub fn decode_value(source: &str) -> Result<Value, CodecError> {
    let value: serde_json::Value =
        serde_json::from_str(source).map_err(|error| CodecError::Json(error.to_string()))?;
    decode_json_value(&value)
}

/// Decode a duplicate-aware top-level parameter object while preserving member order.
pub fn decode_parameter_object(source: &str) -> Result<Vec<(String, Value)>, CodecError> {
    let mut deserializer = serde_json::Deserializer::from_str(source);
    let encoded = UniqueParameterObject
        .deserialize(&mut deserializer)
        .map_err(|error| CodecError::Json(error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| CodecError::Json(error.to_string()))?;
    encoded
        .into_iter()
        .map(|(name, value)| decode_json_value(&value).map(|value| (name, value)))
        .collect()
}

struct UniqueParameterObject;

impl<'de> DeserializeSeed<'de> for UniqueParameterObject {
    type Value = Vec<(String, serde_json::Value)>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(UniqueParameterVisitor)
    }
}

struct UniqueParameterVisitor;

impl<'de> Visitor<'de> for UniqueParameterVisitor {
    type Value = Vec<(String, serde_json::Value)>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("one top-level JSON object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut names = HashSet::new();
        let mut parameters = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some((name, value)) = map.next_entry::<String, serde_json::Value>()? {
            if !names.insert(name.clone()) {
                return Err(serde::de::Error::custom(
                    CodecError::DuplicateParameter(name).to_string(),
                ));
            }
            parameters.push((name, value));
        }
        Ok(parameters)
    }
}

fn decode_json_value(value: &serde_json::Value) -> Result<Value, CodecError> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Bool(*value)),
        serde_json::Value::String(value) => Ok(Value::String(value.clone())),
        serde_json::Value::Number(value) => decode_number(value),
        serde_json::Value::Array(values) => values
            .iter()
            .map(decode_json_value)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        serde_json::Value::Object(object) => match object.get("$type").and_then(|tag| tag.as_str())
        {
            Some(tag) if recognized_tag(tag) => decode_tagged(tag, object),
            _ => raw_json(value).map(Value::Json),
        },
    }
}

fn decode_number(number: &serde_json::Number) -> Result<Value, CodecError> {
    let text = number.to_string();
    if !text.contains(['.', 'e', 'E']) {
        let value = text.parse::<i128>().map_err(|_| {
            CodecError::Json("integer outside the supported signed 128-bit range".to_string())
        })?;
        if !(-MAX_EXACT_JSON_INTEGER..=MAX_EXACT_JSON_INTEGER).contains(&value) {
            return Err(CodecError::Json(
                "integer outside JSON's exact interoperable range must use an INTEGER tag"
                    .to_string(),
            ));
        }
        return Ok(Value::Int64(value as i64));
    }
    let value = text
        .parse::<f64>()
        .map_err(|_| CodecError::Json("invalid JSON number".to_string()))?;
    Ok(Value::Double(value))
}

fn recognized_tag(tag: &str) -> bool {
    matches!(
        tag,
        "INTEGER"
            | "DECIMAL"
            | "NONFINITE"
            | "JSON"
            | "DATE"
            | "TIMESTAMP"
            | "TIMESTAMP_NS"
            | "TIMESTAMP_MS"
            | "TIMESTAMP_SEC"
            | "TIMESTAMP_TZ"
            | "UUID"
            | "INTERVAL"
            | "BLOB"
            | "STRUCT"
            | "MAP"
            | "UNION"
            | "INTERNAL_ID"
            | "NODE"
            | "REL"
            | "RECURSIVE_REL"
    )
}

fn decode_tagged(
    tag: &str,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Value, CodecError> {
    match tag {
        "INTEGER" => decode_integer(object),
        "DECIMAL" => decode_decimal(object),
        "NONFINITE" => decode_nonfinite(object),
        "JSON" => raw_json(field(object, tag, "value")?).map(Value::Json),
        "DATE" => {
            let text = string_field(object, tag, "value")?;
            parse_date(text)
                .map(Value::Date)
                .ok_or_else(|| tagged(tag, "`value` is not a canonical date"))
        }
        "TIMESTAMP" | "TIMESTAMP_NS" | "TIMESTAMP_MS" | "TIMESTAMP_SEC" | "TIMESTAMP_TZ" => {
            let text = string_field(object, tag, "value")?;
            let value = parse_timestamp(text)
                .ok_or_else(|| tagged(tag, "`value` is not a canonical timestamp"))?;
            Ok(if tag == "TIMESTAMP" {
                Value::Timestamp(value)
            } else {
                Value::TimestampTz(value)
            })
        }
        "UUID" => decode_uuid(object),
        "INTERVAL" => Ok(Value::Interval(Interval {
            months: i32_field(object, tag, "months")?,
            days: i32_field(object, tag, "days")?,
            micros: i64_field(object, tag, "micros")?,
        })),
        "BLOB" => decode_blob(object),
        "STRUCT" => pair_array(field(object, tag, "fields")?, tag).map(Value::Struct),
        "MAP" => map_entries(field(object, tag, "entries")?, tag).map(Value::Map),
        "UNION" => {
            let member = string_field(object, tag, "tag")?.to_string();
            let value = decode_json_value(field(object, tag, "value")?)?;
            let logical_type = value.logical_type();
            Ok(Value::Union {
                variants: vec![(member, logical_type)],
                tag: 0,
                value: Box::new(value),
            })
        }
        "INTERNAL_ID" => decode_internal_id(object, tag).map(Value::InternalId),
        "NODE" => decode_node(object, tag).map(|node| Value::Node(Box::new(node))),
        "REL" => decode_rel(object, tag).map(|rel| Value::Rel(Box::new(rel))),
        "RECURSIVE_REL" => decode_recursive_rel(object, tag)
            .map(|recursive| Value::RecursiveRel(Box::new(recursive))),
        _ => unreachable!("recognized tag dispatch is exhaustive"),
    }
}

fn decode_integer(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Value, CodecError> {
    let tag = "INTEGER";
    let logical_type = string_field(object, tag, "logical_type")?;
    let text = string_field(object, tag, "value")?;
    if logical_type == "UINT128" {
        return text
            .parse::<u128>()
            .map(Value::UInt128)
            .map_err(|_| tagged(tag, "`value` is outside UINT128"));
    }
    let kind = int_kind(logical_type)
        .ok_or_else(|| tagged(tag, "`logical_type` must name an integer type"))?;
    let value = text
        .parse::<i128>()
        .map_err(|_| tagged(tag, "`value` is not a signed decimal integer"))?;
    if !kind.contains(value) {
        return Err(tagged(tag, "`value` is outside its logical type"));
    }
    Ok(if kind == IntKind::I64 {
        Value::Int64(value as i64)
    } else {
        Value::IntX { value, kind }
    })
}

fn int_kind(name: &str) -> Option<IntKind> {
    Some(match name {
        "INT8" => IntKind::I8,
        "INT16" => IntKind::I16,
        "INT32" => IntKind::I32,
        "INT64" => IntKind::I64,
        "INT128" => IntKind::I128,
        "UINT8" => IntKind::U8,
        "UINT16" => IntKind::U16,
        "UINT32" => IntKind::U32,
        "UINT64" => IntKind::U64,
        _ => return None,
    })
}

fn decode_decimal(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Value, CodecError> {
    let tag = "DECIMAL";
    let precision = u8_field(object, tag, "precision")?;
    let scale = u8_field(object, tag, "scale")?;
    if precision == 0 || precision > 38 || scale > precision {
        return Err(tagged(
            tag,
            "precision and scale must satisfy 1 <= scale <= precision <= 38, except scale may be zero",
        ));
    }
    let value = parse_decimal(string_field(object, tag, "value")?, scale)
        .ok_or_else(|| tagged(tag, "`value` is not a plain decimal"))?;
    if value.unsigned_abs() >= 10_u128.pow(u32::from(precision)) {
        return Err(tagged(tag, "`value` exceeds precision"));
    }
    Ok(Value::Decimal {
        value,
        precision,
        scale,
    })
}

fn decode_nonfinite(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<Value, CodecError> {
    let tag = "NONFINITE";
    let logical_type = string_field(object, tag, "logical_type")?;
    let value = match string_field(object, tag, "value")? {
        "NaN" => f64::NAN,
        "+Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        _ => return Err(tagged(tag, "`value` must be NaN, +Infinity, or -Infinity")),
    };
    match logical_type {
        "DOUBLE" => Ok(Value::Double(value)),
        "FLOAT" => Ok(Value::Float(value as f32)),
        _ => Err(tagged(tag, "`logical_type` must be FLOAT or DOUBLE")),
    }
}

fn decode_uuid(object: &serde_json::Map<String, serde_json::Value>) -> Result<Value, CodecError> {
    let text = string_field(object, "UUID", "value")?;
    let compact = text.replace('-', "");
    if compact.len() != 32 {
        return Err(tagged("UUID", "`value` must contain 32 hexadecimal digits"));
    }
    u128::from_str_radix(&compact, 16)
        .map(Value::Uuid)
        .map_err(|_| tagged("UUID", "`value` contains a non-hexadecimal digit"))
}

fn decode_blob(object: &serde_json::Map<String, serde_json::Value>) -> Result<Value, CodecError> {
    let tag = "BLOB";
    if string_field(object, tag, "encoding")? != "base64" {
        return Err(tagged(tag, "`encoding` must be `base64`"));
    }
    base64::engine::general_purpose::STANDARD
        .decode(string_field(object, tag, "value")?)
        .map(Value::Blob)
        .map_err(|error| tagged(tag, format!("invalid base64 payload: {error}")))
}

fn decode_internal_id(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
) -> Result<InternalId, CodecError> {
    let table = string_field(object, tag, "table")?
        .parse::<u64>()
        .map_err(|_| tagged(tag, "`table` must be a decimal u64 string"))?;
    let offset = string_field(object, tag, "offset")?
        .parse::<u64>()
        .map_err(|_| tagged(tag, "`offset` must be a decimal u64 string"))?;
    Ok(InternalId::new(TableId(table), offset))
}

fn decode_node(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
) -> Result<NodeValue, CodecError> {
    let id = tagged_object_field(object, tag, "id", "INTERNAL_ID", decode_internal_id)?;
    Ok(NodeValue {
        id,
        label: string_field(object, tag, "label")?.to_string(),
        props: pair_array(field(object, tag, "properties")?, tag)?,
    })
}

fn decode_rel(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
) -> Result<RelValue, CodecError> {
    Ok(RelValue {
        id: tagged_object_field(object, tag, "id", "INTERNAL_ID", decode_internal_id)?,
        src: tagged_object_field(object, tag, "src", "INTERNAL_ID", decode_internal_id)?,
        dst: tagged_object_field(object, tag, "dst", "INTERNAL_ID", decode_internal_id)?,
        label: string_field(object, tag, "label")?.to_string(),
        props: pair_array(field(object, tag, "properties")?, tag)?,
        src_node: None,
        dst_node: None,
    })
}

fn decode_recursive_rel(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
) -> Result<RecursiveRelValue, CodecError> {
    let nodes = array_field(object, tag, "nodes")?
        .iter()
        .map(|value| {
            let object = tagged_object(value, tag, "NODE")?;
            decode_node(object, "NODE")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let rels = array_field(object, tag, "relationships")?
        .iter()
        .map(|value| {
            let object = tagged_object(value, tag, "REL")?;
            decode_rel(object, "REL")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cost = match object.get("cost") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(json_f64(value, tag, "cost")?),
    };
    let degenerate = object
        .get("degenerate")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| tagged(tag, "`degenerate` must be a boolean"))?;
    let null_nodes = usize_field(object, tag, "null_nodes")?;
    Ok(RecursiveRelValue {
        nodes,
        rels,
        degenerate,
        cost,
        null_nodes,
    })
}

fn tagged_object_field<T>(
    object: &serde_json::Map<String, serde_json::Value>,
    parent_tag: &str,
    name: &str,
    expected_tag: &str,
    decode: impl FnOnce(&serde_json::Map<String, serde_json::Value>, &str) -> Result<T, CodecError>,
) -> Result<T, CodecError> {
    let object = tagged_object(field(object, parent_tag, name)?, parent_tag, expected_tag)?;
    decode(object, expected_tag)
}

fn tagged_object<'a>(
    value: &'a serde_json::Value,
    parent_tag: &str,
    expected_tag: &str,
) -> Result<&'a serde_json::Map<String, serde_json::Value>, CodecError> {
    let object = value.as_object().ok_or_else(|| {
        tagged(
            parent_tag,
            format!("expected a tagged {expected_tag} object"),
        )
    })?;
    if object.get("$type").and_then(serde_json::Value::as_str) != Some(expected_tag) {
        return Err(tagged(
            parent_tag,
            format!("expected a tagged {expected_tag} object"),
        ));
    }
    Ok(object)
}

fn pair_array(value: &serde_json::Value, tag: &str) -> Result<Vec<(String, Value)>, CodecError> {
    let pairs = pair_values(value, tag)?;
    pairs
        .iter()
        .map(|pair| {
            let key = pair[0]
                .as_str()
                .ok_or_else(|| tagged(tag, "field/property names must be strings"))?
                .to_string();
            Ok((key, decode_json_value(&pair[1])?))
        })
        .collect()
}

fn map_entries(value: &serde_json::Value, tag: &str) -> Result<Vec<(Value, Value)>, CodecError> {
    let pairs = pair_values(value, tag)?;
    pairs
        .iter()
        .map(|pair| Ok((decode_json_value(&pair[0])?, decode_json_value(&pair[1])?)))
        .collect()
}

fn pair_values<'a>(
    value: &'a serde_json::Value,
    tag: &str,
) -> Result<Vec<&'a Vec<serde_json::Value>>, CodecError> {
    value
        .as_array()
        .ok_or_else(|| tagged(tag, "ordered pairs must be an array"))?
        .iter()
        .map(|pair| {
            pair.as_array()
                .filter(|pair| pair.len() == 2)
                .ok_or_else(|| tagged(tag, "each ordered pair must contain two values"))
        })
        .collect()
}

fn raw_json(value: &serde_json::Value) -> Result<JsonValue, CodecError> {
    match value {
        serde_json::Value::Null => Ok(JsonValue::Null),
        serde_json::Value::Bool(value) => Ok(JsonValue::Bool(*value)),
        serde_json::Value::String(value) => Ok(JsonValue::String(value.clone())),
        serde_json::Value::Number(number) => {
            let text = number.to_string();
            if !text.contains(['.', 'e', 'E']) {
                return text
                    .parse::<i128>()
                    .map(JsonValue::Int)
                    .map_err(|_| CodecError::Json("JSON integer exceeds INT128".to_string()));
            }
            text.parse::<f64>()
                .map(JsonValue::Float)
                .map_err(|_| CodecError::Json("invalid JSON number".to_string()))
        }
        serde_json::Value::Array(values) => values
            .iter()
            .map(raw_json)
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
        serde_json::Value::Object(object) => object
            .iter()
            .map(|(name, value)| raw_json(value).map(|value| (name.clone(), value)))
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Object),
    }
}

fn field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<&'a serde_json::Value, CodecError> {
    object
        .get(name)
        .ok_or_else(|| tagged(tag, format!("missing `{name}`")))
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<&'a str, CodecError> {
    field(object, tag, name)?
        .as_str()
        .ok_or_else(|| tagged(tag, format!("`{name}` must be a string")))
}

fn array_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<&'a [serde_json::Value], CodecError> {
    field(object, tag, name)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| tagged(tag, format!("`{name}` must be an array")))
}

fn json_integer(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<i128, CodecError> {
    let number = field(object, tag, name)?
        .as_number()
        .ok_or_else(|| tagged(tag, format!("`{name}` must be an integer")))?;
    let text = number.to_string();
    if text.contains(['.', 'e', 'E']) {
        return Err(tagged(tag, format!("`{name}` must be an integer")));
    }
    text.parse::<i128>()
        .map_err(|_| tagged(tag, format!("`{name}` is out of range")))
}

fn u8_field(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<u8, CodecError> {
    u8::try_from(json_integer(object, tag, name)?)
        .map_err(|_| tagged(tag, format!("`{name}` is outside u8")))
}

fn i32_field(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<i32, CodecError> {
    i32::try_from(json_integer(object, tag, name)?)
        .map_err(|_| tagged(tag, format!("`{name}` is outside i32")))
}

fn i64_field(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<i64, CodecError> {
    i64::try_from(json_integer(object, tag, name)?)
        .map_err(|_| tagged(tag, format!("`{name}` is outside i64")))
}

fn usize_field(
    object: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
    name: &str,
) -> Result<usize, CodecError> {
    usize::try_from(json_integer(object, tag, name)?)
        .map_err(|_| tagged(tag, format!("`{name}` is outside usize")))
}

fn json_f64(value: &serde_json::Value, tag: &str, name: &str) -> Result<f64, CodecError> {
    value
        .as_number()
        .and_then(serde_json::Number::as_f64)
        .ok_or_else(|| tagged(tag, format!("`{name}` must be a finite number")))
}

fn tagged(tag: &str, message: impl Into<String>) -> CodecError {
    CodecError::Tagged {
        tag: tag.to_string(),
        message: message.into(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("cannot encode {value_kind} as declared type {logical_type}")]
    Type {
        value_kind: &'static str,
        logical_type: String,
    },
}

/// Stream one borrowed result cell through the normative machine mapping.
pub fn write_cell(
    writer: &mut impl std::io::Write,
    cell: Cell<'_>,
    context: &ResultTypeContext,
) -> Result<(), EncodeError> {
    match cell.value() {
        CellValue::Null => writer.write_all(b"null")?,
        CellValue::Bool(value) => write_bool(writer, value)?,
        CellValue::Int { value, .. } => {
            write_integer(writer, value, &cell.logical_type().to_string())?
        }
        CellValue::UInt128(value) => {
            write_unsigned_integer(writer, value, &cell.logical_type().to_string())?
        }
        CellValue::Decimal {
            value,
            precision,
            scale,
        } => write_decimal(writer, value, precision, scale)?,
        CellValue::Double(value) => write_float(writer, value, "DOUBLE")?,
        CellValue::Float(value) => write_float(writer, f64::from(value), "FLOAT")?,
        CellValue::String(value) => write_json_string(writer, value)?,
        CellValue::Date(value) => write_text_wrapper(writer, "DATE", &format_date(value))?,
        CellValue::Timestamp(value) => write_text_wrapper(
            writer,
            &cell.logical_type().to_string(),
            &format_timestamp(value),
        )?,
        CellValue::TimestampTz(value) => write_text_wrapper(
            writer,
            "TIMESTAMP_TZ",
            &format!("{}+00", format_timestamp(value)),
        )?,
        CellValue::Interval(value) => write_interval(writer, value)?,
        CellValue::Uuid(value) => write_text_wrapper(writer, "UUID", &format_uuid(value))?,
        CellValue::InternalId(value) => write_internal_id(writer, value)?,
        CellValue::Generic(value) => {
            write_typed_value(writer, value, cell.logical_type(), context)?
        }
        _ => write_typed_value(writer, &cell.to_owned(), cell.logical_type(), context)?,
    }
    Ok(())
}

/// Stream one owned/nested value with its declared logical type.
pub fn write_typed_value(
    writer: &mut impl std::io::Write,
    value: &Value,
    logical_type: &LogicalType,
    context: &ResultTypeContext,
) -> Result<(), EncodeError> {
    match value {
        Value::Null => writer.write_all(b"null")?,
        Value::Bool(value) => write_bool(writer, *value)?,
        Value::Int64(value) => {
            write_integer(writer, i128::from(*value), &logical_type.to_string())?
        }
        Value::IntX { value, kind } => write_integer(writer, *value, kind.name())?,
        Value::UInt128(value) => write_unsigned_integer(writer, *value, "UINT128")?,
        Value::Decimal {
            value,
            precision,
            scale,
        } => write_decimal(writer, *value, *precision, *scale)?,
        Value::Double(value) => write_float(writer, *value, "DOUBLE")?,
        Value::Float(value) => write_float(writer, f64::from(*value), "FLOAT")?,
        Value::String(value) => write_json_string(writer, value)?,
        Value::Json(value) => write_raw_json(writer, value)?,
        Value::Date(value) => write_text_wrapper(writer, "DATE", &format_date(*value))?,
        Value::Timestamp(value) => {
            write_text_wrapper(writer, &logical_type.to_string(), &format_timestamp(*value))?
        }
        Value::TimestampTz(value) => write_text_wrapper(
            writer,
            "TIMESTAMP_TZ",
            &format!("{}+00", format_timestamp(*value)),
        )?,
        Value::Interval(value) => write_interval(writer, *value)?,
        Value::Uuid(value) => write_text_wrapper(writer, "UUID", &format_uuid(*value))?,
        Value::Blob(value) => write_blob(writer, value)?,
        Value::List(values) => {
            let element_type = match logical_type {
                LogicalType::List(element) | LogicalType::Array(element, _) => element.as_ref(),
                _ => &LogicalType::Any,
            };
            writer.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                write_separator(writer, index)?;
                write_typed_value(writer, value, element_type, context)?;
            }
            writer.write_all(b"]")?;
        }
        Value::Struct(fields) => {
            writer.write_all(br#"{"$type":"STRUCT","fields":["#)?;
            for (index, (name, value)) in fields.iter().enumerate() {
                write_separator(writer, index)?;
                writer.write_all(b"[")?;
                write_json_string(writer, name)?;
                writer.write_all(b",")?;
                let field_type = match logical_type {
                    LogicalType::Struct(types) => types
                        .iter()
                        .find_map(|(candidate, ty)| (candidate == name).then_some(ty))
                        .unwrap_or(&LogicalType::Any),
                    _ => &LogicalType::Any,
                };
                write_typed_value(writer, value, field_type, context)?;
                writer.write_all(b"]")?;
            }
            writer.write_all(b"]}")?;
        }
        Value::Map(entries) => {
            let (key_type, value_type) = match logical_type {
                LogicalType::Map(key, value) => (key.as_ref(), value.as_ref()),
                _ => (&LogicalType::Any, &LogicalType::Any),
            };
            writer.write_all(br#"{"$type":"MAP","entries":["#)?;
            for (index, (key, value)) in entries.iter().enumerate() {
                write_separator(writer, index)?;
                writer.write_all(b"[")?;
                write_typed_value(writer, key, key_type, context)?;
                writer.write_all(b",")?;
                write_typed_value(writer, value, value_type, context)?;
                writer.write_all(b"]")?;
            }
            writer.write_all(b"]}")?;
        }
        Value::InternalId(value) => write_internal_id(writer, *value)?,
        Value::Node(value) => write_node(writer, value, context)?,
        Value::Rel(value) => write_rel(writer, value, context)?,
        Value::RecursiveRel(value) => write_recursive_rel(writer, value, context)?,
        Value::Union {
            variants,
            tag,
            value,
        } => {
            let (name, member_type) = variants
                .get(*tag)
                .map(|(name, ty)| (name.as_str(), ty))
                .unwrap_or(("", &LogicalType::Any));
            writer.write_all(br#"{"$type":"UNION","tag":"#)?;
            write_json_string(writer, name)?;
            writer.write_all(b",\"value\":")?;
            write_typed_value(writer, value, member_type, context)?;
            writer.write_all(b"}")?;
        }
    }
    Ok(())
}

fn write_bool(writer: &mut impl std::io::Write, value: bool) -> std::io::Result<()> {
    writer.write_all(if value { b"true" } else { b"false" })
}

fn write_integer(
    writer: &mut impl std::io::Write,
    value: i128,
    logical_type: &str,
) -> Result<(), EncodeError> {
    if (-MAX_EXACT_JSON_INTEGER..=MAX_EXACT_JSON_INTEGER).contains(&value) {
        write!(writer, "{value}")?;
    } else {
        writer.write_all(br#"{"$type":"INTEGER","logical_type":"#)?;
        write_json_string(writer, logical_type)?;
        writer.write_all(b",\"value\":")?;
        write_json_string(writer, &value.to_string())?;
        writer.write_all(b"}")?;
    }
    Ok(())
}

fn write_unsigned_integer(
    writer: &mut impl std::io::Write,
    value: u128,
    logical_type: &str,
) -> Result<(), EncodeError> {
    if value <= MAX_EXACT_JSON_INTEGER as u128 {
        write!(writer, "{value}")?;
    } else {
        writer.write_all(br#"{"$type":"INTEGER","logical_type":"#)?;
        write_json_string(writer, logical_type)?;
        writer.write_all(b",\"value\":")?;
        write_json_string(writer, &value.to_string())?;
        writer.write_all(b"}")?;
    }
    Ok(())
}

fn write_decimal(
    writer: &mut impl std::io::Write,
    value: i128,
    precision: u8,
    scale: u8,
) -> Result<(), EncodeError> {
    write!(
        writer,
        r#"{{"$type":"DECIMAL","precision":{precision},"scale":{scale},"value":"#
    )?;
    write_json_string(writer, &format_decimal(value, scale))?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_float(
    writer: &mut impl std::io::Write,
    value: f64,
    logical_type: &str,
) -> Result<(), EncodeError> {
    if value.is_finite() {
        write!(writer, "{value}")?;
    } else {
        let value = if value.is_nan() {
            "NaN"
        } else if value.is_sign_positive() {
            "+Infinity"
        } else {
            "-Infinity"
        };
        writer.write_all(br#"{"$type":"NONFINITE","logical_type":"#)?;
        write_json_string(writer, logical_type)?;
        writer.write_all(b",\"value\":")?;
        write_json_string(writer, value)?;
        writer.write_all(b"}")?;
    }
    Ok(())
}

fn write_text_wrapper(
    writer: &mut impl std::io::Write,
    tag: &str,
    value: &str,
) -> Result<(), EncodeError> {
    writer.write_all(br#"{"$type":"#)?;
    write_json_string(writer, tag)?;
    writer.write_all(b",\"value\":")?;
    write_json_string(writer, value)?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_interval(writer: &mut impl std::io::Write, value: Interval) -> Result<(), EncodeError> {
    write!(
        writer,
        r#"{{"$type":"INTERVAL","months":{},"days":{},"micros":{}}}"#,
        value.months, value.days, value.micros
    )?;
    Ok(())
}

fn write_blob(writer: &mut impl std::io::Write, value: &[u8]) -> Result<(), EncodeError> {
    writer.write_all(br#"{"$type":"BLOB","encoding":"base64","value":""#)?;
    {
        let mut encoded = base64::write::EncoderWriter::new(
            &mut *writer,
            &base64::engine::general_purpose::STANDARD,
        );
        encoded.write_all(value)?;
        encoded.finish()?;
    }
    writer.write_all(br#""}"#)?;
    Ok(())
}

fn write_internal_id(
    writer: &mut impl std::io::Write,
    value: InternalId,
) -> Result<(), EncodeError> {
    writer.write_all(br#"{"$type":"INTERNAL_ID","table":"#)?;
    write_json_string(writer, &value.table_id.0.to_string())?;
    writer.write_all(b",\"offset\":")?;
    write_json_string(writer, &value.offset.0.to_string())?;
    writer.write_all(b"}")?;
    Ok(())
}

fn write_node(
    writer: &mut impl std::io::Write,
    value: &NodeValue,
    context: &ResultTypeContext,
) -> Result<(), EncodeError> {
    writer.write_all(br#"{"$type":"NODE","id":"#)?;
    write_internal_id(writer, value.id)?;
    writer.write_all(b",\"label\":")?;
    write_json_string(writer, &value.label)?;
    writer.write_all(b",\"properties\":[")?;
    for (index, (name, property)) in value.props.iter().enumerate() {
        write_separator(writer, index)?;
        writer.write_all(b"[")?;
        write_json_string(writer, name)?;
        writer.write_all(b",")?;
        let logical_type = graph_property_type(context, value.id.table_id.0, name)
            .unwrap_or_else(|| value_type(property));
        write_typed_value(writer, property, logical_type, context)?;
        writer.write_all(b"]")?;
    }
    writer.write_all(b"]}")?;
    Ok(())
}

fn write_rel(
    writer: &mut impl std::io::Write,
    value: &RelValue,
    context: &ResultTypeContext,
) -> Result<(), EncodeError> {
    writer.write_all(br#"{"$type":"REL","id":"#)?;
    write_internal_id(writer, value.id)?;
    writer.write_all(b",\"src\":")?;
    write_internal_id(writer, value.src)?;
    writer.write_all(b",\"dst\":")?;
    write_internal_id(writer, value.dst)?;
    writer.write_all(b",\"label\":")?;
    write_json_string(writer, &value.label)?;
    writer.write_all(b",\"properties\":[")?;
    for (index, (name, property)) in value.props.iter().enumerate() {
        write_separator(writer, index)?;
        writer.write_all(b"[")?;
        write_json_string(writer, name)?;
        writer.write_all(b",")?;
        let logical_type = graph_property_type(context, value.id.table_id.0, name)
            .unwrap_or_else(|| value_type(property));
        write_typed_value(writer, property, logical_type, context)?;
        writer.write_all(b"]")?;
    }
    writer.write_all(b"]}")?;
    Ok(())
}

fn write_recursive_rel(
    writer: &mut impl std::io::Write,
    value: &RecursiveRelValue,
    context: &ResultTypeContext,
) -> Result<(), EncodeError> {
    writer.write_all(br#"{"$type":"RECURSIVE_REL","nodes":["#)?;
    for (index, node) in value.nodes.iter().enumerate() {
        write_separator(writer, index)?;
        write_node(writer, node, context)?;
    }
    writer.write_all(b"],\"relationships\":[")?;
    for (index, rel) in value.rels.iter().enumerate() {
        write_separator(writer, index)?;
        write_rel(writer, rel, context)?;
    }
    writer.write_all(b"],\"cost\":")?;
    if let Some(cost) = value.cost {
        write_float(writer, cost, "DOUBLE")?;
    } else {
        writer.write_all(b"null")?;
    }
    write!(
        writer,
        r#","degenerate":{},"null_nodes":{}}}"#,
        value.degenerate, value.null_nodes
    )?;
    Ok(())
}

fn graph_property_type<'a>(
    context: &'a ResultTypeContext,
    table: u64,
    name: &str,
) -> Option<&'a LogicalType> {
    context
        .graph_value(table)?
        .properties()
        .iter()
        .find(|property| property.name() == name)
        .map(|property| property.logical_type())
}

fn value_type(value: &Value) -> &LogicalType {
    // Static fallback types avoid allocating a transient `LogicalType` solely for
    // schema-less nested values. Structured graph contexts supply exact types.
    match value {
        Value::Null => &LogicalType::Any,
        Value::Bool(_) => &LogicalType::Bool,
        Value::Int64(_) => &LogicalType::Int64,
        Value::Double(_) => &LogicalType::Double,
        Value::Float(_) => &LogicalType::Float,
        Value::String(_) => &LogicalType::String,
        Value::Json(_) => &LogicalType::Json,
        Value::Date(_) => &LogicalType::Date,
        Value::Timestamp(_) => &LogicalType::Timestamp,
        Value::TimestampTz(_) => &LogicalType::TimestampTz,
        Value::Interval(_) => &LogicalType::Interval,
        Value::Uuid(_) => &LogicalType::Uuid,
        Value::Blob(_) => &LogicalType::Blob,
        Value::InternalId(_) => &LogicalType::InternalId,
        Value::RecursiveRel(_) => &LogicalType::RecursiveRel,
        _ => &LogicalType::Any,
    }
}

fn write_raw_json(writer: &mut impl std::io::Write, value: &JsonValue) -> Result<(), EncodeError> {
    match value {
        JsonValue::Null => writer.write_all(b"null")?,
        JsonValue::Bool(value) => write_bool(writer, *value)?,
        JsonValue::Int(value) => write!(writer, "{value}")?,
        JsonValue::Float(value) => write_float(writer, *value, "DOUBLE")?,
        JsonValue::String(value) => write_json_string(writer, value)?,
        JsonValue::Array(values) => {
            writer.write_all(b"[")?;
            for (index, value) in values.iter().enumerate() {
                write_separator(writer, index)?;
                write_raw_json(writer, value)?;
            }
            writer.write_all(b"]")?;
        }
        JsonValue::Object(fields) => {
            writer.write_all(b"{")?;
            for (index, (name, value)) in fields.iter().enumerate() {
                write_separator(writer, index)?;
                write_json_string(writer, name)?;
                writer.write_all(b":")?;
                write_raw_json(writer, value)?;
            }
            writer.write_all(b"}")?;
        }
    }
    Ok(())
}

fn write_json_string(writer: &mut impl std::io::Write, value: &str) -> Result<(), EncodeError> {
    serde_json::to_writer(writer, value)
        .map_err(|error| EncodeError::Io(std::io::Error::other(error)))
}

fn write_separator(writer: &mut impl std::io::Write, index: usize) -> std::io::Result<()> {
    if index != 0 {
        writer.write_all(b",")?;
    }
    Ok(())
}
