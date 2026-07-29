use super::*;

/// Cast a value to a target logical type. `NULL` casts to `NULL`; range and parse
/// failures return categorized Koko errors.
///
/// CSV parsing treats empty and configured null strings as NULL. STRING columns
/// preserve other text; every other type casts from text through the same general
/// conversion machinery used by query expressions.
pub fn parse_csv_cell(s: &str, ty: &LogicalType, null_strings: &[String]) -> Result<Value> {
    if matches!(ty, LogicalType::String) {
        if null_strings.iter().any(|null| null == s) {
            return Ok(Value::Null);
        }
        return Ok(Value::String(s.to_string()));
    }
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    if matches!(ty, LogicalType::Date) && t.bytes().any(|byte| matches!(byte, b'/' | b'\\' | b' '))
    {
        let normalized = t.replace(['/', '\\', ' '], "-");
        return cast_value(&Value::String(normalized), ty);
    }
    cast_value(&Value::String(s.to_string()), ty)
}

/// Render a type for cast errors under the C++ invariant that bind-time ANY
/// has already resolved to INT64 (an empty `[]` literal casts as INT64[]).
pub(crate) fn resolved_any(t: &LogicalType) -> LogicalType {
    match t {
        LogicalType::Any => LogicalType::Int64,
        LogicalType::List(i) => LogicalType::List(Box::new(resolved_any(i))),
        LogicalType::Array(i, n) => LogicalType::Array(Box::new(resolved_any(i)), *n),
        LogicalType::Struct(fs) => LogicalType::Struct(
            fs.iter()
                .map(|(n, t)| (n.clone(), resolved_any(t)))
                .collect(),
        ),
        LogicalType::Map(k, v) => {
            LogicalType::Map(Box::new(resolved_any(k)), Box::new(resolved_any(v)))
        }
        other => other.clone(),
    }
}

/// The C++ cast-function-resolution rendering for a MISMATCHED nested cast:
/// nested kinds print generically ("LIST", "STRUCT", "MAP", "UNION"), scalars
/// keep their full name ("INT64 to LIST", "STRUCT to MAP").
pub(crate) fn cast_kind_name(t: &LogicalType) -> String {
    match &resolved_any(t) {
        LogicalType::List(_) | LogicalType::Array(_, _) => "LIST".to_string(),
        LogicalType::Struct(_) => "STRUCT".to_string(),
        LogicalType::Map(_, _) => "MAP".to_string(),
        LogicalType::Union(_) => "UNION".to_string(),
        other => other.to_string(),
    }
}

