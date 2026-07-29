//! The owned [`Value`] type and its result-string formatting.
//!
//! `Value` is the materialized public cell type. [`Value::to_result_string`]
//! preserves Koko's established list-format rendering, inherited during the
//! clean-room migration and now covered by fixed regressions. See
//! `docs/cpp-reference/02-value-formatting.md` for its provenance.

use crate::temporal::{self, Interval};
use crate::types::{IntKind, InternalId, LogicalType, TableId};
use std::fmt::Write as _;

/// A node value as surfaced in a query result: identity + label + properties.
///
/// Properties are kept in schema order; a `Null` property is *omitted* from the
/// rendered form (matching `nodeToString`).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeValue {
    pub id: InternalId,
    pub label: String,
    pub props: Vec<(String, Value)>,
}

/// A relationship value as surfaced in a query result.
#[derive(Debug, Clone, PartialEq)]
pub struct RelValue {
    pub src: InternalId,
    pub dst: InternalId,
    pub id: InternalId,
    pub label: String,
    pub props: Vec<(String, Value)>,
    /// Materialized endpoint nodes (populated when the rel value is assembled
    /// with storage access — `start_node()`/`end_node()` read these).
    pub src_node: Option<Box<NodeValue>>,
    pub dst_node: Option<Box<NodeValue>>,
}

/// A recursive-relationship / path value: an ordered list of nodes and the
/// relationships linking them. Both a variable-length rel variable `e` and a
/// named path `p` use this; they render identically as `{_NODES: …, _RELS: …}`.
/// For a *rel* value `_NODES` holds only the intermediate nodes (endpoints
/// excluded); for a *path* value it holds every node (endpoints included).
#[derive(Debug, Clone, PartialEq)]
pub struct RecursiveRelValue {
    pub nodes: Vec<NodeValue>,
    pub rels: Vec<RelValue>,
    /// The degenerate single-node path an unmatched OPTIONAL var-length MATCH
    /// leaves behind: it renders like a matched zero-length path, but `length()`
    /// returns NULL on it in C++ (audit V13).
    pub degenerate: bool,
    /// The accumulated edge weight of a (ALL) WSHORTEST path — `cost(e)` reads
    /// it. `None` for the unweighted modes.
    pub cost: Option<f64>,
    /// Trailing NULL node slots for a degenerate unmatched-OPTIONAL path — its
    /// `_NODES` renders the matched prefix then this many empty slots
    /// (`{_NODES: [{A}, , ], …}`). 0 for a normal path.
    pub null_nodes: usize,
}

/// A single materialized result value.
///
/// Recursive graph variants are boxed to keep the enum small.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// The canonical 64-bit signed integer (bare integer literals, counts, …).
    Int64(i64),
    /// An integer of a non-`INT64` width, backed by `i128` (see [`IntKind`]).
    IntX {
        value: i128,
        kind: IntKind,
    },
    /// `UINT128` — kept separate because its range exceeds `i128`.
    UInt128(u128),
    /// `DECIMAL(precision, scale)`: the unscaled `i128` value plus its type
    /// parameters. Denotes `value / 10^scale`.
    Decimal {
        value: i128,
        precision: u8,
        scale: u8,
    },
    Double(f64),
    Float(f32),
    String(String),
    /// Ordered JSON with compact result rendering.
    Json(crate::JsonValue),
    /// Days since the Unix epoch.
    Date(i32),
    /// Microseconds since the Unix epoch.
    Timestamp(i64),
    /// `TIMESTAMP_TZ`: microseconds since the Unix epoch, rendered with a `+00`
    /// UTC suffix. Compares equal to a `Timestamp` of the same instant.
    TimestampTz(i64),
    Interval(Interval),
    Uuid(u128),
    Blob(Vec<u8>),
    /// A list/array of values: `[a,b,c]`.
    List(Vec<Value>),
    /// A struct: ordered `(field, value)` pairs, rendered `{k: v, …}`.
    Struct(Vec<(String, Value)>),
    /// A map: ordered `(key, value)` entries, rendered `{k=v, …}`.
    Map(Vec<(Value, Value)>),
    InternalId(InternalId),
    Node(Box<NodeValue>),
    Rel(Box<RelValue>),
    /// A recursive relationship / path value (`{_NODES, _RELS}`).
    RecursiveRel(Box<RecursiveRelValue>),
    /// A tagged UNION value. `variants` is the *full* member list (so the value
    /// carries its complete `UNION(...)` type), `tag` indexes the active member,
    /// and `value` is that member's payload. It renders as the bare payload; the
    /// active member name is surfaced by `union_tag` (see the union functions).
    Union {
        variants: Vec<(String, LogicalType)>,
        tag: usize,
        value: Box<Value>,
    },
}

