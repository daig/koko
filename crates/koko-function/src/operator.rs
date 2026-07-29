use super::compare::eval_comparison;
use super::*;

/// A scalar operator (arithmetic, comparison, boolean, null test).
///
/// Comparison/boolean operators are first-class here (matching the C++
/// first-class `ExpressionType`s) so the evaluator has one uniform dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Xor,
    Not,
    IsNull,
    IsNotNull,
}

impl ScalarOp {
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            ScalarOp::Eq | ScalarOp::Ne | ScalarOp::Lt | ScalarOp::Le | ScalarOp::Gt | ScalarOp::Ge
        )
    }
    pub fn is_boolean(self) -> bool {
        matches!(
            self,
            ScalarOp::And | ScalarOp::Or | ScalarOp::Xor | ScalarOp::Not
        )
    }
    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            ScalarOp::Add | ScalarOp::Sub | ScalarOp::Mul | ScalarOp::Div | ScalarOp::Mod
        )
    }
}

/// The `+` overload rows, in C++ registration order (shared by both `+` error
/// shapes below).
pub(crate) const PLUS_OVERLOADS: &[&str] = &[
    "(INT128,INT128) -> INT128",
    "(INT64,INT64) -> INT64",
    "(INT32,INT32) -> INT32",
    "(INT16,INT16) -> INT16",
    "(INT8,INT8) -> INT8",
    "(SERIAL,SERIAL) -> SERIAL",
    "(UINT128,UINT128) -> UINT128",
    "(UINT64,UINT64) -> UINT64",
    "(UINT32,UINT32) -> UINT32",
    "(UINT16,UINT16) -> UINT16",
    "(UINT8,UINT8) -> UINT8",
    "(DOUBLE,DOUBLE) -> DOUBLE",
    "(FLOAT,FLOAT) -> FLOAT",
    "(DECIMAL,DECIMAL) -> DECIMAL",
    "(LIST,LIST) -> LIST",
    "(STRING,STRING) -> STRING",
    "(INTERVAL,INTERVAL) -> INTERVAL",
    "(DATE,INT64) -> DATE",
    "(INT64,DATE) -> DATE",
    "(DATE,INTERVAL) -> DATE",
    "(INTERVAL,DATE) -> DATE",
    "(TIMESTAMP,INTERVAL) -> TIMESTAMP",
    "(INTERVAL,TIMESTAMP) -> TIMESTAMP",
];

/// The C++ `+` overload-table error, byte-for-byte (emitted when a STRING
/// operand meets a non-STRING one — the one `+` shape where a candidate
/// matched but `validateSpecialCases` refuses the coercion).
pub(crate) fn plus_overload_error(args: &[LogicalType]) -> Error {
    let shown = args
        .iter()
        .map(LogicalType::name)
        .collect::<Vec<_>>()
        .join(",");
    Error::binder(format!(
        "Cannot match a built-in function for given function +({shown}). Supported inputs are\n{}\n",
        PLUS_OVERLOADS.join("\n")
    ))
}

/// The standard signature block for `+` with NO matching candidate at all
/// (e.g. `id(a) + 1` — INTERNAL_ID has no `+` overload): C++
/// `validateNonEmptyCandidateFunctions` formatting, two trailing newlines.
pub(crate) fn plus_signature_error(args: &[LogicalType]) -> Error {
    let shown = args
        .iter()
        .map(LogicalType::name)
        .collect::<Vec<_>>()
        .join(",");
    let mut msg =
        format!("Function + did not receive correct arguments:\nActual:   ({shown})\nExpected: ");
    msg.push_str(PLUS_OVERLOADS[0]);
    for line in &PLUS_OVERLOADS[1..] {
        msg.push_str("\n          ");
        msg.push_str(line);
    }
    msg.push_str("\n\n");
    Error::binder(msg)
}