pub fn cast_value(v: &Value, target: &LogicalType) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    // Entity values cast only to STRING (their rendering); any other target
    // is the C++ "Unsupported casting function" error.
    if matches!(
        v.logical_type(),
        LogicalType::Node(_) | LogicalType::Rel(_) | LogicalType::RecursiveRel
    ) && !matches!(target, LogicalType::String)
    {
        return Err(Error::conversion(format!(
            "Unsupported casting function from {} to {}.",
            resolved_any(&v.logical_type()),
            target
        )));
    }
    // CAST has no BOOL<->numeric path in C++ (`CAST(true AS INT8)` /
    // `CAST(1 AS BOOL)` are "Unsupported casting function" errors); the to_*
    // aliases instead reach BOOL via the (STRING) overload — see the
    // string-route in `scalarfn`'s cast-alias eval.
    if matches!(v, Value::Bool(_))
        && matches!(
            target,
            LogicalType::Int(_)
                | LogicalType::Serial
                | LogicalType::UInt128
                | LogicalType::Decimal(_, _)
                | LogicalType::Double
                | LogicalType::Float
        )
    {
        return Err(Error::conversion(format!(
            "Unsupported casting function from BOOL to {}.",
            target.name()
        )));
    }
    match target {
        LogicalType::Int(kind) => cast_to_int(v, *kind),
        LogicalType::Serial => cast_to_int(v, IntKind::I64),
        LogicalType::UInt128 => cast_to_uint128(v),
        LogicalType::Decimal(p, s) => cast_to_decimal(v, *p, *s),
        LogicalType::Double => cast_to_double(v),
        LogicalType::Float => {
            if let Some(x) = v.as_f64() {
                Ok(Value::Float(x as f32))
            } else if let Value::String(s) = v {
                let t = s.trim();
                let fail = || Error::conversion(format!("Cast failed. {s} is not in FLOAT range."));
                if cpp_rejects_numeric_string(t) {
                    return Err(fail());
                }
                t.parse::<f32>().map(Value::Float).map_err(|_| fail())
            } else {
                Err(Error::conversion(format!(
                    "Unsupported casting function from {} to FLOAT.",
                    resolved_any(&v.logical_type())
                )))
            }
        }
        LogicalType::Bool => cast_to_bool(v),
        // CAST(node/rel AS STRING) renders the STRUCT-backed physical form,
        // not the graph display: `{_ID: 0:0, _LABEL: A, id: 1, name: a}` /
        // `{_SRC: 0:0, _DST: 0:1, _LABEL: R, _ID: 1:0, w: 7}`.
        LogicalType::String => Ok(Value::String(match v {
            Value::Node(n) => {
                let mut parts = vec![
                    format!("_ID: {}", n.id),
                    format!("_LABEL: {}", n.label),
                ];
                parts.extend(n.props.iter().map(|(k, pv)| format!("{k}: {}", pv.to_result_string())));
                format!("{{{}}}", parts.join(", "))
            }
            Value::Rel(r) => {
                let mut parts = vec![
                    format!("_SRC: {}", r.src),
                    format!("_DST: {}", r.dst),
                    format!("_LABEL: {}", r.label),
                    format!("_ID: {}", r.id),
                ];
                parts.extend(r.props.iter().map(|(k, pv)| format!("{k}: {}", pv.to_result_string())));
                format!("{{{}}}", parts.join(", "))
            }
            other => other.to_result_string(),
        })),
        LogicalType::Json => match v {
            Value::Json(_) => Ok(v.clone()),
            Value::String(value) => {
                koko_common::JsonValue::parse(value).map(Value::Json)
            }
            Value::Struct(_) => {
                koko_common::JsonValue::from_value(v).map(Value::Json)
            }
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to JSON.",
                resolved_any(&v.logical_type())
            ))),
        },
        LogicalType::Uuid => match v {
            Value::Uuid(_) => Ok(v.clone()),
            Value::String(s) => koko_common::scalar::parse_uuid(s)
                .map(Value::Uuid)
                .ok_or_else(|| Error::conversion(format!("Invalid UUID: {s}"))),
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to UUID.",
                resolved_any(&v.logical_type())
            ))),
        },
        LogicalType::Blob => match v {
            Value::Blob(_) => Ok(v.clone()),
            Value::String(s) => koko_common::scalar::parse_blob(s).map(Value::Blob),
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to BLOB.",
                resolved_any(&v.logical_type())
            ))),
        },
        LogicalType::Date => match v {
            Value::Date(_) => Ok(v.clone()),
            Value::Timestamp(t) | Value::TimestampTz(t) => {
                Ok(Value::Date(t.div_euclid(MICROS_PER_DAY) as i32))
            }
            Value::String(s) => temporal::parse_date(s)
                .map(Value::Date)
                .ok_or_else(|| {
                    Error::conversion(format!(
                        "Error occurred during parsing date. Given: \"{s}\". Expected format: (YYYY-MM-DD)"
                    ))
                }),
            // A numeric source matched the (STRING) -> DATE overload via the
            // cast-cost matrix: it stringifies first, then parses (and fails
            // with the parse error — `date(2012)`, oracle-verified).
            other if other.as_int128().is_some() || other.as_f64().is_some() => {
                let s = other.to_result_string();
                temporal::parse_date(&s).map(Value::Date).ok_or_else(|| {
                    Error::conversion(format!(
                        "Error occurred during parsing date. Given: \"{s}\". Expected format: (YYYY-MM-DD)"
                    ))
                })
            }
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to DATE.",
                resolved_any(&v.logical_type())
            ))),
        },
        // TIMESTAMP and TIMESTAMP_NS store microseconds (sub-µs is dropped).
        // Parse errors name the flavor as written (TIMESTAMP_MS etc.).
        LogicalType::Timestamp => Ok(Value::Timestamp(cast_ts_micros(v, "TIMESTAMP")?)),
        LogicalType::TimestampNs => Ok(Value::Timestamp(cast_ts_micros(v, "TIMESTAMP_NS")?)),
        // TIMESTAMP_MS / _SEC truncate the stored instant to that resolution.
        LogicalType::TimestampMs => Ok(Value::Timestamp(
            cast_ts_micros(v, "TIMESTAMP_MS")?.div_euclid(1_000) * 1_000,
        )),
        LogicalType::TimestampSec => Ok(Value::Timestamp(
            cast_ts_micros(v, "TIMESTAMP_SEC")?.div_euclid(1_000_000) * 1_000_000,
        )),
        // TIMESTAMP_TZ keeps microseconds but renders with a `+00` UTC suffix.
        LogicalType::TimestampTz => Ok(Value::TimestampTz(cast_ts_micros(v, "TIMESTAMP_TZ")?)),
        LogicalType::Interval => match v {
            Value::Interval(_) => Ok(v.clone()),
            Value::String(s) => temporal::parse_interval(s).map(Value::Interval),
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to INTERVAL.",
                resolved_any(&v.logical_type())
            ))),
        },
        // A fixed-size `Array` casts element-wise exactly like a `List` (values are
        // stored as `Value::List`); only its declared length distinguishes the type.
        LogicalType::List(inner) | LogicalType::Array(inner, _) => match v {
            Value::List(items) => {
                // Casting a LIST to a fixed-size ARRAY enforces the declared length,
                // matching C++ `cast_array.cpp` (a runtime `ConversionException`).
                if let LogicalType::Array(_, n) = target {
                    if items.len() as u64 != *n {
                        return Err(Error::conversion(format!(
                            "Unsupported casting LIST with incorrect list entry to ARRAY. \
                             Expected: {n}, Actual: {}.",
                            items.len()
                        )));
                    }
                }
                Ok(Value::List(
                    items
                        .iter()
                        .map(|e| cast_value(e, inner))
                        .collect::<Result<_>>()?,
                ))
            }
            Value::String(s) => koko_common::literal::parse_string_cast(s, target),
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to {}.",
                cast_kind_name(&v.logical_type()),
                cast_kind_name(target)
            ))),
        },
        // STRUCT/MAP recasts recurse element-wise into the target child types
        // (positional for struct, per-entry for map) so e.g. MAP(STRING,STRING)
        // -> MAP(INT32,UINT8) actually converts and range-checks each element.
        LogicalType::Struct(target_fields) => match v {
            Value::Struct(src) => {
                // C++ resolves STRUCT→STRUCT only when the field-name
                // sequences agree (count and, positionally, names); otherwise
                // there is no cast function between the two shapes.
                let names_match = src.len() == target_fields.len()
                    && src
                        .iter()
                        .zip(target_fields)
                        .all(|((sn, _), (tn, _))| sn.eq_ignore_ascii_case(tn));
                if !names_match {
                    return Err(Error::conversion(format!(
                        "Unsupported casting function from {} to {}.",
                        resolved_any(&v.logical_type()),
                        target
                    )));
                }
                Ok(Value::Struct(
                    target_fields
                        .iter()
                        .enumerate()
                        .map(|(i, (name, fty))| {
                            let val = match src.get(i) {
                                Some((_, sv)) => cast_value(sv, fty)?,
                                None => Value::Null,
                            };
                            Ok((name.clone(), val))
                        })
                        .collect::<Result<_>>()?,
                ))
            }
            Value::String(s) => koko_common::literal::parse_string_cast(s, target),
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to {}.",
                cast_kind_name(&v.logical_type()),
                cast_kind_name(target)
            ))),
        },
        LogicalType::Map(kt, vt) => match v {
            Value::Map(entries) => Ok(Value::Map(
                entries
                    .iter()
                    .map(|(k, val)| Ok((cast_value(k, kt)?, cast_value(val, vt)?)))
                    .collect::<Result<_>>()?,
            )),
            Value::String(s) => koko_common::literal::parse_string_cast(s, target),
            _ => Err(Error::conversion(format!(
                "Unsupported casting function from {} to {}.",
                cast_kind_name(&v.logical_type()),
                cast_kind_name(target)
            ))),
        },
        LogicalType::Union(dfields) => match v {
            // UNION → UNION (C++ `resolveNestedVector`): every source alternative
            // must exist in the target *by name* and implicitly cast to the
            // target's type for it; the active payload is then cast and its tag
            // remapped. Both violations are exec-time Conversion exceptions.
            Value::Union {
                variants,
                tag,
                value,
            } => {
                for (name, sty) in variants {
                    let Some((_, dty)) = dfields.iter().find(|(dn, _)| dn == name) else {
                        return Err(Error::conversion(format!(
                            "Cannot cast from {} to {}, target type is missing field '{}'.",
                            v.logical_type(),
                            target,
                            name
                        )));
                    };
                    if !koko_common::types::implicitly_castable(sty, dty) {
                        return Err(Error::conversion(format!(
                            "Unsupported casting function from {sty} to {dty}."
                        )));
                    }
                }
                let (active, _) = &variants[*tag];
                let didx = dfields
                    .iter()
                    .position(|(dn, _)| dn == active)
                    .expect("active field checked present above");
                let inner = cast_value(value, &dfields[didx].1)?;
                Ok(Value::Union {
                    variants: dfields.clone(),
                    tag: didx,
                    value: Box::new(inner),
                })
            }
            Value::String(s) => koko_common::literal::parse_string_cast(s, target),
            // Scalar → UNION: pick the min-cast-cost alternative (C++
            // `findUnionMinCostTag`; first field wins a tie). Fields admitted only
            // by the numeric catch-all have UNDEFINED cost and never win — with no
            // cost-defined field at all the cast fails even if one would be
            // implicitly castable.
            other => {
                let sty = other.logical_type();
                let Some(tag) = koko_common::types::union_min_cost_tag(&sty, dfields) else {
                    return Err(Error::conversion(format!(
                        "Cannot cast from {sty} to {target}, target type has no compatible field."
                    )));
                };
                let inner = cast_value(other, &dfields[tag].1)?;
                Ok(Value::Union {
                    variants: dfields.clone(),
                    tag,
                    value: Box::new(inner),
                })
            }
        },
        // No cast function reaches INTERNAL_ID from any source type.
        LogicalType::InternalId => Err(Error::conversion(format!(
            "Unsupported casting function from {} to INTERNAL_ID.",
            resolved_any(&v.logical_type())
        ))),
        other => Err(Error::not_implemented(format!(
            "cast to {other} is not supported in this phase"
        ))),
    }
}

