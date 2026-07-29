//! Identity newtypes, Koko's logical type system, and logical-to-physical mappings.
//!
//! Logical types cover scalar, decimal, temporal, nested, graph-entity, path,
//! polymorphic, and internal values. Catalog and execution layers share these
//! definitions without depending on one another.

use crate::error::{Error, Result};
use std::fmt;

/// A catalog object id (table id). Monotonically allocated by the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableId(pub u64);

/// A dense row position within a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Offset(pub u64);

/// Stable per-table column identifier (assigned at table creation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ColumnId(pub u32);

/// A rel table's multiplicity constraint (`CREATE REL TABLE … (FROM a TO b, X_Y)`).
/// In Kùzu's `<src>_<dst>` keyword the **second** token bounds each *source*
/// node's outgoing edges and the **first** bounds each *destination* node's
/// incoming edges — so `MANY_ONE` means each src has ≤1 outgoing. The default,
/// `MANY_MANY`, is unconstrained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RelMultiplicity {
    /// At most one outgoing edge of this table per source node (the `_ONE` dst
    /// token). A second one errors in the "fwd" direction on the source node.
    pub src_single: bool,
    /// At most one incoming edge per destination node (the `ONE_` src token). A
    /// second one errors in the "bwd" direction on the destination node.
    pub dst_single: bool,
}

impl RelMultiplicity {
    /// Parse a `<src>_<dst>` keyword (e.g. `MANY_ONE`); unknown/`MANY_MANY` →
    /// unconstrained.
    pub fn from_keyword(kw: &str) -> RelMultiplicity {
        match kw.split_once('_') {
            Some((src, dst)) => RelMultiplicity {
                src_single: dst.eq_ignore_ascii_case("ONE"),
                dst_single: src.eq_ignore_ascii_case("ONE"),
            },
            None => RelMultiplicity::default(),
        }
    }
}

/// System-level identity of a node or relationship: `(table_id, offset)`.
///
/// Rendered as `tableID:offset` in query results (note the order — table id
/// first), matching the C++ `internalID_t` formatting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InternalId {
    pub table_id: TableId,
    pub offset: Offset,
}

impl InternalId {
    pub fn new(table_id: TableId, offset: u64) -> Self {
        Self {
            table_id,
            offset: Offset(offset),
        }
    }
}

impl fmt::Display for InternalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.table_id.0, self.offset.0)
    }
}

/// Direction in which to follow a relationship from a given node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtendDir {
    /// Follow outgoing edges: given the FROM node, yield TO neighbors.
    Forward,
    /// Follow incoming edges: given the TO node, yield FROM neighbors.
    Backward,
    /// Both directions (undirected pattern).
    Both,
}

/// The physical storage representation of a logical type — what actually sits
/// in a [`crate::vector::ValueVector`]'s column buffer.
///
/// In the pipeline a node/rel variable is carried as its `INTERNAL_ID`; the
/// full node/rel *value* is assembled only at result materialization, so
/// `NODE`/`REL` logical types map to the `InternalId` physical type here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalType {
    Bool,
    Int64,
    Int128,
    UInt128,
    Double,
    Float,
    Date,
    Timestamp,
    Interval,
    Uuid,
    Decimal,
    String,
    InternalId,
    /// Backs variable-width and nested values without a specialized buffer.
    Generic,
    /// Unresolved (binding incomplete) — an error to materialize.
    Any,
}

/// The signed/unsigned integer widths whose values fit in an `i128` backing.
/// `UINT128`, whose range exceeds `i128`, is handled separately by
/// [`LogicalType::UInt128`] / [`crate::value::Value::UInt128`]. `INT64` is the
/// canonical width used for bare integer literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntKind {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
}

