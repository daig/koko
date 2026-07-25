//! Name-keyed scalar function registry — the analogue of the C++
//! `FunctionCollection`. Functions are dispatched by (lower-cased) name so the
//! library can grow without a giant enum. Operators (`+ - * …`) stay in
//! [`crate::ScalarOp`]; aggregates in [`crate::AggOp`]; `CAST` is handled by the
//! binder. Everything else — math, string, list, temporal constructors, the
//! `to_*` cast aliases — registers here.

use crate::cast_value;
use koko_common::{Error, LogicalType, Result, Value, temporal};
use unicode_segmentation::UnicodeSegmentation;

/// Every registered scalar function name (lower-case). The binder consults this
/// to decide whether a call is a scalar function before erroring.
pub const SCALAR_NAMES: &[&str] = &[
    // temporal constructors
    "date",
    "timestamp",
    "interval",
    "duration",
    "uuid",
    "string",
    "blob",
    "make_date",
    "to_timestamp",
    "century",
    // numeric / math
    "abs",
    "floor",
    "ceil",
    "ceiling",
    "round",
    "sign",
    "even",
    "factorial",
    "sqrt",
    "cbrt",
    "ln",
    "log",
    "log2",
    "log10",
    "exp",
    "pow",
    "power",
    "sin",
    "cos",
    "tan",
    "cot",
    "asin",
    "acos",
    "atan",
    "atan2",
    "degrees",
    "radians",
    "gamma",
    "lgamma",
    "pi",
    "negate",
    // bitwise
    "bitwise_and",
    "bitwise_or",
    "bitwise_xor",
    "bitshift_left",
    "bitshift_right",
    // generic utility
    "coalesce",
    "ifnull",
    "nullif",
    "greatest",
    "least",
    "typeof",
    "constant_or_null",
    // uuid generator
    "gen_random_uuid",
    // union accessors (`union_value` is constructed in the binder/processor)
    "union_tag",
    "union_extract",
    // string
    "concat",
    "contains",
    "prefix",
    "suffix",
    "starts_with",
    "ends_with",
    "lower",
    "lcase",
    "upper",
    "ucase",
    "trim",
    "ltrim",
    "rtrim",
    "reverse",
    "size",
    "length",
    "left",
    "right",
    "lpad",
    "rpad",
    "substr",
    "substring",
    "repeat",
    "initcap",
    "string_split",
    "split_part",
    "str_split",
    "string_to_array",
    "array_extract",
    "levenshtein",
    "list_to_string",
    "replace",
    "regexp_matches",
    "regexp_full_match",
    "regexp_replace",
    "regexp_extract",
    "regexp_extract_all",
    "regexp_split_to_array",
    // list
    "range",
    "list_creation",
    "list_extract",
    "list_element",
    "list_slice",
    "array_slice",
    "list_concat",
    "list_cat",
    "list_append",
    "list_prepend",
    "list_contains",
    "list_position",
    "list_indexof",
    "list_reverse",
    "list_sort",
    "list_reverse_sort",
    "list_distinct",
    "list_unique",
    "list_sum",
    "list_product",
    "list_any_value",
    // array aliases of the list functions
    "array_contains",
    "array_concat",
    "array_cat",
    "array_position",
    "array_indexof",
    "array_reverse",
    "array_distinct",
    "array_sort",
    "array_to_string",
    // fixed-array / vector functions
    "array_value",
    "array_distance",
    "array_squared_distance",
    "array_inner_product",
    "array_dot_product",
    "array_cosine_similarity",
    "array_cross_product",
    // struct / map
    "struct_extract",
    "map",
    "map_extract",
    "element_at",
    "map_keys",
    "map_values",
    "cardinality",
    // date / timestamp / interval
    "dayname",
    "monthname",
    "last_day",
    "date_part",
    "datepart",
    "date_trunc",
    "datetrunc",
    "to_years",
    "to_months",
    "to_days",
    "to_hours",
    "to_minutes",
    "to_seconds",
    "to_milliseconds",
    "to_microseconds",
    // cast aliases
    "to_int8",
    "to_int16",
    "to_int32",
    "to_int64",
    "to_int128",
    "to_uint8",
    "to_uint16",
    "to_uint32",
    "to_uint64",
    "to_uint128",
    "to_serial",
    "to_double",
    "to_float",
    "to_bool",
    "to_string",
    "to_blob",
    "to_uuid",
    "to_date",
    "to_timestamp",
    // Node/rel accessors (evaluated specially in koko-expr, which has the
    // table-name map; registered here so the binder accepts and types them).
    "id",
    "offset",
    "label",
    "labels",
    // Recursive-rel / path accessors.
    "nodes",
    "rels",
    "relationships",
    "properties",
    // M5 backfill: string/blob/temporal/hash/random utilities and the
    // remaining list/array aliases.
    "cost",
    "rowid",
    "is_trail",
    "is_acyclic",
    "start_node",
    "end_node",
    "equals",
    "not_equals",
    "greater_than",
    "greater_than_equals",
    "less_than",
    "less_than_equals",
    "concat_ws",
    "count_if",
    "current_date",
    "current_timestamp",
    "octet_length",
    "encode",
    "decode",
    "epoch_ms",
    "to_epoch_ms",
    "hash",
    "md5",
    "sha256",
    "internal_id",
    "random",
    "setseed",
    "error",
    "tolower",
    "toupper",
    "to_interval",
    "list_has",
    "list_has_all",
    "array_has",
    "array_append",
    "array_prepend",
    "array_push_back",
    "array_push_front",
];

/// The argument positions a function's C++ signature declares as STRING — every
/// implicitly castable type coerces to STRING there at bind (audit §6.1 slice,
/// oracle-verified: `lower(123)` → `123`, `left(to_double(1.34), 8)` →
/// `1.340000`, `upper(true)` → `TRUE`). BLOB and graph types do NOT coerce
/// (C++ rejects `lower(BLOB)` with the overload table). `None` = no STRING
/// params / not in this interim table (the full M3 catalog supersedes it).
pub fn string_coerce_positions(name: &str) -> Option<&'static [usize]> {
    const P0: &[usize] = &[0];
    const P01: &[usize] = &[0, 1];
    const P012: &[usize] = &[0, 1, 2];
    const P02: &[usize] = &[0, 2];
    Some(match name {
        "lower" | "lcase" | "upper" | "ucase" | "trim" | "ltrim" | "rtrim" | "initcap" | "left"
        | "right" | "repeat" | "substr" | "substring" => P0,
        "lpad" | "rpad" => P02,
        "contains"
        | "prefix"
        | "suffix"
        | "starts_with"
        | "ends_with"
        | "string_split"
        | "str_split"
        | "string_to_array"
        | "levenshtein"
        | "regexp_matches"
        | "regexp_full_match"
        | "regexp_extract"
        | "regexp_extract_all"
        | "regexp_split_to_array"
        | "split_part" => P01,
        "replace" | "regexp_replace" => P012,
        "list_to_string" => P0,
        "reverse" => P0,
        "md5" | "sha256" => P0,
        _ => return None,
    })
}

/// Whether `name` (case-insensitive) is a registered scalar function.
pub fn is_scalar(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SCALAR_NAMES.contains(&n.as_str())
}

/// Canonicalize `array_*` aliases to their `list_*` equivalents.
fn canonical(name: &str) -> &str {
    match name {
        "array_contains" => "list_contains",
        "array_concat" | "array_cat" => "list_concat",
        "array_position" | "array_indexof" => "list_position",
        "array_reverse" => "list_reverse",
        "array_distinct" => "list_distinct",
        // C++ CARDINALITY is a SIZE alias (its four overloads are SIZE's).
        "cardinality" => "size",
        "array_sort" => "list_sort",
        "array_to_string" => "list_to_string",
        "tolower" => "lower",
        "toupper" => "upper",
        "to_interval" => "interval",
        "list_has" | "array_has" => "list_contains",
        "array_append" | "array_push_back" => "list_append",
        "array_prepend" | "array_push_front" => "list_prepend",
        other => other,
    }
}

/// The cast-target type for a `to_*` alias, if `name` is one.
fn cast_alias_target(name: &str) -> Option<LogicalType> {
    use koko_common::IntKind::*;
    Some(match name {
        "to_int8" => LogicalType::Int(I8),
        "to_int16" => LogicalType::Int(I16),
        "to_int32" => LogicalType::Int(I32),
        "to_int64" => LogicalType::Int64,
        "to_serial" => LogicalType::Serial,
        "to_int128" => LogicalType::Int(I128),
        "to_uint8" => LogicalType::Int(U8),
        "to_uint16" => LogicalType::Int(U16),
        "to_uint32" => LogicalType::Int(U32),
        "to_uint64" => LogicalType::Int(U64),
        "to_uint128" => LogicalType::UInt128,
        "to_double" => LogicalType::Double,
        "to_float" => LogicalType::Float,
        "to_bool" => LogicalType::Bool,
        "to_string" => LogicalType::String,
        "to_blob" => LogicalType::Blob,
        "to_uuid" => LogicalType::Uuid,
        "to_date" => LogicalType::Date,
        _ => return None,
    })
}

/// The catalog signature gate: every scalar call whose (as-called) name exists
/// in the oracle catalog is matched against that name's overload rows with the
/// C++ algorithm (`BuiltInFunctionsUtils::matchFunction`): a candidate survives
/// when every argument has a *defined* cast cost to the overload's parameter
/// type-ID (var-length functions match any arity against their one parameter).
/// No surviving candidate -> the C++ "did not receive correct arguments" block,
/// whose Expected lines are the catalog rows verbatim (zero-parameter overloads
/// omitted). A `None` return means "gate passed" -- the per-name binding logic
/// still applies its own stricter refinements after.
mod sigcatalog {
    use super::signature_error;
    use crate::catalog_data::FUNCTION_CATALOG;
    use koko_common::types::{UNDEFINED_CAST_COST, cast_cost};
    use koko_common::{Error, LogicalType, TableId};
    use std::collections::HashMap;
    use std::sync::LazyLock;

    /// A parsed overload: the parameter type-ID representatives (`None` for
    /// JSON, which this engine has no value of) and the verbatim signature row.
    struct Overload {
        params: Vec<Option<LogicalType>>,
        sig: &'static str,
    }

    /// A representative of the C++ `LogicalTypeID` a signature token names --
    /// [`cast_cost`] compares container/parameterized types by constructor only,
    /// so placeholder children suffice.
    fn token_type(tok: &str) -> Option<LogicalType> {
        use koko_common::types::IntKind::*;
        Some(match tok {
            "ANY" => LogicalType::Any,
            "BOOL" => LogicalType::Bool,
            "INT8" => LogicalType::Int(I8),
            "INT16" => LogicalType::Int(I16),
            "INT32" => LogicalType::Int(I32),
            "INT64" => LogicalType::Int(I64),
            "INT128" => LogicalType::Int(I128),
            "UINT8" => LogicalType::Int(U8),
            "UINT16" => LogicalType::Int(U16),
            "UINT32" => LogicalType::Int(U32),
            "UINT64" => LogicalType::Int(U64),
            "UINT128" => LogicalType::UInt128,
            "SERIAL" => LogicalType::Serial,
            "DECIMAL" => LogicalType::Decimal(0, 0),
            "DOUBLE" => LogicalType::Double,
            "FLOAT" => LogicalType::Float,
            "STRING" => LogicalType::String,
            "DATE" => LogicalType::Date,
            "TIMESTAMP" => LogicalType::Timestamp,
            "TIMESTAMP_NS" => LogicalType::TimestampNs,
            "TIMESTAMP_MS" => LogicalType::TimestampMs,
            "TIMESTAMP_SEC" => LogicalType::TimestampSec,
            "TIMESTAMP_TZ" => LogicalType::TimestampTz,
            "INTERVAL" => LogicalType::Interval,
            "UUID" => LogicalType::Uuid,
            "BLOB" => LogicalType::Blob,
            "LIST" => LogicalType::List(Box::new(LogicalType::Any)),
            "ARRAY" => LogicalType::Array(Box::new(LogicalType::Any), 0),
            "STRUCT" => LogicalType::Struct(Vec::new()),
            "MAP" => LogicalType::Map(Box::new(LogicalType::Any), Box::new(LogicalType::Any)),
            "UNION" => LogicalType::Union(Vec::new()),
            "NODE" => LogicalType::Node(TableId(0)),
            "REL" => LogicalType::Rel(TableId(0)),
            "RECURSIVE_REL" => LogicalType::RecursiveRel,
            "INTERNAL_ID" => LogicalType::InternalId,
            _ => return None, // JSON (and anything future) never matches a value
        })
    }

    static SCALAR_SIGS: LazyLock<HashMap<&'static str, Vec<Overload>>> = LazyLock::new(|| {
        let mut m: HashMap<&'static str, Vec<Overload>> = HashMap::new();
        for (name, kind, sig) in FUNCTION_CATALOG.iter() {
            // REWRITE functions (LENGTH, ID, LABEL, NULLIF, ...) share the same
            // signature matching and error block; their signature rows simply
            // have no `-> RETURN` part.
            if *kind != "SCALAR FUNCTION" && *kind != "REWRITE FUNCTION" {
                continue;
            }
            let inner = sig
                .strip_prefix('(')
                .and_then(|s| s.split_once(')'))
                .map(|(p, _)| p)
                .unwrap_or_default();
            let params = if inner.is_empty() {
                Vec::new()
            } else {
                inner.split(',').map(token_type).collect()
            };
            m.entry(name).or_default().push(Overload { params, sig });
        }
        m
    });

    /// C++ functions registered with `isVarLength = true`: one parameter type,
    /// matched against every argument regardless of arity.
    const VAR_LENGTH: &[&str] = &[
        "CONCAT",
        "CONCAT_WS",
        "LIST_CREATION",
        "STRUCT_PACK",
        "ARRAY_VALUE",
        "COALESCE",
    ];

    fn overload_matches(o: &Overload, args: &[LogicalType], var_len: bool) -> bool {
        if var_len {
            let Some(Some(p)) = o.params.first() else {
                return false;
            };
            return args.iter().all(|a| cast_cost(a, p) != UNDEFINED_CAST_COST);
        }
        o.params.len() == args.len()
            && o.params.iter().zip(args).all(|(p, a)| match p {
                Some(p) => cast_cost(a, p) != UNDEFINED_CAST_COST,
                None => false,
            })
    }

    /// Ledgered pure-superset extensions (docs/DIVERGENCES.md
    /// `pure-superset-extensions`): call shapes the C++ engine rejects but this
    /// engine deliberately accepts -- exempt from the catalog gate by
    /// `(name, arity)`.
    const LEDGERED_SUPERSETS: &[(&str, usize)] = &[("SUBSTR", 2), ("SUBSTRING", 2), ("ROUND", 1)];

    /// Parsed AGGREGATE FUNCTION rows: catalog order, with the distinct flag
    /// decoded from the adjacent-duplicate convention (each type registers its
    /// DISTINCT variant first, then the plain one — oracle-verified via the
    /// MIN(NODE) error block).
    struct AggOverload {
        distinct: bool,
        params: Vec<Option<LogicalType>>,
        sig: &'static str,
    }

    static AGG_SIGS: LazyLock<HashMap<&'static str, Vec<AggOverload>>> = LazyLock::new(|| {
        let mut m: HashMap<&'static str, Vec<AggOverload>> = HashMap::new();
        let rows: Vec<(&str, &str)> = FUNCTION_CATALOG
            .iter()
            .filter(|(_, kind, _)| *kind == "AGGREGATE FUNCTION")
            .map(|(name, _, sig)| (*name, *sig))
            .collect();
        let mut i = 0;
        while i < rows.len() {
            let (name, sig) = rows[i];
            let paired = i + 1 < rows.len() && rows[i + 1] == (name, sig);
            let parse = |sig: &'static str| {
                let inner = sig
                    .strip_prefix('(')
                    .and_then(|s| s.split_once(')'))
                    .map(|(p, _)| p)
                    .unwrap_or_default();
                if inner.is_empty() {
                    Vec::new()
                } else {
                    inner.split(',').map(token_type).collect()
                }
            };
            if paired {
                m.entry(name).or_default().push(AggOverload {
                    distinct: true,
                    params: parse(sig),
                    sig,
                });
                m.entry(name).or_default().push(AggOverload {
                    distinct: false,
                    params: parse(sig),
                    sig,
                });
                i += 2;
            } else {
                m.entry(name).or_default().push(AggOverload {
                    distinct: false,
                    params: parse(sig),
                    sig,
                });
                i += 1;
            }
        }
        m
    });

    /// The aggregate gate (C++ `matchAggregateFunction`): arity and distinct
    /// must match exactly and each argument's type-ID must equal the parameter
    /// (ANY skips; no implicit casts). `Some(err)` = catalogued, no candidate.
    pub(super) fn agg_gate(called: &str, args: &[LogicalType], distinct: bool) -> Option<Error> {
        use koko_common::types::cast_cost;
        let upper = called.to_ascii_uppercase();
        let overloads = AGG_SIGS.get(upper.as_str())?;
        let matched = overloads.iter().any(|o| {
            o.distinct == distinct
                && o.params.len() == args.len()
                && o.params.iter().zip(args).all(|(p, a)| match p {
                    Some(LogicalType::Any) => true,
                    Some(p) => cast_cost(a, p) == 0,
                    None => false,
                })
        });
        if matched {
            return None;
        }
        let shown = args
            .iter()
            .map(LogicalType::name)
            .collect::<Vec<_>>()
            .join(",");
        let actual = format!(
            "{}{}",
            if distinct { "DISTINCT " } else { "" },
            if args.is_empty() {
                "()".to_string()
            } else {
                format!("({shown})")
            }
        );
        let mut msg = format!(
            "Function {upper} did not receive correct arguments:\nActual:   {actual}\nExpected: "
        );
        let lines: Vec<String> = overloads
            .iter()
            .filter(|o| !o.params.is_empty())
            .map(|o| {
                if o.distinct {
                    format!("DISTINCT {}", o.sig)
                } else {
                    o.sig.to_string()
                }
            })
            .collect();
        if lines.is_empty() {
            msg.push_str("()");
        } else {
            msg.push_str(&lines[0]);
            for line in &lines[1..] {
                msg.push_str("\n          ");
                msg.push_str(line);
            }
        }
        msg.push_str("\n\n");
        Some(Error::binder(msg))
    }

    /// `Some(err)` = the name is catalogued and no overload accepts the args.
    pub(super) fn gate(called: &str, args: &[LogicalType]) -> Option<Error> {
        let upper = called.to_ascii_uppercase();
        let overloads = SCALAR_SIGS.get(upper.as_str())?;
        // The exemption covers the *documented* superset shape only: each arg
        // must be individually plausible for the function — castable to some
        // parameter of some overload (`round('x')` still errors with the C++
        // block; only `round(1.2)` is the ledgered accept).
        if LEDGERED_SUPERSETS.contains(&(upper.as_str(), args.len()))
            && args.iter().all(|a| {
                overloads.iter().any(|o| {
                    o.params.iter().any(|p| match p {
                        Some(p) => cast_cost(a, p) != UNDEFINED_CAST_COST,
                        None => false,
                    })
                })
            })
        {
            return None;
        }
        let var_len = VAR_LENGTH.contains(&upper.as_str());
        if overloads.iter().any(|o| overload_matches(o, args, var_len)) {
            return None;
        }
        let expected: Vec<&str> = overloads
            .iter()
            .filter(|o| !o.params.is_empty())
            .map(|o| o.sig)
            .collect();
        Some(signature_error(&upper, args, &expected))
    }
}

