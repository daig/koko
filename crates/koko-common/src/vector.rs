//! The vectorized data layer: `ValueVector` + `DataChunk`, with a bit-packed
//! null mask and a selection vector.
//!
//! This is the currency every operator speaks. A typed [`ColumnData`] enum
//! replaces erased byte buffers and casts, so this layer requires no `unsafe`.
//!
//! Ownership differs from C++ on purpose: instead of every column holding a
//! `shared_ptr<DataChunkState>`, the [`DataChunk`] owns the single selection
//! once and the columns are plain fields. Operators take `&mut DataChunk` and
//! split-borrow `&columns[..]` against `&sel` — the borrow checker permits this
//! because they are disjoint fields, which gives the C++ "filter once, all
//! columns follow" semantics with none of the aliasing.

use crate::temporal::Interval;
use crate::types::{IntKind, InternalId, LogicalType, PhysicalType};
use crate::value::Value;

include!(concat!(env!("OUT_DIR"), "/vector_capacity.rs"));

const NULL_WORDS: usize = VECTOR_CAPACITY / 64;

/// A bit-per-value null bitmap with a "definitely no nulls" fast path.
#[derive(Debug, Clone)]
pub struct NullMask {
    words: Box<[u64]>,
    /// `true` ⇒ guaranteed no nulls. `false` ⇒ *may* contain nulls
    /// (conservative — clearing a bit does not re-set this flag).
    no_nulls: bool,
}

impl Default for NullMask {
    fn default() -> Self {
        Self::new()
    }
}

impl NullMask {
    pub fn new() -> Self {
        Self {
            words: vec![0u64; NULL_WORDS].into_boxed_slice(),
            no_nulls: true,
        }
    }

    /// Mark every physical slot null. Operators clear bits as they populate a chunk.
    pub fn set_all_null(&mut self) {
        self.words.fill(u64::MAX);
        self.no_nulls = false;
    }

    #[inline]
    pub fn is_null(&self, pos: usize) -> bool {
        if self.no_nulls {
            return false;
        }
        (self.words[pos / 64] >> (pos % 64)) & 1 != 0
    }

    #[inline]
    pub fn set_null(&mut self, pos: usize, is_null: bool) {
        if is_null {
            self.words[pos / 64] |= 1u64 << (pos % 64);
            self.no_nulls = false;
        } else {
            self.words[pos / 64] &= !(1u64 << (pos % 64));
        }
    }

    /// Reset the whole mask to "no nulls" (used when a vector is reused).
    pub fn reset(&mut self) {
        if !self.no_nulls {
            self.words.fill(0);
            self.no_nulls = true;
        }
    }
}

/// A typed, fixed-stride column buffer. Each variant holds exactly
/// [`VECTOR_CAPACITY`] slots; the live row count is governed by the chunk's
/// [`Selection`].
#[derive(Debug, Clone)]
pub enum ColumnData {
    Bool(Box<[bool]>),
    Int64(Box<[i64]>),
    Int128(Box<[i128]>),
    UInt128(Box<[u128]>),
    Double(Box<[f64]>),
    Float(Box<[f32]>),
    Date(Box<[i32]>),
    Timestamp(Box<[i64]>),
    Interval(Box<[Interval]>),
    Uuid(Box<[u128]>),
    Decimal(Box<[i128]>),
    /// Owned variable-width strings; empty slots allocate no payload.
    Str(Box<[String]>),
    InternalId(Box<[InternalId]>),
    /// Managed fallback for BLOB, nested, and graph values.
    Generic(Box<[Value]>),
}