/// The result type of a scalar operator given its argument types.
pub fn scalar_result_type(op: ScalarOp, args: &[LogicalType]) -> Result<LogicalType> {
    use ScalarOp::*;
    match op {
        Add | Sub | Mul | Div | Mod => {
            // `+` doubles as STRING concatenation, but ONLY for (STRING,STRING):
            // C++ rejects a mixed `'a' + 1` — and even `'a' + NULL` — with the
            // full `+` overload table (audit V6, oracle-verified).
            if op == Add && args.contains(&LogicalType::String) {
                if args.iter().all(|a| *a == LogicalType::String) {
                    return Ok(LogicalType::String);
                }
                return Err(plus_overload_error(args));
            }
            if op == Add {
                if let Some(result) = list_concat_operator_type(args) {
                    return result;
                }
            }
            // DATE / TIMESTAMP / INTERVAL arithmetic.
            if let Some(t) = temporal_result_type(op, args) {
                return Ok(t);
            }
            for a in args {
                if !a.is_numeric() && *a != LogicalType::Any {
                    // `+` reports its full overload table. When every operand
                    // still casts to STRING, the C++ matcher lands on the
                    // (STRING,STRING) candidate and `validateSpecialCases`
                    // rejects it ("Cannot match a built-in function ..."); an
                    // operand outside `castFromString` (INTERNAL_ID etc.) leaves
                    // no candidate at all — the standard signature block.
                    if op == Add {
                        let all_stringable = args.iter().all(|t| {
                            !matches!(
                                t,
                                LogicalType::Blob
                                    | LogicalType::InternalId
                                    | LogicalType::Node(_)
                                    | LogicalType::Rel(_)
                                    | LogicalType::RecursiveRel
                            )
                        });
                        if all_stringable {
                            return Err(plus_overload_error(args));
                        }
                        return Err(plus_signature_error(args));
                    }
                    return Err(Error::binder(format!(
                        "arithmetic requires numeric operands, got {a}"
                    )));
                }
            }
            // Any DOUBLE operand → DOUBLE; FLOAT-only (no DOUBLE) → FLOAT.
            if args.contains(&LogicalType::Double) {
                return Ok(LogicalType::Double);
            }
            if args.contains(&LogicalType::Float) {
                return Ok(LogicalType::Float);
            }
            // UINT128 is wider than every `IntKind`; any UINT128 operand wins
            // unless a DECIMAL operand selects the decimal arithmetic overload.
            if args.contains(&LogicalType::UInt128)
                && !args.iter().any(|a| matches!(a, LogicalType::Decimal(_, _)))
            {
                return Ok(LogicalType::UInt128);
            }
            // DECIMAL arithmetic result type (mirrors `eval_decimal_arith`).
            // A non-decimal integer operand adopts the other DECIMAL operand's
            // precision/scale, matching the C++ decimal arithmetic binder.
            if let Some(decimal_part) = args.iter().find_map(|a| match a {
                LogicalType::Decimal(p, s) => Some((*p, *s)),
                _ => None,
            }) {
                use koko_common::decimal;
                if op == Div {
                    return Ok(LogicalType::Double); // decimal division drops to double
                }
                let parts: Vec<(u8, u8)> = args
                    .iter()
                    .map(|a| match a {
                        LogicalType::Decimal(p, s) => (*p, *s),
                        _ => decimal_part,
                    })
                    .collect();
                let (p1, s1) = parts[0];
                let (p2, s2) = parts.get(1).copied().unwrap_or((p1, s1));
                let (prec, scale) = match op {
                    Add | Sub => decimal::add_sub_params(p1, s1, p2, s2),
                    Mul => decimal::mul_params(p1, s1, p2, s2).ok_or_else(|| {
                        Error::overflow(
                            "Resulting precision of decimal multiplication greater than 38"
                                .to_string(),
                        )
                    })?,
                    Mod => decimal::mod_params(p1, s1, p2, s2),
                    _ => unreachable!(),
                };
                return Ok(LogicalType::Decimal(prec, scale));
            }
            // Combine the integer widths (widest wins); unknown (Any) operands
            // contribute nothing, defaulting to INT64.
            let mut acc: Option<IntKind> = None;
            for a in args {
                if let Some(k) = a.int_kind() {
                    acc = Some(match acc {
                        Some(prev) => prev.combine(k),
                        None => k,
                    });
                }
            }
            Ok(acc.map(LogicalType::Int).unwrap_or(LogicalType::Int64))
        }
        // Negation keeps the operand's width (signed may overflow at MIN;
        // unsigned wraps modularly).
        Neg => Ok(args.first().cloned().unwrap_or(LogicalType::Int64)),
        And | Or | Xor | Not => {
            for a in args {
                if !matches!(a, LogicalType::Bool | LogicalType::Any) {
                    return Err(Error::binder(format!(
                        "boolean operator requires BOOL operands, got {a}"
                    )));
                }
            }
            Ok(LogicalType::Bool)
        }
        Eq | Ne | Lt | Le | Gt | Ge | IsNull | IsNotNull => {
            // Non-comparable pairs (struct arity mismatch included) error at
            // bind via `comparison_comparable`; same-arity structs with
            // different field names bind fine and evaluate False like C++.
            if let [a, b] = args {
                if !comparison_comparable(a, b) {
                    return Err(Error::binder(format!(
                        "Type Mismatch: Cannot compare types {a} and {b}"
                    )));
                }
            }
            Ok(LogicalType::Bool)
        }
    }
}
pub(crate) fn list_concat_child_type(
    left: &LogicalType,
    right: &LogicalType,
) -> Result<LogicalType> {
    match (left, right) {
        (LogicalType::Any, LogicalType::Any) => Ok(LogicalType::Int64),
        (LogicalType::Any, t) | (t, LogicalType::Any) => Ok(t.clone()),
        (l, r) if l == r => Ok(l.clone()),
        (l, r) => Err(Error::binder(format!(
            "Cannot bind LIST_CONCAT with parameter type {} and {}.",
            LogicalType::List(Box::new(l.clone())),
            LogicalType::List(Box::new(r.clone()))
        ))),
    }
}