impl IntKind {
    pub fn name(self) -> &'static str {
        match self {
            IntKind::I8 => "INT8",
            IntKind::I16 => "INT16",
            IntKind::I32 => "INT32",
            IntKind::I64 => "INT64",
            IntKind::I128 => "INT128",
            IntKind::U8 => "UINT8",
            IntKind::U16 => "UINT16",
            IntKind::U32 => "UINT32",
            IntKind::U64 => "UINT64",
        }
    }
    pub fn byte_width(self) -> u8 {
        match self {
            IntKind::I8 | IntKind::U8 => 1,
            IntKind::I16 | IntKind::U16 => 2,
            IntKind::I32 | IntKind::U32 => 4,
            IntKind::I64 | IntKind::U64 => 8,
            IntKind::I128 => 16,
        }
    }
    pub fn is_signed(self) -> bool {
        matches!(
            self,
            IntKind::I8 | IntKind::I16 | IntKind::I32 | IntKind::I64 | IntKind::I128
        )
    }
    pub fn min(self) -> i128 {
        match self {
            IntKind::I8 => i8::MIN as i128,
            IntKind::I16 => i16::MIN as i128,
            IntKind::I32 => i32::MIN as i128,
            IntKind::I64 => i64::MIN as i128,
            IntKind::I128 => i128::MIN,
            IntKind::U8 | IntKind::U16 | IntKind::U32 | IntKind::U64 => 0,
        }
    }
    pub fn max(self) -> i128 {
        match self {
            IntKind::I8 => i8::MAX as i128,
            IntKind::I16 => i16::MAX as i128,
            IntKind::I32 => i32::MAX as i128,
            IntKind::I64 => i64::MAX as i128,
            IntKind::I128 => i128::MAX,
            IntKind::U8 => u8::MAX as i128,
            IntKind::U16 => u16::MAX as i128,
            IntKind::U32 => u32::MAX as i128,
            IntKind::U64 => u64::MAX as i128,
        }
    }
    pub fn contains(self, v: i128) -> bool {
        v >= self.min() && v <= self.max()
    }
    /// The result integer width of a binary arithmetic op (widest wins; on a
    /// width tie a signed operand wins).
    pub fn combine(self, other: IntKind) -> IntKind {
        use std::cmp::Ordering;
        // Mixed sign promotes to the smallest SIGNED width that holds both
        // (audit V14, oracle-verified: UINT8+INT8 → INT16, UINT16+INT8 → INT32,
        // UINT32+INT16 → INT64, UINT64+INT32 → INT128).
        if self.is_signed() != other.is_signed() {
            let (signed, unsigned) = if self.is_signed() {
                (self, other)
            } else {
                (other, self)
            };
            if signed.byte_width() > unsigned.byte_width() {
                return signed;
            }
            return match unsigned.byte_width() {
                1 => IntKind::I16,
                2 => IntKind::I32,
                4 => IntKind::I64,
                _ => IntKind::I128,
            };
        }
        match self.byte_width().cmp(&other.byte_width()) {
            Ordering::Greater => self,
            Ordering::Less => other,
            Ordering::Equal => self,
        }
    }
    pub fn to_logical(self) -> LogicalType {
        LogicalType::Int(self)
    }
}

/// A logical (user-facing) type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalType {
    Bool,
    /// An integer of a specific width (`Int(I64)` is the default integer).
    Int(IntKind),
    /// `SERIAL` — auto-incrementing INT64 surface type; physically stored as INT64.
    Serial,
    /// `UINT128` — separate from [`IntKind`] because its range exceeds `i128`.
    UInt128,
    /// `DECIMAL(precision, scale)` — fixed-point, backed by an `i128`.
    Decimal(u8, u8),
    Double,
    Float,
    String,
    /// Ordered UTF-8 JSON used by schemaless graph properties.
    Json,
    Date,
    /// `TIMESTAMP` — microseconds since the Unix epoch.
    Timestamp,
    /// `TIMESTAMP_NS` — nanosecond surface type, stored at microsecond precision.
    TimestampNs,
    /// `TIMESTAMP_MS` — millisecond resolution (stored truncated to ms).
    TimestampMs,
    /// `TIMESTAMP_SEC` / `TIMESTAMP_S` — second resolution (stored truncated).
    TimestampSec,
    /// `TIMESTAMP_TZ` — like `TIMESTAMP` but rendered with a `+00` UTC suffix.
    TimestampTz,
    Interval,
    Uuid,
    Blob,
    /// A variable-length list `T[]` with an element type.
    List(Box<LogicalType>),
    /// A fixed-length array `T[N]` — distinct from [`LogicalType::List`]. Carries the
    /// element type and the declared length `N`. Stored generically like a list (values
    /// are `Value::List`); the length is enforced at bind time and it renders as `T[N]`.
    Array(Box<LogicalType>, u64),
    /// A struct: ordered `(field, type)` pairs.
    Struct(Vec<(String, LogicalType)>),
    /// A map with key and value types.
    Map(Box<LogicalType>, Box<LogicalType>),
    /// A union: ordered `(tag, type)` alternatives.
    Union(Vec<(String, LogicalType)>),
    InternalId,
    /// A node of a specific table. Carries its id in the pipeline.
    Node(TableId),
    /// A relationship of a specific table. Carries its id in the pipeline.
    Rel(TableId),
    /// A recursive relationship / path value: a `{_NODES, _RELS}` aggregate
    /// produced by a variable-length pattern or a named path. Stored generically.
    RecursiveRel,
    /// Unresolved type (used transiently during binding).
    Any,
}

#[allow(non_upper_case_globals)]
impl LogicalType {
    /// The canonical 64-bit signed integer type (the common case).
    pub const Int64: LogicalType = LogicalType::Int(IntKind::I64);
}

