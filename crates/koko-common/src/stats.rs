//! In-memory table & column statistics for the cost-based optimizer (P3).
//!
//! Mirrors the C++ `storage::stats` contract closely enough to drive the same
//! cardinality-based plan choices, without copying its byte layout: a per-table
//! row count ([`TableStats::num_tuples`]) and, per column, a HyperLogLog
//! distinct-count sketch plus min/max and a null count ([`ColumnStats`]). The
//! engine *maintains* these over the in-memory store (folding each row in at
//! commit) and exposes them directly through `InMemStorage` to the planner and
//! `stats_info`. Persistence is outside the active scope.
//!
//! The [`HyperLogLog`] is a faithful port of the DuckDB/Kùzu estimator (`P = 6`,
//! 64 registers, the Redis sigma/tau cardinality formula) so distinct-count
//! estimates — and therefore join-order decisions — match the C++ engine within
//! HLL error. Only the hash function feeding it differs (a deterministic Rust
//! hash), which does not affect estimate accuracy.
//!
//! [`Value`] implements neither `Hash` nor `Ord` (it carries floats), so this
//! module provides [`hash_value`] (for the sketch) and [`value_cmp`] (for min/max)
//! that operate on the scalar variants and treat `NULL`/nested/graph values as
//! "not tracked".

use crate::types::LogicalType;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

// --- HyperLogLog (distinct-count sketch) ---

/// Register-index bits — matches C++ `HyperLogLog::P`.
const P: u32 = 6;
/// Rank bits (`64 - P`): the shifted hash carries `Q` significant bits before the
/// sentinel set in [`HyperLogLog::insert_hash`].
const Q: usize = 64 - P as usize; // 58
/// Register count (`2^P`).
const M: usize = 1 << P; // 64
/// Bias constant `1 / (2 ln 2)` (C++ uses the same value at `f64` precision).
const ALPHA: f64 = 0.721_347_520_444_481_7;

/// A HyperLogLog distinct-count sketch — a port of the DuckDB implementation the
/// C++ engine uses (`P = 6`, 64 one-byte registers). Fold hashed values in with
/// [`insert_hash`](Self::insert_hash); read the estimate with [`count`](Self::count);
/// combine partial sketches with [`merge`](Self::merge).
#[derive(Debug, Clone)]
pub struct HyperLogLog {
    registers: [u8; M],
}

impl Default for HyperLogLog {
    fn default() -> Self {
        HyperLogLog { registers: [0; M] }
    }
}

impl HyperLogLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a 64-bit hash into the sketch (C++ `insertElement`, "Algorithm 1"):
    /// the low `P` bits pick the register; the rank is one past the trailing-zero
    /// count of the remaining bits, with a sentinel bit bounding it at `Q + 1`.
    pub fn insert_hash(&mut self, mut h: u64) {
        let i = (h & ((1 << P) - 1)) as usize;
        h >>= P;
        h |= 1u64 << Q;
        let z = (h.trailing_zeros() + 1) as u8;
        if z > self.registers[i] {
            self.registers[i] = z;
        }
    }

    /// Merge another sketch into this one using register-wise maxima. This is the
    /// deterministic union operation for independently collected sketches.
    pub fn merge(&mut self, other: &HyperLogLog) {
        for (r, &o) in self.registers.iter_mut().zip(other.registers.iter()) {
            if o > *r {
                *r = o;
            }
        }
    }

    /// The estimated number of distinct values inserted (C++ `count`); `0` for an
    /// empty sketch.
    pub fn count(&self) -> u64 {
        // Histogram of register values: c[v] = number of registers holding v.
        let mut c = [0u32; Q + 2];
        for &r in &self.registers {
            c[r as usize] += 1;
        }
        estimate_cardinality(&c)
    }
}

/// The Redis/DuckDB sigma/tau cardinality estimator (C++ `estimateCardinality`,
/// "Algorithm 6"). `c` is the register-value histogram of length `Q + 2`.
fn estimate_cardinality(c: &[u32; Q + 2]) -> u64 {
    let m = M as f64;
    let mut z = m * hll_tau((m - c[Q] as f64) / m);
    for k in (1..=Q).rev() {
        z += c[k] as f64;
        z *= 0.5;
    }
    z += m * hll_sigma(c[0] as f64 / m);
    (ALPHA * m * m / z).round() as u64
}