pub(crate) fn list_concat_operator_type(args: &[LogicalType]) -> Option<Result<LogicalType>> {
    let (left, right) = (args.first()?, args.get(1)?);
    match (left.list_child(), right.list_child()) {
        (Some(l), Some(r)) => {
            Some(list_concat_child_type(l, r).map(|inner| LogicalType::List(Box::new(inner))))
        }
        (Some(inner), None) if *right == LogicalType::Any => {
            Some(Ok(LogicalType::List(Box::new(inner.clone()))))
        }
        (None, Some(inner)) if *left == LogicalType::Any => {
            Some(Ok(LogicalType::List(Box::new(inner.clone()))))
        }
        _ => None,
    }
}

/// Evaluate a scalar operator over already-evaluated argument values.
pub fn eval_scalar(op: ScalarOp, args: &[Value]) -> Result<Value> {
    use ScalarOp::*;
    match op {
        Add | Sub | Mul | Div | Mod => eval_arithmetic(op, &args[0], &args[1]),
        Neg => match &args[0] {
            Value::Null => Ok(Value::Null),
            Value::Double(x) => Ok(Value::Double(-x)),
            v if v.int_parts().is_some() => {
                let (val, kind) = v.int_parts().unwrap();
                if kind.is_signed() {
                    let neg = val
                        .checked_neg()
                        .filter(|n| kind.contains(*n))
                        .ok_or_else(|| {
                            Error::overflow(format!(
                                "Value {val} cannot be negated within {} range.",
                                kind.name()
                            ))
                        })?;
                    Ok(Value::make_int(neg, kind))
                } else {
                    // Unsigned negation wraps modularly within the width.
                    let modulus = kind.max() + 1;
                    let neg = (modulus - (val % modulus)) % modulus;
                    Ok(Value::make_int(neg, kind))
                }
            }
            other => Err(Error::runtime(format!(
                "cannot negate {}",
                other.logical_type()
            ))),
        },
        Eq | Ne | Lt | Le | Gt | Ge => eval_comparison(op, &args[0], &args[1]),
        And => Ok(eval_and(args)),
        Or => Ok(eval_or(args)),
        Xor => Ok(match (args[0].as_bool(), args[1].as_bool()) {
            (Some(a), Some(b)) => Value::Bool(a ^ b),
            _ => Value::Null,
        }),
        Not => Ok(match args[0].as_bool() {
            Some(b) => Value::Bool(!b),
            None => Value::Null,
        }),
        IsNull => Ok(Value::Bool(args[0].is_null())),
        IsNotNull => Ok(Value::Bool(!args[0].is_null())),
    }
}