/// Whether the C++ numeric-string cast rejects this (trimmed) text on *form*:
/// a leading `+` is never accepted, and an unsigned number may not have leading
/// zeros (`007`, `00`) — while a signed one may (`-007` → -7, oracle-verified).
/// `0` itself and `0.5`-style fractions are fine (the zero is not followed by a
/// digit). DECIMAL is looser: it rejects only the leading `+` (`007` casts fine).
pub(crate) fn cpp_rejects_numeric_string(t: &str) -> bool {
    if t.starts_with('+') {
        return true;
    }
    let b = t.as_bytes();
    b.len() >= 2 && b[0] == b'0' && b[1].is_ascii_digit()
}

/// Float → integer with C++ semantics (audit V1): `nearbyint` half-to-even
/// rounding, pre-round negativity check for unsigned targets, and the overflow
/// message rendering the offending float like `%f`.
pub(crate) fn float_to_int(x: f64, kind: IntKind) -> Result<Value> {
    let fail = || {
        Error::overflow(format!(
            "Value {} is not within {} range",
            koko_common::value::format_float(x),
            kind.name()
        ))
    };
    if !kind.is_signed() && x < 0.0 {
        return Err(fail());
    }
    if x.is_nan() {
        return Err(fail());
    }
    // C++ range-checks the UNROUNDED double against [min, max+1] and then
    // rounds half-to-even and truncates to the width — so a value in the
    // (max, max+1] window WRAPS (oracle: CAST(127.9 AS INT8) = -128,
    // CAST(255.9 AS UINT8) = 0) while CAST(128.4 AS INT8) errors.
    use IntKind::*;
    let (min_f, max_plus_1) = match kind {
        I8 => (i8::MIN as f64, i8::MAX as f64 + 1.0),
        I16 => (i16::MIN as f64, i16::MAX as f64 + 1.0),
        I32 => (i32::MIN as f64, i32::MAX as f64 + 1.0),
        I64 => (i64::MIN as f64, 9223372036854775808.0),
        I128 => (
            -170141183460469231731687303715884105728.0,
            170141183460469231731687303715884105728.0,
        ),
        U8 => (0.0, u8::MAX as f64 + 1.0),
        U16 => (0.0, u16::MAX as f64 + 1.0),
        U32 => (0.0, u32::MAX as f64 + 1.0),
        U64 => (0.0, 18446744073709551616.0),
    };
    if x < min_f || x > max_plus_1 {
        return Err(fail());
    }
    let r = x.round_ties_even();
    let val = match kind {
        I8 => r as i128 as i8 as i128,
        I16 => r as i128 as i16 as i128,
        I32 => r as i128 as i32 as i128,
        I64 => r as i128 as i64 as i128,
        I128 => r as i128,
        U8 => r as i128 as u8 as i128,
        U16 => r as i128 as u16 as i128,
        U32 => r as i128 as u32 as i128,
        U64 => r as i128 as u64 as i128,
    };
    Ok(Value::make_int(val, kind))
}