fn actual_signature(args: &[LogicalType]) -> String {
    if args.is_empty() {
        "()".to_string()
    } else {
        // C++ `LogicalTypeUtils::toString(vector)` joins with `,` (no space).
        format!(
            "({})",
            args.iter()
                .map(LogicalType::name)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

pub fn signature_error(name: &str, args: &[LogicalType], expected: &[&str]) -> Error {
    let mut msg = format!(
        "Function {} did not receive correct arguments:\nActual:   {}\nExpected: ",
        name.to_uppercase(),
        actual_signature(args)
    );
    // C++ `signatureToString` joins parameter types with `,` (no space); the
    // hardcoded overload strings here use `, `, so normalize the separator (the
    // only `, ` in a `(T1, T2) -> R` signature is between types; ` -> ` has none).
    if let Some((first, rest)) = expected.split_first() {
        msg.push_str(&first.replace(", ", ","));
        for line in rest {
            msg.push_str("\n          ");
            msg.push_str(&line.replace(", ", ","));
        }
    } else {
        msg.push_str("()");
    }
    // C++ signature errors end with two trailing newlines (audit §3.4,
    // byte-verified); the corpus runner rtrims both sides, but the raw-message
    // battery compares them.
    msg.push_str("\n\n");
    Error::binder(msg)
}

fn first_concrete_or_string(args: &[LogicalType]) -> LogicalType {
    args.iter()
        .find(|a| **a != LogicalType::Any)
        .cloned()
        .unwrap_or(LogicalType::String)
}

fn integer_compatible(t: &LogicalType) -> bool {
    matches!(
        t,
        LogicalType::Int(_) | LogicalType::Serial | LogicalType::UInt128 | LogicalType::Any
    )
}

fn list_compatible(t: &LogicalType) -> bool {
    matches!(
        t,
        LogicalType::List(_) | LogicalType::Array(_, _) | LogicalType::Any
    )
}

fn list_or_string_compatible(t: &LogicalType) -> bool {
    // Anything that implicitly casts to STRING reaches the (STRING, INT64)
    // overload (a MAP argument char-extracts over its rendering, like C++).
    list_compatible(t)
        || koko_common::types::cast_cost(t, &LogicalType::String)
            != koko_common::types::UNDEFINED_CAST_COST
}

fn list_result_or_any(t: Option<&LogicalType>) -> LogicalType {
    match t {
        Some(LogicalType::List(inner) | LogicalType::Array(inner, _)) => {
            LogicalType::List(inner.clone())
        }
        _ => LogicalType::List(Box::new(LogicalType::Any)),
    }
}

pub fn typeof_type_name(ty: &LogicalType) -> String {
    match ty {
        LogicalType::Any => "NULL".to_string(),
        LogicalType::List(inner) if inner.as_ref() == &LogicalType::Any => "INT64[]".to_string(),
        _ => ty.name(),
    }
}

fn list_concat_child_type(left: &LogicalType, right: &LogicalType) -> Result<LogicalType> {
    match (left, right) {
        (LogicalType::Any, LogicalType::Any) => Ok(LogicalType::Int64),
        (LogicalType::Any, t) | (t, LogicalType::Any) => Ok(t.clone()),
        (l, r) if l == r => Ok(l.clone()),
        _ => Err(Error::binder(format!(
            "list concatenation requires matching child types, got {} and {}",
            left.name(),
            right.name()
        ))),
    }
}

fn list_concat_result_type(args: &[LogicalType], called: &str) -> Result<LogicalType> {
    // Overload-resolution failures (wrong arity, or an arg that is not a LIST) report
    // the *called* name — C++ names the alias the user invoked (e.g. ARRAY_CONCAT), not
    // the canonical LIST_CONCAT.
    if args.len() != 2 {
        return Err(signature_error(called, args, &["(LIST, LIST) -> LIST"]));
    }
    let a_any = args[0] == LogicalType::Any;
    let b_any = args[1] == LogicalType::Any;
    match (args[0].list_child(), args[1].list_child()) {
        // Both args are list-like (LIST or ARRAY): the overload matched; the element
        // types must still agree, and the result is always a variable LIST. C++
        // `ListConcatFunction::bindFunc` reports a mismatch by the *internal* name
        // (LIST_CONCAT) with the full argument types.
        (Some(left), Some(right)) => match list_concat_child_type(left, right) {
            Ok(child) => Ok(LogicalType::List(Box::new(child))),
            Err(_) => Err(Error::binder(format!(
                "Cannot bind LIST_CONCAT with parameter type {} and {}.",
                args[0].name(),
                args[1].name()
            ))),
        },
        // A bare NULL (`Any`) on one side takes the other's element type.
        (Some(inner), None) if b_any => Ok(LogicalType::List(Box::new(inner.clone()))),
        (None, Some(inner)) if a_any => Ok(LogicalType::List(Box::new(inner.clone()))),
        (None, None) if a_any && b_any => Ok(LogicalType::List(Box::new(LogicalType::Int64))),
        _ => Err(signature_error(called, args, &["(LIST, LIST) -> LIST"])),
    }
}

/// Result type of a binary vector function (`array_distance`, `array_dot_product`, …):
/// both args must be `FLOAT[]`/`DOUBLE[]`, at least one a fixed `ARRAY`; the result is the
/// array's scalar element type. Mirrors C++ `validateArrayFunctionParameters`.
fn array_binary_scalar_result_type(called: &str, args: &[LogicalType]) -> Result<LogicalType> {
    if args.len() != 2 {
        return Err(signature_error(called, args, &["(ARRAY, ARRAY) -> ANY"]));
    }
    let fname = called.to_uppercase();
    for a in args {
        if !matches!(
            a.list_child(),
            Some(LogicalType::Float | LogicalType::Double)
        ) {
            return Err(Error::binder(format!(
                "{fname} requires argument type to be FLOAT[] or DOUBLE[]."
            )));
        }
    }
    // Result = the array element type. C++ also requires at least one operand to be a
    // fixed ARRAY, but it reads list *literals* as arrays; since we only see types here,
    // we accept any FLOAT[]/DOUBLE[] operands and prefer a fixed ARRAY's child type.
    let child = args
        .iter()
        .find(|a| matches!(a, LogicalType::Array(_, _)))
        .or(args.first())
        .and_then(|a| a.list_child())
        .unwrap();
    Ok(child.clone())
}

/// Result type of `array_cross_product`: a 3-D `ARRAY` of the (signed-int or float)
/// element type. Mirrors C++ `ArrayCrossProductBindFunc`: a list *literal* is read as an
/// `ARRAY` of its length, then both operands must be the same element type and size
/// (checked *before* element validity); the result preserves that type.
fn array_cross_product_result_type(called: &str, args: &[LogicalType]) -> Result<LogicalType> {
    if args.len() != 2 {
        return Err(signature_error(called, args, &["(ARRAY, ARRAY) -> ARRAY"]));
    }
    let fname = called.to_uppercase();
    let (lchild, rchild) = (args[0].list_child(), args[1].list_child());
    // Two fixed arrays compare fully (element type + length); a list literal has no static
    // length, so we compare element types and defer the size-3 check to evaluation.
    let same = match (&args[0], &args[1]) {
        (LogicalType::Array(l, ln), LogicalType::Array(r, rn)) => l == r && ln == rn,
        _ => lchild.is_some() && lchild == rchild,
    };
    if !same {
        return Err(Error::binder(format!(
            "{fname} requires both arrays to have the same element type and size of 3"
        )));
    }
    let child = lchild.unwrap();
    let valid = matches!(child, LogicalType::Int(k) if k.is_signed())
        || matches!(child, LogicalType::Float | LogicalType::Double);
    if !valid {
        return Err(Error::binder(format!(
            "{fname} can only be applied on array of floating points or signed integers"
        )));
    }
    Ok(LogicalType::Array(Box::new(child.clone()), 3))
}

/// The `f64` elements of an array/list value.
fn array_f64(v: &Value) -> Result<Vec<f64>> {
    match v {
        Value::List(items) => items
            .iter()
            .map(|e| {
                e.as_f64()
                    .ok_or_else(|| Error::conversion("array element is not numeric".to_string()))
            })
            .collect(),
        _ => Err(Error::conversion("expected an array argument".to_string())),
    }
}

/// The two operands of a binary vector function as `f64` vectors, checking equal length
/// (a dimension mismatch is C++'s cast-to-ARRAY length error).
fn array_pair_f64(args: &[Value]) -> Result<(Vec<f64>, Vec<f64>)> {
    let (l, r) = (array_f64(&args[0])?, array_f64(&args[1])?);
    if l.len() != r.len() {
        // C++ rejects the mismatched literal at bind.
        return Err(Error::binder(format!(
            "Cannot change literal expression data type from DOUBLE[{}] to DOUBLE[{}].",
            r.len(),
            l.len()
        )));
    }
    Ok((l, r))
}

/// Wrap a vector-function scalar result in `FLOAT` if the input arrays are `FLOAT[]`,
/// else `DOUBLE` (matching the array's element type).
fn vec_scalar_result(x: f64, sample: &Value) -> Value {
    if matches!(sample, Value::List(items) if matches!(items.first(), Some(Value::Float(_)))) {
        Value::Float(x as f32)
    } else {
        Value::Double(x)
    }
}

/// Fold a list's non-null elements with `f`, accumulating in the child's
/// numeric family (f64 for floats — result FLOAT iff the child is FLOAT — u128
/// for unsigned/UINT128, else i128 wrapped back to the child width like C++'s
/// modular fixed-width arithmetic). Shared by `list_sum`/`list_product`.
fn list_fold(items: &[Value], mul: bool) -> Result<Value> {
    use koko_common::IntKind;
    let vals: Vec<&Value> = items.iter().filter(|v| !v.is_null()).collect();
    if vals
        .iter()
        .any(|v| matches!(v, Value::Double(_) | Value::Float(_)))
    {
        let init = if mul { 1.0 } else { 0.0 };
        let acc = vals
            .iter()
            .filter_map(|v| v.as_f64())
            .fold(init, |a, b| if mul { a * b } else { a + b });
        return Ok(if vals.iter().all(|v| matches!(v, Value::Float(_))) {
            Value::Float(acc as f32)
        } else {
            Value::Double(acc)
        });
    }
    if vals.iter().any(|v| {
        matches!(v, Value::UInt128(_))
            || matches!(
                v,
                Value::IntX {
                    kind: IntKind::U8 | IntKind::U16 | IntKind::U32 | IntKind::U64,
                    ..
                }
            )
    }) {
        let mut acc: u128 = if mul { 1 } else { 0 };
        for v in &vals {
            let x = v.as_u128().unwrap_or(0);
            acc = if mul {
                acc.wrapping_mul(x)
            } else {
                acc.wrapping_add(x)
            };
        }
        // Narrow unsigned widths wrap back to their width; UINT128 stays wide.
        return Ok(match vals.first() {
            Some(Value::IntX { kind, .. }) => wrap_int(acc as i128, *kind),
            _ => Value::UInt128(acc),
        });
    }
    let mut acc: i128 = if mul { 1 } else { 0 };
    for v in &vals {
        let x = v.as_int128().unwrap_or(0);
        acc = if mul {
            acc.wrapping_mul(x)
        } else {
            acc.wrapping_add(x)
        };
    }
    let kind = match vals.first() {
        Some(Value::IntX { kind, .. }) => *kind,
        _ => IntKind::I64,
    };
    Ok(wrap_int(acc, kind))
}

/// Wrap an `i128` to a fixed integer width (two's-complement), matching C++ overflow.
fn wrap_int(value: i128, kind: koko_common::IntKind) -> Value {
    use koko_common::IntKind::*;
    let w = match kind {
        I8 => value as i8 as i128,
        I16 => value as i16 as i128,
        I32 => value as i32 as i128,
        I64 => value as i64 as i128,
        I128 => value,
        U8 => value as u8 as i128,
        U16 => value as u16 as i128,
        U32 => value as u32 as i128,
        U64 => value as u64 as i128,
    };
    Value::make_int(w, kind)
}

/// `a*b - c*d`, preserving the numeric variant of `a` (used by `array_cross_product`).
fn mul_sub(a: &Value, b: &Value, c: &Value, d: &Value) -> Value {
    if let (Some(av), Some(bv), Some(cv), Some(dv)) =
        (a.as_int128(), b.as_int128(), c.as_int128(), d.as_int128())
    {
        let res = av * bv - cv * dv;
        // Preserve the element type, wrapping on overflow like C++'s fixed-width integer
        // arithmetic (modular, so wrapping the exact result once matches per-op wrapping).
        match a {
            Value::IntX { kind, .. } => wrap_int(res, *kind),
            Value::UInt128(_) => Value::UInt128(res as u128),
            _ => Value::Int64(res as i64),
        }
    } else {
        let res = a.as_f64().unwrap_or(0.0) * b.as_f64().unwrap_or(0.0)
            - c.as_f64().unwrap_or(0.0) * d.as_f64().unwrap_or(0.0);
        if matches!(a, Value::Float(_)) {
            Value::Float(res as f32)
        } else {
            Value::Double(res)
        }
    }
}

/// 3-D cross product of two arrays, preserving the element type.
fn array_cross_product_eval(args: &[Value]) -> Result<Value> {
    let (l, r) = match (&args[0], &args[1]) {
        (Value::List(l), Value::List(r)) => (l, r),
        _ => return Err(Error::conversion("expected array arguments".to_string())),
    };
    if l.len() != r.len() || l.len() > 3 {
        return Err(Error::conversion(
            "array_cross_product requires 3-dimensional arrays".to_string(),
        ));
    }
    // C++ computes the 3-D formula with missing components as 0 and returns a
    // result of the INPUT dimension (oracle: 2-D inputs → [0.000000,0.000000],
    // 1-D → [0.000000] — the z-component is truncated away).
    let zero = Value::Double(0.0);
    let at = |v: &[Value], i: usize| v.get(i).cloned().unwrap_or_else(|| zero.clone());
    let full = [
        mul_sub(&at(l, 1), &at(r, 2), &at(l, 2), &at(r, 1)),
        mul_sub(&at(l, 2), &at(r, 0), &at(l, 0), &at(r, 2)),
        mul_sub(&at(l, 0), &at(r, 1), &at(l, 1), &at(r, 0)),
    ];
    Ok(Value::List(full[..l.len()].to_vec()))
}

fn greatest_least_result_type(name: &str, args: &[LogicalType]) -> Result<LogicalType> {
    let expected = &[
        "(DATE, DATE) -> DATE",
        "(TIMESTAMP, TIMESTAMP) -> TIMESTAMP",
    ];
    if args.len() != 2 {
        return Err(signature_error(name, args, expected));
    }
    match (&args[0], &args[1]) {
        (LogicalType::Date, LogicalType::Date)
        | (LogicalType::Date, LogicalType::Any)
        | (LogicalType::Any, LogicalType::Date) => Ok(LogicalType::Date),
        (LogicalType::Timestamp, LogicalType::Timestamp)
        | (LogicalType::Timestamp, LogicalType::Any)
        | (LogicalType::Any, LogicalType::Timestamp) => Ok(LogicalType::Timestamp),
        // Timestamp flavors promote to plain TIMESTAMP (oracle:
        // greatest(TIMESTAMP, TIMESTAMP_NS) casts the NS side).
        (
            LogicalType::Timestamp
            | LogicalType::TimestampNs
            | LogicalType::TimestampMs
            | LogicalType::TimestampSec
            | LogicalType::TimestampTz,
            LogicalType::Timestamp
            | LogicalType::TimestampNs
            | LogicalType::TimestampMs
            | LogicalType::TimestampSec
            | LogicalType::TimestampTz,
        ) => Ok(LogicalType::Timestamp),
        // Two untyped NULLs bind and answer NULL (oracle: greatest(null,null)).
        (LogicalType::Any, LogicalType::Any) => Ok(LogicalType::Any),
        _ => Err(signature_error(name, args, expected)),
    }
}

/// Interim bind-time arity bounds (audit C1): the min/max argument count of every
/// scalar whose result-type path doesn't validate arity itself, so a wrong-arity
/// call is a clean binder error instead of an `args[i]` eval panic. Bounds mirror
/// the C++ overload tables (verified against the oracle shell), except where Rust
/// deliberately accepts a wider form C++ lacks (1-arg `round`, 2-arg `substr` —
/// pure-superset extension space, ledgered in `docs/DIVERGENCES.md`). `None` means
/// the function's bespoke check in [`scalar_result_type`] owns arity (and its
/// C++-parity signature error). The declarative signature catalog (M3) replaces this.
fn arity_bounds(name: &str) -> Option<(usize, usize)> {
    const MANY: usize = usize::MAX;
    Some(match name {
        "pi" | "gen_random_uuid" | "current_date" | "current_timestamp" | "random" => (0, 0),
        "count_if" | "octet_length" | "encode" | "decode" | "epoch_ms" | "to_epoch_ms" | "hash"
        | "md5" | "sha256" | "setseed" | "error" => (1, 1),
        "internal_id"
        | "list_has_all"
        | "equals"
        | "not_equals"
        | "greater_than"
        | "greater_than_equals"
        | "less_than"
        | "less_than_equals" => (2, 2),
        "date" | "timestamp" | "interval" | "duration" | "uuid" | "string" | "blob"
        | "to_timestamp" | "century" | "abs" | "negate" | "floor" | "ceil" | "ceiling" | "sign"
        | "even" | "factorial" | "sqrt" | "cbrt" | "ln" | "log" | "log2" | "log10" | "exp"
        | "sin" | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "degrees" | "radians"
        | "gamma" | "lgamma" | "typeof" | "union_tag" | "id" | "offset" | "label" | "labels"
        | "nodes" | "rels" | "relationships" | "lower" | "lcase" | "upper" | "ucase" | "trim"
        | "ltrim" | "rtrim" | "reverse" | "initcap" | "length" | "dayname" | "monthname"
        | "last_day" | "cardinality" | "map_keys" | "map_values" | "to_years" | "to_months"
        | "to_days" | "to_hours" | "to_minutes" | "to_seconds" | "to_milliseconds"
        | "to_microseconds" => (1, 1),
        "pow"
        | "power"
        | "atan2"
        | "bitwise_and"
        | "bitwise_or"
        | "bitwise_xor"
        | "bitshift_left"
        | "bitshift_right"
        | "contains"
        | "prefix"
        | "suffix"
        | "starts_with"
        | "ends_with"
        | "left"
        | "right"
        | "repeat"
        | "string_split"
        | "str_split"
        | "string_to_array"
        | "levenshtein"
        | "regexp_matches"
        | "regexp_full_match"
        | "regexp_split_to_array"
        | "struct_extract"
        | "map"
        | "map_extract"
        | "element_at"
        | "date_part"
        | "datepart"
        | "date_trunc"
        | "datetrunc"
        | "properties"
        | "union_extract"
        | "nullif" => (2, 2),
        "make_date" | "lpad" | "rpad" | "replace" | "split_part" => (3, 3),
        "round" => (1, 2),
        "substr" | "substring" => (2, 3),
        "regexp_replace" => (3, 4),
        "regexp_extract" | "regexp_extract_all" => (2, 3),
        "concat" | "list_creation" => (0, MANY),
        _ => return None,
    })
}

/// The INT64 argument positions of functions whose C++ signature rejects
/// non-integer args with the overload table (audit V4 — the old eval path
/// yielded garbage: `left('hello', 2.0)` → `''`, `factorial('5')` → `1`), plus
/// the verbatim C++ "Expected:" lines. Lossless integer upcasts (INT8..INT64,
/// SERIAL, UINT8..UINT32) are accepted; DOUBLE/STRING/INT128/UINT64+ reject —
/// all oracle-verified. The M3 declarative catalog supersedes this table.
fn int_strict_positions(name: &str) -> Option<(&'static [usize], &'static [&'static str])> {
    Some(match name {
        "left" => (&[1], &["(STRING, INT64) -> STRING"]),
        "right" => (&[1], &["(STRING, INT64) -> STRING"]),
        "lpad" => (&[1], &["(STRING, INT64, STRING) -> STRING"]),
        "rpad" => (&[1], &["(STRING, INT64, STRING) -> STRING"]),
        "substr" | "substring" => (&[1, 2], &["(STRING, INT64, INT64) -> STRING"]),
        "repeat" => (&[1], &["(STRING, INT64) -> STRING"]),
        "split_part" => (&[2], &["(STRING, STRING, INT64) -> STRING"]),
        "factorial" => (&[0], &["(INT64) -> INT64"]),
        "round" => (&[1], &["(DOUBLE, INT64) -> DOUBLE"]),
        "regexp_extract" | "regexp_extract_all" => (&[2], &[]),
        _ => return None,
    })
}

/// Whether a type is accepted at a C++ INT64 parameter position: only lossless
/// integer upcasts (oracle-verified: INT8/UINT32 pass, INT128/UINT64/DOUBLE
/// reject).
fn int64_upcastable(t: &LogicalType) -> bool {
    use koko_common::IntKind::*;
    matches!(
        t,
        LogicalType::Any
            | LogicalType::Serial
            | LogicalType::Int(I8 | I16 | I32 | I64 | U8 | U16 | U32)
    )
}

/// The result [`LogicalType`] of scalar function `name` over `args`.
/// The aggregate-signature gate (see `sigcatalog::agg_gate`), for the binder's
/// aggregate dispatch. `Some(err)` = catalogued name, no matching overload.
pub fn aggregate_signature_error(
    name: &str,
    args: &[LogicalType],
    distinct: bool,
) -> Option<Error> {
    sigcatalog::agg_gate(name, args, distinct)
}

/// Run only the catalog signature gate (as-called name, raw argument types) —
/// the binder calls this BEFORE its STRING-position coercions so signature
/// errors render the ORIGINAL actual types (C++ matchFunction order).
pub fn signature_gate(called: &str, args: &[LogicalType]) -> Result<()> {
    match sigcatalog::gate(&called.to_ascii_lowercase(), args) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

pub fn scalar_result_type(name: &str, args: &[LogicalType]) -> Result<LogicalType> {
    let called = name.to_ascii_lowercase();
    // The catalog signature gate runs first, under the *as-called* name (each
    // C++ alias is its own catalog entry, and the error echoes the called name);
    // per-name logic below then applies its stricter bind-time refinements.
    // concat_ws validates before the generic gate: its own arity wording, and
    // a strict all-STRING rule (no coercion) with the C++ message.
    if called == "concat_ws" {
        if args.len() < 2 {
            return Err(Error::binder(format!(
                "concat_ws expects at least two parameters. Got: {}.",
                args.len()
            )));
        }
        for a in args {
            if !matches!(a, LogicalType::String | LogicalType::Any) {
                return Err(Error::binder(format!(
                    "concat_ws expects all string parameters. Got: {}.",
                    a.name()
                )));
            }
        }
        return Ok(LogicalType::String);
    }
    if let Some(err) = sigcatalog::gate(&called, args) {
        return Err(err);
    }
    let name = canonical(&called).to_string();
    let arity_err = || {
        Error::binder(format!(
            "Function {} got {} argument(s).",
            name.to_uppercase(),
            args.len()
        ))
    };
    if let Some((min, max)) = arity_bounds(&name) {
        if args.len() < min || args.len() > max {
            return Err(arity_err());
        }
    }
    // C++ INT64-position strictness (audit V4): a non-upcastable arg is a bind
    // error with the overload table, never a garbage value at eval.
    if let Some((positions, expected)) = int_strict_positions(&name) {
        for &i in positions {
            if let Some(t) = args.get(i) {
                if !int64_upcastable(t) {
                    return Err(signature_error(&called, args, expected));
                }
            }
        }
    }
    if let Some(target) = cast_alias_target(&name) {
        if args.len() != 1 {
            return Err(arity_err());
        }
        return Ok(target);
    }
    // `size` accepts only LIST/ARRAY/MAP/STRING; a NODE/REL/RECURSIVE_REL arg is a
    // binder error with the C++ signature listing (matches `path.test`).
    if name == "size" {
        // Numerics reach SIZE via the (STRING) overload (implicit cast):
        // size(12345) is the rendered string's length, 5.
        let ok = args.len() == 1
            && (args[0].is_numeric()
                || matches!(
                    &args[0],
                    LogicalType::List(_)
                        | LogicalType::Array(_, _)
                        | LogicalType::Map(_, _)
                        | LogicalType::String
                        | LogicalType::Any
                ));
        if !ok {
            return Err(signature_error(
                &name,
                args,
                &[
                    "(LIST) -> INT64",
                    "(ARRAY) -> INT64",
                    "(MAP) -> INT64",
                    "(STRING) -> INT64",
                ],
            ));
        }
        return Ok(LogicalType::Int64);
    }
    match name.as_str() {
        "coalesce" => {
            if args.is_empty() {
                return Err(Error::binder(
                    "COALESCE requires at least one argument".to_string(),
                ));
            }
            return Ok(first_concrete_or_string(args));
        }
        "ifnull" => {
            if args.len() != 2 {
                return Err(signature_error(&name, args, &["(ANY, ANY) -> ANY"]));
            }
            return Ok(first_concrete_or_string(args));
        }
        "constant_or_null" => {
            if args.len() != 2 {
                return Err(signature_error(&name, args, &["(ANY, ANY) -> ANY"]));
            }
            return Ok(if args[0] == LogicalType::Any {
                LogicalType::String
            } else {
                args[0].clone()
            });
        }
        "range" => {
            if !(args.len() == 2 || args.len() == 3) || !args.iter().all(integer_compatible) {
                // The verbatim C++ per-width overload table (oracle-captured).
                const RANGE_EXPECTED: &[&str] = &[
                    "(INT128,INT128) -> LIST",
                    "(INT128,INT128,INT128) -> LIST",
                    "(INT64,INT64) -> LIST",
                    "(INT64,INT64,INT64) -> LIST",
                    "(INT32,INT32) -> LIST",
                    "(INT32,INT32,INT32) -> LIST",
                    "(INT16,INT16) -> LIST",
                    "(INT16,INT16,INT16) -> LIST",
                    "(INT8,INT8) -> LIST",
                    "(INT8,INT8,INT8) -> LIST",
                    "(SERIAL,SERIAL) -> LIST",
                    "(SERIAL,SERIAL,SERIAL) -> LIST",
                    "(UINT128,UINT128) -> LIST",
                    "(UINT128,UINT128,UINT128) -> LIST",
                    "(UINT64,UINT64) -> LIST",
                    "(UINT64,UINT64,UINT64) -> LIST",
                    "(UINT32,UINT32) -> LIST",
                    "(UINT32,UINT32,UINT32) -> LIST",
                    "(UINT16,UINT16) -> LIST",
                    "(UINT16,UINT16,UINT16) -> LIST",
                    "(UINT8,UINT8) -> LIST",
                    "(UINT8,UINT8,UINT8) -> LIST",
                ];
                return Err(signature_error(&name, args, RANGE_EXPECTED));
            }
            // C++ registers per-width overloads: the element type is the (uniform)
            // endpoint type — range(UINT128, UINT128) -> UINT128[] (audit V16).
            let elem = if args
                .iter()
                .all(|a| matches!(a, LogicalType::Any) || a == &args[0])
            {
                match &args[0] {
                    t @ (LogicalType::Int(_) | LogicalType::UInt128) => t.clone(),
                    _ => LogicalType::Int64,
                }
            } else {
                LogicalType::Int64
            };
            return Ok(LogicalType::List(Box::new(elem)));
        }
        "list_extract" | "list_element" | "array_extract" => {
            if args.len() != 2
                || !list_or_string_compatible(&args[0])
                || !integer_compatible(&args[1])
            {
                return Err(signature_error(
                    &name,
                    args,
                    &[
                        "(LIST, INT64) -> ANY",
                        "(STRING, INT64) -> STRING",
                        "(ARRAY, INT64) -> ANY",
                    ],
                ));
            }
            // ARRAY_EXTRACT's only C++ overload is (STRING,INT64) -> STRING —
            // a LIST argument implicitly casts to STRING and gets *character*
            // extraction (oracle: array_extract([10,20,30], 1) = '[').
            if name == "array_extract" {
                return Ok(LogicalType::String);
            }
            return Ok(match &args[0] {
                LogicalType::List(inner) | LogicalType::Array(inner, _) => (**inner).clone(),
                // MAP/STRUCT/numerics take the (STRING, INT64) overload.
                _ => LogicalType::String,
            });
        }
        "list_slice" | "array_slice" => {
            if args.len() != 3
                || !list_or_string_compatible(&args[0])
                || !integer_compatible(&args[1])
                || !integer_compatible(&args[2])
            {
                return Err(signature_error(
                    &name,
                    args,
                    &[
                        "(LIST, INT64, INT64) -> LIST",
                        "(ARRAY, INT64, INT64) -> LIST",
                        "(STRING, INT64, INT64) -> STRING",
                    ],
                ));
            }
            return Ok(match &args[0] {
                LogicalType::String | LogicalType::Any => LogicalType::String,
                // A slice of an ARRAY is a variable-length LIST (it loses the fixed
                // length) — never the input ARRAY type, else the mistyped result would
                // bypass ARRAY length enforcement when written back.
                LogicalType::Array(inner, _) => LogicalType::List(inner.clone()),
                t => t.clone(),
            });
        }
        "list_concat" | "list_cat" => {
            return list_concat_result_type(args, &called);
        }
        // Fixed-array constructor: `ARRAY(common_type(args), argcount)`.
        "array_value" => {
            let inner = first_concrete_or_string(args);
            return Ok(LogicalType::Array(Box::new(inner), args.len() as u64));
        }
        // Binary vector functions returning a scalar (the array's FLOAT/DOUBLE child).
        "array_distance"
        | "array_squared_distance"
        | "array_inner_product"
        | "array_dot_product"
        | "array_cosine_similarity" => {
            return array_binary_scalar_result_type(&called, args);
        }
        // 3-D cross product returning an `ARRAY` of the (preserved) element type.
        "array_cross_product" => {
            return array_cross_product_result_type(&called, args);
        }
        "list_append" | "list_prepend" => {
            if args.len() != 2 || !list_compatible(&args[0]) {
                return Err(signature_error(&name, args, &["(LIST, ANY) -> LIST"]));
            }
            return Ok(match &args[0] {
                LogicalType::List(inner) | LogicalType::Array(inner, _) => {
                    LogicalType::List(inner.clone())
                }
                _ => LogicalType::List(Box::new(args[1].clone())),
            });
        }
        "list_contains" => {
            if args.len() != 2 || !list_compatible(&args[0]) {
                return Err(signature_error(&name, args, &["(LIST, ANY) -> BOOL"]));
            }
            return Ok(LogicalType::Bool);
        }
        "list_position" | "list_indexof" => {
            if args.len() != 2 || !list_compatible(&args[0]) {
                return Err(signature_error(&name, args, &["(LIST, ANY) -> INT64"]));
            }
            return Ok(LogicalType::Int64);
        }
        "list_any_value" => {
            if args.len() != 1 || !list_compatible(&args[0]) {
                return Err(signature_error(&name, args, &["(LIST) -> ANY"]));
            }
            return Ok(match &args[0] {
                LogicalType::List(inner) | LogicalType::Array(inner, _) => (**inner).clone(),
                _ => LogicalType::Any,
            });
        }
        "list_reverse" | "list_distinct" => {
            if args.len() != 1 || !list_compatible(&args[0]) {
                return Err(signature_error(&name, args, &["(LIST) -> LIST"]));
            }
            return Ok(list_result_or_any(args.first()));
        }
        "list_unique" => {
            if args.len() != 1 || !list_compatible(&args[0]) {
                return Err(signature_error(&name, args, &["(LIST) -> INT64"]));
            }
            return Ok(LogicalType::Int64);
        }
        "list_sum" | "list_product" => {
            if args.len() != 1 || !list_compatible(&args[0]) {
                return Err(signature_error(&name, args, &["(LIST) -> INT64"]));
            }
            // C++ dispatches on the child type and keeps it (INT16[] -> INT16,
            // FLOAT[] -> FLOAT, UINT128[] -> UINT128 — audit V16); a
            // non-numeric child is its bespoke binder error.
            return Ok(match args.first() {
                Some(LogicalType::List(inner) | LogicalType::Array(inner, _)) => match &**inner {
                    t @ (LogicalType::Double
                    | LogicalType::Float
                    | LogicalType::Int(_)
                    | LogicalType::UInt128
                    | LogicalType::Serial) => t.clone(),
                    LogicalType::Any => LogicalType::Int64,
                    other => {
                        return Err(Error::binder(format!(
                            "Unsupported inner data type for {}: {}",
                            name.to_uppercase(),
                            other.name()
                        )));
                    }
                },
                _ => LogicalType::Int64,
            });
        }
        "list_sort" => {
            if !(1..=3).contains(&args.len())
                || !list_compatible(&args[0])
                || args
                    .iter()
                    .skip(1)
                    .any(|a| !matches!(a, LogicalType::String | LogicalType::Any))
            {
                return Err(signature_error(
                    &name,
                    args,
                    &[
                        "(LIST) -> LIST",
                        "(LIST, STRING) -> LIST",
                        "(LIST, STRING, STRING) -> LIST",
                    ],
                ));
            }
            return Ok(list_result_or_any(args.first()));
        }
        "list_reverse_sort" => {
            if !(1..=2).contains(&args.len())
                || !list_compatible(&args[0])
                || args
                    .iter()
                    .skip(1)
                    .any(|a| !matches!(a, LogicalType::String | LogicalType::Any))
            {
                return Err(signature_error(
                    &name,
                    args,
                    &["(LIST) -> LIST", "(LIST, STRING) -> LIST"],
                ));
            }
            return Ok(list_result_or_any(args.first()));
        }
        "list_to_string" => {
            if args.len() != 2
                || !matches!(&args[0], LogicalType::String | LogicalType::Any)
                || !list_compatible(&args[1])
            {
                return Err(signature_error(&name, args, &["(STRING, LIST) -> STRING"]));
            }
            return Ok(LogicalType::String);
        }
        _ => {}
    }
    let ty = match name.as_str() {
        "date" | "make_date" => LogicalType::Date,
        "timestamp" | "to_timestamp" => LogicalType::Timestamp,
        "interval" | "duration" => LogicalType::Interval,
        "uuid" | "gen_random_uuid" => LogicalType::Uuid,
        "blob" => LogicalType::Blob,
        "typeof" | "string" | "union_tag" => LogicalType::String,
        // `union_extract(u, 'field')` yields the member type, resolved at eval from
        // the runtime union value (like `struct_extract`).
        "union_extract" => LogicalType::Any,
        // Node/rel accessors.
        "id" => LogicalType::InternalId,
        "offset" => LogicalType::Int64,
        // `labels` aliases `label` — scalar STRING in the C++ oracle (audit V8).
        "label" | "labels" => LogicalType::String,
        "century" => LogicalType::Int64,
        "pi" => LogicalType::Double,
        // Keep the operand's numeric type — except floor/ceil on DECIMAL(p,s),
        // which reduce to DECIMAL(p,0) like C++ (audit R2).
        "floor" | "ceil" | "ceiling" => match args.first() {
            Some(LogicalType::Decimal(p, _)) => LogicalType::Decimal(*p, 0),
            other => other.cloned().unwrap_or(LogicalType::Any),
        },
        "abs" | "negate" => args.first().cloned().unwrap_or(LogicalType::Any),
        "round" if args.len() == 2 => LogicalType::Double,
        "round" => args.first().cloned().unwrap_or(LogicalType::Any),
        // Pure double-valued math.
        "sqrt" | "cbrt" | "ln" | "log" | "log2" | "log10" | "exp" | "pow" | "power" | "sin"
        | "cos" | "tan" | "cot" | "asin" | "acos" | "atan" | "atan2" | "degrees" | "radians"
        | "gamma" | "lgamma" | "even" => LogicalType::Double,
        // sign() is always INT64, regardless of the operand's numeric type.
        "sign" | "factorial" => LogicalType::Int64,
        "bitwise_and" | "bitwise_or" | "bitwise_xor" | "bitshift_left" | "bitshift_right" => {
            LogicalType::Int64
        }
        "greatest" | "least" => greatest_least_result_type(&name, args)?,
        // Common type of the arguments (first non-Any wins).
        "nullif" => args
            .iter()
            .find(|a| **a != LogicalType::Any)
            .cloned()
            .unwrap_or(LogicalType::Any),
        "count_if" => LogicalType::Int(koko_common::IntKind::U8),
        "current_date" => LogicalType::Date,
        "current_timestamp" => LogicalType::Timestamp,
        "octet_length" | "to_epoch_ms" => LogicalType::Int64,
        "encode" => LogicalType::Blob,
        "decode" | "md5" | "sha256" => LogicalType::String,
        "epoch_ms" => LogicalType::Timestamp,
        "hash" => LogicalType::Int(koko_common::IntKind::U64),
        "internal_id" => LogicalType::InternalId,
        "random" => LogicalType::Double,
        "setseed" | "error" => LogicalType::Int(koko_common::IntKind::I32),
        // cost(recursive_rel) — registered so `cost()` gets the catalog
        // signature error; the weighted-path value lands with WSHORTEST.
        "cost" => LogicalType::Double,
        // rowid(node) — the node's internal offset.
        "rowid" => LogicalType::Int64,
        "is_trail" | "is_acyclic" => LogicalType::Bool,
        // start_node/end_node(rel) — the physical endpoint nodes; the element
        // type resolves from the runtime value.
        "start_node" | "end_node" => LogicalType::Any,
        "list_has_all"
        | "equals"
        | "not_equals"
        | "greater_than"
        | "greater_than_equals"
        | "less_than"
        | "less_than_equals" => LogicalType::Bool,
        // String functions.
        "concat" | "lower" | "lcase" | "upper" | "ucase" | "trim" | "ltrim" | "rtrim"
        | "initcap" | "left" | "right" | "lpad" | "rpad" | "substr" | "substring" | "repeat"
        | "split_part" | "replace" | "regexp_replace" | "regexp_extract" => LogicalType::String,
        "contains" | "prefix" | "suffix" | "starts_with" | "ends_with" | "regexp_matches"
        | "regexp_full_match" => LogicalType::Bool,
        "length" | "levenshtein" => LogicalType::Int64,
        // Recursive-rel / path accessors. `nodes`/`rels` extract the node/rel
        // value lists; `properties(list, key)` maps a property over them. The
        // element types are resolved from the runtime values, so they type as
        // `LIST(ANY)` here (display is value-driven).
        // nodes()/rels() type as NODE/REL lists (table resolved from runtime
        // values — the sentinel id renders plain "NODE"/"REL" in errors).
        "nodes" => LogicalType::List(Box::new(LogicalType::Node(koko_common::TableId(u64::MAX)))),
        "rels" | "relationships" => {
            LogicalType::List(Box::new(LogicalType::Rel(koko_common::TableId(u64::MAX))))
        }
        "properties" => LogicalType::List(Box::new(LogicalType::Any)),
        "reverse" => match args.first() {
            // `reverse` of an ARRAY yields a LIST (matches the list oracle).
            Some(LogicalType::Array(inner, _)) => LogicalType::List(inner.clone()),
            Some(t) => t.clone(),
            None => LogicalType::String,
        },
        "string_split" | "regexp_extract_all" | "regexp_split_to_array" => {
            LogicalType::List(Box::new(LogicalType::String))
        }
        // List construction/string splitting functions not covered by fixed
        // C++ signatures above.
        "list_creation" => {
            let inner = args.first().cloned().unwrap_or(LogicalType::Any);
            LogicalType::List(Box::new(inner))
        }
        "str_split" | "string_to_array" => LogicalType::List(Box::new(LogicalType::String)),
        // Struct / map functions.
        "struct_extract" => LogicalType::Any, // resolved from the field at eval
        "map" | "map_checked" => match (args.first(), args.get(1)) {
            (Some(LogicalType::List(k)), Some(LogicalType::List(v))) => {
                LogicalType::Map(k.clone(), v.clone())
            }
            _ => LogicalType::Map(Box::new(LogicalType::Any), Box::new(LogicalType::Any)),
        },
        "map_extract" | "element_at" => match args.first() {
            Some(LogicalType::Map(_, v)) => LogicalType::List(v.clone()),
            _ => LogicalType::List(Box::new(LogicalType::Any)),
        },
        "map_keys" => match args.first() {
            Some(LogicalType::Map(k, _)) => LogicalType::List(k.clone()),
            _ => LogicalType::List(Box::new(LogicalType::Any)),
        },
        "map_values" => match args.first() {
            Some(LogicalType::Map(_, v)) => LogicalType::List(v.clone()),
            _ => LogicalType::List(Box::new(LogicalType::Any)),
        },
        "cardinality" => LogicalType::Int64,
        // Date / timestamp / interval functions.
        "dayname" | "monthname" => LogicalType::String,
        "date_part" | "datepart" => LogicalType::Int64,
        "last_day" => LogicalType::Date,
        "date_trunc" | "datetrunc" => match args.get(1) {
            Some(
                LogicalType::Timestamp
                | LogicalType::TimestampNs
                | LogicalType::TimestampMs
                | LogicalType::TimestampSec
                | LogicalType::TimestampTz,
            ) => LogicalType::Timestamp,
            _ => LogicalType::Date,
        },
        "to_years" | "to_months" | "to_days" | "to_hours" | "to_minutes" | "to_seconds"
        | "to_milliseconds" | "to_microseconds" => LogicalType::Interval,
        _ => {
            return Err(Error::binder(format!(
                "scalar function {name} is not registered"
            )));
        }
    };
    Ok(ty)
}

/// Evaluate scalar function `name` over already-evaluated `args` with
/// connection-owned nondeterministic-function state.
pub fn eval_with_context(
    name: &str,
    args: &[Value],
    random: &crate::oracle_hash::RandomState,
) -> Result<Value> {
    let name = canonical(&name.to_ascii_lowercase()).to_string();

    // Cast aliases delegate to the cast matrix (NULL → NULL inside cast_value).
    // The `to_uint*` functions reject a negative INT128 with the C++
    // CastToUnsigned wording (to_uint8(to_int128(-1))); narrower sources keep
    // the range message (to_uint64(-500) — oracle-verified split).
    if let Some(target) = cast_alias_target(&name) {
        let unsigned_target = matches!(
            target,
            LogicalType::Int(k) if !k.is_signed()
        ) || matches!(target, LogicalType::UInt128);
        if unsigned_target
            && matches!(
                args[0].logical_type(),
                LogicalType::Int(koko_common::IntKind::I128)
            )
        {
            if let Some(n) = args[0].as_int128() {
                if n < 0 {
                    // UINT128 has its own C++ wording (no trailing period).
                    if matches!(target, LogicalType::UInt128) {
                        return Err(koko_common::Error::overflow(format!(
                            "Cannot cast negative INT128 value {n} to UINT128"
                        )));
                    }
                    return Err(koko_common::Error::overflow(format!(
                        "Cast failed. Cannot cast {n} to unsigned type."
                    )));
                }
            }
        }
        // The C++ to_* functions have no BOOL overloads (and to_bool only a
        // STRING one): such arguments bind as CAST(arg, STRING) and take the
        // string-parse path, with its errors (`to_int8(true)` → `Cast failed.
        // Could not convert "True" to INT8.`; `to_bool(2)` → `Value 2 is not
        // a valid boolean`).
        let string_route = (matches!(args[0], Value::Bool(_))
            && !matches!(target, LogicalType::Bool | LogicalType::String))
            || (matches!(target, LogicalType::Bool)
                && !matches!(args[0], Value::Bool(_) | Value::String(_)));
        if string_route {
            return cast_value(&Value::String(args[0].to_result_string()), &target);
        }
        return cast_value(&args[0], &target);
    }

    // Null-aware functions (must see NULLs).
    match name.as_str() {
        "pi" => return Ok(Value::Double(std::f64::consts::PI)),
        "gen_random_uuid" => return Ok(Value::Uuid(random.next_uuid())),
        "typeof" => return Ok(Value::String(typeof_type_name(&args[0].logical_type()))),
        // Fixed-array constructor — like `list_creation`, but NULL-aware so element NULLs
        // are preserved rather than propagating to a NULL result.
        "array_value" => return Ok(Value::List(args.to_vec())),
        "concat" => {
            return Ok(Value::String(
                args.iter()
                    .filter(|a| !a.is_null())
                    .map(|a| a.to_result_string())
                    .collect(),
            ));
        }
        "concat_ws" => {
            // NULL separator -> NULL. NULL values are skipped, and the
            // separator is emitted before a value only when the *immediately
            // preceding* value argument was non-NULL (oracle:
            // concat_ws('-','a','b',NULL,'c') = 'a-bc').
            if args[0].is_null() {
                return Ok(Value::Null);
            }
            let sep = args[0].to_result_string();
            let mut out = String::new();
            for (i, a) in args[1..].iter().enumerate() {
                if a.is_null() {
                    continue;
                }
                if i > 0 && !args[i].is_null() {
                    out.push_str(&sep);
                }
                out.push_str(&a.to_result_string());
            }
            return Ok(Value::String(out));
        }
        // hash() mirrors the C++ vector executor: a NULL input hashes to
        // NULL_HASH (UINT64_MAX), not to NULL.
        "hash" => {
            return Ok(Value::IntX {
                value: crate::oracle_hash::hash_value(&args[0])? as i128,
                kind: koko_common::IntKind::U64,
            });
        }
        "coalesce" | "ifnull" => {
            return Ok(args
                .iter()
                .find(|a| !a.is_null())
                .cloned()
                .unwrap_or(Value::Null));
        }
        "nullif" => {
            // NULL if a == b, else a.
            return Ok(match crate::cypher_cmp(&args[0], &args[1]) {
                Some(std::cmp::Ordering::Equal) => Value::Null,
                _ => args[0].clone(),
            });
        }
        "constant_or_null" => {
            // constant_or_null(value, ...): NULL if any trailing arg is NULL.
            return Ok(if args[1..].iter().any(|a| a.is_null()) {
                Value::Null
            } else {
                args[0].clone()
            });
        }
        _ => {}
    }

    // NULL propagation for the remaining (strict) functions.
    if args.iter().any(|a| a.is_null()) {
        return Ok(Value::Null);
    }

    match name.as_str() {
        "count_if" => {
            let truthy = match &args[0] {
                Value::Bool(b) => *b,
                v => v
                    .as_f64()
                    .map(|f| f != 0.0)
                    .or_else(|| v.as_int128().map(|n| n != 0))
                    .ok_or_else(|| {
                        Error::binder(format!(
                            "count_if expects a numeric or boolean argument, got {}.",
                            v.logical_type().name()
                        ))
                    })?,
            };
            Ok(Value::IntX {
                value: truthy as i128,
                kind: koko_common::IntKind::U8,
            })
        }
        "current_date" => {
            let secs = std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Ok(Value::Date((secs / 86_400) as i32))
        }
        "current_timestamp" => {
            let micros = std::time::UNIX_EPOCH
                .elapsed()
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0);
            Ok(Value::Timestamp(micros))
        }
        "octet_length" => match &args[0] {
            Value::Blob(b) => Ok(Value::Int64(b.len() as i64)),
            v => Ok(Value::Int64(v.to_result_string().len() as i64)),
        },
        "encode" => Ok(Value::Blob(arg_str(&args[0])?.as_bytes().to_vec())),
        "decode" => match &args[0] {
            Value::Blob(b) => match std::str::from_utf8(b) {
                Ok(t) => Ok(Value::String(t.to_string())),
                Err(_) => Err(Error::runtime(
                    "Failure in decode: could not convert blob to UTF8 string, \
                     the blob contained invalid UTF8 characters"
                        .to_string(),
                )),
            },
            v => Ok(Value::String(v.to_result_string())),
        },
        "epoch_ms" => {
            let n = args[0].as_int128().unwrap_or(0) as i64;
            Ok(Value::Timestamp(n.wrapping_mul(1000)))
        }
        "to_epoch_ms" => match &args[0] {
            Value::Timestamp(t) | Value::TimestampTz(t) => Ok(Value::Int64(t / 1000)),
            // DATE promotes implicitly (midnight UTC).
            Value::Date(d) => Ok(Value::Int64(i64::from(*d) * 86_400_000)),
            v => Err(Error::binder(format!(
                "to_epoch_ms expects a TIMESTAMP, got {}.",
                v.logical_type().name()
            ))),
        },
        "md5" => Ok(Value::String(crate::digest::md5_hex(
            arg_str(&args[0])?.as_bytes(),
        ))),
        "sha256" => Ok(Value::String(crate::digest::sha256_hex(
            arg_str(&args[0])?.as_bytes(),
        ))),
        // start_node/end_node: the rel's materialized physical endpoints.
        "start_node" | "end_node" => match &args[0] {
            Value::Rel(r) => {
                let ep = if name == "start_node" {
                    &r.src_node
                } else {
                    &r.dst_node
                };
                Ok(ep
                    .as_ref()
                    .map(|n| Value::Node(n.clone()))
                    .unwrap_or(Value::Null))
            }
            v => Err(Error::binder(format!(
                "{} expects a REL argument, got {}.",
                name.to_uppercase(),
                v.logical_type().name()
            ))),
        },
        // is_trail: no repeated relationship; is_acyclic: no repeated node.
        "is_trail" | "is_acyclic" => match &args[0] {
            Value::RecursiveRel(r) => {
                let ids: Vec<_> = if name == "is_trail" {
                    r.rels.iter().map(|e| e.id).collect()
                } else {
                    r.nodes.iter().map(|n| n.id).collect()
                };
                let mut seen = std::collections::HashSet::new();
                Ok(Value::Bool(ids.into_iter().all(|id| seen.insert(id))))
            }
            v => Err(Error::binder(format!(
                "{} expects a RECURSIVE_REL argument, got {}.",
                name.to_uppercase(),
                v.logical_type().name()
            ))),
        },
        "rowid" => match &args[0] {
            Value::InternalId(id) => Ok(Value::Int64(id.offset.0 as i64)),
            Value::Node(n) => Ok(Value::Int64(n.id.offset.0 as i64)),
            Value::Rel(r) => Ok(Value::Int64(r.id.offset.0 as i64)),
            v => Err(Error::binder(format!(
                "rowid expects a NODE or REL argument, got {}.",
                v.logical_type().name()
            ))),
        },
        "internal_id" => {
            let t = args[0].as_int128().unwrap_or(0) as u64;
            let o = args[1].as_int128().unwrap_or(0) as u64;
            Ok(Value::InternalId(koko_common::InternalId::new(
                koko_common::TableId(t),
                o,
            )))
        }
        "random" => Ok(Value::Double(random.next_random())),
        "setseed" => {
            if let Some(seed) = args[0].as_f64() {
                random.set_seed(seed);
            }
            Ok(Value::Null)
        }
        "error" => Err(Error::runtime(arg_str(&args[0])?.to_string())),
        "equals"
        | "not_equals"
        | "greater_than"
        | "greater_than_equals"
        | "less_than"
        | "less_than_equals" => {
            let ord = crate::cypher_cmp(&args[0], &args[1]);
            let b = match (name.as_str(), ord) {
                (_, None) => return Ok(Value::Null),
                ("equals", Some(o)) => o == std::cmp::Ordering::Equal,
                ("not_equals", Some(o)) => o != std::cmp::Ordering::Equal,
                ("greater_than", Some(o)) => o == std::cmp::Ordering::Greater,
                ("greater_than_equals", Some(o)) => o != std::cmp::Ordering::Less,
                ("less_than", Some(o)) => o == std::cmp::Ordering::Less,
                (_, Some(o)) => o != std::cmp::Ordering::Greater,
            };
            Ok(Value::Bool(b))
        }
        "list_has_all" => {
            let hay = arg_list(&args[0])?;
            let needles = arg_list(&args[1])?;
            Ok(Value::Bool(needles.iter().all(|n| {
                n.is_null()
                    || hay
                        .iter()
                        .any(|h| crate::cypher_cmp(h, n) == Some(std::cmp::Ordering::Equal))
            })))
        }
        "greatest" | "least" => eval_greatest_least(&name, args),
        // `union_tag(u)` → the active member's name; `union_extract(u, 'field')` →
        // the payload if `field` is the active member (else NULL, per C++'s
        // struct-extract reuse over the physical union layout).
        "union_tag" => match &args[0] {
            Value::Union { variants, tag, .. } => Ok(Value::String(
                variants
                    .get(*tag)
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default(),
            )),
            other => Err(Error::binder(format!(
                "union_tag expects a UNION value, got {}.",
                other.logical_type().name()
            ))),
        },
        "union_extract" => match (&args[0], &args[1]) {
            (
                Value::Union {
                    variants,
                    tag,
                    value,
                },
                Value::String(field),
            ) => {
                // A name that isn't a member at all is the C++ binder error;
                // an inactive member is NULL.
                if !variants.iter().any(|(n, _)| n == field) {
                    return Err(Error::binder(format!(
                        "Invalid struct field name: {field}."
                    )));
                }
                let active = variants.get(*tag).map(|(n, _)| n.as_str());
                Ok(if active == Some(field.as_str()) {
                    (**value).clone()
                } else {
                    Value::Null
                })
            }
            (other, _) => Err(Error::binder(format!(
                "union_extract expects (UNION, STRING), got {}.",
                other.logical_type().name()
            ))),
        },
        "date" => cast_value(&args[0], &LogicalType::Date),
        "timestamp" => cast_value(&args[0], &LogicalType::Timestamp),
        "interval" | "duration" => cast_value(&args[0], &LogicalType::Interval),
        "uuid" => cast_value(&args[0], &LogicalType::Uuid),
        "string" => cast_value(&args[0], &LogicalType::String),
        "blob" => cast_value(&args[0], &LogicalType::Blob),
        // to_timestamp(seconds-since-epoch) → TIMESTAMP (micros). C++ computes
        // `sec * MICROS_PER_SEC` in double precision (so large inputs pick up a
        // rounding artifact) and overflow-checks the cast back to int64.
        "to_timestamp" => {
            let product = arg_f64(&args[0])? * 1_000_000.0;
            // i64::MIN (-2^63) is exactly representable as f64; i64::MAX rounds up
            // to 2^63, so the upper bound is strict.
            if !product.is_finite()
                || !(-9_223_372_036_854_775_808_f64..9_223_372_036_854_775_808_f64)
                    .contains(&product)
            {
                return Err(Error::conversion(
                    "Could not convert epoch seconds to TIMESTAMP".to_string(),
                ));
            }
            Ok(Value::Timestamp(product as i64))
        }
        "make_date" => {
            let y = args[0].as_i64().unwrap_or(0);
            let m = args[1].as_i64().unwrap_or(1);
            let d = args[2].as_i64().unwrap_or(1);
            // C++ validates instead of rolling over (audit V2): unpadded echo.
            if !(1..=12).contains(&m) || d < 1 || d > temporal::days_in_month(y, m) {
                return Err(Error::conversion(format!(
                    "Date out of range: {y}-{m}-{d}."
                )));
            }
            Ok(Value::Date(temporal::days_from_civil(y, m, d) as i32))
        }
        "century" => date_part("century", &args[0]),
        "abs" => eval_abs(&args[0]),
        "negate" => crate::eval_scalar(crate::ScalarOp::Neg, args),
        "floor" => eval_round_family(&args[0], RoundKind::Floor),
        "ceil" | "ceiling" => eval_round_family(&args[0], RoundKind::Ceil),
        "round" => {
            if args.len() == 2 {
                let digits = args[1].as_i64().unwrap_or(0);
                Ok(Value::Double(round_to(arg_f64(&args[0])?, digits)))
            } else {
                eval_round_family(&args[0], RoundKind::Round)
            }
        }
        "sign" => eval_sign(&args[0]),
        "even" => Ok(Value::Double((arg_f64(&args[0])? / 2.0).ceil() * 2.0)),
        "factorial" => {
            let n = args[0].as_i64().unwrap_or(0);
            let mut acc: i64 = 1;
            for i in 2..=n {
                acc = acc.checked_mul(i).ok_or_else(|| {
                    Error::overflow("Factorial result is out of INT64 range.".to_string())
                })?;
            }
            Ok(Value::Int64(acc))
        }
        "sqrt" => Ok(Value::Double(arg_f64(&args[0])?.sqrt())),
        "cbrt" => Ok(Value::Double(arg_f64(&args[0])?.cbrt())),
        "ln" => Ok(Value::Double(arg_f64(&args[0])?.ln())),
        "log" | "log10" => Ok(Value::Double(arg_f64(&args[0])?.log10())),
        "log2" => Ok(Value::Double(arg_f64(&args[0])?.log2())),
        "exp" => Ok(Value::Double(arg_f64(&args[0])?.exp())),
        "pow" | "power" => Ok(Value::Double(arg_f64(&args[0])?.powf(arg_f64(&args[1])?))),
        "sin" => Ok(Value::Double(arg_f64(&args[0])?.sin())),
        "cos" => Ok(Value::Double(arg_f64(&args[0])?.cos())),
        "tan" => Ok(Value::Double(arg_f64(&args[0])?.tan())),
        "cot" => Ok(Value::Double(1.0 / arg_f64(&args[0])?.tan())),
        "asin" => Ok(Value::Double(arg_f64(&args[0])?.asin())),
        "acos" => Ok(Value::Double(arg_f64(&args[0])?.acos())),
        "atan" => Ok(Value::Double(arg_f64(&args[0])?.atan())),
        "atan2" => Ok(Value::Double(arg_f64(&args[0])?.atan2(arg_f64(&args[1])?))),
        "degrees" => Ok(Value::Double(arg_f64(&args[0])?.to_degrees())),
        "radians" => Ok(Value::Double(arg_f64(&args[0])?.to_radians())),
        "gamma" => Ok(Value::Double(gamma(arg_f64(&args[0])?))),
        "lgamma" => Ok(Value::Double(lgamma(arg_f64(&args[0])?))),
        "bitwise_and" => bitwise(args, |a, b| a & b),
        "bitwise_or" => bitwise(args, |a, b| a | b),
        "bitwise_xor" => bitwise(args, |a, b| a ^ b),
        "bitshift_left" => bitwise(args, |a, b| a << b),
        "bitshift_right" => bitwise(args, |a, b| a >> b),

        // --- string functions ---
        "concat" => Ok(Value::String(
            args.iter().map(|a| a.to_result_string()).collect(),
        )),
        "lower" | "lcase" => Ok(Value::String(case_map_1to1(arg_str(&args[0])?, false))),
        "upper" | "ucase" => Ok(Value::String(case_map_1to1(arg_str(&args[0])?, true))),
        "trim" => Ok(Value::String(arg_str(&args[0])?.trim().to_string())),
        "ltrim" => Ok(Value::String(arg_str(&args[0])?.trim_start().to_string())),
        "rtrim" => Ok(Value::String(arg_str(&args[0])?.trim_end().to_string())),
        "initcap" => Ok(Value::String(initcap(arg_str(&args[0])?))),
        "contains" => {
            // C++ quirk: an empty needle is never contained (`contains('a','')`
            // and `contains('','')` are both False), unlike starts/ends_with.
            let needle = arg_str(&args[1])?;
            Ok(Value::Bool(
                !needle.is_empty() && arg_str(&args[0])?.contains(needle),
            ))
        }
        "starts_with" | "prefix" => Ok(Value::Bool(
            arg_str(&args[0])?.starts_with(arg_str(&args[1])?),
        )),
        "ends_with" | "suffix" => Ok(Value::Bool(
            arg_str(&args[0])?.ends_with(arg_str(&args[1])?),
        )),
        "reverse" => match &args[0] {
            Value::List(items) => Ok(Value::List(items.iter().rev().cloned().collect())),
            v => Ok(Value::String(
                UnicodeSegmentation::graphemes(arg_str(v)?, true)
                    .rev()
                    .collect(),
            )),
        },
        "size" => match &args[0] {
            Value::List(items) => Ok(Value::Int64(items.len() as i64)),
            Value::Map(entries) => Ok(Value::Int64(entries.len() as i64)),
            // Non-strings arrive via the (STRING) overload's implicit cast:
            // size(12345) = 5.
            v => match v.as_str() {
                Some(s) => Ok(Value::Int64(grapheme_len(s) as i64)),
                None => Ok(Value::Int64(grapheme_len(&v.to_result_string()) as i64)),
            },
        },
        // `cost(e)` is the accumulated edge weight of a (ALL) WSHORTEST path
        // (a DOUBLE); NULL on a non-weighted path.
        "cost" => match &args[0] {
            Value::RecursiveRel(r) => Ok(r.cost.map(Value::Double).unwrap_or(Value::Null)),
            v => Err(type_err("cost", "RECURSIVE_REL", v)),
        },
        // `length` is the rel count of a recursive-rel / path value, else a
        // string/list length.
        "length" => match &args[0] {
            // The unmatched-OPTIONAL degenerate path is NULL-length (audit V13);
            // a genuinely matched zero-length path is 0.
            Value::RecursiveRel(r) if r.degenerate => Ok(Value::Null),
            Value::RecursiveRel(r) => Ok(Value::Int64(r.rels.len() as i64)),
            Value::List(items) => Ok(Value::Int64(items.len() as i64)),
            Value::Map(entries) => Ok(Value::Int64(entries.len() as i64)),
            v => Ok(Value::Int64(grapheme_len(arg_str(v)?) as i64)),
        },
        // `nodes(p)` / `rels(p)` extract a recursive-rel / path value's node and
        // relationship lists as `LIST[NODE]` / `LIST[REL]`.
        "nodes" => match &args[0] {
            Value::RecursiveRel(r) => Ok(Value::List(
                r.nodes
                    .iter()
                    .cloned()
                    .map(|n| Value::Node(Box::new(n)))
                    .collect(),
            )),
            v => Err(type_err("nodes", "RECURSIVE_REL", v)),
        },
        "rels" | "relationships" => match &args[0] {
            Value::RecursiveRel(r) => Ok(Value::List(
                r.rels
                    .iter()
                    .cloned()
                    .map(|r| Value::Rel(Box::new(r)))
                    .collect(),
            )),
            v => Err(type_err("rels", "RECURSIVE_REL", v)),
        },
        // `properties(list, key)` maps property `key` over a `LIST[NODE|REL]`,
        // returning a list of the corresponding values (NULL where absent). The
        // special keys `_id` and `_label` read the identity/label.
        "properties" => {
            let key = arg_str(&args[1])?;
            let out = arg_list(&args[0])?
                .iter()
                .map(|elem| element_property(elem, key))
                .collect();
            Ok(Value::List(out))
        }
        "left" => Ok(Value::String(str_left(
            arg_str(&args[0])?,
            args[1].as_i64().unwrap_or(0),
        ))),
        "right" => Ok(Value::String(str_right(
            arg_str(&args[0])?,
            args[1].as_i64().unwrap_or(0),
        ))),
        "lpad" => Ok(Value::String(str_pad(
            arg_str(&args[0])?,
            args[1].as_i64().unwrap_or(0),
            arg_str(&args[2])?,
            true,
        ))),
        "rpad" => Ok(Value::String(str_pad(
            arg_str(&args[0])?,
            args[1].as_i64().unwrap_or(0),
            arg_str(&args[2])?,
            false,
        ))),
        "substr" | "substring" => {
            let len = args.get(2).and_then(|v| v.as_i64());
            Ok(Value::String(str_substr(
                arg_str(&args[0])?,
                args[1].as_i64().unwrap_or(1),
                len,
            )))
        }
        "repeat" => {
            let n = args[1].as_i64().unwrap_or(0).max(0) as usize;
            Ok(Value::String(arg_str(&args[0])?.repeat(n)))
        }
        "string_split" | "str_split" | "string_to_array" => {
            let s = arg_str(&args[0])?;
            let sep = arg_str(&args[1])?;
            // An empty separator splits into individual characters (Kùzu).
            let parts: Vec<Value> = if sep.is_empty() {
                s.chars().map(|c| Value::String(c.to_string())).collect()
            } else {
                // C++ drops EMPTY tokens except the final tail, which is
                // always kept (',a,b,' → [a,b,''], 'a,,b' → [a,b]).
                let raw: Vec<&str> = s.split(sep).collect();
                let last = raw.len() - 1;
                raw.into_iter()
                    .enumerate()
                    .filter(|(i, p)| *i == last || !p.is_empty())
                    .map(|(_, p)| Value::String(p.to_string()))
                    .collect()
            };
            Ok(Value::List(parts))
        }
        "split_part" => {
            let s = arg_str(&args[0])?;
            let sep = arg_str(&args[1])?;
            let idx = args[2].as_i64().unwrap_or(1);
            // An empty separator splits per character like C++ (audit V17:
            // split_part('Alice','',5) → 'e'), same as string_split.
            let per_char: Vec<String>;
            let parts: Vec<&str> = if sep.is_empty() {
                per_char = s.chars().map(|c| c.to_string()).collect();
                per_char.iter().map(|c| c.as_str()).collect()
            } else {
                s.split(sep).collect()
            };
            // 1-indexed; out of range → empty string.
            let part = usize::try_from(idx - 1)
                .ok()
                .and_then(|i| parts.get(i))
                .copied()
                .unwrap_or("");
            Ok(Value::String(part.to_string()))
        }
        // list_to_string(delimiter, list) — C++ signature is (STRING, LIST); NULL
        // list elements are skipped (no surrounding delimiter).
        "list_to_string" => match &args[1] {
            Value::List(items) => {
                let sep = arg_str(&args[0])?;
                Ok(Value::String(
                    items
                        .iter()
                        .filter(|v| !v.is_null())
                        .map(|v| v.to_result_string())
                        .collect::<Vec<_>>()
                        .join(sep),
                ))
            }
            v => Err(Error::runtime(format!(
                "list_to_string expects a list, got {}",
                v.logical_type()
            ))),
        },
        "levenshtein" => Ok(Value::Int64(
            levenshtein(arg_str(&args[0])?, arg_str(&args[1])?) as i64,
        )),
        "replace" => {
            let (s, from, to) = (arg_str(&args[0])?, arg_str(&args[1])?, arg_str(&args[2])?);
            // An empty search string leaves the input unchanged (unlike Rust's
            // str::replace, which would insert between every char).
            Ok(Value::String(if from.is_empty() {
                s.to_string()
            } else {
                s.replace(from, to)
            }))
        }
        // Invalid patterns follow RE2's lenient object semantics (audit V10):
        // matches → False, replace → unchanged, split → whole input; only the
        // extract forms raise (RE2's group-index error) — all oracle-verified.
        "regexp_matches" => {
            let hay = arg_str(&args[0])?;
            let re = compile_regex_lenient(arg_str(&args[1])?);
            Ok(Value::Bool(re.is_some_and(|re| re.is_match(hay))))
        }
        "regexp_full_match" => {
            let hay = arg_str(&args[0])?;
            let re = compile_regex_lenient(&format!("^(?:{})$", arg_str(&args[1])?));
            Ok(Value::Bool(re.is_some_and(|re| re.is_match(hay))))
        }
        "regexp_replace" => {
            let s = arg_str(&args[0])?;
            let Some(re) = compile_regex_lenient(arg_str(&args[1])?) else {
                return Ok(Value::String(s.to_string()));
            };
            // RE2 expands `\N` backreferences in the replacement (audit V10).
            let repl = re2_rewrite(arg_str(&args[2])?);
            // A 4th "options" arg containing 'g' replaces all (else the first).
            let global = args
                .get(3)
                .and_then(|v| v.as_str())
                .is_some_and(|o| o.contains('g'));
            let out = if global {
                re.replace_all(s, repl.as_str()).into_owned()
            } else {
                re.replace(s, repl.as_str()).into_owned()
            };
            Ok(Value::String(out))
        }
        "regexp_extract" => {
            let re = compile_regex_lenient(arg_str(&args[1])?).ok_or_else(|| {
                Error::runtime("Regex match group index is out of range".to_string())
            })?;
            let group = args.get(2).and_then(|v| v.as_i64()).unwrap_or(0) as usize;
            let out = re
                .captures(arg_str(&args[0])?)
                .and_then(|c| c.get(group))
                .map_or(String::new(), |m| m.as_str().to_string());
            Ok(Value::String(out))
        }
        "regexp_extract_all" => {
            let re = compile_regex_lenient(arg_str(&args[1])?).ok_or_else(|| {
                Error::runtime("Regex match group index is out of range".to_string())
            })?;
            let group = args.get(2).and_then(|v| v.as_i64()).unwrap_or(0) as usize;
            let out: Vec<Value> = re
                .captures_iter(arg_str(&args[0])?)
                .filter_map(|c| c.get(group).map(|m| Value::String(m.as_str().to_string())))
                .collect();
            Ok(Value::List(out))
        }
        "regexp_split_to_array" => {
            let s0 = arg_str(&args[0])?;
            let Some(re) = compile_regex_lenient(arg_str(&args[1])?) else {
                return Ok(Value::List(vec![Value::String(s0.to_string())]));
            };
            let mut out: Vec<Value> = re.split(s0).map(|p| Value::String(p.to_string())).collect();
            // C++ drops a single trailing empty piece (input ending in a match).
            if out.len() > 1 && matches!(out.last(), Some(Value::String(s)) if s.is_empty()) {
                out.pop();
            }
            Ok(Value::List(out))
        }

        // --- list functions ---
        "range" => {
            // Wide-safe iteration (audit V16): UINT128 endpoints step in u128
            // (the old `as_i64` path silently collapsed them to defaults);
            // signed endpoints step in i128 and keep their width.
            let step_v = args.get(2);
            if step_v.and_then(|v| v.as_int128()) == Some(0) {
                return Err(Error::runtime("Step of range cannot be 0.".to_string()));
            }
            if matches!(args[0], Value::UInt128(_)) || matches!(args[1], Value::UInt128(_)) {
                let start = args[0].as_u128().unwrap_or(0);
                let end = args[1].as_u128().unwrap_or(0);
                let step = step_v.and_then(|v| v.as_int128()).unwrap_or(1);
                let mut out = Vec::new();
                let mut i = start;
                loop {
                    if (step > 0 && i > end) || (step < 0 && i < end) {
                        break;
                    }
                    out.push(Value::UInt128(i));
                    let next = if step > 0 {
                        i.checked_add(step as u128)
                    } else {
                        i.checked_sub(step.unsigned_abs())
                    };
                    match next {
                        Some(n) => i = n,
                        None => break,
                    }
                }
                return Ok(Value::List(out));
            }
            let kind = match (&args[0], &args[1]) {
                (Value::IntX { kind: a, .. }, Value::IntX { kind: b, .. }) if a == b => *a,
                _ => koko_common::IntKind::I64,
            };
            let start = args[0].as_int128().unwrap_or(0);
            let end = args[1].as_int128().unwrap_or(0);
            let step = step_v.and_then(|v| v.as_int128()).unwrap_or(1);
            if step == 0 {
                return Err(Error::runtime("Step of range cannot be 0.".to_string()));
            }
            let mut out = Vec::new();
            let mut i = start;
            // Inclusive of `end`, matching Kùzu.
            while (step > 0 && i <= end) || (step < 0 && i >= end) {
                out.push(Value::make_int(i, kind));
                i += step;
            }
            Ok(Value::List(out))
        }
        "list_creation" => Ok(Value::List(args.to_vec())),
        "array_distance" => {
            let (l, r) = array_pair_f64(args)?;
            let sq: f64 = l.iter().zip(&r).map(|(a, b)| (a - b) * (a - b)).sum();
            Ok(vec_scalar_result(sq.sqrt(), &args[0]))
        }
        "array_squared_distance" => {
            let (l, r) = array_pair_f64(args)?;
            let sq: f64 = l.iter().zip(&r).map(|(a, b)| (a - b) * (a - b)).sum();
            Ok(vec_scalar_result(sq, &args[0]))
        }
        "array_inner_product" | "array_dot_product" => {
            let (l, r) = array_pair_f64(args)?;
            let dp: f64 = l.iter().zip(&r).map(|(a, b)| a * b).sum();
            Ok(vec_scalar_result(dp, &args[0]))
        }
        "array_cosine_similarity" => {
            let (l, r) = array_pair_f64(args)?;
            let dp: f64 = l.iter().zip(&r).map(|(a, b)| a * b).sum();
            let nl = l.iter().map(|a| a * a).sum::<f64>().sqrt();
            let nr = r.iter().map(|b| b * b).sum::<f64>().sqrt();
            Ok(vec_scalar_result(dp / (nl * nr), &args[0]))
        }
        "array_cross_product" => array_cross_product_eval(args),
        // String indexing (`s[i]` / array_extract on a string) returns a 1-char
        // string; list indexing returns the element.
        "list_extract" | "list_element" | "array_extract"
            if !matches!(args[0], Value::List(_)) || name == "array_extract" =>
        {
            let owned;
            let s = match args[0].as_str() {
                Some(s) => s,
                None => {
                    // array_extract casts its operand to STRING (see the type
                    // arm): character extraction over the rendered value.
                    owned = args[0].to_result_string();
                    &owned
                }
            };
            let parts = grapheme_vec(s);
            let len = parts.len() as i64;
            let idx = args[1].as_i64().unwrap_or(0);
            if idx == 0 || len == 0 {
                return Ok(Value::String(String::new()));
            }
            // C++ `ListExtract` (string overload) returns "" when the index exceeds
            // the byte length; `ArrayExtract` instead clamps a positive index to the
            // last grapheme. `list_extract`/`list_element` are ListExtract.
            if name != "array_extract" && idx > s.len() as i64 {
                return Ok(Value::String(String::new()));
            }
            let pos = if idx > 0 {
                idx.min(len) - 1
            } else {
                (len + idx).max(0)
            };
            Ok(Value::String(
                usize::try_from(pos)
                    .ok()
                    .and_then(|i| parts.get(i))
                    .copied()
                    .unwrap_or_default()
                    .to_string(),
            ))
        }
        "list_extract" | "list_element" | "array_extract" => {
            let items = arg_list(&args[0])?;
            let idx = args[1].as_i64().unwrap_or(0);
            // 1-indexed; negative counts from the end; 0 and out-of-range error
            // (matching the C++ list_extract).
            if idx == 0 {
                return Err(Error::runtime(
                    "List extract takes 1-based position.".to_string(),
                ));
            }
            let pos = if idx > 0 {
                idx - 1
            } else {
                items.len() as i64 + idx
            };
            usize::try_from(pos)
                .ok()
                .and_then(|i| items.get(i))
                .cloned()
                .ok_or_else(|| {
                    Error::runtime(format!(
                        "list_extract(list, index): index={idx} is out of range."
                    ))
                })
        }
        // An empty / all-NULL list answers the C++ physical default, 0 —
        // not NULL (oracle: list_any_value([]) = list_any_value([null,null]) = 0).
        "list_any_value" => Ok(arg_list(&args[0])?
            .iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Int64(0))),
        "list_slice" | "array_slice" => {
            // 1-indexed, END-INCLUSIVE (Kùzu list_slice); negatives count from the
            // end. Works on a STRING too (returns a substring).
            let lo = args[1].as_i64().unwrap_or(1);
            let hi = args[2].as_i64();
            if let Some(s) = args[0].as_str() {
                let parts = grapheme_vec(s);
                let (from, to) = slice_bounds(lo, hi, parts.len());
                Ok(Value::String(parts[from..to].concat()))
            } else {
                let items = arg_list(&args[0])?;
                let (from, to) = slice_bounds(lo, hi, items.len());
                Ok(Value::List(items[from..to].to_vec()))
            }
        }
        "list_concat" | "list_cat" => {
            let mut out = arg_list(&args[0])?.to_vec();
            out.extend(arg_list(&args[1])?.iter().cloned());
            Ok(Value::List(out))
        }
        "list_append" => {
            let mut out = arg_list(&args[0])?.to_vec();
            out.push(args[1].clone());
            Ok(Value::List(out))
        }
        // list_prepend(list, value) — C++ signature is (LIST, ANY).
        "list_prepend" => {
            let mut out = vec![args[1].clone()];
            out.extend(arg_list(&args[0])?.iter().cloned());
            Ok(Value::List(out))
        }
        "list_contains" => {
            Ok(Value::Bool(arg_list(&args[0])?.iter().any(|v| {
                crate::cypher_cmp(v, &args[1]) == Some(std::cmp::Ordering::Equal)
            })))
        }
        "list_position" | "list_indexof" => {
            let pos = arg_list(&args[0])?
                .iter()
                .position(|v| crate::cypher_cmp(v, &args[1]) == Some(std::cmp::Ordering::Equal));
            // 1-indexed; 0 when absent (matching Kùzu).
            Ok(Value::Int64(pos.map_or(0, |p| p as i64 + 1)))
        }
        "list_reverse" => Ok(Value::List(
            arg_list(&args[0])?.iter().rev().cloned().collect(),
        )),
        "list_sort" | "list_reverse_sort" => {
            let items = arg_list(&args[0])?;
            // Optional args differ by function:
            //   list_sort(list[, order[, null_order]]) — order is 'ASC'/'DESC',
            //     null_order is 'NULLS FIRST'/'NULLS LAST' at position 2.
            //   list_reverse_sort(list[, null_order]) — order is fixed DESC, the
            //     optional null_order sits at position 1.
            // The default null order is always NULLS FIRST, regardless of sort
            // direction (matching the C++ oracle).
            let reverse = name == "list_reverse_sort";
            let desc = reverse
                || args
                    .get(1)
                    .and_then(|v| v.as_str())
                    .is_some_and(|o| o.eq_ignore_ascii_case("desc"));
            let null_order_arg = if reverse { args.get(1) } else { args.get(2) };
            // Default null order is NULLS FIRST; only an explicit "LAST" overrides.
            let nulls_first = !matches!(
                null_order_arg.and_then(|v| v.as_str()),
                Some(o) if o.to_ascii_uppercase().contains("LAST")
            );
            let (mut nulls, mut vals): (Vec<Value>, Vec<Value>) =
                items.iter().cloned().partition(Value::is_null);
            vals.sort_by(crate::order_cmp);
            if desc {
                vals.reverse();
            }
            let mut out = Vec::with_capacity(items.len());
            if nulls_first {
                out.append(&mut nulls);
                out.append(&mut vals);
            } else {
                out.append(&mut vals);
                out.append(&mut nulls);
            }
            Ok(Value::List(out))
        }
        "list_distinct" | "list_unique" => {
            let items = arg_list(&args[0])?;
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for v in items {
                if v.is_null() {
                    continue; // list_distinct/unique drop NULLs
                }
                if seen.insert(crate::ValueKey::from_value(v)) {
                    out.push(v.clone());
                }
            }
            if name == "list_unique" {
                Ok(Value::Int64(out.len() as i64))
            } else {
                Ok(Value::List(out))
            }
        }
        // Typed accumulators keeping the child width and wrapping like C++'s
        // fixed-width arithmetic (audit V16): list_product([100,2] :: INT8[])
        // -> -56; UINT128 values accumulate exactly in u128 (the old `as_i64`
        // path silently skipped them). NULL elements are skipped (oracle).
        "list_sum" => list_fold(arg_list(&args[0])?, false),
        "list_product" => list_fold(arg_list(&args[0])?, true),

        // --- struct / map functions ---
        "struct_extract" => {
            let field = arg_str(&args[1])?;
            let find_named = |props: &[(String, Value)]| {
                props
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(field))
                    .map(|(_, v)| v.clone())
            };
            match &args[0] {
                Value::Null => Ok(Value::Null),
                // An unknown field is the C++ binder error, not NULL.
                Value::Struct(fields) => find_named(fields)
                    .ok_or_else(|| Error::binder(format!("Invalid struct field name: {field}."))),
                Value::Map(entries) => {
                    let key = Value::String(field.to_string());
                    Ok(entries
                        .iter()
                        .find(|(k, _)| {
                            crate::cypher_cmp(k, &key) == Some(std::cmp::Ordering::Equal)
                        })
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Null))
                }
                // Node/rel identity fields answer like C++'s struct-backed values
                // (audit V12): `_id`/`_label` and a rel's `_src`/`_dst` come from
                // the value itself, not the property list.
                Value::Node(n) => Ok(match field.to_ascii_lowercase().as_str() {
                    "_id" => Value::InternalId(n.id),
                    "_label" => Value::String(n.label.clone()),
                    _ => find_named(&n.props).unwrap_or(Value::Null),
                }),
                Value::Rel(r) => Ok(match field.to_ascii_lowercase().as_str() {
                    "_id" => Value::InternalId(r.id),
                    "_label" => Value::String(r.label.clone()),
                    "_src" => Value::InternalId(r.src),
                    "_dst" => Value::InternalId(r.dst),
                    _ => find_named(&r.props).unwrap_or(Value::Null),
                }),
                v => Err(Error::runtime(format!(
                    "struct_extract expects a STRUCT, MAP, NODE, or REL, got {}",
                    v.logical_type()
                ))),
            }
        }
        "map" | "map_checked" => {
            let keys = arg_list(&args[0])?;
            let vals = arg_list(&args[1])?;
            if keys.len() != vals.len() {
                return Err(Error::runtime(
                    "Unaligned key list and value list.".to_string(),
                ));
            }
            // NULL/duplicate keys are ALLOWED by default: C++ gates the check on
            // the `disable_map_key_check` session setting (default true = off).
            // The binder routes map() through the checked name when enabled;
            // the check then rejects a NULL key, then a duplicate (C++
            // `validateKeys` order — the duplicate reports the key's rendering).
            if name == "map_checked" {
                if keys.iter().any(|k| k.is_null()) {
                    return Err(Error::runtime(
                        "Null value key is not allowed in map.".to_string(),
                    ));
                }
                for (i, k) in keys.iter().enumerate() {
                    if keys[..i]
                        .iter()
                        .any(|p| crate::cypher_cmp(p, k) == Some(std::cmp::Ordering::Equal))
                    {
                        return Err(Error::runtime(format!(
                            "Found duplicate key: {} in map.",
                            k.to_result_string()
                        )));
                    }
                }
            }
            Ok(Value::Map(
                keys.iter().cloned().zip(vals.iter().cloned()).collect(),
            ))
        }
        "map_extract" | "element_at" => match &args[0] {
            Value::Map(entries) => Ok(Value::List(
                entries
                    .iter()
                    .filter(|(k, _)| {
                        crate::cypher_cmp(k, &args[1]) == Some(std::cmp::Ordering::Equal)
                    })
                    .map(|(_, v)| v.clone())
                    .collect(),
            )),
            v => Err(Error::runtime(format!(
                "map_extract expects a MAP, got {}",
                v.logical_type()
            ))),
        },
        "map_keys" => match &args[0] {
            Value::Map(entries) => Ok(Value::List(
                entries.iter().map(|(k, _)| k.clone()).collect(),
            )),
            v => Err(Error::runtime(format!(
                "map_keys expects a MAP, got {}",
                v.logical_type()
            ))),
        },
        "map_values" => match &args[0] {
            Value::Map(entries) => Ok(Value::List(
                entries.iter().map(|(_, v)| v.clone()).collect(),
            )),
            v => Err(Error::runtime(format!(
                "map_values expects a MAP, got {}",
                v.logical_type()
            ))),
        },
        "cardinality" => match &args[0] {
            Value::Map(entries) => Ok(Value::Int64(entries.len() as i64)),
            Value::List(items) => Ok(Value::Int64(items.len() as i64)),
            v => Err(Error::runtime(format!(
                "cardinality expects a MAP/LIST, got {}",
                v.logical_type()
            ))),
        },

        // --- date / timestamp / interval functions ---
        "dayname" => {
            const NAMES: [&str; 7] = [
                "Sunday",
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
            ];
            Ok(Value::String(
                NAMES[temporal::day_of_week(arg_date(&args[0])?)].to_string(),
            ))
        }
        "monthname" => {
            const NAMES: [&str; 12] = [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ];
            let (_, m, _) = temporal::civil_from_days(arg_date(&args[0])? as i64);
            Ok(Value::String(NAMES[(m - 1) as usize].to_string()))
        }
        "last_day" => {
            let (y, m, _) = temporal::civil_from_days(arg_date(&args[0])? as i64);
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            Ok(Value::Date(
                (temporal::days_from_civil(ny, nm, 1) - 1) as i32,
            ))
        }
        "date_part" | "datepart" => {
            let part = arg_str(&args[0])?.to_ascii_lowercase();
            date_part(&part, &args[1])
        }
        "date_trunc" | "datetrunc" => {
            let part = arg_str(&args[0])?.to_ascii_lowercase();
            date_trunc(&part, &args[1])
        }
        "to_years" => Ok(iv_months(args[0].as_i64().unwrap_or(0) * 12)),
        "to_months" => Ok(iv_months(args[0].as_i64().unwrap_or(0))),
        "to_days" => Ok(iv_days(args[0].as_i64().unwrap_or(0))),
        "to_hours" => Ok(iv_micros(args[0].as_i64().unwrap_or(0) * 3_600_000_000)),
        "to_minutes" => Ok(iv_micros(args[0].as_i64().unwrap_or(0) * 60_000_000)),
        "to_seconds" => Ok(iv_micros(args[0].as_i64().unwrap_or(0) * 1_000_000)),
        "to_milliseconds" => Ok(iv_micros(args[0].as_i64().unwrap_or(0) * 1_000)),
        "to_microseconds" => Ok(iv_micros(args[0].as_i64().unwrap_or(0))),

        other => Err(Error::not_implemented(format!(
            "scalar function {other} is not supported in this phase"
        ))),
    }
}

/// Evaluate a context-free scalar. Query execution should use
/// [`eval_with_context`]; this convenience entry point serves deterministic
/// helper/tests which do not retain function state between calls.
pub fn eval(name: &str, args: &[Value]) -> Result<Value> {
    eval_with_context(name, args, &crate::oracle_hash::RandomState::default())
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

fn iv_months(months: i64) -> Value {
    Value::Interval(koko_common::Interval {
        months: months as i32,
        days: 0,
        micros: 0,
    })
}
fn iv_days(days: i64) -> Value {
    Value::Interval(koko_common::Interval {
        months: 0,
        days: days as i32,
        micros: 0,
    })
}
fn iv_micros(micros: i64) -> Value {
    Value::Interval(koko_common::Interval {
        months: 0,
        days: 0,
        micros,
    })
}

/// The day count of a DATE, or the date part of a TIMESTAMP.
fn arg_date(v: &Value) -> Result<i32> {
    match v {
        Value::Date(d) => Ok(*d),
        Value::Timestamp(t) | Value::TimestampTz(t) => Ok(t.div_euclid(MICROS_PER_DAY) as i32),
        _ => Err(Error::runtime(format!(
            "expected a DATE/TIMESTAMP argument, got {}",
            v.logical_type()
        ))),
    }
}

/// The C++ `Interval::tryGetDatePartSpecifier` alias table: maps a specifier
/// string (case-insensitive) to its canonical part name, or the Conversion
/// error. `dow`/`doy`/`epoch`/`isoyear` etc. are NOT recognized.
fn date_part_specifier(s: &str) -> Result<&'static str> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "year" | "yr" | "y" | "years" | "yrs" => "year",
        "month" | "mon" | "months" | "mons" => "month",
        "day" | "days" | "d" | "dayofmonth" => "day",
        "decade" | "dec" | "decades" | "decs" => "decade",
        "century" | "cent" | "centuries" | "c" => "century",
        "millennium" | "mil" | "millenniums" | "millennia" | "mils" | "millenium"
        | "milleniums" => "millennium",
        "microseconds" | "microsecond" | "us" | "usec" | "usecs" | "usecond" | "useconds" => {
            "microsecond"
        }
        "milliseconds" | "millisecond" | "ms" | "msec" | "msecs" | "msecond" | "mseconds" => {
            "millisecond"
        }
        "second" | "sec" | "seconds" | "secs" | "s" => "second",
        "minute" | "min" | "minutes" | "mins" | "m" => "minute",
        "hour" | "hr" | "hours" | "hrs" | "h" => "hour",
        "week" | "weeks" | "w" | "weekofyear" => "week",
        "quarter" | "quarters" => "quarter",
        other => {
            return Err(Error::conversion(format!(
                "Unrecognized interval specifier string: {other}."
            )));
        }
    })
}