pub(crate) fn eval_and(args: &[Value]) -> Value {
    // false dominates; otherwise null if any null; else true.
    let mut saw_null = false;
    for a in args {
        match a.as_bool() {
            Some(false) => return Value::Bool(false),
            Some(true) => {}
            None => saw_null = true,
        }
    }
    if saw_null {
        Value::Null
    } else {
        Value::Bool(true)
    }
}

pub(crate) fn eval_or(args: &[Value]) -> Value {
    // true dominates; otherwise null if any null; else false.
    let mut saw_null = false;
    for a in args {
        match a.as_bool() {
            Some(true) => return Value::Bool(true),
            Some(false) => {}
            None => saw_null = true,
        }
    }
    if saw_null {
        Value::Null
    } else {
        Value::Bool(false)
    }
}

pub(crate) fn eval_arithmetic(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
    if a.is_null() || b.is_null() {
        return Ok(Value::Null);
    }
    // DATE / TIMESTAMP / INTERVAL arithmetic (date_t/interval_t/timestamp_t ops).
    if is_temporal(a) || is_temporal(b) {
        return eval_temporal_arith(op, a, b);
    }
    if op == ScalarOp::Add {
        if let (Value::List(left), Value::List(right)) = (a, b) {
            let mut out = Vec::with_capacity(left.len() + right.len());
            out.extend(left.iter().cloned());
            out.extend(right.iter().cloned());
            return Ok(Value::List(out));
        }
    }
    // `+` concatenates only when BOTH operands are STRING (mixed shapes were
    // rejected at bind — audit V6).
    if op == ScalarOp::Add && (matches!(a, Value::String(_)) && matches!(b, Value::String(_))) {
        return Ok(Value::String(format!(
            "{}{}",
            a.to_result_string(),
            b.to_result_string()
        )));
    }
    // UINT128 arithmetic: when either operand is UINT128, compute in `u128`. A
    // negative integer operand is out of `UINT128` range.
    if matches!(a, Value::UInt128(_)) || matches!(b, Value::UInt128(_)) {
        let oor = || {
            Error::overflow(format!(
                "UINT128 is out of range: cannot {}.",
                arith_word(op)
            ))
        };
        let x = a.as_u128().ok_or_else(oor)?;
        let y = b.as_u128().ok_or_else(oor)?;
        let val = match op {
            ScalarOp::Add => x.checked_add(y),
            ScalarOp::Sub => x.checked_sub(y),
            ScalarOp::Mul => x.checked_mul(y),
            ScalarOp::Div => {
                if y == 0 {
                    return Err(Error::runtime("Divide by zero.".to_string()));
                }
                x.checked_div(y)
            }
            ScalarOp::Mod => {
                if y == 0 {
                    return Err(Error::runtime("Modulo by zero.".to_string()));
                }
                x.checked_rem(y)
            }
            _ => unreachable!(),
        }
        .ok_or_else(oor)?;
        return Ok(Value::UInt128(val));
    }
    // DECIMAL arithmetic: when a DECIMAL meets a DECIMAL or integer (but not a
    // float — that drops to double below). Division also drops to double.
    let is_float = |v: &Value| matches!(v, Value::Double(_) | Value::Float(_));
    if (matches!(a, Value::Decimal { .. }) || matches!(b, Value::Decimal { .. }))
        && !is_float(a)
        && !is_float(b)
        && op != ScalarOp::Div
    {
        return eval_decimal_arith(op, a, b);
    }
    // Integer arithmetic when both operands are integers (any width): promote to
    // i128, compute, and range-check against the combined result width.
    if let (Some((x, xk)), Some((y, yk))) = (a.int_parts(), b.int_parts()) {
        let kind = xk.combine(yk);
        let checked = match op {
            ScalarOp::Add => x.checked_add(y),
            ScalarOp::Sub => x.checked_sub(y),
            ScalarOp::Mul => x.checked_mul(y),
            ScalarOp::Div => {
                if y == 0 {
                    return Err(Error::runtime("Divide by zero.".to_string()));
                }
                x.checked_div(y)
            }
            ScalarOp::Mod => {
                if y == 0 {
                    return Err(Error::runtime("Modulo by zero.".to_string()));
                }
                // MIN % -1 overflows like MIN / -1 (matching the C++ engine),
                // even though the mathematical remainder is 0.
                if y == -1 && x == kind.min() {
                    return Err(Error::overflow(format!(
                        "Value {x} % {y} is not within {} range.",
                        kind.name()
                    )));
                }
                x.checked_rem(y)
            }
            _ => unreachable!(),
        };
        // `checked` only returns None if the i128 itself overflowed, which can
        // happen only at the INT128 boundary.
        let val = checked.ok_or_else(|| {
            Error::overflow(format!(
                "INT128 is out of range: cannot {}.",
                arith_word(op)
            ))
        })?;
        if !kind.contains(val) {
            // The C++ uint64 multiply specialization swaps operands (smaller
            // first) before formatting its overflow message; mirror that.
            let (mx, my) = if op == ScalarOp::Mul && kind == IntKind::U64 && x > y {
                (y, x)
            } else {
                (x, y)
            };
            return Err(Error::overflow(format!(
                "Value {mx} {} {my} is not within {} range.",
                arith_symbol(op),
                kind.name()
            )));
        }
        return Ok(Value::make_int(val, kind));
    }
    // FLOAT with no DOUBLE stays FLOAT (computed in f32, matching C++ promotion);
    // a DOUBLE operand widens the result to DOUBLE.
    let any_float = matches!(a, Value::Float(_)) || matches!(b, Value::Float(_));
    let any_double = matches!(a, Value::Double(_)) || matches!(b, Value::Double(_));
    if any_float && !any_double {
        let (x, y) = (a.as_f64().unwrap() as f32, b.as_f64().unwrap() as f32);
        let r = match op {
            ScalarOp::Add => x + y,
            ScalarOp::Sub => x - y,
            ScalarOp::Mul => x * y,
            ScalarOp::Div => x / y,
            ScalarOp::Mod => x % y,
            _ => unreachable!(),
        };
        return Ok(Value::Float(r));
    }
    let (x, y) = (
        a.as_f64()
            .ok_or_else(|| Error::runtime("non-numeric operand in arithmetic".to_string()))?,
        b.as_f64()
            .ok_or_else(|| Error::runtime("non-numeric operand in arithmetic".to_string()))?,
    );
    let r = match op {
        ScalarOp::Add => x + y,
        ScalarOp::Sub => x - y,
        ScalarOp::Mul => x * y,
        ScalarOp::Div => x / y,
        ScalarOp::Mod => x % y,
        _ => unreachable!(),
    };
    Ok(Value::Double(r))
}