pub(crate) fn cast_to_int(v: &Value, kind: IntKind) -> Result<Value> {
    // String source: parsed within the target's bounds; any failure (unparseable,
    // bad form, or out of range) is the C++ "Could not convert" conversion error.
    if let Value::String(s) = v {
        let fail = || {
            Error::conversion(format!(
                "Cast failed. Could not convert \"{s}\" to {}.",
                kind.name()
            ))
        };
        let t = s.trim();
        if cpp_rejects_numeric_string(t) {
            return Err(fail());
        }
        let val: i128 = t.parse().map_err(|_| fail())?;
        if !kind.contains(val) {
            return Err(fail());
        }
        return Ok(Value::make_int(val, kind));
    }
    // Numeric source: compute the i128 value, then range-check. An out-of-range
    // numeric cast is the C++ "Value ... is not within ... range" overflow error.
    let val: i128 = if let Some(n) = v.as_int128() {
        n
    } else {
        match v {
            // Float sources round half-to-even (C++ `nearbyint` — audit V1) and
            // range-check with the *float* rendered `%f`-style in the message
            // (`Value -0.400000 is not within UINT16 range`). Narrow unsigned
            // targets reject a negative source before rounding (oracle-verified:
            // `CAST(-0.4 AS UINT16)` errors while `CAST(-0.4 AS INT8)` is 0).
            Value::Double(x) => return float_to_int(*x, kind),
            Value::Float(x) => return float_to_int(f64::from(*x), kind),
            Value::Bool(b) => i128::from(*b),
            // DECIMAL → INT rounds half away from zero (matching the C++ cast).
            Value::Decimal { value, scale, .. } => {
                koko_common::decimal::rescale(*value, *scale, 0).unwrap_or(0)
            }
            // A UINT128 that didn't fit `as_int128` exceeds i128, hence is out of
            // range for every signed width. (The C++ INT128 cast path renders this
            // message with a trailing period, unlike the narrower-width path.)
            Value::UInt128(u) => {
                return Err(Error::overflow(format!(
                    "Value {u} is not within {} range.",
                    kind.name()
                )));
            }
            _ => {
                return Err(Error::conversion(format!(
                    "Unsupported casting function from {} to {}.",
                    resolved_any(&v.logical_type()),
                    kind.name()
                )));
            }
        }
    };
    if !kind.contains(val) {
        return Err(Error::overflow(format!(
            "Value {val} is not within {} range",
            kind.name()
        )));
    }
    Ok(Value::make_int(val, kind))
}

