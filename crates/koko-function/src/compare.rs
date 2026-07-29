use super::*;

pub(crate) fn eval_comparison(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    let (a, b) = coerce_comparison_values(a, b)?;
    Ok(match cypher_cmp(a.as_ref(), b.as_ref()) {
        Some(ord) => Value::Bool(match op {
            ScalarOp::Eq => ord == Ordering::Equal,
            ScalarOp::Ne => ord != Ordering::Equal,
            ScalarOp::Lt => ord == Ordering::Less,
            ScalarOp::Le => ord != Ordering::Greater,
            ScalarOp::Gt => ord == Ordering::Greater,
            ScalarOp::Ge => ord != Ordering::Less,
            _ => unreachable!(),
        }),
        None => match op {
            // Different, incomparable types: `=`/`<>` are decidable, the rest are null.
            ScalarOp::Eq => Value::Bool(false),
            ScalarOp::Ne => Value::Bool(true),
            _ => Value::Null,
        },
    })
}

/// The C++ comparison binder treats STRING as the "minimal" comparable type:
/// when it is compared to another implemented scalar/nested family, the STRING
/// side is force-cast to the other side's type before comparison (e.g.
/// `'1' = 1`, `'2020-01-01' = date_col`, UUID strings, etc.).  Other
/// cross-family pairs keep the existing runtime comparison path so numeric
/// equality, decimal exactness, grouping keys, and DATE/TIMESTAMP promotion stay
/// centralized in [`cypher_cmp`] / [`ValueKey`].
pub fn comparison_common_type(a: &LogicalType, b: &LogicalType) -> Option<LogicalType> {
    if a == b {
        return Some(a.clone());
    }
    match (a, b) {
        (LogicalType::Any, t) | (t, LogicalType::Any) => Some(t.clone()),
        (LogicalType::String, t) if string_comparison_target(t) => Some(t.clone()),
        (t, LogicalType::String) if string_comparison_target(t) => Some(t.clone()),
        (LogicalType::List(ae), LogicalType::List(be)) => {
            comparison_common_type(ae, be).map(|t| LogicalType::List(Box::new(t)))
        }
        (LogicalType::Struct(af), LogicalType::Struct(bf)) => {
            if af.len() != bf.len() {
                return None;
            }
            let mut fields = Vec::with_capacity(af.len());
            for ((an, at), (bn, bt)) in af.iter().zip(bf) {
                if an != bn {
                    return None;
                }
                fields.push((an.clone(), comparison_common_type(at, bt)?));
            }
            Some(LogicalType::Struct(fields))
        }
        (LogicalType::Map(ak, av), LogicalType::Map(bk, bv)) => Some(LogicalType::Map(
            Box::new(comparison_common_type(ak, bk)?),
            Box::new(comparison_common_type(av, bv)?),
        )),
        _ => None,
    }
}

/// Whether two types may appear in a comparison at all — C++ rejects
/// non-unifiable pairs at bind with `Type Mismatch: Cannot compare types L
/// and R` (BOOL vs INT64, DATE vs INT64, INT64[] vs BOOL[], …). Numerics
/// inter-compare, STRING casts to the other side, the DATE/TIMESTAMP family
/// unifies, and lists/arrays compare elementwise.
pub fn comparison_comparable(a: &LogicalType, b: &LogicalType) -> bool {
    if comparison_common_type(a, b).is_some() {
        return true;
    }
    if a.is_numeric() && b.is_numeric() {
        return true;
    }
    let temporal = |t: &LogicalType| {
        matches!(
            t,
            LogicalType::Date
                | LogicalType::Timestamp
                | LogicalType::TimestampNs
                | LogicalType::TimestampMs
                | LogicalType::TimestampSec
                | LogicalType::TimestampTz
        )
    };
    if temporal(a) && temporal(b) {
        return true;
    }
    match (a, b) {
        (
            LogicalType::List(ae) | LogicalType::Array(ae, _),
            LogicalType::List(be) | LogicalType::Array(be, _),
        ) => comparison_comparable(ae, be),
        (LogicalType::Node(_), LogicalType::Node(_)) => true,
        (LogicalType::Rel(_), LogicalType::Rel(_)) => true,
        // Structs need only the same arity with positionwise-comparable field
        // types — C++ binds `{a:1} = {b:1}` fine (it evaluates False), but
        // `{a:1} = {a:1,b:2}` is the Type Mismatch.
        (LogicalType::Struct(af), LogicalType::Struct(bf)) => {
            af.len() == bf.len()
                && af
                    .iter()
                    .zip(bf)
                    .all(|((_, at), (_, bt))| comparison_comparable(at, bt))
        }
        _ => false,
    }
}