pub(crate) fn is_temporal(v: &Value) -> bool {
    matches!(
        v,
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) | Value::Interval(_)
    )
}

/// The result type of a DATE/TIMESTAMP/INTERVAL arithmetic op, or `None` if the
/// operand pair isn't a temporal combination.
pub(crate) fn temporal_result_type(op: ScalarOp, args: &[LogicalType]) -> Option<LogicalType> {
    use LogicalType as L;
    use ScalarOp::*;
    let is_int = |t: &L| t.int_kind().is_some();
    let is_ts = |t: &L| {
        matches!(
            t,
            L::Timestamp | L::TimestampNs | L::TimestampMs | L::TimestampSec | L::TimestampTz
        )
    };
    let (a, b) = (args.first()?, args.get(1)?);
    let ts_kind = |x: &L, y: &L| if is_ts(x) { x.clone() } else { y.clone() };
    Some(match op {
        Add => match (a, b) {
            (L::Date, x) | (x, L::Date) if is_int(x) => L::Date,
            (L::Date, L::Interval) | (L::Interval, L::Date) => L::Date,
            (t, L::Interval) | (L::Interval, t) if is_ts(t) => ts_kind(a, b),
            (L::Interval, L::Interval) => L::Interval,
            _ => return None,
        },
        Sub => match (a, b) {
            (L::Date, x) if is_int(x) => L::Date,
            (L::Date, L::Interval) => L::Date,
            (L::Date, L::Date) => L::Int64,
            (t, L::Interval) if is_ts(t) => ts_kind(a, b),
            (t1, t2) if is_ts(t1) && is_ts(t2) => L::Interval,
            (L::Interval, L::Interval) => L::Interval,
            _ => return None,
        },
        Mul => match (a, b) {
            (L::Interval, x) | (x, L::Interval) if is_int(x) => L::Interval,
            _ => return None,
        },
        Div => match (a, b) {
            (L::Interval, x) if is_int(x) => L::Interval,
            _ => return None,
        },
        _ => return None,
    })
}

