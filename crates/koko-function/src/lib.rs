//! `koko-function` — the function library: scalar operators, aggregates, and
//! Cypher value comparison/ordering.
//!
//! P0 evaluates scalars per-row over [`Value`]s (vectorized evaluation is a P3
//! perf concern). Null propagation follows Cypher's three-valued logic.

use koko_common::temporal;
use koko_common::{Error, IntKind, LogicalType, Result, Value, value_payload_bytes};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;

const MICROS_PER_DAY: i64 = 86_400_000_000;

pub mod catalog_data;
pub mod digest;
pub mod oracle_hash;
pub mod scalarfn;
pub use scalarfn::{
    aggregate_signature_error, eval as eval_scalar_func,
    eval_with_context as eval_scalar_func_with_context, is_scalar,
    scalar_result_type as scalar_func_result_type,
};

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
const PLUS_OVERLOADS: &[&str] = &[
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
fn plus_overload_error(args: &[LogicalType]) -> Error {
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
fn plus_signature_error(args: &[LogicalType]) -> Error {
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
fn list_concat_child_type(left: &LogicalType, right: &LogicalType) -> Result<LogicalType> {
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

fn list_concat_operator_type(args: &[LogicalType]) -> Option<Result<LogicalType>> {
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

fn eval_and(args: &[Value]) -> Value {
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

fn eval_or(args: &[Value]) -> Value {
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

fn eval_arithmetic(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
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

fn is_temporal(v: &Value) -> bool {
    matches!(
        v,
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) | Value::Interval(_)
    )
}

/// The result type of a DATE/TIMESTAMP/INTERVAL arithmetic op, or `None` if the
/// operand pair isn't a temporal combination.
fn temporal_result_type(op: ScalarOp, args: &[LogicalType]) -> Option<LogicalType> {
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
fn max_day_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        _ => 28,
    }
}

/// `days_since_epoch + interval` (months applied with end-of-month clamping, then
/// days, then whole days from `micros`), per C++ `date_t::operator+(interval)`.
fn add_interval_to_days(days: i32, iv: &temporal::Interval) -> i32 {
    let (mut y, mut m, mut d) = temporal::civil_from_days(days as i64);
    let total = (y * 12 + (m - 1)) + iv.months as i64;
    y = total.div_euclid(12);
    m = total.rem_euclid(12) + 1;
    d = d.min(max_day_in_month(y, m));
    let base = temporal::days_from_civil(y, m, d);
    (base + iv.days as i64 + iv.micros / MICROS_PER_DAY) as i32
}

fn eval_temporal_arith(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
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

fn interval_of(v: &Value) -> Option<&temporal::Interval> {
    match v {
        Value::Interval(iv) => Some(iv),
        _ => None,
    }
}

fn neg_interval(iv: &temporal::Interval) -> temporal::Interval {
    temporal::Interval {
        months: -iv.months,
        days: -iv.days,
        micros: -iv.micros,
    }
}

/// `timestamp_micros + interval`: months/days on the calendar, then add micros.
fn add_interval_to_timestamp(micros: i64, iv: &temporal::Interval) -> i64 {
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

fn arith_symbol(op: ScalarOp) -> char {
    match op {
        ScalarOp::Add => '+',
        ScalarOp::Sub => '-',
        ScalarOp::Mul => '*',
        ScalarOp::Div => '/',
        ScalarOp::Mod => '%',
        _ => '?',
    }
}

fn arith_word(op: ScalarOp) -> &'static str {
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
fn decimal_bind_params(a: &Value, b: &Value) -> Result<((u8, u8), (u8, u8))> {
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
fn decimal_value_as(v: &Value, precision: u8, scale: u8) -> Result<i128> {
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
fn eval_decimal_arith(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
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

/// Cast a value to a target logical type (the P1 scalar cast matrix). `NULL`
/// casts to `NULL`. Out-of-range integer casts and unparseable strings error.
/// Parse one CSV cell (COPY and LOAD FROM): empty and literal-`null` cells are
/// NULL (C++ CSV semantics); STRING columns honor the reader's null-string list
/// and keep the text otherwise; every other type casts from the text through
/// the general cast machinery, so cell errors carry the C++ cast wording
/// ("Cast failed. Could not convert ... to INT64.").
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
fn resolved_any(t: &LogicalType) -> LogicalType {
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
fn cast_kind_name(t: &LogicalType) -> String {
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
fn cpp_rejects_numeric_string(t: &str) -> bool {
    if t.starts_with('+') {
        return true;
    }
    let b = t.as_bytes();
    b.len() >= 2 && b[0] == b'0' && b[1].is_ascii_digit()
}

/// Float → integer with C++ semantics (audit V1): `nearbyint` half-to-even
/// rounding, pre-round negativity check for unsigned targets, and the overflow
/// message rendering the offending float like `%f`.
fn float_to_int(x: f64, kind: IntKind) -> Result<Value> {
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

fn cast_to_int(v: &Value, kind: IntKind) -> Result<Value> {
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
fn float_to_uint128(x: f64, _v: &Value) -> Result<Value> {
    // Out-of-range (negative, > max, inf) and NaN all wrap to 0 — the C++
    // float→uint128 conversion's deterministic result (oracle-verified:
    // to_uint128(to_float(3.4e38)) → 0, to_uint128(-5.7) → 0).
    let r = x.round_ties_even();
    if !(r == 0.0 || (0.0..340282366920938463463374607431768211456.0).contains(&r)) {
        return Ok(Value::UInt128(0));
    }
    Ok(Value::UInt128(r.max(0.0) as u128))
}

fn cast_to_uint128(v: &Value) -> Result<Value> {
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
fn cast_to_decimal(v: &Value, precision: u8, scale: u8) -> Result<Value> {
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
fn cast_ts_micros(v: &Value, flavor: &str) -> Result<i64> {
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

fn cast_to_double(v: &Value) -> Result<Value> {
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

fn cast_to_bool(v: &Value) -> Result<Value> {
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

fn eval_comparison(op: ScalarOp, a: &Value, b: &Value) -> Result<Value> {
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

fn string_comparison_target(t: &LogicalType) -> bool {
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

fn coerce_comparison_values<'a>(
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

fn coerce_comparison_value<'a>(
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
fn composite_elem_cmp(a: &Value, b: &Value) -> Option<Ordering> {
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
fn decimal_operand(v: &Value) -> Option<(i128, u8)> {
    match v {
        Value::Decimal { value, scale, .. } => Some((*value, *scale)),
        _ => v.as_int128().map(|n| (n, 0)),
    }
}

/// Exact comparison of two decimal/integer operands. The common no-overflow path
/// cross-scales in `i128`; the fallback compares normalized decimal strings so
/// high-scale/high-precision values never collapse through `f64`.
fn decimal_cmp(a: &Value, b: &Value) -> Option<Ordering> {
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

fn float_cmp(x: f64, y: f64) -> Ordering {
    if x.is_nan() || y.is_nan() {
        // C++ comparison templates implement `<`/`<=` as complements of
        // `>=`/`>`, so any comparison involving NaN behaves like "less" for
        // the `cypher_cmp` ordering consumed by scalar comparisons and min/max.
        Ordering::Less
    } else {
        x.partial_cmp(&y).unwrap()
    }
}

fn normalize_decimal(mut value: i128, mut scale: u8) -> (i128, u8) {
    if value == 0 {
        return (0, 0);
    }
    while scale > 0 && value % 10 == 0 {
        value /= 10;
        scale -= 1;
    }
    (value, scale)
}

fn decimal_cmp_slow(va: i128, sa: u8, vb: i128, sb: u8) -> Ordering {
    match (va.is_negative(), vb.is_negative()) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let ord = decimal_abs_cmp(va.unsigned_abs(), sa, vb.unsigned_abs(), sb);
    if va.is_negative() { ord.reverse() } else { ord }
}

fn decimal_abs_cmp(va: u128, sa: u8, vb: u128, sb: u8) -> Ordering {
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

fn decimal_abs_parts(value: u128, scale: u8) -> (String, String) {
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
fn type_rank(v: &Value) -> u8 {
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
fn float_key(x: f64) -> ValueKey {
    let x = if x == 0.0 { 0.0 } else { x };
    if x.is_finite() && x.fract() == 0.0 && x >= i128::MIN as f64 && x <= i128::MAX as f64 {
        ValueKey::Int(x as i128)
    } else {
        ValueKey::Float(x.to_bits())
    }
}

fn decimal_key(value: i128, scale: u8) -> ValueKey {
    let (value, scale) = normalize_decimal(value, scale);
    if scale == 0 {
        ValueKey::Int(value)
    } else {
        ValueKey::Decimal(value, scale)
    }
}

// ---- aggregates ----

/// An aggregate function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggOp {
    /// `count(*)` — counts rows (including those with null args).
    CountStar,
    /// `count(x)` — counts non-null values.
    Count,
    Sum,
    Avg,
    Min,
    Max,
    /// `collect(x)` — gather non-null values into a list.
    Collect,
    /// `percentileDisc(x, p)` — the discrete percentile: the smallest sorted
    /// value whose cumulative fraction reaches `p` (stored as `f64` bits so
    /// the op stays `Copy`/`Eq`).
    PercentileDisc(u64),
}

impl AggOp {
    /// Parse an aggregate function name (case-insensitive). `count` with a
    /// `*`/no argument becomes [`AggOp::CountStar`] in the binder.
    pub fn from_name(name: &str) -> Option<AggOp> {
        match name.to_ascii_lowercase().as_str() {
            // percentileDisc carries its percentile constant — bound specially
            // (see the binder), defaulting here so name checks recognize it.
            // Only the concatenated form exists in C++ (PERCENTILE_DISC is
            // "function PERCENTILE_DISC does not exist.").
            "percentiledisc" => Some(AggOp::PercentileDisc(0)),
            "count" => Some(AggOp::Count),
            "sum" => Some(AggOp::Sum),
            "avg" => Some(AggOp::Avg),
            "min" => Some(AggOp::Min),
            "max" => Some(AggOp::Max),
            "collect" => Some(AggOp::Collect),
            _ => None,
        }
    }
}

/// The result type of an aggregate given its argument type. C++ does not register
/// SUM/AVG overloads for DECIMAL, so reject those at bind time.
pub fn agg_result_type(op: AggOp, arg: &LogicalType) -> Result<LogicalType> {
    match op {
        AggOp::CountStar | AggOp::Count => Ok(LogicalType::Int64),
        AggOp::Sum | AggOp::Avg if matches!(arg, LogicalType::Decimal(_, _)) => {
            Err(Error::binder(format!(
                "Function {} did not receive correct arguments:\nActual:   ({})\nExpected: numeric type excluding DECIMAL",
                if op == AggOp::Sum { "SUM" } else { "AVG" },
                arg
            )))
        }
        AggOp::Avg => Ok(LogicalType::Double),
        AggOp::PercentileDisc(_) => Ok(arg.clone()),
        // C++ SUM widens: INT*/SERIAL -> INT128, UINT* -> UINT128, FLOAT -> DOUBLE
        // (audit V3; the overload table in review_regressions.test is the spec).
        AggOp::Sum => Ok(match arg {
            LogicalType::Int(k) if k.is_signed() => LogicalType::Int(IntKind::I128),
            LogicalType::Serial => LogicalType::Int(IntKind::I128),
            LogicalType::Int(_) => LogicalType::UInt128,
            LogicalType::UInt128 => LogicalType::UInt128,
            LogicalType::Float | LogicalType::Double => LogicalType::Double,
            other => other.clone(),
        }),
        AggOp::Min | AggOp::Max => Ok(arg.clone()),
        AggOp::Collect => Ok(LogicalType::List(Box::new(arg.clone()))),
    }
}

/// Accumulator for one aggregate within one group.
#[derive(Debug, Clone)]
pub struct AggState {
    op: AggOp,
    n_rows: i64,
    n_nonnull: i64,
    sum_i: i128,
    sum_u: u128,
    sum_f: f64,
    is_float: bool,
    is_unsigned: bool,
    extreme: Option<Value>,
    collected: Vec<Value>,
    seen: Option<HashSet<ValueKey>>,
}

impl AggState {
    pub fn new(op: AggOp, distinct: bool) -> Self {
        Self {
            op,
            n_rows: 0,
            n_nonnull: 0,
            sum_i: 0,
            sum_u: 0,
            sum_f: 0.0,
            is_float: false,
            is_unsigned: false,
            extreme: None,
            collected: Vec::new(),
            seen: if distinct { Some(HashSet::new()) } else { None },
        }
    }
    /// Heap bytes retained by variable-width aggregate state.
    pub fn heap_bytes(&self) -> u64 {
        let extreme = self.extreme.as_ref().map(value_payload_bytes).unwrap_or(0);
        let collected = (self.collected.capacity() * std::mem::size_of::<Value>()) as u64
            + self.collected.iter().map(value_payload_bytes).sum::<u64>();
        let seen = self.seen.as_ref().map_or(0, |seen| {
            (seen.capacity() * (std::mem::size_of::<ValueKey>() + std::mem::size_of::<usize>()))
                as u64
                + seen.iter().map(ValueKey::heap_bytes).sum::<u64>()
        });
        extreme.saturating_add(collected).saturating_add(seen)
    }
    /// Conservative bytes to reserve before [`Self::update_n`] may retain `v`.
    ///
    /// The estimate intentionally charges each retained element rather than relying
    /// on allocator-specific `Vec`/`HashSet` growth factors.
    pub fn reservation_bytes_for_update(&self, v: &Value, n: u64) -> u64 {
        if self.op == AggOp::CountStar || v.is_null() {
            return 0;
        }
        let mut bytes = 0u64;
        let mut effective_n = n;
        if let Some(seen) = &self.seen {
            let is_nan = matches!(v, Value::Double(x) if x.is_nan())
                || matches!(v, Value::Float(x) if x.is_nan());
            let key = ValueKey::from_value(v);
            if !is_nan && seen.contains(&key) {
                return 0;
            }
            bytes = bytes
                .saturating_add(std::mem::size_of::<ValueKey>() as u64)
                .saturating_add(key.heap_bytes())
                .saturating_add((2 * std::mem::size_of::<usize>()) as u64);
            effective_n = 1;
        }
        let retained_value =
            (std::mem::size_of::<Value>() as u64).saturating_add(value_payload_bytes(v));
        match self.op {
            AggOp::Collect | AggOp::PercentileDisc(_) => {
                bytes.saturating_add(effective_n.saturating_mul(retained_value))
            }
            AggOp::Min => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|extreme| order_cmp(v, extreme) == Ordering::Less)
                {
                    bytes.saturating_add(value_payload_bytes(v))
                } else {
                    bytes
                }
            }
            AggOp::Max => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|extreme| order_cmp(v, extreme) == Ordering::Greater)
                {
                    bytes.saturating_add(value_payload_bytes(v))
                } else {
                    bytes
                }
            }
            AggOp::CountStar | AggOp::Count | AggOp::Sum | AggOp::Avg => bytes,
        }
    }

    /// Feed one input value (`Value::Null` for the `count(*)` placeholder).
    pub fn update(&mut self, v: &Value) {
        self.update_n(v, 1);
    }

    /// Feed one input value with **factorization multiplicity** `n`: the value
    /// stands for `n` identical logical tuples (a collapsed pattern suffix folded
    /// into a count; see [`koko_common::DataChunk::multiplicity`]). `n == 1` is the
    /// ordinary path. `count(*)` counts `n` rows; `count(x)`/`sum`/`avg` scale by
    /// `n`; `min`/`max` are idempotent in `n`; `collect` gathers `n` copies. Under
    /// `DISTINCT`, `n` is ignored — the same value repeated adds no new distinct
    /// value (so `count(DISTINCT head)` over a fan-out is still 1).
    pub fn update_n(&mut self, v: &Value, n: u64) {
        self.n_rows += n as i64;
        if self.op == AggOp::CountStar {
            return;
        }
        if v.is_null() {
            return;
        }
        if let Some(seen) = &mut self.seen {
            // C++ counts NaNs as distinct from each other (audit V9): a NaN never
            // deduplicates, so count(DISTINCT [nan, nan, 1.0]) is 3.
            let is_nan = matches!(v, Value::Double(x) if x.is_nan())
                || matches!(v, Value::Float(x) if x.is_nan());
            if !is_nan && !seen.insert(ValueKey::from_value(v)) {
                return; // duplicate under DISTINCT
            }
        }
        // A DISTINCT value contributes exactly once regardless of multiplicity.
        let n = if self.seen.is_some() { 1 } else { n };
        let ni = n as i64;
        self.n_nonnull += ni;
        match self.op {
            AggOp::Count => {}
            AggOp::Sum | AggOp::Avg => match v {
                Value::Double(x) => {
                    self.is_float = true;
                    self.sum_f += *x * n as f64;
                }
                Value::Float(x) => {
                    self.is_float = true;
                    self.sum_f += *x as f64 * n as f64;
                }
                // Unsigned widths accumulate exactly in u128 (C++ SUM(UINT*) ->
                // UINT128 — audit V3; the old i128 path silently wrapped).
                Value::UInt128(_)
                | Value::IntX {
                    kind: IntKind::U8 | IntKind::U16 | IntKind::U32 | IntKind::U64,
                    ..
                } => {
                    self.is_unsigned = true;
                    let val = v.as_u128().unwrap_or(0);
                    self.sum_u = self.sum_u.wrapping_add(val.wrapping_mul(n as u128));
                    self.sum_f += val as f64 * n as f64;
                }
                // Exact signed accumulation in i128 (C++ SUM(INT*) -> INT128).
                _ if v.as_int128().is_some() => {
                    let val = v.as_int128().unwrap();
                    self.sum_i = self.sum_i.wrapping_add(val.wrapping_mul(n as i128));
                    self.sum_f += val as f64 * n as f64;
                }
                // DECIMAL SUM/AVG is rejected by the binder to match C++; do
                // not silently accumulate it through f64 if a caller bypasses
                // binding and feeds AggState directly.
                Value::Decimal { .. } => {}
                _ if v.as_f64().is_some() => {
                    self.is_float = true;
                    self.sum_f += v.as_f64().unwrap() * n as f64;
                }
                _ => {}
            },
            AggOp::Min => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|e| order_cmp(v, e) == Ordering::Less)
                {
                    self.extreme = Some(v.clone());
                }
            }
            AggOp::Max => {
                if self
                    .extreme
                    .as_ref()
                    .is_none_or(|e| order_cmp(v, e) == Ordering::Greater)
                {
                    self.extreme = Some(v.clone());
                }
            }
            AggOp::Collect | AggOp::PercentileDisc(_) => {
                for _ in 0..n {
                    self.collected.push(v.clone());
                }
            }
            AggOp::CountStar => unreachable!(),
        }
    }

    /// Fold a partial accumulator for the **same group and op** into this one — the
    /// merge step of the partitioned parallel hash aggregate (P3 step 9), where each
    /// morsel built a local partial. Merging the morsel partials in **morsel order**
    /// reproduces the serial accumulation exactly:
    /// - `count(*)`/`count`/`sum`/`avg` add the row/value counts and the **`i128`**
    ///   integer sum (associative ⇒ bit-identical regardless of partition);
    /// - `min`/`max` take the combined extreme (order-independent);
    /// - `collect` concatenates (the caller merges in morsel order ⇒ serial list order).
    ///
    /// The float sum (`sum_f`) is added too, but the parallel path never reaches this
    /// for a float-typed `SUM`/`AVG` argument (f64 addition is non-associative, so the
    /// optimizer keeps those serial); likewise `DISTINCT` aggregates are never
    /// parallelized, so both states are non-distinct here.
    pub fn merge(&mut self, other: AggState) {
        debug_assert_eq!(self.op, other.op, "merge of mismatched aggregate ops");
        debug_assert!(
            self.seen.is_none() && other.seen.is_none(),
            "DISTINCT aggregates are not parallelized, so are never merged"
        );
        self.n_rows += other.n_rows;
        self.n_nonnull += other.n_nonnull;
        self.sum_i = self.sum_i.wrapping_add(other.sum_i);
        self.sum_u = self.sum_u.wrapping_add(other.sum_u);
        self.sum_f += other.sum_f;
        self.is_float |= other.is_float;
        self.is_unsigned |= other.is_unsigned;
        match self.op {
            AggOp::Min => {
                if let Some(o) = other.extreme {
                    if self
                        .extreme
                        .as_ref()
                        .is_none_or(|e| order_cmp(&o, e) == Ordering::Less)
                    {
                        self.extreme = Some(o);
                    }
                }
            }
            AggOp::Max => {
                if let Some(o) = other.extreme {
                    if self
                        .extreme
                        .as_ref()
                        .is_none_or(|e| order_cmp(&o, e) == Ordering::Greater)
                    {
                        self.extreme = Some(o);
                    }
                }
            }
            AggOp::Collect | AggOp::PercentileDisc(_) => self.collected.extend(other.collected),
            AggOp::CountStar | AggOp::Count | AggOp::Sum | AggOp::Avg => {}
        }
    }

    /// Produce the aggregate's final value for the group.
    ///
    /// Returns `Err` if an INT64 `SUM` overflowed — matching the checked
    /// overflow contract of scalar integer arithmetic (rather than silently
    /// wrapping the `i128` accumulator down to `i64`).
    pub fn finalize(self) -> Result<Value> {
        Ok(match self.op {
            AggOp::CountStar => Value::Int64(self.n_rows),
            AggOp::Count => Value::Int64(self.n_nonnull),
            // SUM widens like C++ (audit V3): INT* -> INT128, UINT* -> UINT128,
            // FLOAT -> DOUBLE — the old INT64 "overflow" error was invented.
            AggOp::Sum => {
                if self.n_nonnull == 0 {
                    Value::Null
                } else if self.is_float {
                    Value::Double(self.sum_f)
                } else if self.is_unsigned {
                    Value::UInt128(self.sum_u)
                } else {
                    Value::IntX {
                        value: self.sum_i,
                        kind: IntKind::I128,
                    }
                }
            }
            AggOp::Avg => {
                if self.n_nonnull == 0 {
                    Value::Null
                } else if self.is_float {
                    Value::Double(self.sum_f / self.n_nonnull as f64)
                } else if self.is_unsigned {
                    Value::Double(self.sum_u as f64 / self.n_nonnull as f64)
                } else {
                    // Divide the exact integer sum (one rounding) rather than the
                    // lossily-accumulated f64, which loses precision past 2^53.
                    Value::Double(self.sum_i as f64 / self.n_nonnull as f64)
                }
            }
            AggOp::Min | AggOp::Max => self.extreme.unwrap_or(Value::Null),
            // `collect` returns NULL (not an empty list) when no non-null value was
            // gathered, matching C++ (the list aggregate carries a null state until
            // its first value). An explicit empty-list literal `[]` still has size 0;
            // only an aggregate that collected nothing is NULL.
            AggOp::Collect if self.collected.is_empty() => Value::Null,
            AggOp::Collect => Value::List(self.collected),
            AggOp::PercentileDisc(_) if self.collected.is_empty() => Value::Null,
            AggOp::PercentileDisc(bits) => {
                let p = f64::from_bits(bits).clamp(0.0, 1.0);
                let mut vals = self.collected;
                vals.sort_by(order_cmp);
                let n = vals.len();
                let idx = ((p * n as f64).ceil() as usize).clamp(1, n) - 1;
                vals[idx].clone()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Merging two partial accumulators (the parallel partitioned-aggregate combine)
    /// gives the same answer as feeding all values to one accumulator — and for
    /// `collect`, in the order the partials are merged.
    #[test]
    fn agg_merge_matches_single_accumulator() {
        // count(*): two morsels of 3 and 2 rows -> 5.
        let mut a = AggState::new(AggOp::CountStar, false);
        let mut b = AggState::new(AggOp::CountStar, false);
        for _ in 0..3 {
            a.update(&Value::Null);
        }
        for _ in 0..2 {
            b.update(&Value::Null);
        }
        a.merge(b);
        assert_eq!(a.finalize().unwrap(), Value::Int64(5));

        // integer sum: {1,2,3} + {4,5} == 15 (exact, associative).
        let mut a = AggState::new(AggOp::Sum, false);
        let mut b = AggState::new(AggOp::Sum, false);
        [1, 2, 3].iter().for_each(|&v| a.update(&Value::Int64(v)));
        [4, 5].iter().for_each(|&v| b.update(&Value::Int64(v)));
        a.merge(b);
        // SUM widens to INT128 (audit V3), so the merged result is IntX/I128.
        assert_eq!(
            a.finalize().unwrap(),
            Value::IntX {
                value: 15,
                kind: IntKind::I128
            }
        );

        // min / max take the combined extreme regardless of which partial held it.
        let mut a = AggState::new(AggOp::Min, false);
        let mut b = AggState::new(AggOp::Min, false);
        a.update(&Value::Int64(7));
        b.update(&Value::Int64(3));
        a.merge(b);
        assert_eq!(a.finalize().unwrap(), Value::Int64(3));

        // collect concatenates in merge order (morsel-index order at the call site).
        let mut a = AggState::new(AggOp::Collect, false);
        let mut b = AggState::new(AggOp::Collect, false);
        a.update(&Value::Int64(1));
        a.update(&Value::Int64(2));
        b.update(&Value::Int64(3));
        a.merge(b);
        assert_eq!(
            a.finalize().unwrap(),
            Value::List(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)])
        );
    }

    #[test]
    fn arithmetic_and_nulls() {
        assert_eq!(
            eval_scalar(ScalarOp::Add, &[Value::Int64(2), Value::Int64(3)]).unwrap(),
            Value::Int64(5)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Add, &[Value::Int64(2), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_scalar(ScalarOp::Mul, &[Value::Double(2.0), Value::Int64(3)]).unwrap(),
            Value::Double(6.0)
        );
        assert_eq!(
            scalar_result_type(
                ScalarOp::Mul,
                &[LogicalType::Decimal(4, 2), LogicalType::Int64]
            )
            .unwrap(),
            LogicalType::Decimal(9, 4)
        );
        assert_eq!(
            eval_scalar(
                ScalarOp::Mul,
                &[
                    Value::Decimal {
                        value: 123,
                        precision: 4,
                        scale: 2
                    },
                    Value::Int64(2)
                ]
            )
            .unwrap(),
            Value::Decimal {
                value: 24600,
                precision: 9,
                scale: 4
            }
        );
        assert!(eval_scalar(ScalarOp::Add, &[Value::Int64(i64::MAX), Value::Int64(1)]).is_err());
    }

    #[test]
    fn path_accessor_functions() {
        use koko_common::{InternalId, NodeValue, RecursiveRelValue, RelValue};
        let iid = |t: u64, o: u64| InternalId::new(koko_common::TableId(t), o);
        let rr = Value::RecursiveRel(Box::new(RecursiveRelValue {
            nodes: vec![NodeValue {
                id: iid(0, 0),
                label: "person".into(),
                props: vec![("fName".into(), Value::String("Alice".into()))],
            }],
            rels: vec![
                RelValue {
                    src: iid(0, 3),
                    dst: iid(0, 0),
                    id: iid(3, 9),
                    label: "knows".into(),
                    props: vec![],
                    src_node: None,
                    dst_node: None,
                },
                RelValue {
                    src: iid(0, 0),
                    dst: iid(0, 1),
                    id: iid(3, 0),
                    label: "knows".into(),
                    props: vec![],
                    src_node: None,
                    dst_node: None,
                },
            ],
            degenerate: false,
            cost: None,
            null_nodes: 0,
        }));
        // length = rel count.
        assert_eq!(
            eval_scalar_func("length", std::slice::from_ref(&rr)).unwrap(),
            Value::Int64(2)
        );
        // nodes(p) → LIST[NODE]; rels(p) → LIST[REL].
        let nodes = eval_scalar_func("nodes", std::slice::from_ref(&rr)).unwrap();
        assert_eq!(
            nodes.to_result_string(),
            "[{_ID: 0:0, _LABEL: person, fName: Alice}]"
        );
        let rels = eval_scalar_func("rels", std::slice::from_ref(&rr)).unwrap();
        assert!(matches!(rels, Value::List(ref v) if v.len() == 2));
        // properties(nodes(p), key): plain prop, _id, _label, and a missing prop.
        assert_eq!(
            eval_scalar_func(
                "properties",
                &[nodes.clone(), Value::String("fName".into())]
            )
            .unwrap()
            .to_result_string(),
            "[Alice]"
        );
        assert_eq!(
            eval_scalar_func("properties", &[rels.clone(), Value::String("_id".into())])
                .unwrap()
                .to_result_string(),
            "[3:9,3:0]"
        );
        assert_eq!(
            eval_scalar_func(
                "properties",
                &[nodes.clone(), Value::String("_Label".into())]
            )
            .unwrap()
            .to_result_string(),
            "[person]"
        );
        // A missing property renders empty (NULL).
        assert_eq!(
            eval_scalar_func("properties", &[nodes, Value::String("age".into())])
                .unwrap()
                .to_result_string(),
            "[]"
        );
    }

    #[test]
    fn size_rejects_graph_types() {
        use koko_common::TableId;
        // `size` accepts LIST/MAP/STRING but rejects NODE/REL/RECURSIVE_REL with
        // the C++ signature listing.
        assert!(
            scalar_func_result_type("size", &[LogicalType::List(Box::new(LogicalType::Int64))])
                .is_ok()
        );
        assert!(scalar_func_result_type("size", &[LogicalType::String]).is_ok());
        let err = scalar_func_result_type("size", &[LogicalType::Node(TableId(0))]).unwrap_err();
        assert!(
            err.to_string()
                .contains("Function SIZE did not receive correct arguments:")
        );
        assert!(err.to_string().contains("Actual:   (NODE)"));
        assert!(scalar_func_result_type("size", &[LogicalType::RecursiveRel]).is_err());
    }

    #[test]
    fn three_valued_logic() {
        assert_eq!(
            eval_scalar(ScalarOp::And, &[Value::Bool(false), Value::Null]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_scalar(ScalarOp::And, &[Value::Bool(true), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_scalar(ScalarOp::Or, &[Value::Bool(true), Value::Null]).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn comparisons() {
        assert_eq!(
            eval_scalar(ScalarOp::Gt, &[Value::Int64(5), Value::Int64(3)]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Lt, &[Value::Int64(5), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_scalar(ScalarOp::Eq, &[Value::Double(3.0), Value::Int64(3)]).unwrap(),
            Value::Bool(true)
        );
        let nan = Value::Double(f64::NAN);
        assert_eq!(
            eval_scalar(ScalarOp::Eq, &[nan.clone(), nan.clone()]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Ne, &[nan.clone(), nan.clone()]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Lt, &[nan, Value::Double(0.0)]).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn aggregates() {
        let mut s = AggState::new(AggOp::Sum, false);
        for v in [Value::Int64(10), Value::Null, Value::Int64(5)] {
            s.update(&v);
        }
        // SUM widens to INT128 (audit V3).
        assert_eq!(
            s.finalize().unwrap(),
            Value::IntX {
                value: 15,
                kind: IntKind::I128
            }
        );

        let mut s = AggState::new(AggOp::Sum, false);
        s.update(&Value::Null);
        assert_eq!(s.finalize().unwrap(), Value::Null);

        let mut s = AggState::new(AggOp::Avg, false);
        for v in [Value::Int64(35), Value::Int64(40)] {
            s.update(&v);
        }
        assert_eq!(s.finalize().unwrap(), Value::Double(37.5));

        let mut s = AggState::new(AggOp::CountStar, false);
        for v in [Value::Null, Value::Int64(1)] {
            s.update(&v);
        }
        assert_eq!(s.finalize().unwrap(), Value::Int64(2));

        let mut s = AggState::new(AggOp::Count, true);
        for v in [Value::Int64(1), Value::Int64(1), Value::Int64(2)] {
            s.update(&v);
        }
        assert_eq!(s.finalize().unwrap(), Value::Int64(2));
    }

    #[test]
    fn sum_widens_past_the_argument_width() {
        // C++ SUM(INT64) -> INT128 (audit V3): two i64::MAX values sum exactly,
        // where the old accumulator raised an invented overflow error.
        let mut s = AggState::new(AggOp::Sum, false);
        s.update(&Value::Int64(i64::MAX));
        s.update(&Value::Int64(i64::MAX));
        assert_eq!(
            s.finalize().unwrap(),
            Value::IntX {
                value: 2 * (i64::MAX as i128),
                kind: IntKind::I128
            }
        );

        // SUM(UINT*) -> UINT128: no silent i128 wrap (two 2^127-1 values).
        let big = (1u128 << 127) - 1;
        let mut s = AggState::new(AggOp::Sum, false);
        s.update(&Value::UInt128(big));
        s.update(&Value::UInt128(big));
        assert_eq!(s.finalize().unwrap(), Value::UInt128(big * 2));
    }

    #[test]
    fn group_key_agrees_with_equality() {
        // 1 and 1.0 compare equal, so they must key equal; -0.0 == 0.0 likewise.
        assert_eq!(
            ValueKey::from_value(&Value::Int64(1)),
            ValueKey::from_value(&Value::Double(1.0))
        );
        assert_eq!(
            ValueKey::from_value(&Value::Double(0.0)),
            ValueKey::from_value(&Value::Double(-0.0))
        );
        assert_ne!(
            ValueKey::from_value(&Value::Decimal {
                value: 10_000_000_000_000_000_001,
                precision: 38,
                scale: 0
            }),
            ValueKey::from_value(&Value::Decimal {
                value: 10_000_000_000_000_000_002,
                precision: 38,
                scale: 0
            })
        );
    }
    #[test]
    fn casts_struct_values_to_ordered_json() {
        let value = Value::Struct(vec![("answer".to_string(), Value::Int64(42))]);
        assert_eq!(
            cast_value(&value, &LogicalType::Json).unwrap(),
            Value::Json(koko_common::JsonValue::Object(vec![(
                "answer".to_string(),
                koko_common::JsonValue::Int(42),
            )]))
        );
    }
}