/// Float → UINT128 with C++ semantics: half-to-even rounding first, then the
/// range check — so `CAST(-0.4 AS UINT128)` rounds to -0.0 and is accepted as 0
/// (oracle-verified; narrower unsigned widths instead reject pre-round).
pub(crate) fn float_to_uint128(x: f64, _v: &Value) -> Result<Value> {
    // Out-of-range (negative, > max, inf) and NaN all wrap to 0 — the C++
    // float→uint128 conversion's deterministic result (oracle-verified:
    // to_uint128(to_float(3.4e38)) → 0, to_uint128(-5.7) → 0).
    let r = x.round_ties_even();
    if !(r == 0.0 || (0.0..340282366920938463463374607431768211456.0).contains(&r)) {
        return Ok(Value::UInt128(0));
    }
    Ok(Value::UInt128(r.max(0.0) as u128))
}

pub(crate) fn cast_to_uint128(v: &Value) -> Result<Value> {
    // Any non-negative integer (incl. UINT128) maps directly.
    if let Some(u) = v.as_u128() {
        return Ok(Value::UInt128(u));
    }
    match v {
        Value::Double(x) => float_to_uint128(*x, v),
        Value::Float(x) => float_to_uint128(f64::from(*x), v),
        Value::Bool(b) => Ok(Value::UInt128(u128::from(*b))),
        Value::String(s) => {
            let t = s.trim();
            let fail = || {
                Error::conversion(format!(
                    "Cast failed. Could not convert \"{s}\" to UINT128."
                ))
            };
            if cpp_rejects_numeric_string(t) {
                return Err(fail());
            }
            t.parse::<u128>().map(Value::UInt128).map_err(|_| fail())
        }
        // A negative integer is out of UINT128 range. The C++ INT128 path names
        // the source ("Cannot cast negative INT128 value {n} to UINT128"); the
        // generic path uses "Cannot cast negative value to UINT128." (with period).
        Value::IntX {
            value,
            kind: IntKind::I128,
        } => Err(Error::overflow(format!(
            "Cannot cast negative INT128 value {value} to UINT128"
        ))),
        _ if v.as_int128().is_some() => Err(Error::overflow(
            "Cannot cast negative value to UINT128.".to_string(),
        )),
        _ => Err(Error::conversion(format!(
            "Unsupported casting function from {} to UINT128.",
            resolved_any(&v.logical_type())
        ))),
    }
}