impl LogicalType {
    /// The physical representation backing this logical type.
    pub fn physical_type(&self) -> PhysicalType {
        match self {
            LogicalType::Bool => PhysicalType::Bool,
            LogicalType::Int(IntKind::I64) | LogicalType::Serial => PhysicalType::Int64,
            LogicalType::Int(_) => PhysicalType::Int128,
            LogicalType::UInt128 => PhysicalType::UInt128,
            LogicalType::Decimal(_, _) => PhysicalType::Decimal,
            LogicalType::Double => PhysicalType::Double,
            LogicalType::Float => PhysicalType::Float,
            LogicalType::String => PhysicalType::String,
            LogicalType::Json => PhysicalType::Generic,
            LogicalType::Date => PhysicalType::Date,
            LogicalType::Timestamp
            | LogicalType::TimestampNs
            | LogicalType::TimestampMs
            | LogicalType::TimestampSec
            | LogicalType::TimestampTz => PhysicalType::Timestamp,
            LogicalType::Interval => PhysicalType::Interval,
            LogicalType::Uuid => PhysicalType::Uuid,
            LogicalType::Blob
            | LogicalType::List(_)
            | LogicalType::Array(_, _)
            | LogicalType::Struct(_)
            | LogicalType::Map(_, _)
            | LogicalType::Union(_) => PhysicalType::Generic,
            LogicalType::InternalId => PhysicalType::InternalId,
            // A whole NODE/REL *value* (e.g. a relationship carried through `WITH`,
            // or an UNWIND of `collect(node)`) is stored generically; node/rel
            // *id* columns use `LogicalType::InternalId` directly.
            LogicalType::Node(_) | LogicalType::Rel(_) => PhysicalType::Generic,
            // A recursive-rel / path value is an assembled `{_NODES, _RELS}`
            // aggregate carried in a generic column.
            LogicalType::RecursiveRel => PhysicalType::Generic,
            LogicalType::Any => PhysicalType::Any,
        }
    }

    /// The element type of a `LIST` or fixed-size `ARRAY` (both are list-like), else
    /// `None`. Use this wherever a list operation reads its argument's element type, so
    /// `ARRAY` is handled like `LIST`.
    pub fn list_child(&self) -> Option<&LogicalType> {
        match self {
            LogicalType::List(inner) | LogicalType::Array(inner, _) => Some(inner),
            _ => None,
        }
    }