/// Redis `HLLSigma`.
fn hll_sigma(mut x: f64) -> f64 {
    if x == 1.0 {
        return f64::INFINITY;
    }
    let mut y = 1.0;
    let mut z = x;
    loop {
        x *= x;
        let z_prime = z;
        z += x * y;
        y += y;
        if z_prime == z {
            break;
        }
    }
    z
}

/// Redis `HLLTau`.
fn hll_tau(mut x: f64) -> f64 {
    if x == 0.0 || x == 1.0 {
        return 0.0;
    }
    let mut y = 1.0;
    let mut z = 1.0 - x;
    loop {
        x = x.sqrt();
        let z_prime = z;
        y *= 0.5;
        z -= (1.0 - x).powi(2) * y;
        if z_prime == z {
            break;
        }
    }
    z / 3.0
}

// --- value hashing & comparison (Value has neither Hash nor Ord) ---

/// A deterministic 64-bit hash of a *scalar* value, for HyperLogLog distinct-
/// counting. Returns `None` for values that carry no scalar distinct-count: `NULL`
/// and nested/graph values (matching C++ `ColumnStats`, which builds no HLL for
/// nested types). Floats hash by canonicalized bits so `±0.0` and the NaNs each
/// collapse to a single value. This is also the single source of truth for "is
/// this a scalar the stats track".
pub fn hash_value(v: &Value) -> Option<u64> {
    let mut s = DefaultHasher::new();
    match v {
        Value::Null => return None,
        Value::Bool(b) => b.hash(&mut s),
        Value::Int64(n) => (*n as i128).hash(&mut s),
        Value::IntX { value, .. } => value.hash(&mut s),
        Value::UInt128(u) => u.hash(&mut s),
        Value::Decimal { value, scale, .. } => {
            value.hash(&mut s);
            scale.hash(&mut s);
        }
        Value::Double(x) => canonical_f64_bits(*x).hash(&mut s),
        Value::Float(x) => canonical_f64_bits(*x as f64).hash(&mut s),
        Value::String(t) => t.hash(&mut s),
        Value::Json(value) => value.render().hash(&mut s),
        Value::Date(d) => d.hash(&mut s),
        Value::Timestamp(t) | Value::TimestampTz(t) => t.hash(&mut s),
        Value::Interval(iv) => iv.hash(&mut s),
        Value::Uuid(u) => u.hash(&mut s),
        Value::Blob(b) => b.hash(&mut s),
        Value::InternalId(id) => {
            id.table_id.0.hash(&mut s);
            id.offset.0.hash(&mut s);
        }
        // Nested / graph values carry no scalar distinct-count.
        Value::List(_)
        | Value::Struct(_)
        | Value::Map(_)
        | Value::Node(_)
        | Value::Rel(_)
        | Value::RecursiveRel(_)
        | Value::Union { .. } => return None,
    }
    Some(s.finish())
}

/// Canonicalize a float's bits so equal values hash equally: every zero (`±0.0`)
/// maps to `0` and every NaN to one pattern.
fn canonical_f64_bits(x: f64) -> u64 {
    if x == 0.0 {
        0
    } else if x.is_nan() {
        0x7ff8_0000_0000_0000
    } else {
        x.to_bits()
    }
}

/// Order two values for min/max tracking. Defined only *within* a scalar type
/// (columns are homogeneous); cross-type, nested, `NULL`, and `NaN` comparisons
/// return `None` and are simply skipped by the stats.
pub fn value_cmp(a: &Value, b: &Value) -> Option<Ordering> {
    use Value::*;
    match (a, b) {
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (String(x), String(y)) => Some(x.cmp(y)),
        (Date(x), Date(y)) => Some(x.cmp(y)),
        (Timestamp(x) | TimestampTz(x), Timestamp(y) | TimestampTz(y)) => Some(x.cmp(y)),
        (Uuid(x), Uuid(y)) => Some(x.cmp(y)),
        (Blob(x), Blob(y)) => Some(x.cmp(y)),
        (Interval(x), Interval(y)) => Some(x.cmp_micros().cmp(&y.cmp_micros())),
        (Double(x), Double(y)) => x.partial_cmp(y),
        (Float(x), Float(y)) => x.partial_cmp(y),
        (
            Decimal {
                value: xv,
                scale: xs,
                ..
            },
            Decimal {
                value: yv,
                scale: ys,
                ..
            },
        ) if xs == ys => Some(xv.cmp(yv)),
        _ => {
            if let (Some(x), Some(y)) = (a.as_int128(), b.as_int128()) {
                Some(x.cmp(&y))
            } else if let (Some(x), Some(y)) = (a.as_u128(), b.as_u128()) {
                Some(x.cmp(&y))
            } else {
                None
            }
        }
    }
}