/// `date_part(part, value)` — extract a calendar/time component as INT64. Part
/// names go through the C++ specifier alias table.
fn date_part(part: &str, v: &Value) -> Result<Value> {
    let part = date_part_specifier(part)?;
    // INTERVAL: extract directly from its (months, days, micros) fields.
    if let Value::Interval(iv) = v {
        let n = match part {
            "year" => (iv.months / 12) as i64,
            "month" => (iv.months % 12) as i64,
            "day" => iv.days as i64,
            // Calendar buckets derive from the INTERVAL's month count
            // (matching C++ `Interval::getIntervalPart`): 120/1200/12000
            // months per decade/century/millennium; quarter is (month % 12)/3+1.
            "decade" => (iv.months / 120) as i64,
            "century" => (iv.months / 1200) as i64,
            "millennium" => (iv.months / 12000) as i64,
            "quarter" => ((iv.months % 12) / 3 + 1) as i64,
            "hour" => iv.micros / 3_600_000_000,
            "minute" => (iv.micros / 60_000_000) % 60,
            "second" => (iv.micros / 1_000_000) % 60,
            "millisecond" => (iv.micros / 1_000) % 60_000,
            "microsecond" => iv.micros % 60_000_000,
            // "week" on an INTERVAL is the C++ UNREACHABLE (ledgered crash);
            // answer 0 like the DATE path rather than reproduce it.
            _ => 0,
        };
        return Ok(Value::Int64(n));
    }
    let micros_in_day = match v {
        Value::Timestamp(t) | Value::TimestampTz(t) => t.rem_euclid(MICROS_PER_DAY),
        _ => 0,
    };
    let (y, m, d) = temporal::civil_from_days(arg_date(v)? as i64);
    let n = match part {
        "year" => y,
        "month" => m,
        "day" => d,
        "decade" => y / 10,
        "century" => (y - 1) / 100 + 1,
        "millennium" => (y - 1) / 1000 + 1,
        "quarter" => (m - 1) / 3 + 1,
        "hour" => micros_in_day / 3_600_000_000,
        "minute" => (micros_in_day / 60_000_000) % 60,
        "second" => (micros_in_day / 1_000_000) % 60,
        "millisecond" => (micros_in_day / 1_000) % 60_000,
        "microsecond" => micros_in_day % 60_000_000,
        // C++ getDatePart(WEEK) yields 0 for dates/timestamps.
        _ => 0,
    };
    Ok(Value::Int64(n))
}