/// Days in month `m` (1-12) of year `y`.
pub(crate) fn max_day_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        _ => 28,
    }
}

/// `days_since_epoch + interval` (months applied with end-of-month clamping, then
/// days, then whole days from `micros`), per C++ `date_t::operator+(interval)`.
pub(crate) fn add_interval_to_days(days: i32, iv: &temporal::Interval) -> i32 {
    let (mut y, mut m, mut d) = temporal::civil_from_days(days as i64);
    let total = (y * 12 + (m - 1)) + iv.months as i64;
    y = total.div_euclid(12);
    m = total.rem_euclid(12) + 1;
    d = d.min(max_day_in_month(y, m));
    let base = temporal::days_from_civil(y, m, d);
    (base + iv.days as i64 + iv.micros / MICROS_PER_DAY) as i32
}

pub(crate) fn eval_temporal_arith(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
    use ScalarOp::*;
    use Value as V;
    let unsupported = || {
        Error::runtime(format!(
            "unsupported temporal arithmetic: {} {} {}",
            a.logical_type(),
            arith_symbol(op),
            b.logical_type()
        ))
    };
    // The micros-since-epoch and TZ-ness of a timestamp value.
    let ts_parts = |v: &Value| match v {
        V::Timestamp(t) => Some((*t, false)),
        V::TimestampTz(t) => Some((*t, true)),
        _ => None,
    };
    let mk_ts = |micros: i64, tz: bool| {
        if tz {
            V::TimestampTz(micros)
        } else {
            V::Timestamp(micros)
        }
    };
    Ok(match op {
        Add => match (a, b) {
            (V::Date(d), n) | (n, V::Date(d)) if n.as_i64().is_some() => {
                V::Date(d + n.as_i64().unwrap() as i32)
            }
            (V::Date(d), V::Interval(iv)) | (V::Interval(iv), V::Date(d)) => {
                V::Date(add_interval_to_days(*d, iv))
            }
            (V::Interval(x), V::Interval(y)) => V::Interval(temporal::Interval {
                months: x.months + y.months,
                days: x.days + y.days,
                micros: x.micros + y.micros,
            }),
            _ => {
                // TIMESTAMP + INTERVAL (either order).
                if let (Some((t, tz)), Some(iv)) = (ts_parts(a), interval_of(b)) {
                    mk_ts(add_interval_to_timestamp(t, iv), tz)
                } else if let (Some((t, tz)), Some(iv)) = (ts_parts(b), interval_of(a)) {
                    mk_ts(add_interval_to_timestamp(t, iv), tz)
                } else {
                    return Err(unsupported());
                }
            }
        },
        Sub => match (a, b) {
            (V::Date(d), n) if n.as_i64().is_some() => V::Date(d - n.as_i64().unwrap() as i32),
            (V::Date(d), V::Interval(iv)) => V::Date(add_interval_to_days(*d, &neg_interval(iv))),
            (V::Date(x), V::Date(y)) => V::Int64(*x as i64 - *y as i64),
            (V::Interval(x), V::Interval(y)) => V::Interval(temporal::Interval {
                months: x.months - y.months,
                days: x.days - y.days,
                micros: x.micros - y.micros,
            }),
            _ => {
                if let (Some((t, tz)), Some(iv)) = (ts_parts(a), interval_of(b)) {
                    mk_ts(add_interval_to_timestamp(t, &neg_interval(iv)), tz)
                } else if let (Some((x, _)), Some((y, _))) = (ts_parts(a), ts_parts(b)) {
                    let diff = x - y;
                    V::Interval(temporal::Interval {
                        months: 0,
                        days: (diff / MICROS_PER_DAY) as i32,
                        micros: diff % MICROS_PER_DAY,
                    })
                } else {
                    return Err(unsupported());
                }
            }
        },
        Mul => match (a, b) {
            (V::Interval(iv), n) | (n, V::Interval(iv)) if n.as_i64().is_some() => {
                let k = n.as_i64().unwrap() as i32;
                V::Interval(temporal::Interval {
                    months: iv.months * k,
                    days: iv.days * k,
                    micros: iv.micros * k as i64,
                })
            }
            _ => return Err(unsupported()),
        },
        Div => match (a, b) {
            (V::Interval(iv), n) if n.as_i64().is_some() => {
                let k = n.as_i64().unwrap();
                if k == 0 {
                    return Err(Error::runtime("Divide by zero.".to_string()));
                }
                // C++ interval_t::operator/ carry logic (DAYS_PER_MONTH = 30).
                let months_rem = (iv.months as i64) % k;
                let months = (iv.months as i64) / k;
                let days_num = iv.days as i64 + months_rem * 30;
                let days_rem = days_num % k;
                let days = days_num / k;
                let micros = (iv.micros + days_rem * MICROS_PER_DAY) / k;
                V::Interval(temporal::Interval {
                    months: months as i32,
                    days: days as i32,
                    micros,
                })
            }
            _ => return Err(unsupported()),
        },
        _ => return Err(unsupported()),
    })
}