    /// Whether this is a numeric type (participates in arithmetic / numeric coercion).
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            LogicalType::Int(_)
                | LogicalType::Serial
                | LogicalType::UInt128
                | LogicalType::Decimal(_, _)
                | LogicalType::Double
                | LogicalType::Float
        )
    }

    /// The integer width, if this is an integer type.
    pub fn int_kind(&self) -> Option<IntKind> {
        match self {
            LogicalType::Int(k) => Some(*k),
            LogicalType::Serial => Some(IntKind::I64),
            _ => None,
        }
    }

    /// Whether this is a nested (container) type in the C++ `isNested` sense:
    /// list/array/struct/map/union plus the graph values (all struct-backed).
    pub fn is_nested(&self) -> bool {
        matches!(
            self,
            LogicalType::List(_)
                | LogicalType::Array(_, _)
                | LogicalType::Struct(_)
                | LogicalType::Map(_, _)
                | LogicalType::Union(_)
                | LogicalType::Node(_)
                | LogicalType::Rel(_)
                | LogicalType::RecursiveRel
        )
    }

    /// Parse a DDL (table column) type string. Deliberately limited to the
    /// scalar types the chunk/storage layer can carry today; narrower integer
    /// widths and richer types are deferred (clear "not implemented").
    pub fn from_ddl_str(s: &str) -> Result<LogicalType> {
        parse_type_str(s)
    }

    /// Like [`from_ddl_str`](Self::from_ddl_str), but `resolve_alias` resolves any
    /// type name that isn't a built-in. This threads the catalog's user-defined
    /// `TYPE` registry into *nested* type positions — a UDT alias inside a
    /// `STRUCT`/`LIST`/`MAP`/`ARRAY` declaration — which the binder can't
    /// pre-resolve because it only sees (and can only alias-check) the outer
    /// type string, not the names buried inside it.
    pub fn from_ddl_str_with(
        s: &str,
        resolve_alias: &dyn Fn(&str) -> Option<LogicalType>,
    ) -> Result<LogicalType> {
        parse_type_str_with(s, resolve_alias)
    }

    /// Parse a `CAST` target type string (same grammar as DDL types).
    pub fn from_cast_str(s: &str) -> Result<LogicalType> {
        parse_type_str(s)
    }

    /// Like [`from_cast_str`](Self::from_cast_str), with a user-`TYPE` alias resolver.
    pub fn from_cast_str_with(
        s: &str,
        resolve_alias: &dyn Fn(&str) -> Option<LogicalType>,
    ) -> Result<LogicalType> {
        parse_type_str_with(s, resolve_alias)
    }

    /// The canonical display name of this type (used in diagnostics; complex
    /// types render recursively, e.g. `INT64[]`, `STRUCT(a INT64)`).
    pub fn name(&self) -> String {
        match self {
            LogicalType::Bool => "BOOL".to_string(),
            LogicalType::Int(k) => k.name().to_string(),
            LogicalType::Serial => "SERIAL".to_string(),
            LogicalType::Double => "DOUBLE".to_string(),
            LogicalType::Float => "FLOAT".to_string(),
            LogicalType::UInt128 => "UINT128".to_string(),
            LogicalType::Decimal(p, s) => format!("DECIMAL({p}, {s})"),
            LogicalType::String => "STRING".to_string(),
            LogicalType::Json => "JSON".to_string(),
            LogicalType::Date => "DATE".to_string(),
            LogicalType::Timestamp => "TIMESTAMP".to_string(),
            LogicalType::TimestampNs => "TIMESTAMP_NS".to_string(),
            LogicalType::TimestampMs => "TIMESTAMP_MS".to_string(),
            LogicalType::TimestampSec => "TIMESTAMP_SEC".to_string(),
            LogicalType::TimestampTz => "TIMESTAMP_TZ".to_string(),
            LogicalType::Interval => "INTERVAL".to_string(),
            LogicalType::Uuid => "UUID".to_string(),
            LogicalType::Blob => "BLOB".to_string(),
            LogicalType::List(inner) => format!("{}[]", inner.name()),
            LogicalType::Array(inner, n) => format!("{}[{}]", inner.name(), n),
            LogicalType::Struct(fields) => {
                let inner: Vec<String> = fields
                    .iter()
                    .map(|(n, t)| format!("{n} {}", t.name()))
                    .collect();
                format!("STRUCT({})", inner.join(", "))
            }
            LogicalType::Map(k, v) => format!("MAP({}, {})", k.name(), v.name()),
            LogicalType::Union(variants) => {
                let inner: Vec<String> = variants
                    .iter()
                    .map(|(n, t)| format!("{n} {}", t.name()))
                    .collect();
                format!("UNION({})", inner.join(", "))
            }
            LogicalType::InternalId => "INTERNAL_ID".to_string(),
            LogicalType::Node(_) => "NODE".to_string(),
            LogicalType::Rel(_) => "REL".to_string(),
            LogicalType::RecursiveRel => "RECURSIVE_REL".to_string(),
            LogicalType::Any => "ANY".to_string(),
        }
    }
}

/// The established common type for numeric operands. `ANY` operands are ignored
/// so NULL literals and unconstrained parameters adopt a concrete numeric peer.
/// Returns `None` when any concrete operand is not numeric.
pub fn common_numeric_type<'a>(
    types: impl IntoIterator<Item = &'a LogicalType>,
) -> Option<LogicalType> {
    let mut common = LogicalType::Any;
    for logical_type in types {
        if *logical_type == LogicalType::Any {
            continue;
        }
        if !logical_type.is_numeric() {
            return None;
        }
        common = match (&common, logical_type) {
            (LogicalType::Any, _) => logical_type.clone(),
            (left, right) if left == right => common,
            (LogicalType::Decimal(p1, s1), LogicalType::Decimal(p2, s2)) => {
                let scale = (*s1).max(*s2);
                let integer_digits = (p1 - s1).max(p2 - s2);
                LogicalType::Decimal((integer_digits + scale).min(38), scale)
            }
            (LogicalType::Decimal(precision, scale), other)
            | (other, LogicalType::Decimal(precision, scale)) => {
                if other.int_kind().is_some() || *other == LogicalType::Serial {
                    LogicalType::Decimal((*precision).max(19 + *scale).min(38), *scale)
                } else {
                    LogicalType::Double
                }
            }
            (LogicalType::Double, _) | (_, LogicalType::Double) => LogicalType::Double,
            (LogicalType::Float, _) | (_, LogicalType::Float) => LogicalType::Float,
            // UINT128 is wider than every IntKind. This matches arithmetic
            // promotion; a negative signed value still fails its value cast.
            (LogicalType::UInt128, _) | (_, LogicalType::UInt128) => LogicalType::UInt128,
            (left, right) => match (left.int_kind(), right.int_kind()) {
                (Some(left), Some(right)) => LogicalType::Int(left.combine(right)),
                _ => LogicalType::Int64,
            },
        };
    }
    Some(common)
}

impl fmt::Display for LogicalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