/// `date_trunc(part, value)` — floor a DATE/TIMESTAMP to the given granularity.
/// Part names tolerate a trailing plural `s`.
fn date_trunc(part: &str, v: &Value) -> Result<Value> {
    let part = part.strip_suffix('s').unwrap_or(part);
    let (y, m, d) = temporal::civil_from_days(arg_date(v)? as i64);
    let (ty, tm, td) = match part {
        "millennium" => (y / 1000 * 1000, 1, 1),
        "century" => (y / 100 * 100, 1, 1),
        "decade" => (y / 10 * 10, 1, 1),
        "year" => (y, 1, 1),
        "quarter" => (y, (m - 1) / 3 * 3 + 1, 1),
        "month" => (y, m, 1),
        // day and finer: a DATE has no sub-day component, so keep the date.
        _ => (y, m, d),
    };
    let days = temporal::days_from_civil(ty, tm, td) as i32;
    match v {
        Value::Timestamp(_) | Value::TimestampTz(_) => {
            // Truncating to day-or-coarser zeroes the time; finer parts keep it.
            let micros_in_day = match v {
                Value::Timestamp(t) | Value::TimestampTz(t) => t.rem_euclid(MICROS_PER_DAY),
                _ => 0,
            };
            let keep_time = matches!(
                part,
                "hour" | "minute" | "second" | "millisecond" | "microsecond"
            );
            let frac = if keep_time {
                trunc_time(micros_in_day, part)
            } else {
                0
            };
            Ok(Value::Timestamp(days as i64 * MICROS_PER_DAY + frac))
        }
        _ => Ok(Value::Date(days)),
    }
}