impl ColumnData {
    fn with_capacity(pt: PhysicalType) -> Self {
        match pt {
            PhysicalType::Bool => ColumnData::Bool(vec![false; VECTOR_CAPACITY].into_boxed_slice()),
            PhysicalType::Int64 => {
                ColumnData::Int64(vec![0i64; VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::Int128 => {
                ColumnData::Int128(vec![0i128; VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::UInt128 => {
                ColumnData::UInt128(vec![0u128; VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::Double => {
                ColumnData::Double(vec![0.0f64; VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::Float => {
                ColumnData::Float(vec![0.0f32; VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::Date => ColumnData::Date(vec![0i32; VECTOR_CAPACITY].into_boxed_slice()),
            PhysicalType::Timestamp => {
                ColumnData::Timestamp(vec![0i64; VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::Interval => ColumnData::Interval(
                vec![
                    Interval {
                        months: 0,
                        days: 0,
                        micros: 0,
                    };
                    VECTOR_CAPACITY
                ]
                .into_boxed_slice(),
            ),
            PhysicalType::Uuid => ColumnData::Uuid(vec![0u128; VECTOR_CAPACITY].into_boxed_slice()),
            PhysicalType::Decimal => {
                ColumnData::Decimal(vec![0i128; VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::String => {
                ColumnData::Str(vec![String::new(); VECTOR_CAPACITY].into_boxed_slice())
            }
            PhysicalType::InternalId => ColumnData::InternalId(
                vec![InternalId::new(crate::types::TableId(0), 0); VECTOR_CAPACITY]
                    .into_boxed_slice(),
            ),
            PhysicalType::Generic | PhysicalType::Any => {
                ColumnData::Generic(vec![Value::Null; VECTOR_CAPACITY].into_boxed_slice())
            }
        }
    }

    /// Heap bytes allocated for one fixed-capacity vector backing.
    pub fn allocation_bytes(pt: PhysicalType) -> u64 {
        let element = match pt {
            PhysicalType::Bool => std::mem::size_of::<bool>(),
            PhysicalType::Int64 | PhysicalType::Timestamp => std::mem::size_of::<i64>(),
            PhysicalType::Int128 | PhysicalType::Decimal => std::mem::size_of::<i128>(),
            PhysicalType::UInt128 | PhysicalType::Uuid => std::mem::size_of::<u128>(),
            PhysicalType::Double => std::mem::size_of::<f64>(),
            PhysicalType::Float => std::mem::size_of::<f32>(),
            PhysicalType::Date => std::mem::size_of::<i32>(),
            PhysicalType::Interval => std::mem::size_of::<Interval>(),
            PhysicalType::String => std::mem::size_of::<String>(),
            PhysicalType::InternalId => std::mem::size_of::<InternalId>(),
            PhysicalType::Generic | PhysicalType::Any => std::mem::size_of::<Value>(),
        };
        (element * VECTOR_CAPACITY + NULL_WORDS * std::mem::size_of::<u64>()) as u64
    }

    fn payload_bytes(&self) -> u64 {
        match self {
            ColumnData::Str(values) => values.iter().map(|value| value.capacity() as u64).sum(),
            ColumnData::Generic(values) => values.iter().map(value_payload_bytes).sum(),
            _ => 0,
        }
    }
}

/// A single column of one [`LogicalType`].
#[derive(Debug, Clone)]
pub struct ValueVector {
    pub logical_type: LogicalType,
    pub data: ColumnData,
    pub nulls: NullMask,
}

impl ValueVector {
    pub fn new(logical_type: LogicalType) -> Self {
        let data = ColumnData::with_capacity(logical_type.physical_type());
        Self {
            logical_type,
            data,
            nulls: NullMask::new(),
        }
    }

    /// Allocate a vector whose slots begin as NULL.
    pub fn new_null(logical_type: LogicalType) -> Self {
        let mut vector = Self::new(logical_type);
        vector.nulls.set_all_null();
        vector
    }

    /// Read the value at *physical* position `pos` as an owned [`Value`].
    ///
    /// Node/Rel-typed vectors carry only the `InternalId` in the pipeline, so
    /// this returns `Value::InternalId` for them; full node/rel assembly is the
    /// result collector's job.
    pub fn get_value(&self, pos: usize) -> Value {
        if self.nulls.is_null(pos) {
            return Value::Null;
        }
        match &self.data {
            ColumnData::Bool(v) => Value::Bool(v[pos]),
            ColumnData::Int64(v) => Value::Int64(v[pos]),
            ColumnData::Int128(v) => Value::IntX {
                value: v[pos],
                kind: match self.logical_type {
                    LogicalType::Int(kind) => kind,
                    _ => IntKind::I128,
                },
            },
            ColumnData::UInt128(v) => Value::UInt128(v[pos]),
            ColumnData::Double(v) => Value::Double(v[pos]),
            ColumnData::Float(v) => Value::Float(v[pos]),
            ColumnData::Date(v) => Value::Date(v[pos]),
            ColumnData::Timestamp(v) => {
                if self.logical_type == LogicalType::TimestampTz {
                    Value::TimestampTz(v[pos])
                } else {
                    Value::Timestamp(v[pos])
                }
            }
            ColumnData::Interval(v) => Value::Interval(v[pos]),
            ColumnData::Uuid(v) => Value::Uuid(v[pos]),
            ColumnData::Decimal(v) => {
                let (precision, scale) = match self.logical_type {
                    LogicalType::Decimal(precision, scale) => (precision, scale),
                    _ => (38, 0),
                };
                Value::Decimal {
                    value: v[pos],
                    precision,
                    scale,
                }
            }
            ColumnData::Str(v) => Value::String(v[pos].clone()),
            ColumnData::InternalId(v) => Value::InternalId(v[pos]),
            ColumnData::Generic(v) => v[pos].clone(),
        }
    }

    /// Write an owned [`Value`] at physical position `pos`.
    ///
    /// `Value::Null` sets the null bit. A type mismatch is an internal invariant
    /// violation (the evaluator/scan is responsible for producing the right
    /// type) and panics with a descriptive message.
    pub fn set_value(&mut self, pos: usize, value: &Value) {
        if value.is_null() {
            self.nulls.set_null(pos, true);
            return;
        }
        self.nulls.set_null(pos, false);
        match (&mut self.data, value) {
            (ColumnData::Bool(v), Value::Bool(b)) => v[pos] = *b,
            (ColumnData::Int64(v), Value::Int64(n)) => v[pos] = *n,
            (
                ColumnData::Int64(v),
                Value::IntX {
                    value,
                    kind: IntKind::I64,
                },
            ) => v[pos] = *value as i64,
            (ColumnData::Int128(v), Value::IntX { value, .. }) => v[pos] = *value,
            (ColumnData::Int128(v), Value::Int64(value)) => v[pos] = *value as i128,
            (ColumnData::UInt128(v), Value::UInt128(value)) => v[pos] = *value,
            (ColumnData::Double(v), Value::Double(x)) => v[pos] = *x,
            (ColumnData::Float(v), Value::Float(x)) => v[pos] = *x,
            (ColumnData::Date(v), Value::Date(value)) => v[pos] = *value,
            (ColumnData::Timestamp(v), Value::Timestamp(value) | Value::TimestampTz(value)) => {
                v[pos] = *value;
            }
            (ColumnData::Interval(v), Value::Interval(value)) => v[pos] = *value,
            (ColumnData::Uuid(v), Value::Uuid(value)) => v[pos] = *value,
            (ColumnData::Decimal(v), Value::Decimal { value, .. }) => v[pos] = *value,
            // Numeric widening so an INT64 result can land in a DOUBLE column.
            (ColumnData::Double(v), Value::Int64(n)) => v[pos] = *n as f64,
            (ColumnData::Str(v), Value::String(s)) => v[pos] = s.clone(),
            (ColumnData::InternalId(v), Value::InternalId(id)) => v[pos] = *id,
            (ColumnData::Generic(v), val) => v[pos] = val.clone(),
            (data, value) => panic!(
                "ValueVector::set_value type mismatch: column is {:?}, value is {value:?}",
                std::mem::discriminant(data)
            ),
        }
    }

    /// Write an owned value, moving variable-width payloads into the vector.
    pub fn set_value_owned(&mut self, pos: usize, value: Value) {
        if value.is_null() {
            self.nulls.set_null(pos, true);
            return;
        }
        self.nulls.set_null(pos, false);
        match (&mut self.data, value) {
            (ColumnData::Bool(v), Value::Bool(value)) => v[pos] = value,
            (ColumnData::Int64(v), Value::Int64(value)) => v[pos] = value,
            (
                ColumnData::Int64(v),
                Value::IntX {
                    value,
                    kind: IntKind::I64,
                },
            ) => v[pos] = value as i64,
            (ColumnData::Int128(v), Value::IntX { value, .. }) => v[pos] = value,
            (ColumnData::Int128(v), Value::Int64(value)) => v[pos] = value as i128,
            (ColumnData::UInt128(v), Value::UInt128(value)) => v[pos] = value,
            (ColumnData::Double(v), Value::Double(value)) => v[pos] = value,
            (ColumnData::Double(v), Value::Int64(value)) => v[pos] = value as f64,
            (ColumnData::Float(v), Value::Float(value)) => v[pos] = value,
            (ColumnData::Date(v), Value::Date(value)) => v[pos] = value,
            (ColumnData::Timestamp(v), Value::Timestamp(value) | Value::TimestampTz(value)) => {
                v[pos] = value
            }
            (ColumnData::Interval(v), Value::Interval(value)) => v[pos] = value,
            (ColumnData::Uuid(v), Value::Uuid(value)) => v[pos] = value,
            (ColumnData::Decimal(v), Value::Decimal { value, .. }) => v[pos] = value,
            (ColumnData::Str(v), Value::String(value)) => v[pos] = value,
            (ColumnData::InternalId(v), Value::InternalId(value)) => v[pos] = value,
            (ColumnData::Generic(v), value) => v[pos] = value,
            (data, value) => panic!(
                "ValueVector::set_value_owned type mismatch: column is {:?}, value is {value:?}",
                std::mem::discriminant(data)
            ),
        }
    }

    /// Copy one physical value between vectors of the same physical type without
    /// materializing an intermediate [`Value`]. This is the hot path for
    /// columnar operators carrying existing columns into a fresh output chunk.
    pub fn copy_value_from(&mut self, dst: usize, source: &Self, src: usize) {
        if source.nulls.is_null(src) {
            self.nulls.set_null(dst, true);
            match &mut self.data {
                ColumnData::Str(values) => values[dst].clear(),
                ColumnData::Generic(values) => values[dst] = Value::Null,
                _ => {}
            }
            return;
        }
        self.nulls.set_null(dst, false);
        match (&mut self.data, &source.data) {
            (ColumnData::Bool(dst_values), ColumnData::Bool(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Int64(dst_values), ColumnData::Int64(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Int128(dst_values), ColumnData::Int128(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::UInt128(dst_values), ColumnData::UInt128(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Double(dst_values), ColumnData::Double(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Float(dst_values), ColumnData::Float(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Date(dst_values), ColumnData::Date(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Timestamp(dst_values), ColumnData::Timestamp(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Interval(dst_values), ColumnData::Interval(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Uuid(dst_values), ColumnData::Uuid(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Decimal(dst_values), ColumnData::Decimal(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Str(dst_values), ColumnData::Str(src_values)) => {
                dst_values[dst].clone_from(&src_values[src])
            }
            (ColumnData::InternalId(dst_values), ColumnData::InternalId(src_values)) => {
                dst_values[dst] = src_values[src]
            }
            (ColumnData::Generic(dst_values), ColumnData::Generic(src_values)) => {
                dst_values[dst].clone_from(&src_values[src])
            }
            (dst_data, src_data) => panic!(
                "ValueVector::copy_value_from type mismatch: destination is {:?}, source is {:?}",
                std::mem::discriminant(dst_data),
                std::mem::discriminant(src_data)
            ),
        }
    }

    #[inline]
    pub fn set_internal_id(&mut self, pos: usize, id: InternalId) {
        if let ColumnData::InternalId(v) = &mut self.data {
            self.nulls.set_null(pos, false);
            v[pos] = id;
        } else {
            panic!("set_internal_id on non-internal-id column");
        }
    }

    /// Move an owned variable-width value out of `pos`, resetting the slot.
    pub fn take_value(&mut self, pos: usize) -> Value {
        if self.nulls.is_null(pos) {
            self.nulls.set_null(pos, false);
            return Value::Null;
        }
        match &mut self.data {
            ColumnData::Str(values) => Value::String(std::mem::take(&mut values[pos])),
            ColumnData::Generic(values) => std::mem::replace(&mut values[pos], Value::Null),
            _ => {
                let value = self.get_value(pos);
                self.nulls.set_null(pos, true);
                value
            }
        }
    }
}

/// The STATIC (identity) vs DYNAMIC (filtered) selection distinction, encoded
/// as a checked Rust enum rather than a pointer that may alias a global array.
#[derive(Debug, Clone)]
pub enum Selection {
    /// Identity selection `0..len` — the zero-allocation fast path.
    Flat { len: usize },
    /// Explicit selected physical positions after a filter.
    Filtered(Vec<usize>),
}

impl Selection {
    pub fn full(len: usize) -> Self {
        Selection::Flat { len }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Selection::Flat { len } => *len,
            Selection::Filtered(v) => v.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate the selected physical positions in order.
    pub fn iter(&self) -> SelectionIter<'_> {
        match self {
            Selection::Flat { len } => SelectionIter::Range(0..*len),
            Selection::Filtered(v) => SelectionIter::Slice(v.iter()),
        }
    }
}

/// Iterator over selected positions (avoids boxing the two cases).
pub enum SelectionIter<'a> {
    Range(std::ops::Range<usize>),
    Slice(std::slice::Iter<'a, usize>),
}

impl Iterator for SelectionIter<'_> {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        match self {
            SelectionIter::Range(r) => r.next(),
            SelectionIter::Slice(it) => it.next().copied(),
        }
    }
}

/// A batch of columns sharing one selection (and therefore one logical size).
///
/// # Factorization (multiplicity)
///
/// `mult` carries the **factorization multiplicity** (P3 step 6): each row may stand
/// for more than one logical tuple, when a *collapsible* pattern suffix — one whose
/// variables are never read, only counted — has been folded into a count instead of
/// materialized as a cross-product. `None` means every row's multiplicity is `1` (the
/// universal fast path: scans, extends-with-fan-out, unwind, …). It is indexed by
/// *physical* position, so [`Selection`] narrowing (filters) preserves it for free;
/// only the factorizing extend produces it, and only `aggregate` consumes it (it is
/// always back to `1` by the time a projection / `WITH` carry runs).
#[derive(Debug, Clone)]
pub struct DataChunk {
    pub columns: Vec<ValueVector>,
    pub sel: Selection,
    /// Per-physical-position factorization multiplicity; `None` ⇒ all `1`.
    pub mult: Option<Box<[u64]>>,
}

impl DataChunk {
    /// Allocate a chunk with one column per supplied logical type, empty.
    pub fn new(types: &[LogicalType]) -> Self {
        Self {
            columns: types.iter().cloned().map(ValueVector::new_null).collect(),
            sel: Selection::Flat { len: 0 },
            mult: None,
        }
    }

    /// Number of live logical rows.
    #[inline]
    pub fn size(&self) -> usize {
        self.sel.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sel.is_empty()
    }

    /// Set the chunk to a dense `0..len` selection (used after a fresh fill).
    pub fn set_flat(&mut self, len: usize) {
        self.sel = Selection::Flat { len };
    }

    /// The factorization multiplicity of the row at physical position `pos` — how
    /// many logical tuples it stands for (`1` unless a collapsible suffix was folded
    /// into it). See the struct-level note on `mult`.
    #[inline]
    pub fn multiplicity(&self, pos: usize) -> u64 {
        match &self.mult {
            None => 1,
            Some(m) => m.get(pos).copied().unwrap_or(1),
        }
    }

    /// Heap bytes owned by this chunk's vector backings, selections, multiplicities,
    /// and variable-width cell payloads.
    pub fn allocated_bytes(&self) -> u64 {
        let column_backings = self
            .columns
            .iter()
            .map(|column| {
                ColumnData::allocation_bytes(column.logical_type.physical_type())
                    + column.data.payload_bytes()
            })
            .sum::<u64>();
        let column_headers = (self.columns.capacity() * std::mem::size_of::<ValueVector>()) as u64;
        let selection = match &self.sel {
            Selection::Flat { .. } => 0,
            Selection::Filtered(positions) => {
                (positions.capacity() * std::mem::size_of::<usize>()) as u64
            }
        };
        let multiplicities = self
            .mult
            .as_ref()
            .map_or(0, |values| std::mem::size_of_val(values.as_ref()) as u64);
        column_headers + column_backings + selection + multiplicities
    }
}

pub fn value_payload_bytes(value: &Value) -> u64 {
    let value_vec = |values: &Vec<Value>| {
        (values.capacity() * std::mem::size_of::<Value>()) as u64
            + values.iter().map(value_payload_bytes).sum::<u64>()
    };
    let property_vec = |properties: &Vec<(String, Value)>| {
        (properties.capacity() * std::mem::size_of::<(String, Value)>()) as u64
            + properties
                .iter()
                .map(|(name, value)| name.capacity() as u64 + value_payload_bytes(value))
                .sum::<u64>()
    };
    match value {
        Value::String(value) => value.capacity() as u64,
        Value::Blob(value) => value.capacity() as u64,
        Value::List(values) => value_vec(values),
        Value::Struct(properties) => property_vec(properties),
        Value::Map(entries) => {
            (entries.capacity() * std::mem::size_of::<(Value, Value)>()) as u64
                + entries
                    .iter()
                    .map(|(key, value)| value_payload_bytes(key) + value_payload_bytes(value))
                    .sum::<u64>()
        }
        Value::Node(node) => {
            std::mem::size_of_val(node.as_ref()) as u64
                + node.label.capacity() as u64
                + property_vec(&node.props)
        }
        Value::Rel(rel) => {
            let endpoint = |node: &Option<Box<crate::value::NodeValue>>| {
                node.as_ref().map_or(0, |node| {
                    std::mem::size_of_val(node.as_ref()) as u64
                        + node.label.capacity() as u64
                        + property_vec(&node.props)
                })
            };
            std::mem::size_of_val(rel.as_ref()) as u64
                + rel.label.capacity() as u64
                + property_vec(&rel.props)
                + endpoint(&rel.src_node)
                + endpoint(&rel.dst_node)
        }
        Value::RecursiveRel(path) => {
            std::mem::size_of_val(path.as_ref()) as u64
                + (path.nodes.capacity() * std::mem::size_of::<crate::value::NodeValue>()) as u64
                + path
                    .nodes
                    .iter()
                    .map(|node| node.label.capacity() as u64 + property_vec(&node.props))
                    .sum::<u64>()
                + (path.rels.capacity() * std::mem::size_of::<crate::value::RelValue>()) as u64
                + path
                    .rels
                    .iter()
                    .map(|rel| rel.label.capacity() as u64 + property_vec(&rel.props))
                    .sum::<u64>()
        }
        Value::Union {
            variants, value, ..
        } => {
            std::mem::size_of::<Value>() as u64
                + (variants.capacity() * std::mem::size_of::<(String, LogicalType)>()) as u64
                + variants
                    .iter()
                    .map(|(name, _)| name.capacity() as u64)
                    .sum::<u64>()
                + value_payload_bytes(value)
        }
        _ => 0,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_mask_roundtrip() {
        let mut m = NullMask::new();
        assert!(!m.is_null(5));
        m.set_null(5, true);
        m.set_null(100, true);
        assert!(m.is_null(5));
        assert!(m.is_null(100));
        assert!(!m.is_null(6));
        m.set_null(5, false);
        assert!(!m.is_null(5));
    }

    #[test]
    fn vector_get_set_and_null() {
        let mut v = ValueVector::new(LogicalType::Int64);
        v.set_value(0, &Value::Int64(42));
        v.set_value(1, &Value::Null);
        assert_eq!(v.get_value(0), Value::Int64(42));
        assert_eq!(v.get_value(1), Value::Null);
    }

    #[test]
    fn chunk_cells_start_null_until_populated() {
        let mut chunk = DataChunk::new(&[LogicalType::Int64, LogicalType::String]);
        chunk.set_flat(1);
        assert_eq!(chunk.columns[0].get_value(0), Value::Null);
        assert_eq!(chunk.columns[1].get_value(0), Value::Null);
        chunk.columns[0].set_value(0, &Value::Int64(7));
        assert_eq!(chunk.columns[0].get_value(0), Value::Int64(7));
    }

    #[test]
    fn selection_iter() {
        let s = Selection::Flat { len: 3 };
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![0, 1, 2]);
        let s = Selection::Filtered(vec![1, 4, 9]);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![1, 4, 9]);
    }
}