pub(crate) fn string_comparison_target(t: &LogicalType) -> bool {
    !matches!(
        t,
        LogicalType::Any
            | LogicalType::String
            | LogicalType::InternalId
            | LogicalType::Node(_)
            | LogicalType::Rel(_)
            | LogicalType::RecursiveRel
    )
}

pub(crate) fn coerce_comparison_values<'a>(
    a: &'a Value,
    b: &'a Value,
) -> Result<(Cow<'a, Value>, Cow<'a, Value>)> {
    let a_ty = a.logical_type();
    let b_ty = b.logical_type();
    let Some(target) = comparison_common_type(&a_ty, &b_ty) else {
        return Ok((Cow::Borrowed(a), Cow::Borrowed(b)));
    };
    Ok((
        coerce_comparison_value(a, &a_ty, &target)?,
        coerce_comparison_value(b, &b_ty, &target)?,
    ))
}

pub(crate) fn coerce_comparison_value<'a>(
    v: &'a Value,
    src: &LogicalType,
    target: &LogicalType,
) -> Result<Cow<'a, Value>> {
    if *target == LogicalType::Any || *src == *target || v.is_null() {
        Ok(Cow::Borrowed(v))
    } else {
        Ok(Cow::Owned(cast_value(v, target)?))
    }
}

/// Cypher value comparison. `None` ⇒ incomparable (different value families).
/// Callers must handle NULLs before calling (NULLs are not passed here).
/// Element comparison *inside* composite values (audit V5): C++ compares
/// list/struct/map contents with a total order — NULL equals NULL and sorts
/// above every non-NULL — so `=`/`<`/`>` on composites never return NULL even
/// with NULL elements. Top-level scalar NULLs keep ordinary 3VL semantics.
pub(crate) fn composite_elem_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => Some(Ordering::Equal),
        (true, false) => Some(Ordering::Greater),
        (false, true) => Some(Ordering::Less),
        (false, false) => cypher_cmp(a, b),
    }
}