fn trunc_time(micros_in_day: i64, part: &str) -> i64 {
    let unit = match part {
        "hour" => 3_600_000_000,
        "minute" => 60_000_000,
        "second" => 1_000_000,
        "millisecond" => 1_000,
        _ => 1,
    };
    micros_in_day / unit * unit
}

/// 1-based, end-inclusive slice bounds → 0-based `[from, to)` over `n` items.
/// Negative indices count from the end; `hi == None` means "to the end".
fn slice_bounds(lo: i64, hi: Option<i64>, n: usize) -> (usize, usize) {
    let n = n as i64;
    let from = if lo < 0 { n + lo } else { lo - 1 }.clamp(0, n);
    let to = match hi {
        None => n,
        Some(h) if h < 0 => (n + h + 1).clamp(0, n),
        Some(h) => h.clamp(0, n),
    };
    (from.min(to) as usize, to as usize)
}

fn arg_list(v: &Value) -> Result<&[Value]> {
    match v {
        Value::List(items) => Ok(items),
        _ => Err(Error::runtime(format!(
            "expected a LIST argument, got {}",
            v.logical_type()
        ))),
    }
}

fn type_err(func: &str, expected: &str, got: &Value) -> Error {
    Error::runtime(format!(
        "{func} expected a {expected} argument, got {}",
        got.logical_type()
    ))
}