/// Split `s` on `sep` at the top nesting level only — commas/colons inside
/// `[]`, `{}`, `()` or quotes are not split points. Used to parse both type
/// strings (`STRUCT(a INT64, b STRING)`) and value literals (`[1,[2,3]]`).
pub fn split_top_level(s: &str, sep: char) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                b'\'' | b'"' => quote = Some(c),
                b'[' | b'{' | b'(' => depth += 1,
                b']' | b'}' | b')' => depth -= 1,
                _ if c == sep as u8 && depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

/// Given `t` ending with `]`, return the byte index of its matching `[`.
fn matching_open_bracket(t: &str) -> Option<usize> {
    let bytes = t.as_bytes();
    let mut depth = 0i32;
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b']' | b')' => depth += 1,
            b'(' => depth -= 1,
            b'[' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a `field TYPE` (or `tag TYPE`) declaration on the first top-level space.
/// With no space (e.g. `UNION(uint64, int128)`), the whole token is both the
/// field name and the type — matching C++ `parseStructTypeInfo`.
fn split_decl(s: &str) -> Option<(String, &str)> {
    // Trim only the LEADING padding: `a: ` splits into name `a:` and the
    // EMPTY type, which resolves as the type named "" (C++ catalog error).
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(idx) => {
            let name = s[..idx]
                .trim()
                .trim_matches(['\'', '"'])
                .trim_end_matches(':')
                .to_string();
            Some((name, s[idx..].trim()))
        }
        None => Some((s.trim_matches(['\'', '"']).to_string(), s)),
    }
}

/// Parse a DDL/CAST type string into a [`LogicalType`]: scalars, the `[]`/`[N]`
/// list suffix, and `STRUCT(...)`, `MAP(...)`, `UNION(...)` (recursively).
pub fn parse_type_str(s: &str) -> Result<LogicalType> {
    parse_type_str_with(s, &|_| None)
}