pub(crate) fn interval_of(v: &Value) -> Option<&temporal::Interval> {
    match v {
        Value::Interval(iv) => Some(iv),
        _ => None,
    }
}

pub(crate) fn neg_interval(iv: &temporal::Interval) -> temporal::Interval {
    temporal::Interval {
        months: -iv.months,
        days: -iv.days,
        micros: -iv.micros,
    }
}

/// `timestamp_micros + interval`: months/days on the calendar, then add micros.
pub(crate) fn add_interval_to_timestamp(micros: i64, iv: &temporal::Interval) -> i64 {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let in_day = micros.rem_euclid(MICROS_PER_DAY);
    // Apply months+days to the date part (without the interval's own micros).
    let date_only = temporal::Interval {
        months: iv.months,
        days: iv.days,
        micros: 0,
    };
    let new_days = add_interval_to_days(days as i32, &date_only) as i64;
    new_days * MICROS_PER_DAY + in_day + iv.micros
}

pub(crate) fn arith_symbol(op: ScalarOp) -> char {
    match op {
        ScalarOp::Add => '+',
        ScalarOp::Sub => '-',
        ScalarOp::Mul => '*',
        ScalarOp::Div => '/',
        ScalarOp::Mod => '%',
        _ => '?',
    }
}

pub(crate) fn arith_word(op: ScalarOp) -> &'static str {
    match op {
        ScalarOp::Add => "add",
        ScalarOp::Sub => "subtract",
        ScalarOp::Mul => "multiply",
        ScalarOp::Div => "divide",
        ScalarOp::Mod => "modulo",
        _ => "operate",
    }
}