// --- column & table statistics ---

/// Per-column statistics: a distinct-count sketch plus min/max and a null count.
/// Distinct counts include `NULL` as one value, matching C++ `stats_info`; min/max
/// cover only scalar values, while `null_count` separately counts every `NULL`.
/// All are folded in incrementally as rows commit.
#[derive(Debug, Clone)]
pub struct ColumnStats {
    hll: HyperLogLog,
    min: Option<Value>,
    max: Option<Value>,
    null_count: u64,
    track_distinct: bool,
}

impl Default for ColumnStats {
    fn default() -> Self {
        Self {
            hll: HyperLogLog::default(),
            min: None,
            max: None,
            null_count: 0,
            track_distinct: true,
        }
    }
}

impl ColumnStats {
    /// Fold one cell value into the column's statistics.
    pub fn record(&mut self, v: &Value) {
        if v.is_null() {
            self.null_count += 1;
            if self.track_distinct {
                self.hll.insert_hash(0x9e37_79b9_7f4a_7c15);
            }
            return;
        }
        // `hash_value` returns `Some` exactly for the scalar values we track.
        if self.track_distinct {
            if let Some(hash) = hash_value(v) {
                self.hll.insert_hash(hash);
            }
        }
        // min/max only for self-comparable scalars (skips NaN, which would stick).
        if value_cmp(v, v).is_some() {
            if self
                .min
                .as_ref()
                .is_none_or(|m| value_cmp(v, m) == Some(Ordering::Less))
            {
                self.min = Some(v.clone());
            }
            if self
                .max
                .as_ref()
                .is_none_or(|m| value_cmp(v, m) == Some(Ordering::Greater))
            {
                self.max = Some(v.clone());
            }
        }
    }

    /// Estimated number of distinct scalar values, including `NULL` when present.
    pub fn num_distinct(&self) -> u64 {
        self.hll.count()
    }
    /// Number of `NULL`s recorded.
    pub fn null_count(&self) -> u64 {
        self.null_count
    }
    /// The smallest value recorded (scalar columns only).
    pub fn min(&self) -> Option<&Value> {
        self.min.as_ref()
    }
    /// The largest value recorded (scalar columns only).
    pub fn max(&self) -> Option<&Value> {
        self.max.as_ref()
    }
}

/// Per-table statistics: a row count and one [`ColumnStats`] per column, kept in
/// the table's column order.
#[derive(Debug, Clone, Default)]
pub struct TableStats {
    num_tuples: u64,
    columns: Vec<ColumnStats>,
}

impl TableStats {
    /// Empty stats for a table with `num_columns` columns.
    pub fn with_columns(num_columns: usize) -> Self {
        TableStats {
            num_tuples: 0,
            columns: vec![ColumnStats::default(); num_columns],
        }
    }

    /// Cardinality-only stats for a source whose values are not materialized.
    pub fn with_row_count(num_columns: usize, num_tuples: u64) -> Self {
        TableStats {
            num_tuples,
            columns: vec![ColumnStats::default(); num_columns],
        }
    }