/// Like [`parse_type_str`], resolving any non-built-in leaf type name through
/// `resolve_alias` (the user-defined-`TYPE` registry) before erroring — so a UDT
/// alias works in a nested type position, not just at the top level.
pub fn parse_type_str_with(
    s: &str,
    resolve_alias: &dyn Fn(&str) -> Option<LogicalType>,
) -> Result<LogicalType> {
    let t = s.trim();

    // List/array suffix: `T[]` is a variable-length list; `T[N]` is a fixed-length
    // array that preserves the declared length.
    if t.ends_with(']') {
        if let Some(open) = matching_open_bracket(t) {
            let inner = Box::new(parse_type_str_with(&t[..open], resolve_alias)?);
            let between = t[open + 1..t.len() - 1].trim();
            return Ok(if between.is_empty() {
                LogicalType::List(inner)
            } else {
                let n: u64 = between
                    .parse()
                    .map_err(|_| Error::binder(format!("invalid array length in type {t}")))?;
                if n == 0 {
                    return Err(Error::binder(format!(
                        "The number of elements in an array must be greater than 0. Given: {n}."
                    )));
                }
                LogicalType::Array(inner, n)
            });
        }
    }

    let up = t.to_ascii_uppercase();

    // Parameterized complex types.
    if t.ends_with(')') {
        if let Some(inner) = strip_ctor(&up, t, "STRUCT") {
            let fields = parse_decls(inner, resolve_alias, "STRUCT")?;
            return Ok(LogicalType::Struct(fields));
        }
        if let Some(inner) = strip_ctor(&up, t, "UNION") {
            let variants = parse_decls(inner, resolve_alias, "UNION")?;
            if variants.len() > 65_536 {
                return Err(Error::binder(format!(
                    "Too many fields in UNION definition (max 65536, got {})",
                    variants.len()
                )));
            }
            return Ok(LogicalType::Union(variants));
        }
        if let Some(inner) = strip_ctor(&up, t, "MAP") {
            // C++ splits at the FIRST top-level comma: everything after is the
            // value-type string (`MAP(INT, STRING, INT)` resolves the "type"
            // `STRING, INT`, and a missing part resolves the type named "").
            let parts = split_top_level(inner, ',');
            let key = parts[0];
            let value = if parts.len() > 1 {
                &inner[key.len() + 1..]
            } else {
                ""
            };
            return Ok(LogicalType::Map(
                Box::new(parse_type_str_with(key, resolve_alias)?),
                Box::new(parse_type_str_with(value, resolve_alias)?),
            ));
        }
        if let Some(inner) = strip_ctor(&up, t, "DECIMAL") {
            let parts = split_top_level(inner, ',');
            if parts.len() != 2 {
                return Err(Error::binder(
                    "DECIMAL expects (precision, scale)".to_string(),
                ));
            }
            let precision: u8 = parts[0]
                .trim()
                .parse()
                .map_err(|_| Error::binder("invalid DECIMAL precision".to_string()))?;
            let scale: u8 = parts[1]
                .trim()
                .parse()
                .map_err(|_| Error::binder("invalid DECIMAL scale".to_string()))?;
            if precision == 0 || precision > crate::decimal::MAX_PRECISION || scale > precision {
                return Err(Error::binder(format!(
                    "DECIMAL({precision}, {scale}) is not a valid type"
                )));
            }
            return Ok(LogicalType::Decimal(precision, scale));
        }
        // Other parameterized scalars fall through to the scalar matcher below.
    }

    match up.as_str() {
        "BOOL" | "BOOLEAN" => Ok(LogicalType::Bool),
        "DECIMAL" | "NUMERIC" => Ok(LogicalType::Decimal(
            crate::decimal::DEFAULT_PRECISION,
            crate::decimal::DEFAULT_SCALE,
        )),
        "INT8" => Ok(LogicalType::Int(IntKind::I8)),
        "INT16" => Ok(LogicalType::Int(IntKind::I16)),
        "INT32" | "INT" => Ok(LogicalType::Int(IntKind::I32)),
        "INT64" | "INTEGER" => Ok(LogicalType::Int64),
        "SERIAL" => Ok(LogicalType::Serial),
        "INT128" => Ok(LogicalType::Int(IntKind::I128)),
        "UINT8" => Ok(LogicalType::Int(IntKind::U8)),
        "UINT16" => Ok(LogicalType::Int(IntKind::U16)),
        "UINT32" => Ok(LogicalType::Int(IntKind::U32)),
        "UINT64" => Ok(LogicalType::Int(IntKind::U64)),
        "UINT128" => Ok(LogicalType::UInt128),
        // REAL/FLOAT4 alias FLOAT, FLOAT8 aliases DOUBLE in C++ (audit R3) —
        // visible in table_info/rendering.
        "DOUBLE" | "FLOAT8" => Ok(LogicalType::Double),
        "REAL" | "FLOAT4" => Ok(LogicalType::Float),
        "FLOAT" => Ok(LogicalType::Float),
        "STRING" | "TEXT" => Ok(LogicalType::String),
        "JSON" => Ok(LogicalType::Json),
        "DATE" => Ok(LogicalType::Date),
        "TIMESTAMP" => Ok(LogicalType::Timestamp),
        "TIMESTAMP_NS" => Ok(LogicalType::TimestampNs),
        "TIMESTAMP_MS" => Ok(LogicalType::TimestampMs),
        "TIMESTAMP_SEC" | "TIMESTAMP_S" => Ok(LogicalType::TimestampSec),
        "TIMESTAMP_TZ" => Ok(LogicalType::TimestampTz),
        "INTERVAL" | "DURATION" => Ok(LogicalType::Interval),
        "UUID" => Ok(LogicalType::Uuid),
        "BLOB" | "BYTEA" => Ok(LogicalType::Blob),
        // A legal cast TARGET name (the cast itself then fails: no cast
        // function reaches INTERNAL_ID from any type).
        "INTERNAL_ID" => Ok(LogicalType::InternalId),
        // A user-defined `TYPE` alias, resolved via the catalog at bind time.
        // Consulted only after the built-ins, so an alias never shadows one.
        // A bare `STRUCT` (no field list) is the C++ prefix-less parse failure.
        "STRUCT" => Err(Error::Raw("Cannot parse struct type: STRUCT".to_string())),
        // An unterminated `MAP(` is the analogous raw map-parse failure.
        m if m.starts_with("MAP(") => Err(Error::Raw(format!("Cannot parse map type: {m}"))),
        other => resolve_alias(t).map(Ok).unwrap_or_else(|| {
            Err(Error::catalog(format!(
                "{other} is neither an internal type nor a user defined type."
            )))
        }),
    }
}

/// If `up` (uppercased `t`) is `NAME(...)`, return the inner `...` from `t`.
fn strip_ctor<'a>(up: &str, t: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}(");
    if up.starts_with(&prefix) && t.ends_with(')') {
        Some(&t[prefix.len()..t.len() - 1])
    } else {
        None
    }
}

fn parse_decls(
    inner: &str,
    resolve_alias: &dyn Fn(&str) -> Option<LogicalType>,
    def_type: &str,
) -> Result<Vec<(String, LogicalType)>> {
    let mut fields: Vec<(String, LogicalType)> = Vec::new();
    // `STRUCT()` resolves its empty declaration as the type named "" (the C++
    // catalog error, with the empty name embedded).
    if inner.trim().is_empty() {
        return Err(Error::catalog(format!(
            "{} is neither an internal type nor a user defined type.",
            inner.trim()
        )));
    }
    for decl in split_top_level(inner, ',') {
        if decl.trim().is_empty() {
            continue;
        }
        let (name, ty) = split_decl(decl)
            .ok_or_else(|| Error::binder(format!("malformed field declaration: {decl}")))?;
        // C++ `parseStructTypeInfo` rejects duplicate field names (case-sensitive)
        // in both STRUCT and UNION definitions, before parsing the field's type.
        if fields.iter().any(|(n, _)| n == &name) {
            return Err(Error::binder(format!(
                "Duplicate field '{name}' in {def_type} definition"
            )));
        }
        fields.push((name, parse_type_str_with(ty, resolve_alias)?));
    }
    Ok(fields)
}