pub fn cypher_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    // UINT128 vs any integer: compare in u128 (a negative operand is < any u128).
    if matches!(a, Value::UInt128(_)) || matches!(b, Value::UInt128(_)) {
        match (a.as_u128(), b.as_u128()) {
            (Some(x), Some(y)) => return Some(x.cmp(&y)),
            // One side is a negative integer (no u128) → it sorts below the UINT128.
            (None, Some(_)) if a.as_int128().is_some() => return Some(Ordering::Less),
            (Some(_), None) if b.as_int128().is_some() => return Some(Ordering::Greater),
            _ => {}
        }
    }
    // DECIMAL vs DECIMAL/INTEGER: compare exactly via cross-scaling in i128
    // (a DECIMAL-vs-DOUBLE pair falls through to the float path below).
    if matches!(a, Value::Decimal { .. }) || matches!(b, Value::Decimal { .. }) {
        if let Some(ord) = decimal_cmp(a, b) {
            return Some(ord);
        }
    }
    // Integer vs integer (any width): exact i128 comparison.
    if let (Some(x), Some(y)) = (a.as_int128(), b.as_int128()) {
        return Some(x.cmp(&y));
    }
    // Numeric comparison involving a float/double (int-vs-int handled above).
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        return Some(float_cmp(x, y));
    }
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Json(x), Value::Json(y)) => Some(x.render().cmp(&y.render())),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        // TIMESTAMP / TIMESTAMP_TZ compare by instant (microseconds).
        (
            Value::Timestamp(x) | Value::TimestampTz(x),
            Value::Timestamp(y) | Value::TimestampTz(y),
        ) => Some(x.cmp(y)),
        // DATE vs TIMESTAMP: promote the DATE to midnight micros and compare.
        (Value::Timestamp(x) | Value::TimestampTz(x), Value::Date(d)) => {
            Some(x.cmp(&(*d as i64 * MICROS_PER_DAY)))
        }
        (Value::Date(d), Value::Timestamp(y) | Value::TimestampTz(y)) => {
            Some((*d as i64 * MICROS_PER_DAY).cmp(y))
        }
        (Value::Interval(x), Value::Interval(y)) => Some(x.cmp_micros().cmp(&y.cmp_micros())),
        (Value::Uuid(x), Value::Uuid(y)) => Some(x.cmp(y)),
        (Value::Blob(x), Value::Blob(y)) => Some(x.cmp(y)),
        (Value::List(x), Value::List(y)) => {
            // Element-wise, then by length (Cypher list ordering). Elements use
            // the composite total order (NULL == NULL, NULL sorts greatest), so
            // composite comparisons never return NULL — audit V5, oracle-verified
            // ([1,NULL]=[1,NULL] → True; [3,4]>[3,NULL] → False).
            for (a, b) in x.iter().zip(y.iter()) {
                match composite_elem_cmp(a, b) {
                    Some(Ordering::Equal) => continue,
                    other => return other,
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        (Value::Struct(x), Value::Struct(y)) => {
            // Field-wise by NAME then value, then by field count — the names
            // participate ({a:1} = {b:1} is False in C++), and a name mismatch
            // orders lexically so equality is simply false.
            for ((nx, vx), (ny, vy)) in x.iter().zip(y.iter()) {
                match nx.cmp(ny) {
                    Ordering::Equal => {}
                    other => return Some(other),
                }
                match composite_elem_cmp(vx, vy) {
                    Some(Ordering::Equal) => continue,
                    other => return other,
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        (Value::Map(x), Value::Map(y)) => {
            for ((kx, vx), (ky, vy)) in x.iter().zip(y.iter()) {
                match composite_elem_cmp(kx, ky) {
                    Some(Ordering::Equal) => {}
                    other => return other,
                }
                match composite_elem_cmp(vx, vy) {
                    Some(Ordering::Equal) => continue,
                    other => return other,
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        (Value::InternalId(x), Value::InternalId(y)) => Some(x.cmp(y)),
        // Node/rel equality is identity by internal id (consistent with `ValueKey`,
        // which collapses InternalId/Node/Rel to the same key). Reached once full
        // node/rel values land in containers carried across a `WITH` boundary.
        (Value::Node(x), Value::Node(y)) => Some(x.id.cmp(&y.id)),
        (Value::Rel(x), Value::Rel(y)) => Some(x.id.cmp(&y.id)),
        // Unions compare by active tag, then payload (consistent with `ValueKey`).
        (
            Value::Union {
                tag: tx, value: vx, ..
            },
            Value::Union {
                tag: ty, value: vy, ..
            },
        ) => match tx.cmp(ty) {
            Ordering::Equal => cypher_cmp(vx, vy),
            other => Some(other),
        },
        _ => None,
    }
}

/// `(unscaled, scale)` of a DECIMAL, or an integer treated as scale-0. `None`
/// for non-exact operands (e.g. DOUBLE) so the caller can fall back to float.
pub(crate) fn decimal_operand(v: &Value) -> Option<(i128, u8)> {
    match v {
        Value::Decimal { value, scale, .. } => Some((*value, *scale)),
        _ => v.as_int128().map(|n| (n, 0)),
    }
}

/// Exact comparison of two decimal/integer operands. The common no-overflow path
/// cross-scales in `i128`; the fallback compares normalized decimal strings so
/// high-scale/high-precision values never collapse through `f64`.
pub(crate) fn decimal_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    let (mut va, mut sa) = decimal_operand(a)?;
    let (mut vb, mut sb) = decimal_operand(b)?;
    (va, sa) = normalize_decimal(va, sa);
    (vb, sb) = normalize_decimal(vb, sb);
    if sa == sb {
        return Some(va.cmp(&vb));
    }
    if let (Some(lhs), Some(rhs)) = (
        va.checked_mul(koko_common::decimal::pow10(sb)),
        vb.checked_mul(koko_common::decimal::pow10(sa)),
    ) {
        return Some(lhs.cmp(&rhs));
    }
    Some(decimal_cmp_slow(va, sa, vb, sb))
}

pub(crate) fn float_cmp(x: f64, y: f64) -> Ordering {
    if x.is_nan() || y.is_nan() {
        // C++ comparison templates implement `<`/`<=` as complements of
        // `>=`/`>`, so any comparison involving NaN behaves like "less" for
        // the `cypher_cmp` ordering consumed by scalar comparisons and min/max.
        Ordering::Less
    } else {
        x.partial_cmp(&y).unwrap()
    }
}

pub(crate) fn normalize_decimal(mut value: i128, mut scale: u8) -> (i128, u8) {
    if value == 0 {
        return (0, 0);
    }
    while scale > 0 && value % 10 == 0 {
        value /= 10;
        scale -= 1;
    }
    (value, scale)
}

pub(crate) fn decimal_cmp_slow(va: i128, sa: u8, vb: i128, sb: u8) -> Ordering {
    match (va.is_negative(), vb.is_negative()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let ord = decimal_abs_cmp(va.unsigned_abs(), sa, vb.unsigned_abs(), sb);
    if va.is_negative() { ord.reverse() } else { ord }
}

pub(crate) fn decimal_abs_cmp(va: u128, sa: u8, vb: u128, sb: u8) -> Ordering {
    let (ia, fa) = decimal_abs_parts(va, sa);
    let (ib, fb) = decimal_abs_parts(vb, sb);
    ia.len()
        .cmp(&ib.len())
        .then_with(|| ia.cmp(&ib))
        .then_with(|| {
            let n = fa.len().max(fb.len());
            fa.bytes()
                .chain(std::iter::repeat(b'0'))
                .take(n)
                .cmp(fb.bytes().chain(std::iter::repeat(b'0')).take(n))
        })
}

pub(crate) fn decimal_abs_parts(value: u128, scale: u8) -> (String, String) {
    let digits = value.to_string();
    let scale = scale as usize;
    let (mut int, mut frac) = if scale == 0 {
        (digits, String::new())
    } else if digits.len() > scale {
        let split = digits.len() - scale;
        (digits[..split].to_string(), digits[split..].to_string())
    } else {
        (
            "0".to_string(),
            format!("{}{}", "0".repeat(scale - digits.len()), digits),
        )
    };
    let trimmed_int = int.trim_start_matches('0');
    int = if trimmed_int.is_empty() {
        "0".to_string()
    } else {
        trimmed_int.to_string()
    };
    while frac.ends_with('0') {
        frac.pop();
    }
    (int, frac)
}

/// A type rank used to give a *total* order across value families for ORDER BY.
pub(crate) fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Bool(_) => 0,
        Value::Int64(_)
        | Value::IntX { .. }
        | Value::UInt128(_)
        | Value::Decimal { .. }
        | Value::Double(_)
        | Value::Float(_) => 1,
        Value::String(_) => 2,
        Value::Json(_) => 3,
        Value::Date(_) => 4,
        Value::Timestamp(_) | Value::TimestampTz(_) => 5,
        Value::Interval(_) => 6,
        Value::Uuid(_) => 7,
        Value::Blob(_) => 8,
        Value::List(_) => 9,
        Value::Struct(_) => 10,
        Value::Map(_) => 11,
        Value::Union { .. } => 12,
        Value::InternalId(_) => 13,
        Value::Node(_) => 14,
        Value::Rel(_) => 15,
        Value::RecursiveRel(_) => 16,
        Value::Null => 17, // sorts last in ascending order
    }
}

/// A total ordering over values for ORDER BY (NULLs last in ascending order).
pub fn order_cmp(a: &Value, b: &Value) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    if let Some(ord) = cypher_cmp(a, b) {
        return ord;
    }
    type_rank(a).cmp(&type_rank(b))
}

/// A hashable projection of a value, for `DISTINCT` and `GROUP BY` keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ValueKey {
    Null,
    Bool(bool),
    /// All integer widths fold onto one key (so `1` and `1.0` group together).
    Int(i128),
    /// Doubles keyed by bit pattern (sufficient for grouping/distinct).
    Float(u64),
    /// Exact normalized DECIMAL key (not collapsed through f64).
    Decimal(i128, u8),
    Str(String),
    Json(String),
    Date(i32),
    Ts(i64),
    Iv(i128),
    U128(u128),
    Bytes(Vec<u8>),
    List(Vec<ValueKey>),
    Struct(Vec<(String, ValueKey)>),
    Map(Vec<(ValueKey, ValueKey)>),
    /// A union keys by its active tag index plus the payload key.
    Union(usize, Box<ValueKey>),
    Iid(u64, u64),
}

impl ValueKey {
    /// Heap bytes retained below this key, excluding the key value itself.
    pub fn heap_bytes(&self) -> u64 {
        match self {
            ValueKey::Str(value) | ValueKey::Json(value) => value.capacity() as u64,
            ValueKey::Bytes(value) => value.capacity() as u64,
            ValueKey::List(values) => {
                (values.capacity() * std::mem::size_of::<ValueKey>()) as u64
                    + values.iter().map(ValueKey::heap_bytes).sum::<u64>()
            }
            ValueKey::Struct(fields) => {
                (fields.capacity() * std::mem::size_of::<(String, ValueKey)>()) as u64
                    + fields
                        .iter()
                        .map(|(name, value)| name.capacity() as u64 + value.heap_bytes())
                        .sum::<u64>()
            }
            ValueKey::Map(entries) => {
                (entries.capacity() * std::mem::size_of::<(ValueKey, ValueKey)>()) as u64
                    + entries
                        .iter()
                        .map(|(key, value)| key.heap_bytes() + value.heap_bytes())
                        .sum::<u64>()
            }
            ValueKey::Union(_, value) => {
                std::mem::size_of::<ValueKey>() as u64 + value.heap_bytes()
            }
            ValueKey::Null
            | ValueKey::Bool(_)
            | ValueKey::Int(_)
            | ValueKey::Float(_)
            | ValueKey::Decimal(_, _)
            | ValueKey::Date(_)
            | ValueKey::Ts(_)
            | ValueKey::Iv(_)
            | ValueKey::U128(_)
            | ValueKey::Iid(_, _) => 0,
        }
    }

    pub fn from_value(v: &Value) -> ValueKey {
        match v {
            Value::Null => ValueKey::Null,
            Value::Bool(b) => ValueKey::Bool(*b),
            Value::Int64(n) => ValueKey::Int(*n as i128),
            Value::IntX { value, .. } => ValueKey::Int(*value),
            // Fold onto the Int key when it fits, so a UINT128 keys equal to the
            // INT64/INTX that `=`-compares equal to it.
            Value::UInt128(u) => match i128::try_from(*u) {
                Ok(n) => ValueKey::Int(n),
                Err(_) => ValueKey::U128(*u),
            },
            // Key numerics so that values which `=`-compare equal also key equal:
            // normalize -0.0 → 0.0, and fold integral doubles onto the Int key so
            // `1.0` groups with `1` (matching `cypher_cmp`'s cross-numeric equality).
            Value::Double(x) => float_key(*x),
            Value::Float(x) => float_key(*x as f64),
            // Key DECIMAL by its exact normalized fixed-point value. Integral
            // decimals still fold onto the integer key, but non-integral decimals
            // never collapse through f64.
            Value::Decimal { value, scale, .. } => decimal_key(*value, *scale),
            Value::String(s) => ValueKey::Str(s.clone()),
            Value::Json(value) => ValueKey::Json(value.render()),
            Value::Date(d) => ValueKey::Date(*d),
            Value::Timestamp(t) | Value::TimestampTz(t) => ValueKey::Ts(*t),
            Value::Interval(iv) => ValueKey::Iv(iv.cmp_micros()),
            Value::Uuid(u) => ValueKey::U128(*u),
            Value::Blob(b) => ValueKey::Bytes(b.clone()),
            Value::List(items) => ValueKey::List(items.iter().map(ValueKey::from_value).collect()),
            Value::Struct(fields) => ValueKey::Struct(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), ValueKey::from_value(v)))
                    .collect(),
            ),
            Value::Map(entries) => ValueKey::Map(
                entries
                    .iter()
                    .map(|(k, v)| (ValueKey::from_value(k), ValueKey::from_value(v)))
                    .collect(),
            ),
            Value::InternalId(id) => ValueKey::Iid(id.table_id.0, id.offset.0),
            Value::Node(n) => ValueKey::Iid(n.id.table_id.0, n.id.offset.0),
            Value::Rel(r) => ValueKey::Iid(r.id.table_id.0, r.id.offset.0),
            // A recursive-rel / path value keys by the ids of its rels then nodes
            // (structurally distinct paths key distinctly).
            Value::RecursiveRel(rr) => ValueKey::List(
                rr.rels
                    .iter()
                    .map(|r| ValueKey::Iid(r.id.table_id.0, r.id.offset.0))
                    .chain(
                        rr.nodes
                            .iter()
                            .map(|n| ValueKey::Iid(n.id.table_id.0, n.id.offset.0)),
                    )
                    .collect(),
            ),
            Value::Union { tag, value, .. } => {
                ValueKey::Union(*tag, Box::new(ValueKey::from_value(value)))
            }
        }
    }
}

/// Key a float so values that `=`-compare equal also key equal: normalize -0.0
/// and fold integral values onto the integer key (so `1.0` groups with `1`).
pub(crate) fn float_key(x: f64) -> ValueKey {
    let x = if x == 0.0 { 0.0 } else { x };
    if x.is_finite() && x.fract() == 0.0 && x >= i128::MIN as f64 && x <= i128::MAX as f64 {
        ValueKey::Int(x as i128)
    } else {
        ValueKey::Float(x.to_bits())
    }
}

pub(crate) fn decimal_key(value: i128, scale: u8) -> ValueKey {
    let (value, scale) = normalize_decimal(value, scale);
    if scale == 0 {
        ValueKey::Int(value)
    } else {
        ValueKey::Decimal(value, scale)
    }
}