/// Read property `key` off a single node/rel value (an element of `nodes(p)` /
/// `rels(p)`). The special keys `_id` and `_label` return the identity / label;
/// any other key is matched (case-insensitively) against the value's properties,
/// yielding `NULL` when absent. A non-node/rel element yields `NULL`.
fn element_property(v: &Value, key: &str) -> Value {
    let lookup = |props: &[(String, Value)]| {
        props
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, val)| val.clone())
            .unwrap_or(Value::Null)
    };
    match v {
        Value::Node(n) if key.eq_ignore_ascii_case("_id") => Value::InternalId(n.id),
        Value::Node(n) if key.eq_ignore_ascii_case("_label") => Value::String(n.label.clone()),
        Value::Node(n) => lookup(&n.props),
        Value::Rel(r) if key.eq_ignore_ascii_case("_id") => Value::InternalId(r.id),
        Value::Rel(r) if key.eq_ignore_ascii_case("_label") => Value::String(r.label.clone()),
        Value::Rel(r) if key.eq_ignore_ascii_case("_src") => Value::InternalId(r.src),
        Value::Rel(r) if key.eq_ignore_ascii_case("_dst") => Value::InternalId(r.dst),
        Value::Rel(r) => lookup(&r.props),
        _ => Value::Null,
    }
}