/// Cast to `DECIMAL(p, s)`. The overflow message differs by source type to match
/// the C++ engine: integer source ⇒ "To Decimal Cast Failed: …", decimal source
/// ⇒ "Decimal Cast Failed: input …", string source ⇒ the generic "Cast failed.".
pub(crate) fn cast_to_decimal(v: &Value, precision: u8, scale: u8) -> Result<Value> {
    use koko_common::decimal;
    let make = |value: i128| Value::Decimal {
        value,
        precision,
        scale,
    };
    match v {
        // Decimal → decimal: rescale, then range-check.
        Value::Decimal {
            value,
            scale: from_scale,
            ..
        } => {
            let rescaled = decimal::rescale(*value, *from_scale, scale)
                .filter(|r| decimal::fits(*r, precision))
                .ok_or_else(|| {
                    Error::overflow(format!(
                        "Decimal Cast Failed: input {} is not in range of DECIMAL({precision}, {scale})",
                        v.to_result_string()
                    ))
                })?;
            Ok(make(rescaled))
        }
        // Integer → decimal: scale up by 10^scale, then range-check.
        _ if v.as_int128().is_some() => {
            let n = v.as_int128().unwrap();
            let value = n.checked_mul(decimal::pow10(scale));
            match value.filter(|x| decimal::fits(*x, precision)) {
                Some(x) => Ok(make(x)),
                None => Err(Error::overflow(format!(
                    "To Decimal Cast Failed: {n} is not in DECIMAL({precision}, {scale}) range"
                ))),
            }
        }
        // Double → decimal: route through the shortest round-trip string so the
        // result matches the literal the user sees (avoids f64*10^s drift).
        Value::Double(x) => {
            let s = format!("{x}");
            let value = decimal::parse_to_unscaled(&s, scale)
                .filter(|r| decimal::fits(*r, precision))
                .ok_or_else(|| {
                    // The C++ message renders the double %f-style (99.999000).
                    Error::overflow(format!(
                        "To Decimal Cast Failed: {} is not in DECIMAL({precision}, {scale}) range",
                        koko_common::value::format_float(*x)
                    ))
                })?;
            Ok(make(value))
        }
        Value::String(s) => {
            // DECIMAL's string form is looser than INT/DOUBLE: leading zeros are
            // accepted (`007` → 7.00); only a leading `+` is rejected.
            let value = Some(s.trim())
                .filter(|t| !t.starts_with('+'))
                .and_then(|t| decimal::parse_to_unscaled(t, scale))
                .ok_or_else(|| {
                    Error::conversion(format!(
                        "Cast failed. {s} is not in DECIMAL({precision}, {scale}) range."
                    ))
                })?;
            if !decimal::fits(value, precision) {
                return Err(Error::conversion(format!(
                    "Cast failed. {s} is not in DECIMAL({precision}, {scale}) range."
                )));
            }
            Ok(make(value))
        }
        _ => Err(Error::conversion(format!(
            "Unsupported casting function from {} to DECIMAL({precision}, {scale}).",
            resolved_any(&v.logical_type())
        ))),
    }
}