/// The C++ `UNDEFINED_CAST_COST` sentinel: no defined cast between the two type IDs.
pub const UNDEFINED_CAST_COST: u32 = u32::MAX;

/// C++ `BuiltInFunctionsUtils::getCastCost` (built_in_function_utils.cpp): the cost
/// of implicitly casting `src` to `dst`, at *type-ID* granularity (a `DECIMAL(5, 2)`
/// and a `DECIMAL(9, 3)` are the same ID → cost 0; two lists likewise). Drives
/// overload resolution and union-field ("min-cost tag") selection. Note the matrix
/// is narrower than [`implicitly_castable`]: numeric narrowing (INT64→INT32) is
/// admitted only by that gate's catch-all and has *no* defined cost here.
pub fn cast_cost(src: &LogicalType, dst: &LogicalType) -> u32 {
    use IntKind::*;
    use LogicalType::*;
    // C++ `getTargetTypeCost`.
    fn target_cost(t: &LogicalType) -> u32 {
        match t {
            Serial | Int(I16) => 100,
            Int(I64) => 101,
            Int(I32) => 102,
            Int(I128) => 103,
            Decimal(_, _) => 104,
            Double => 105,
            Timestamp => 120,
            String => 149,
            Struct(_) | Map(_, _) | Array(_, _) | List(_) | Union(_) => 160,
            _ => 110,
        }
    }
    if same_type_id(src, dst) {
        return 0;
    }
    if matches!(src, Any) || matches!(dst, Any) {
        return 1;
    }
    // Any type except the blob / graph / internal-id family casts to STRING,
    // at a deliberately high cost (C++ `castFromString`).
    if matches!(dst, String) {
        return match src {
            Blob | InternalId | Node(_) | Rel(_) | RecursiveRel => UNDEFINED_CAST_COST,
            _ => target_cost(&String),
        };
    }
    let cost = |t: &LogicalType| target_cost(t);
    match (src, dst) {
        // Integer widths widen (never narrow) along the C++ per-width tables;
        // SERIAL and INT64 inter-cast for free.
        (Int(I64), Serial) | (Serial, Int(I64)) => 0,
        (Int(I64), Int(I128)) => cost(dst),
        (Int(I64), Float | Double | Decimal(_, _)) => cost(dst),
        (Int(I32), Serial | Int(I64) | Int(I128) | Float | Double | Decimal(_, _)) => cost(dst),
        (Int(I16), Serial | Int(I32) | Int(I64) | Int(I128) | Float | Double | Decimal(_, _)) => {
            cost(dst)
        }
        (
            Int(I8),
            Serial | Int(I16) | Int(I32) | Int(I64) | Int(I128) | Float | Double | Decimal(_, _),
        ) => cost(dst),
        (Int(U64), Int(I128) | Float | Double | Decimal(_, _)) => cost(dst),
        (Int(U32), Serial | Int(I64) | Int(I128) | Int(U64) | Float | Double | Decimal(_, _)) => {
            cost(dst)
        }
        (
            Int(U16),
            Serial
            | Int(I32)
            | Int(I64)
            | Int(I128)
            | Int(U32)
            | Int(U64)
            | Float
            | Double
            | Decimal(_, _),
        ) => cost(dst),
        (
            Int(U8),
            Serial
            | Int(I16)
            | Int(I32)
            | Int(I64)
            | Int(I128)
            | Int(U16)
            | Int(U32)
            | Int(U64)
            | Float
            | Double
            | Decimal(_, _),
        ) => cost(dst),
        (Int(I128), Float | Double | Decimal(_, _)) => cost(dst),
        (Float, Double) => cost(dst),
        (Decimal(_, _), Float | Double) => cost(dst),
        (Date, Timestamp) => cost(dst),
        // The sub-microsecond flavors cast (only) to plain TIMESTAMP.
        (TimestampNs | TimestampMs | TimestampSec | TimestampTz, Timestamp) => cost(dst),
        (List(_), Array(_, _)) | (Array(_, _), List(_)) => cost(dst),
        (String, Json) => cost(dst),
        _ => UNDEFINED_CAST_COST,
    }
}