/// Per-codepoint 1:1 case mapping (audit V10): C++ maps through utf8proc, so
/// no expansion ever happens — `upper('ß')` is `ẞ` (not `SS`), `lower('İ')` is
/// a bare `i`. A Rust full-mapping expansion collapses to the utf8proc target
/// where one exists, else keeps the original character.
fn case_map_1to1(s: &str, upper: bool) -> String {
    s.chars()
        .map(|c| {
            if upper {
                if c == 'ß' {
                    return 'ẞ';
                }
                let mut it = c.to_uppercase();
                match (it.next(), it.next()) {
                    (Some(u), None) => u,
                    _ => c,
                }
            } else {
                if c == 'İ' {
                    return 'i';
                }
                let mut it = c.to_lowercase();
                match (it.next(), it.next()) {
                    (Some(l), None) => l,
                    _ => c,
                }
            }
        })
        .collect()
}

/// Rewrite Perl character classes to their ASCII (RE2) forms (audit V10):
/// `\w`/`\d`/`\s` are ASCII-only in C++'s RE2, Unicode in the Rust regex
/// crate. Handles use both outside and inside `[...]` classes; negated forms
/// rewrite only outside a class (a bracketed `[\D]` is left alone — rare).
fn ascii_classes(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len() + 16);
    let mut chars = pat.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('d') => out.push_str(if in_class { "0-9" } else { "[0-9]" }),
                Some('w') => out.push_str(if in_class {
                    "0-9A-Za-z_"
                } else {
                    "[0-9A-Za-z_]"
                }),
                Some('s') => out.push_str(if in_class {
                    "\\t\\n\\x0C\\r "
                } else {
                    "[\\t\\n\\x0C\\r ]"
                }),
                Some('D') if !in_class => out.push_str("[^0-9]"),
                Some('W') if !in_class => out.push_str("[^0-9A-Za-z_]"),
                Some('S') if !in_class => out.push_str("[^\\t\\n\\x0C\\r ]"),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
            continue;
        }
        if c == '[' && !in_class {
            in_class = true;
        } else if c == ']' && in_class {
            in_class = false;
        }
        out.push(c);
    }
    out
}