/// The microseconds-since-epoch of any timestamp-family source (TIMESTAMP,
/// TIMESTAMP_TZ, DATE at midnight, or a parseable string), for the timestamp
/// cast targets.
pub(crate) fn cast_ts_micros(v: &Value, flavor: &str) -> Result<i64> {
    match v {
        Value::Timestamp(t) | Value::TimestampTz(t) => Ok(*t),
        Value::Date(d) => Ok(*d as i64 * MICROS_PER_DAY),
        Value::String(s) => temporal::parse_timestamp(s).ok_or_else(|| {
            Error::conversion(format!(
                "Error occurred during parsing {flavor}. Given: \"{s}\". Expected format: (YYYY-MM-DD hh:mm:ss[.zzzzzz][+-TT[:tt]])"
            ))
        }),
        _ => Err(Error::conversion(format!(
            "Unsupported casting function from {} to {flavor}.",
            resolved_any(&v.logical_type())
        ))),
    }
}

pub(crate) fn cast_to_double(v: &Value) -> Result<Value> {
    if let Some(x) = v.as_f64() {
        return Ok(Value::Double(x));
    }
    match v {
        Value::Bool(b) => Ok(Value::Double(if *b { 1.0 } else { 0.0 })),
        Value::String(s) => {
            let t = s.trim();
            let fail = || Error::conversion(format!("Cast failed. {s} is not in DOUBLE range."));
            if cpp_rejects_numeric_string(t) {
                return Err(fail());
            }
            t.parse::<f64>().map(Value::Double).map_err(|_| fail())
        }
        _ => Err(Error::conversion(format!(
            "Unsupported casting function from {} to DOUBLE.",
            resolved_any(&v.logical_type())
        ))),
    }
}

pub(crate) fn cast_to_bool(v: &Value) -> Result<Value> {
    match v {
        Value::Bool(_) => Ok(v.clone()),
        // Matches C++ `tryCastToBool`: `true/t/1` and `false/f/0` (case-insensitive).
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Ok(Value::Bool(true)),
            "false" | "f" | "0" => Ok(Value::Bool(false)),
            _ => Err(Error::conversion(format!(
                "Value {s} is not a valid boolean"
            ))),
        },
        _ => Err(Error::conversion(format!(
            "Unsupported casting function from {} to BOOL.",
            resolved_any(&v.logical_type()).name()
        ))),
    }
}