/// Decimal arithmetic bind parameters. When exactly one side is DECIMAL, C++
/// treats the integer side as that DECIMAL type before deriving result params.
pub(crate) fn decimal_bind_params(a: &Value, b: &Value) -> Result<((u8, u8), (u8, u8))> {
    match (a.decimal_parts(), b.decimal_parts()) {
        (Some((_, pa, sa)), Some((_, pb, sb))) => Ok(((pa, sa), (pb, sb))),
        (Some((_, p, s)), None) if b.as_int128().is_some() => Ok(((p, s), (p, s))),
        (None, Some((_, p, s))) if a.as_int128().is_some() => Ok(((p, s), (p, s))),
        _ => Err(Error::runtime(
            "non-numeric operand in decimal arithmetic".to_string(),
        )),
    }
}

/// Cast a DECIMAL/integer runtime value to a DECIMAL physical value with the
/// requested precision/scale.
pub(crate) fn decimal_value_as(v: &Value, precision: u8, scale: u8) -> Result<i128> {
    use koko_common::decimal::{fits, pow10, rescale};
    let value = match v {
        Value::Decimal {
            value,
            scale: from_scale,
            ..
        } => rescale(*value, *from_scale, scale).ok_or_else(|| {
            Error::overflow("Decimal Arithmetic result is out of range".to_string())
        })?,
        _ => {
            let n = v.as_int128().ok_or_else(|| {
                Error::runtime("non-numeric operand in decimal arithmetic".to_string())
            })?;
            n.checked_mul(pow10(scale)).ok_or_else(|| {
                Error::overflow("Decimal Arithmetic result is out of range".to_string())
            })?
        }
    };
    if fits(value, precision) {
        Ok(value)
    } else {
        Err(Error::overflow(
            "Decimal Arithmetic result is out of range".to_string(),
        ))
    }
}

/// Exact DECIMAL `+ - * %` (division drops to double upstream), mirroring the
/// C++ `DecimalAdd/Subtract/Multiply/Modulo`: result `(precision, scale)` comes
/// from `resultingParams`; add/sub/mod cast operands to the result type; multiply
/// keeps each operand at its bound input type.
pub(crate) fn eval_decimal_arith(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
    use koko_common::decimal::{self, fits};
    let ((pa, sa), (pb, sb)) = decimal_bind_params(a, b)?;
    let make = |value: i128, precision: u8, scale: u8| Value::Decimal {
        value,
        precision,
        scale,
    };
    match op {
        ScalarOp::Add | ScalarOp::Sub => {
            let word = if op == ScalarOp::Add {
                "Addition"
            } else {
                "Subtraction"
            };
            let oor = || Error::overflow(format!("Decimal {word} result is out of range"));
            let (p, s) = decimal::add_sub_params(pa, sa, pb, sb);
            let la = decimal_value_as(a, p, s).map_err(|_| oor())?;
            let lb = decimal_value_as(b, p, s).map_err(|_| oor())?;
            let val = if op == ScalarOp::Add {
                la.checked_add(lb)
            } else {
                la.checked_sub(lb)
            }
            .filter(|v| fits(*v, p))
            .ok_or_else(oor)?;
            Ok(make(val, p, s))
        }
        ScalarOp::Mul => {
            let (p, s) = decimal::mul_params(pa, sa, pb, sb).ok_or_else(|| {
                Error::overflow(
                    "Resulting precision of decimal multiplication greater than 38".to_string(),
                )
            })?;
            let va = decimal_value_as(a, pa, sa)?;
            let vb = decimal_value_as(b, pb, sb)?;
            let val = va.checked_mul(vb).filter(|v| fits(*v, p)).ok_or_else(|| {
                Error::overflow("Decimal Multiplication Result is out of range".to_string())
            })?;
            Ok(make(val, p, s))
        }
        ScalarOp::Mod => {
            let oor = || Error::overflow("Decimal Modulo result is out of range".to_string());
            let (p, s) = decimal::mod_params(pa, sa, pb, sb);
            let la = decimal_value_as(a, p, s).map_err(|_| oor())?;
            let lb = decimal_value_as(b, p, s).map_err(|_| oor())?;
            if lb == 0 {
                return Err(Error::runtime("Modulo by zero.".to_string()));
            }
            Ok(make(la % lb, p, s))
        }
        _ => unreachable!(),
    }
}