/// Compile like RE2: an invalid pattern is not an immediate error — it becomes
/// a regex that never matches (C++: `regexp_matches('x','[')` → False,
/// `regexp_replace` → input unchanged, `regexp_split_to_array` → whole input;
/// `regexp_extract`/`_all` instead raise RE2's group-index error at use).
fn compile_regex_lenient(pat: &str) -> Option<regex::Regex> {
    // RE2 has no `(?<name>...)` named groups and no lookbehind — any `(?<` makes
    // the pattern invalid in C++ (the Rust regex crate would accept named groups).
    if pat.contains("(?<") {
        return None;
    }
    regex::Regex::new(&ascii_classes(pat)).ok()
}

/// Translate an RE2 rewrite string to the regex crate's expansion syntax:
/// `\N` backreferences become `${N}` and literal `$` is escaped (`$$`).
fn re2_rewrite(repl: &str) -> String {
    let mut out = String::with_capacity(repl.len() + 8);
    let mut chars = repl.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '$' => out.push_str("$$"),
            '\\' => match chars.peek() {
                Some(d @ '0'..='9') => {
                    out.push_str("${");
                    out.push(*d);
                    out.push('}');
                    chars.next();
                }
                _ => out.push('\\'),
            },
            _ => out.push(c),
        }
    }
    out
}

fn arg_str(v: &Value) -> Result<&str> {
    v.as_str().ok_or_else(|| {
        Error::runtime(format!(
            "expected a STRING argument, got {}",
            v.logical_type()
        ))
    })
}

/// `initcap` (Kùzu): lower-case the whole string, then upper-case only its first
/// character — NOT per-word.
fn initcap(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `left(s, n)`: first `n` grapheme clusters; `n < 0` keeps all but the last `|n|`.
fn str_left(s: &str, n: i64) -> String {
    let parts = grapheme_vec(s);
    let len = parts.len() as i64;
    let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
    parts[..take as usize].concat()
}

/// `right(s, n)`: last `n` grapheme clusters; `n < 0` drops the first `|n|`.
fn str_right(s: &str, n: i64) -> String {
    let parts = grapheme_vec(s);
    let len = parts.len() as i64;
    let take = if n >= 0 { n.min(len) } else { (len + n).max(0) };
    parts[(len - take) as usize..].concat()
}

/// `lpad`/`rpad` to `len` chars, cycling `pad`; longer strings truncate to `len`.
fn str_pad(s: &str, len: i64, pad: &str, left: bool) -> String {
    if len <= 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    let len = len as usize;
    if chars.len() >= len {
        return chars[..len].iter().collect();
    }
    let pad_chars: Vec<char> = pad.chars().collect();
    if pad_chars.is_empty() {
        return s.to_string();
    }
    let need = len - chars.len();
    let fill: String = (0..need).map(|i| pad_chars[i % pad_chars.len()]).collect();
    if left {
        format!("{fill}{s}")
    } else {
        format!("{s}{fill}")
    }
}

/// `substr(s, start, len)` — 1-indexed, grapheme-cluster-based.
fn str_substr(s: &str, start: i64, len: Option<i64>) -> String {
    let parts = grapheme_vec(s);
    let total = parts.len() as i64;
    let from0 = start - 1;
    let end = match len {
        Some(l) => from0 + l,
        None => total,
    };
    if from0 < 0 || from0 >= total || from0 >= end {
        return String::new();
    }
    let to = end.clamp(0, total);
    parts[from0 as usize..to as usize].concat()
}

fn grapheme_vec(s: &str) -> Vec<&str> {
    UnicodeSegmentation::graphemes(s, true).collect()
}

fn grapheme_len(s: &str) -> usize {
    UnicodeSegmentation::graphemes(s, true).count()
}

/// Levenshtein edit distance over bytes, matching the C++ `string_t::len` loop.
fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn arg_f64(v: &Value) -> Result<f64> {
    v.as_f64().ok_or_else(|| {
        Error::runtime(format!(
            "expected a numeric argument, got {}",
            v.logical_type()
        ))
    })
}

fn bitwise(args: &[Value], f: impl Fn(i64, i64) -> i64) -> Result<Value> {
    let a = args[0]
        .as_i64()
        .ok_or_else(|| Error::runtime("bitwise requires INT64 operands".to_string()))?;
    let b = args[1]
        .as_i64()
        .ok_or_else(|| Error::runtime("bitwise requires INT64 operands".to_string()))?;
    Ok(Value::Int64(f(a, b)))
}

fn eval_greatest_least(name: &str, args: &[Value]) -> Result<Value> {
    if args.len() != 2 {
        return Err(Error::runtime(format!("{name} expects two arguments")));
    }
    let ord = match (&args[0], &args[1]) {
        (Value::Date(a), Value::Date(b)) => a.cmp(b),
        // Timestamp flavors all carry epoch micros — compare across them.
        (
            Value::Timestamp(a) | Value::TimestampTz(a),
            Value::Timestamp(b) | Value::TimestampTz(b),
        ) => a.cmp(b),
        (a, b) => {
            return Err(Error::runtime(format!(
                "{name} expects DATE or TIMESTAMP operands, got {} and {}",
                a.logical_type(),
                b.logical_type()
            )));
        }
    };
    let take_left = if name == "greatest" {
        ord != std::cmp::Ordering::Less
    } else {
        ord != std::cmp::Ordering::Greater
    };
    let picked = if take_left {
        args[0].clone()
    } else {
        args[1].clone()
    };
    // The result type is plain TIMESTAMP whenever flavors mix (see
    // greatest_least_result_type) — normalize a TZ value so it renders
    // without the `+00` suffix.
    Ok(match picked {
        Value::TimestampTz(us) if args.iter().any(|a| !matches!(a, Value::TimestampTz(_))) => {
            Value::Timestamp(us)
        }
        other => other,
    })
}

fn eval_abs(v: &Value) -> Result<Value> {
    if matches!(v, Value::UInt128(_)) {
        return Ok(v.clone()); // unsigned: already non-negative
    }
    if let Value::Decimal {
        value,
        precision,
        scale,
    } = v
    {
        return Ok(Value::Decimal {
            value: value.abs(),
            precision: *precision,
            scale: *scale,
        });
    }
    if let Some((val, kind)) = v.int_parts() {
        if !kind.is_signed() {
            return Ok(v.clone());
        }
        let a = val
            .checked_abs()
            .filter(|n| kind.contains(*n))
            .ok_or_else(|| {
                Error::overflow(format!(
                    "Cannot take the absolute value of {val} within {} range.",
                    kind.name()
                ))
            })?;
        Ok(Value::make_int(a, kind))
    } else if let Value::Double(x) = v {
        Ok(Value::Double(x.abs()))
    } else if let Value::Float(x) = v {
        Ok(Value::Float(x.abs()))
    } else {
        Err(Error::runtime(format!(
            "Function abs expects a numeric argument but got {}.",
            v.logical_type()
        )))
    }
}

fn eval_sign(v: &Value) -> Result<Value> {
    if let Some(n) = v.as_int128() {
        return Ok(Value::Int64(n.signum() as i64));
    }
    // sign() always returns INT64 (matching the C++ oracle), even for floats.
    let x = arg_f64(v)?;
    Ok(Value::Int64(if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }))
}

enum RoundKind {
    Floor,
    Ceil,
    Round,
}

/// `floor`/`ceil`/`round` keep the operand's numeric type: integers are
/// identity, doubles use the f64 op, decimals round to integer in-type.
fn eval_round_family(v: &Value, kind: RoundKind) -> Result<Value> {
    match v {
        Value::Int64(_) | Value::IntX { .. } | Value::UInt128(_) => Ok(v.clone()),
        Value::Double(x) => Ok(Value::Double(match kind {
            RoundKind::Floor => x.floor(),
            RoundKind::Ceil => x.ceil(),
            RoundKind::Round => x.round(),
        })),
        Value::Float(x) => Ok(Value::Float(match kind {
            RoundKind::Floor => x.floor(),
            RoundKind::Ceil => x.ceil(),
            RoundKind::Round => x.round(),
        })),
        Value::Decimal {
            value,
            precision,
            scale,
        } => {
            // C++ floor/ceil on DECIMAL(p,s) yield DECIMAL(p,0) — the value
            // reduces to scale 0, not to a same-scale multiple (audit R2:
            // floor(-9.5 :: DECIMAL(4,1)) -> -10 typed DECIMAL(4,0)).
            let f = koko_common::decimal::pow10(*scale);
            let unscaled = match kind {
                RoundKind::Floor => value.div_euclid(f),
                RoundKind::Ceil => -((-value).div_euclid(f)),
                RoundKind::Round => koko_common::decimal::rescale(*value, *scale, 0).unwrap_or(0),
            };
            Ok(Value::Decimal {
                value: unscaled,
                precision: *precision,
                scale: 0,
            })
        }
        _ => Err(Error::runtime(format!(
            "expected a numeric argument, got {}",
            v.logical_type()
        ))),
    }
}

fn round_to(x: f64, digits: i64) -> f64 {
    let factor = 10f64.powi(digits as i32);
    (x * factor).round() / factor
}

/// Log-gamma via the Lanczos approximation, computed in log space so it does not
/// overflow for large `x` (unlike `gamma(x).ln()`).
fn lgamma(x: f64) -> f64 {
    // Poles at 0 and the negative integers: lnΓ → +inf (libm lgamma).
    if x <= 0.0 && x == x.trunc() {
        return f64::INFINITY;
    }
    if x < 0.5 {
        // Reflection: ln|Γ(x)| = ln(π/|sin(πx)|) − lnΓ(1−x).
        (std::f64::consts::PI / (std::f64::consts::PI * x).sin().abs()).ln() - lgamma(1.0 - x)
    } else {
        const G: f64 = 7.0;
        const C: [f64; 9] = [
            0.999_999_999_999_809_9,
            676.520_368_121_885_1,
            -1_259.139_216_722_402_8,
            771.323_428_777_653_1,
            -176.615_029_162_140_6,
            12.507_343_278_686_905,
            -0.138_571_095_265_720_12,
            9.984_369_578_019_572e-6,
            1.505_632_735_149_311_6e-7,
        ];
        let x = x - 1.0;
        let mut a = C[0];
        let t = x + G + 0.5;
        for (i, &c) in C.iter().enumerate().skip(1) {
            a += c / (x + i as f64);
        }
        0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
    }
}

/// Lanczos approximation of the gamma function (for `gamma`/`lgamma`).
fn gamma(x: f64) -> f64 {
    // Γ(0) → inf; the negative-integer poles → nan (libm tgamma).
    if x == 0.0 {
        return f64::INFINITY;
    }
    if x < 0.0 && x == x.trunc() {
        return f64::NAN;
    }
    // Reflection for x < 0.5.
    if x < 0.5 {
        std::f64::consts::PI / ((std::f64::consts::PI * x).sin() * gamma(1.0 - x))
    } else {
        const G: f64 = 7.0;
        const C: [f64; 9] = [
            0.999_999_999_999_809_9,
            676.520_368_121_885_1,
            -1_259.139_216_722_402_8,
            771.323_428_777_653_1,
            -176.615_029_162_140_6,
            12.507_343_278_686_905,
            -0.138_571_095_265_720_12,
            9.984_369_578_019_572e-6,
            1.505_632_735_149_311_6e-7,
        ];
        let x = x - 1.0;
        let mut a = C[0];
        let t = x + G + 0.5;
        for (i, &c) in C.iter().enumerate().skip(1) {
            a += c / (x + i as f64);
        }
        (2.0 * std::f64::consts::PI).sqrt() * t.powf(x + 0.5) * (-t).exp() * a
    }
}