    /// Empty stats preserving whether each logical type supports distinct counts.
    pub fn with_types<'a>(types: impl IntoIterator<Item = &'a LogicalType>) -> Self {
        let columns = types
            .into_iter()
            .map(|logical_type| ColumnStats {
                track_distinct: !matches!(
                    logical_type,
                    LogicalType::List(_)
                        | LogicalType::Array(_, _)
                        | LogicalType::Struct(_)
                        | LogicalType::Map(_, _)
                        | LogicalType::Union(_)
                        | LogicalType::Node(_)
                        | LogicalType::Rel(_)
                        | LogicalType::RecursiveRel
                        | LogicalType::Any
                ),
                ..ColumnStats::default()
            })
            .collect();
        Self {
            num_tuples: 0,
            columns,
        }
    }

    /// Empty statistics with the same per-column tracking policy.
    pub fn empty_like(&self) -> Self {
        Self {
            num_tuples: 0,
            columns: self
                .columns
                .iter()
                .map(|column| ColumnStats {
                    track_distinct: column.track_distinct,
                    ..ColumnStats::default()
                })
                .collect(),
        }
    }

    /// Fold one committed row, read column-major from `columns` at `offset`.
    pub fn record_row(&mut self, columns: &[Vec<Value>], offset: usize) {
        for (cs, col) in self.columns.iter_mut().zip(columns) {
            cs.record(&col[offset]);
        }
        self.num_tuples += 1;
    }

    /// Fold one committed row supplied in schema order.
    pub fn record_values(&mut self, values: &[Value]) {
        for (column, value) in self.columns.iter_mut().zip(values) {
            column.record(value);
        }
        self.num_tuples += 1;
    }

    /// Fold one committed row from an owned value iterator without materializing it.
    pub fn record_owned_row(&mut self, values: impl IntoIterator<Item = Value>) {
        for (column, value) in self.columns.iter_mut().zip(values) {
            column.record(&value);
        }
        self.num_tuples += 1;
    }

    /// `ALTER … ADD`: append stats for a new column whose `count` rows were
    /// backfilled with the constant `default`. A null backfill is recorded exactly
    /// (`null_count = count`); a non-null constant is recorded once (distinct ≈ 1,
    /// `min = max = default`) — an approximation that is sufficient for an estimate
    /// and rare on the perf path (see `ROADMAP.md` PERF-02).
    pub fn push_column(&mut self, logical_type: &LogicalType, default: &Value, count: u64) {
        let mut stats = TableStats::with_types(std::iter::once(logical_type));
        let mut cs = stats.columns.pop().expect("one requested column");
        cs.record(default);
        if default.is_null() {
            cs.null_count = count;
        }
        self.columns.push(cs);
    }

    /// Undo a [`push_column`](Self::push_column) (`ALTER … ADD` rollback).
    pub fn pop_column(&mut self) {
        self.columns.pop();
    }

    /// `ALTER … DROP`: remove the column's stats at `idx`.
    pub fn remove_column(&mut self, idx: usize) {
        if idx < self.columns.len() {
            self.columns.remove(idx);
        }
    }

    /// Rebuild and re-insert a column's stats at `idx` from its stored `values`
    /// (undo of a column drop). `values` may include tombstoned rows, so the result
    /// is an upper bound — acceptable for an estimate.
    pub fn restore_column(&mut self, idx: usize, logical_type: &LogicalType, values: &[Value]) {
        let mut stats = TableStats::with_types(std::iter::once(logical_type));
        let mut cs = stats.columns.pop().expect("one requested column");
        for v in values {
            cs.record(v);
        }
        let idx = idx.min(self.columns.len());
        self.columns.insert(idx, cs);
    }

    /// Number of rows folded in (committed inserts; an upper bound after deletes).
    pub fn num_tuples(&self) -> u64 {
        self.num_tuples
    }
    /// Statistics for the column at `idx`, if present.
    pub fn column(&self, idx: usize) -> Option<&ColumnStats> {
        self.columns.get(idx)
    }
    /// Number of columns tracked.
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(x: u64) -> u64 {
        // A deterministic, well-spread hash for distinct test inputs.
        hash_value(&Value::Int64(x as i64)).unwrap()
    }

    #[test]
    fn hll_empty_is_zero() {
        assert_eq!(HyperLogLog::new().count(), 0);
    }

    #[test]
    fn hll_small_cardinality_is_close() {
        let mut hll = HyperLogLog::new();
        for i in 0..5u64 {
            hll.insert_hash(h(i));
        }
        let est = hll.count();
        assert!((3..=8).contains(&est), "small-cardinality estimate {est}");
    }

    #[test]
    fn hll_estimates_distinct_within_tolerance() {
        // P=6 (64 registers) is coarse (~13% standard error); assert a wide band.
        let mut hll = HyperLogLog::new();
        for i in 0..1000u64 {
            hll.insert_hash(h(i));
        }
        let est = hll.count();
        assert!((750..=1300).contains(&est), "estimate {est} out of band");
    }

    #[test]
    fn hll_insert_is_idempotent_on_repeats() {
        let mut hll = HyperLogLog::new();
        for _ in 0..100 {
            hll.insert_hash(h(42));
        }
        assert!((1..=2).contains(&hll.count()), "{}", hll.count());
    }

    #[test]
    fn hll_merge_unions() {
        let mut a = HyperLogLog::new();
        let mut b = HyperLogLog::new();
        for i in 0..500u64 {
            a.insert_hash(h(i));
        }
        for i in 250..750u64 {
            b.insert_hash(h(i));
        }
        a.merge(&b);
        // Union ≈ 750 distinct.
        let est = a.count();
        assert!((600..=950).contains(&est), "merged estimate {est}");
    }

    #[test]
    fn hash_value_scalars_and_nulls() {
        assert!(hash_value(&Value::Null).is_none());
        assert!(hash_value(&Value::List(vec![])).is_none());
        assert_eq!(hash_value(&Value::Int64(5)), hash_value(&Value::Int64(5)));
        assert_ne!(hash_value(&Value::Int64(5)), hash_value(&Value::Int64(6)));
        // +0.0 and -0.0 hash equal.
        assert_eq!(
            hash_value(&Value::Double(0.0)),
            hash_value(&Value::Double(-0.0))
        );
    }

    #[test]
    fn value_cmp_orders_scalars() {
        assert_eq!(
            value_cmp(&Value::Int64(1), &Value::Int64(2)),
            Some(Ordering::Less)
        );
        assert_eq!(
            value_cmp(&Value::String("a".into()), &Value::String("b".into())),
            Some(Ordering::Less)
        );
        // Cross-type / nested → None.
        assert_eq!(
            value_cmp(&Value::Int64(1), &Value::String("b".into())),
            None
        );
        assert_eq!(value_cmp(&Value::List(vec![]), &Value::List(vec![])), None);
    }

    #[test]
    fn column_stats_record() {
        let mut cs = ColumnStats::default();
        cs.record(&Value::Int64(10));
        cs.record(&Value::Int64(5));
        cs.record(&Value::Int64(10));
        cs.record(&Value::Null);
        assert_eq!(cs.num_distinct(), 3);
        assert_eq!(cs.min(), Some(&Value::Int64(5)));
        assert_eq!(cs.max(), Some(&Value::Int64(10)));
        assert_eq!(cs.null_count(), 1);
    }

    #[test]
    fn table_stats_record_row_columnar() {
        let cols = vec![
            vec![Value::Int64(1), Value::Int64(2)],
            vec![Value::String("a".into()), Value::Null],
        ];
        let mut ts = TableStats::with_columns(2);
        ts.record_row(&cols, 0);
        ts.record_row(&cols, 1);
        assert_eq!(ts.num_tuples(), 2);
        assert_eq!(ts.column(0).unwrap().num_distinct(), 2);
        assert_eq!(ts.column(1).unwrap().null_count(), 1);
    }

    #[test]
    fn table_stats_push_column() {
        let mut ts = TableStats::with_columns(1);
        ts.push_column(&LogicalType::Int64, &Value::Int64(7), 4);
        assert_eq!(ts.num_columns(), 2);
        assert_eq!(ts.column(1).unwrap().num_distinct(), 1);
        assert_eq!(ts.column(1).unwrap().min(), Some(&Value::Int64(7)));
        // A null backfill records its full count.
        ts.push_column(&LogicalType::Int64, &Value::Null, 4);
        assert_eq!(ts.column(2).unwrap().null_count(), 4);
        assert_eq!(ts.column(2).unwrap().num_distinct(), 1);
    }

    #[test]
    fn nested_stats_do_not_count_null_as_distinct() {
        let logical_type = LogicalType::List(Box::new(LogicalType::Int64));
        let mut stats = TableStats::with_types(std::iter::once(&logical_type));
        stats.record_values(&[Value::Null]);
        assert_eq!(stats.column(0).unwrap().null_count(), 1);
        assert_eq!(stats.column(0).unwrap().num_distinct(), 0);
    }
}