impl Value {
    /// The logical type of this value (`Null` is untyped → `Any`).
    pub fn logical_type(&self) -> LogicalType {
        match self {
            Value::Null => LogicalType::Any,
            Value::Bool(_) => LogicalType::Bool,
            Value::Int64(_) => LogicalType::Int64,
            Value::IntX { kind, .. } => LogicalType::Int(*kind),
            Value::UInt128(_) => LogicalType::UInt128,
            Value::Decimal {
                precision, scale, ..
            } => LogicalType::Decimal(*precision, *scale),
            Value::Double(_) => LogicalType::Double,
            Value::Float(_) => LogicalType::Float,
            Value::String(_) => LogicalType::String,
            Value::Json(_) => LogicalType::Json,
            Value::Date(_) => LogicalType::Date,
            Value::Timestamp(_) => LogicalType::Timestamp,
            Value::TimestampTz(_) => LogicalType::TimestampTz,
            Value::Interval(_) => LogicalType::Interval,
            Value::Uuid(_) => LogicalType::Uuid,
            Value::Blob(_) => LogicalType::Blob,
            Value::List(items) => LogicalType::List(Box::new(
                items.first().map_or(LogicalType::Any, |v| v.logical_type()),
            )),
            Value::Struct(fields) => LogicalType::Struct(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), v.logical_type()))
                    .collect(),
            ),
            Value::Map(entries) => {
                let (k, v) = entries
                    .first()
                    .map_or((LogicalType::Any, LogicalType::Any), |(k, v)| {
                        (k.logical_type(), v.logical_type())
                    });
                LogicalType::Map(Box::new(k), Box::new(v))
            }
            Value::InternalId(_) => LogicalType::InternalId,
            Value::Node(n) => LogicalType::Node(n.id.table_id),
            Value::Rel(r) => LogicalType::Rel(r.id.table_id),
            Value::RecursiveRel(_) => LogicalType::RecursiveRel,
            Value::Union { variants, .. } => LogicalType::Union(variants.clone()),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Render this value exactly as the `.test` corpus expects (one result cell).
    pub fn to_result_string(&self) -> String {
        self.render()
    }

    /// Render one CSV interchange cell. C++ serializes relationship values as
    /// value structs in CSV (rather than the shell's arrow notation); path
    /// values use the same relationship form inside `_RELS`.
    pub fn to_csv_string(&self) -> String {
        match self {
            Value::Rel(rel) => format_csv_rel(rel),
            Value::RecursiveRel(path) => format_csv_recursive_rel(path),
            _ => self.render(),
        }
    }

    /// Render a value the way the C++ shell does: strings never self-quote
    /// anywhere (audit R1). Where the corpus shows quotes inside stored nested
    /// strings (`summary: {locations: ['london','toronto']}`), the quote
    /// characters are part of the *data* — retained by the string→nested parser
    /// (audit W4/R6) — not renderer decoration.
    fn render(&self) -> String {
        match self {
            // NULL of any type renders as the empty string (indistinguishable
            // from an empty string value — matches the C++ engine).
            Value::Null => String::new(),
            Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
            Value::Int64(n) => n.to_string(),
            Value::IntX { value, .. } => value.to_string(),
            Value::UInt128(u) => u.to_string(),
            Value::Decimal { value, scale, .. } => crate::decimal::format_decimal(*value, *scale),
            // `std::to_string(double)` == `%f` == exactly 6 fractional digits for
            // finite values; libc renders specials as lowercase `nan`/`inf`.
            Value::Double(x) => format_float(*x),
            Value::Float(x) => format_float(*x as f64),
            Value::String(s) => s.clone(),
            Value::Json(value) => value.render(),
            Value::Date(d) => temporal::format_date(*d),
            Value::Timestamp(t) => temporal::format_timestamp(*t),
            Value::TimestampTz(t) => format!("{}+00", temporal::format_timestamp(*t)),
            Value::Interval(iv) => temporal::format_interval(iv),
            Value::Uuid(u) => crate::scalar::format_uuid(*u),
            Value::Blob(b) => crate::scalar::format_blob(b),
            // Strings never quote anywhere in rendering (audit R1 — C++ renders
            // `{a: [x,y]}` bare). The quote characters the corpus shows inside
            // stored nested strings are *data*, retained by the string→nested
            // parser (audit W4/R6), not renderer decoration.
            Value::List(items) => {
                let inner: Vec<String> = items.iter().map(|v| v.render()).collect();
                format!("[{}]", inner.join(","))
            }
            Value::Map(entries) => {
                let inner: Vec<String> = entries
                    .iter()
                    .map(|(k, v)| format!("{}={}", k.render(), v.render()))
                    .collect();
                format!("{{{}}}", inner.join(", "))
            }
            Value::Struct(fields) => {
                let inner: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.render()))
                    .collect();
                format!("{{{}}}", inner.join(", "))
            }
            Value::InternalId(id) => id.to_string(),
            Value::Node(n) => format_node(n),
            Value::Rel(r) => format_rel(r),
            Value::RecursiveRel(r) => format_recursive_rel(r),
            // A union renders as its active payload (the tag is not shown; it is
            // surfaced by `union_tag`). Matches the C++ corpus, e.g.
            // `union_value(age := 36)` → `36`, `union_value(a := [12,34])` → `[12,34]`.
            Value::Union { value, .. } => value.render(),
        }
    }

    // --- ergonomic typed accessors used by the public row API ---

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int64(n) => Some(*n),
            Value::IntX { value, .. } => i64::try_from(*value).ok(),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Double(x) => Some(*x),
            Value::Float(x) => Some(*x as f64),
            Value::Int64(n) => Some(*n as f64),
            Value::IntX { value, .. } => Some(*value as f64),
            Value::UInt128(u) => Some(*u as f64),
            Value::Decimal { value, scale, .. } => {
                Some(*value as f64 / crate::decimal::pow10(*scale) as f64)
            }
            _ => None,
        }
    }

    /// The `(unscaled value, precision, scale)` of a `DECIMAL` value.
    pub fn decimal_parts(&self) -> Option<(i128, u8, u8)> {
        match self {
            Value::Decimal {
                value,
                precision,
                scale,
            } => Some((*value, *precision, *scale)),
            _ => None,
        }
    }

    /// The value as a 128-bit integer, for any integer width (`None` for a
    /// `UINT128` larger than `i128::MAX`).
    pub fn as_int128(&self) -> Option<i128> {
        match self {
            Value::Int64(n) => Some(*n as i128),
            Value::IntX { value, .. } => Some(*value),
            Value::UInt128(u) => i128::try_from(*u).ok(),
            _ => None,
        }
    }

    /// The value as an unsigned 128-bit integer, for any non-negative integer.
    pub fn as_u128(&self) -> Option<u128> {
        match self {
            Value::UInt128(u) => Some(*u),
            Value::Int64(n) => u128::try_from(*n).ok(),
            Value::IntX { value, .. } => u128::try_from(*value).ok(),
            _ => None,
        }
    }

    /// The `(value, width)` of any integer value.
    pub fn int_parts(&self) -> Option<(i128, IntKind)> {
        match self {
            Value::Int64(n) => Some((*n as i128, IntKind::I64)),
            Value::IntX { value, kind } => Some((*value, *kind)),
            _ => None,
        }
    }

    /// Build an integer value of a given width (normalizing `INT64` to `Int64`).
    pub fn make_int(value: i128, kind: IntKind) -> Value {
        if kind == IntKind::I64 {
            Value::Int64(value as i64)
        } else {
            Value::IntX { value, kind }
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_json(&self) -> Option<&crate::JsonValue> {
        match self {
            Value::Json(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_internal_id(&self) -> Option<InternalId> {
        match self {
            Value::InternalId(id) => Some(*id),
            _ => None,
        }
    }
}

// Ergonomic constructors for the common scalar types, so query parameters can be
// written as bare Rust values (e.g. via the `koko::params!` macro) instead of
// spelling out the `Value` variant. Only one integer and one float impl are
// provided so an unsuffixed literal (`Value::from(30)`) stays unambiguous: it
// resolves to `i64`/`f64`, matching how a bare Cypher numeric literal is typed
// (`INT64`/`DOUBLE`). The reflexive `From<Value> for Value` comes from std's
// blanket impl, so an existing `Value` passes through unchanged.
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int64(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Double(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}

/// Render a value that appears as a `LIST` element or a `MAP` key/value. A bare
/// string is single-quoted **iff we are inside a struct context** (`Nested`);
/// `%f` rendering of a float: 6 fractional digits for finite values; libc-style
/// lowercase `nan`/`-nan` for NaN (infinities already fall through to `{:.6}` as
/// `inf`/`-inf`). Public: cast overflow messages render the offending float in
/// this same C++ `%f` form (`Value -0.400000 is not within UINT16 range`).
pub fn format_float(x: f64) -> String {
    if x.is_nan() {
        if x.is_sign_negative() {
            "-nan".to_string()
        } else {
            "nan".to_string()
        }
    } else {
        format!("{x:.6}")
    }
}

/// `{_ID: t:o, _LABEL: label, prop: val, …}` — null props skipped.
fn format_node(n: &NodeValue) -> String {
    let mut s = String::from("{");
    let _ = write!(s, "_ID: {}, _LABEL: {}", n.id, n.label);
    for (k, v) in &n.props {
        if v.is_null() {
            continue;
        }
        let _ = write!(s, ", {}: {}", k, v.to_result_string());
    }
    s.push('}');
    s
}

/// `{_NODES: [n, …], _RELS: [r, …]}` — the nodes and rels each render in the
/// top-level mode (so their string properties stay unquoted), matching the C++
/// `RecursiveRelValue` rendering.
fn format_recursive_rel(r: &RecursiveRelValue) -> String {
    let mut nodes: Vec<String> = r.nodes.iter().map(format_node).collect();
    // A degenerate OPTIONAL path renders its unmatched tail nodes as empty slots.
    nodes.extend(std::iter::repeat_n(String::new(), r.null_nodes));
    let rels: Vec<String> = r.rels.iter().map(format_rel).collect();
    format!(
        "{{_NODES: [{}], _RELS: [{}]}}",
        nodes.join(","),
        rels.join(",")
    )
}

/// `(src)-{_LABEL: label, _ID: t:o, prop: val, …}->(dst)` — null props skipped.
fn format_rel(r: &RelValue) -> String {
    let mut s = String::new();
    if r.label == "_edges" {
        let _ = write!(s, "({})-{{_LABEL: {}, _id: {}", r.src, r.label, r.id);
    } else {
        let _ = write!(s, "({})-{{_LABEL: {}, _ID: {}", r.src, r.label, r.id);
    }
    for (k, v) in &r.props {
        if v.is_null() {
            continue;
        }
        let _ = write!(s, ", {}: {}", k, v.to_result_string());
    }
    let _ = write!(s, "}}->({})", r.dst);
    s
}

fn format_csv_rel(rel: &RelValue) -> String {
    let mut value = format!(
        "{{_SRC: {}, _DST: {}, _LABEL: {}, _ID: {}",
        rel.src, rel.dst, rel.label, rel.id
    );
    for (name, property) in &rel.props {
        if !property.is_null() {
            let _ = write!(value, ", {name}: {}", property.to_result_string());
        }
    }
    value.push('}');
    value
}

fn format_csv_recursive_rel(path: &RecursiveRelValue) -> String {
    let mut nodes: Vec<String> = path.nodes.iter().map(format_node).collect();
    nodes.extend(std::iter::repeat_n(String::new(), path.null_nodes));
    let rels: Vec<String> = path.rels.iter().map(format_csv_rel).collect();
    format!(
        "{{_NODES: [{}], _RELS: [{}]}}",
        nodes.join(","),
        rels.join(",")
    )
}

/// Construct a `Value::Node` (small convenience for the result collector).
impl NodeValue {
    pub fn new(table_id: TableId, offset: u64, label: impl Into<String>) -> Self {
        Self {
            id: InternalId::new(table_id, offset),
            label: label.into(),
            props: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Offset;

    fn iid(t: u64, o: u64) -> InternalId {
        InternalId {
            table_id: TableId(t),
            offset: Offset(o),
        }
    }

    #[test]
    fn scalar_formatting_matches_cpp() {
        assert_eq!(Value::Null.to_result_string(), "");
        assert_eq!(Value::Bool(true).to_result_string(), "True");
        assert_eq!(Value::Bool(false).to_result_string(), "False");
        assert_eq!(Value::Int64(-12).to_result_string(), "-12");
        assert_eq!(Value::Double(37.25).to_result_string(), "37.250000");
        assert_eq!(Value::Double(1.731).to_result_string(), "1.731000");
        assert_eq!(Value::Double(0.0).to_result_string(), "0.000000");
        assert_eq!(Value::Double(f64::NAN).to_result_string(), "nan");
        assert_eq!(Value::String("Alice".into()).to_result_string(), "Alice");
        assert_eq!(Value::InternalId(iid(3, 7)).to_result_string(), "3:7");
    }

    #[test]
    fn node_formatting_skips_null_props() {
        let n = NodeValue {
            id: iid(0, 4),
            label: "User".into(),
            props: vec![
                ("name".into(), Value::String("Alice".into())),
                ("age".into(), Value::Null),
            ],
        };
        assert_eq!(
            Value::Node(Box::new(n)).to_result_string(),
            "{_ID: 0:4, _LABEL: User, name: Alice}"
        );
    }

    #[test]
    fn rel_formatting() {
        let r = RelValue {
            src: iid(0, 0),
            dst: iid(0, 1),
            id: iid(3, 0),
            label: "knows".into(),
            props: vec![("since".into(), Value::Int64(2020))],
            src_node: None,
            dst_node: None,
        };
        let value = Value::Rel(Box::new(r));
        assert_eq!(
            value.to_result_string(),
            "(0:0)-{_LABEL: knows, _ID: 3:0, since: 2020}->(0:1)"
        );
        assert_eq!(
            value.to_csv_string(),
            "{_SRC: 0:0, _DST: 0:1, _LABEL: knows, _ID: 3:0, since: 2020}"
        );
    }

    fn s(x: &str) -> Value {
        Value::String(x.into())
    }

    #[test]
    fn nested_string_quoting_matches_corpus() {
        // Top-level string + top-level string list: unquoted (matches
        // `RETURN collect(p.fName)` → `[Alice,Bob]`).
        assert_eq!(s("Alice").to_result_string(), "Alice");
        assert_eq!(
            Value::List(vec![s("Alice"), s("Bob")]).to_result_string(),
            "[Alice,Bob]"
        );
        // Strings never self-quote in rendering (audit R1). Where the corpus
        // shows quotes inside stored nested strings, the quote characters are
        // *data* retained by the string→nested parser (audit W4/R6).
        assert_eq!(
            Value::Struct(vec![("a".into(), s("hello"))]).to_result_string(),
            "{a: hello}"
        );
        assert_eq!(
            Value::Struct(vec![(
                "locations".into(),
                Value::List(vec![s("'london'"), s("'toronto'")])
            )])
            .to_result_string(),
            "{locations: ['london','toronto']}"
        );
        // A MAP propagates the (top-level) mode — strings stay unquoted: `{a=b}`.
        assert_eq!(
            Value::Map(vec![(s("a"), s("b"))]).to_result_string(),
            "{a=b}"
        );
    }

    #[test]
    fn recursive_rel_rendering_matches_corpus() {
        // An empty path → `{_NODES: [], _RELS: []}`.
        let empty = RecursiveRelValue {
            nodes: vec![],
            rels: vec![],
            degenerate: false,
            cost: None,
            null_nodes: 0,
        };
        assert_eq!(
            Value::RecursiveRel(Box::new(empty)).to_result_string(),
            "{_NODES: [], _RELS: []}"
        );
        // One intermediate node + one rel; node string props stay unquoted (Top).
        let rr = RecursiveRelValue {
            nodes: vec![NodeValue {
                id: iid(0, 0),
                label: "person".into(),
                props: vec![("fName".into(), s("Alice"))],
            }],
            rels: vec![RelValue {
                src: iid(0, 3),
                dst: iid(0, 0),
                id: iid(3, 9),
                label: "knows".into(),
                props: vec![],
                src_node: None,
                dst_node: None,
            }],
            degenerate: false,
            cost: None,
            null_nodes: 0,
        };
        assert_eq!(
            Value::RecursiveRel(Box::new(rr)).to_result_string(),
            "{_NODES: [{_ID: 0:0, _LABEL: person, fName: Alice}], \
             _RELS: [(0:3)-{_LABEL: knows, _ID: 3:9}->(0:0)]}"
        );
    }

    #[test]
    fn rel_property_list_vs_struct_nested_list() {
        // The decisive corpus case: the `comments` CSV column carries bare list
        // elements while the struct's `locations` list carries quote characters
        // *in the data* (W4 retention) — the renderer itself never quotes (R1).
        let r = RelValue {
            src: iid(0, 0),
            dst: iid(0, 1),
            id: iid(3, 0),
            label: "knows".into(),
            props: vec![
                (
                    "comments".into(),
                    Value::List(vec![s("rnme"), s("m8sihsdnf2990nfiwf")]),
                ),
                (
                    "summary".into(),
                    Value::Struct(vec![(
                        "locations".into(),
                        Value::List(vec![s("'toronto'"), s("'waterloo'")]),
                    )]),
                ),
            ],
            src_node: None,
            dst_node: None,
        };
        assert_eq!(
            Value::Rel(Box::new(r)).to_result_string(),
            "(0:0)-{_LABEL: knows, _ID: 3:0, comments: [rnme,m8sihsdnf2990nfiwf], \
             summary: {locations: ['toronto','waterloo']}}->(0:1)"
        );
    }
}