/// Whether two types share a C++ `LogicalTypeID` (integer widths are distinct IDs;
/// container and parameterized types compare by constructor only).
fn same_type_id(a: &LogicalType, b: &LogicalType) -> bool {
    use LogicalType::*;
    match (a, b) {
        (Int(x), Int(y)) => x == y,
        (Decimal(_, _), Decimal(_, _))
        | (List(_), List(_))
        | (Array(_, _), Array(_, _))
        | (Struct(_), Struct(_))
        | (Map(_, _), Map(_, _))
        | (Union(_), Union(_))
        | (Node(_), Node(_))
        | (Rel(_), Rel(_)) => true,
        _ => std::mem::discriminant(a) == std::mem::discriminant(b),
    }
}

/// The implicit-cast gate, mirroring C++ `CastFunction::hasImplicitCast`
/// (vector_cast_functions.cpp) + `getCastCost` / `castFromString`
/// (built_in_function_utils.cpp). Shared by the binder's assignment checks and
/// the exec-time union casts.
pub fn implicitly_castable(src: &LogicalType, dst: &LogicalType) -> bool {
    use LogicalType::*;
    if src == dst || *src == Any || *dst == Any {
        return true;
    }
    // Every type casts implicitly to STRING except those C++ `castFromString` leaves
    // undefined (BLOB and the graph / internal-id types).
    if matches!(dst, String) {
        return !matches!(src, Blob | InternalId | Node(_) | Rel(_) | RecursiveRel);
    }
    // All numeric types inter-cast implicitly, **narrowing included** (DOUBLE→INT64,
    // INT64→INT32, …) — C++ `hasImplicitCast`'s `isNumerical(src) && isNumerical(dst)`
    // catch-all. The narrowing cast is performed at runtime (e.g. `2.5`→`2`).
    if src.is_numeric() && dst.is_numeric() {
        return true;
    }
    // DATE promotes implicitly into every TIMESTAMP flavor (C++ hasImplicitCast;
    // oracle-verified: [date(...), timestamp(...)] → TIMESTAMP[]).
    if matches!(src, Date)
        && matches!(
            dst,
            Timestamp | TimestampNs | TimestampMs | TimestampSec | TimestampTz
        )
    {
        return true;
    }
    // Nested types assign when their children do — a STRUCT whose fields differ only
    // by an implicitly-castable element type, a list whose element casts, etc.
    // Structs match positionally by name.
    match (src, dst) {
        // LIST and fixed-size ARRAY are mutually assignable when their elements are —
        // C++ `hasImplicitCastListToArray` / `hasImplicitCastArrayToList` check only the
        // element type (length is not part of the implicit-cast gate).
        (List(s) | Array(s, _), List(d) | Array(d, _)) => implicitly_castable(s, d),
        (Map(sk, sv), Map(dk, dv)) => implicitly_castable(sk, dk) && implicitly_castable(sv, dv),
        (Struct(s), Struct(d)) => {
            s.len() == d.len()
                && s.iter().zip(d).all(|((sn, st), (dn, dt))| {
                    sn.eq_ignore_ascii_case(dn) && implicitly_castable(st, dt)
                })
        }
        // UNION→UNION: every source alternative must exist in the target by name
        // with an implicitly-castable type (C++ `hasImplicitCastUnion`, union side).
        (Union(s), Union(d)) => s.iter().all(|(sn, st)| {
            d.iter()
                .find(|(dn, _)| dn == sn)
                .is_some_and(|(_, dt)| implicitly_castable(st, dt))
        }),
        // Non-nested value → UNION: some target alternative accepts it (C++
        // `hasImplicitCastUnion`, scalar side; a nested non-union source is a
        // type-ID mismatch → false). The cost matrix admits a few pairs the
        // gate's arms don't spell out (e.g. TIMESTAMP_NS→TIMESTAMP), so check both.
        (s, Union(d)) if !s.is_nested() => d
            .iter()
            .any(|(_, ft)| implicitly_castable(s, ft) || cast_cost(s, ft) != UNDEFINED_CAST_COST),
        _ => false,
    }
}

/// C++ `findUnionMinCostTag`: the UNION member a scalar source casts into — the
/// minimum-[`cast_cost`] field, first wins a tie. `None` = no cost-defined field
/// (the cast fails with "target type has no compatible field" even when a member
/// would be admissible via the numeric catch-all, whose cost is undefined).
pub fn union_min_cost_tag(src: &LogicalType, fields: &[(String, LogicalType)]) -> Option<usize> {
    let mut best: Option<(usize, u32)> = None;
    for (i, (_, fty)) in fields.iter().enumerate() {
        let c = cast_cost(src, fty);
        if c != UNDEFINED_CAST_COST && best.is_none_or(|(_, bc)| c < bc) {
            best = Some((i, c));
        }
    }
    best.map(|(i, _)| i)
}
