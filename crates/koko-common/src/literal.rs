//! Parse a value-literal *string* (as it appears in a CSV cell or a string
//! cast) into a [`Value`] of a given [`LogicalType`], recursively for nested
//! types. This is the data-ingestion counterpart of the query-literal parser in
//! `koko-parser` (which works on tokens).

use crate::scalar;
use crate::temporal;
use crate::types::{LogicalType, split_top_level};
use crate::value::Value;
use crate::{Error, Result};

/// Parse `s` as a value of type `ty`. An empty string, or the unquoted token
/// `NULL` (case-insensitive), is `NULL` — matching the C++ `isNull` used by every
/// string→type cast (a *quoted* `"null"` is the string, since the quotes remain).
pub fn parse_value_literal(s: &str, ty: &LogicalType) -> Result<Value> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") {
        return Ok(Value::Null);
    }
    match ty {
        LogicalType::Bool => match strip_quotes(t).trim().to_ascii_lowercase().as_str() {
            // The C++ tryCastToBool forms: true/t/1 and false/f/0.
            "true" | "t" | "1" => Ok(Value::Bool(true)),
            "false" | "f" | "0" => Ok(Value::Bool(false)),
            _ => Err(Error::conversion(format!("Cannot parse {t} as BOOL."))),
        },
        LogicalType::Int(k) => {
            // The C++ nested-value cast wording (struct/list fields, LOAD cells).
            let fail = || {
                Error::conversion(format!(
                    "Cast failed. Could not convert \"{t}\" to {}.",
                    k.name()
                ))
            };
            let v: i128 = t.parse().map_err(|_| fail())?;
            if !k.contains(v) {
                return Err(fail());
            }
            Ok(Value::make_int(v, *k))
        }
        LogicalType::Serial => t
            .parse::<i64>()
            .map(Value::Int64)
            .map_err(|_| Error::conversion(format!("Cannot parse {t} as SERIAL."))),
        LogicalType::UInt128 => t.parse::<u128>().map(Value::UInt128).map_err(|_| {
            Error::conversion(format!(
                "Cast failed. Could not convert \"{t}\" to UINT128."
            ))
        }),
        LogicalType::Decimal(precision, scale) => {
            let value =
                crate::decimal::parse_to_unscaled(strip_quotes(t), *scale).ok_or_else(|| {
                    Error::conversion(format!(
                        "Cannot parse {t} as DECIMAL({precision}, {scale})."
                    ))
                })?;
            if !crate::decimal::fits(value, *precision) {
                return Err(Error::conversion(format!(
                    "Cast failed. {t} is not in DECIMAL({precision}, {scale}) range."
                )));
            }
            Ok(Value::Decimal {
                value,
                precision: *precision,
                scale: *scale,
            })
        }
        LogicalType::Double => t
            .parse::<f64>()
            .map(Value::Double)
            .map_err(|_| Error::conversion(format!("Cannot parse {t} as DOUBLE."))),
        LogicalType::Float => t
            .parse::<f32>()
            .map(Value::Float)
            .map_err(|_| Error::conversion(format!("Cannot parse {t} as FLOAT."))),
        // Preserve interior/leading/trailing spaces in STRING data; only strip a
        // surrounding quote pair (the trimmed `t` is used elsewhere for tolerant
        // numeric/temporal parsing, but strings must stay verbatim).
        LogicalType::String => {
            let b = s.as_bytes();
            let inner = if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0]
            {
                &s[1..s.len() - 1]
            } else {
                s
            };
            Ok(Value::String(inner.to_string()))
        }
        LogicalType::Date => temporal::parse_date(strip_quotes(t))
            .map(Value::Date)
            .ok_or_else(|| Error::conversion(format!("Cannot parse {t} as DATE."))),
        LogicalType::Timestamp => temporal::parse_timestamp(strip_quotes(t))
            .map(Value::Timestamp)
            .ok_or_else(|| Error::conversion(format!("Cannot parse {t} as TIMESTAMP."))),
        LogicalType::TimestampNs => temporal::parse_timestamp(strip_quotes(t))
            .map(Value::Timestamp)
            .ok_or_else(|| Error::conversion(format!("Cannot parse {t} as TIMESTAMP_NS."))),
        LogicalType::TimestampMs => temporal::parse_timestamp(strip_quotes(t))
            .map(|m| Value::Timestamp(m.div_euclid(1_000) * 1_000))
            .ok_or_else(|| Error::conversion(format!("Cannot parse {t} as TIMESTAMP_MS."))),
        LogicalType::TimestampSec => temporal::parse_timestamp(strip_quotes(t))
            .map(|m| Value::Timestamp(m.div_euclid(1_000_000) * 1_000_000))
            .ok_or_else(|| Error::conversion(format!("Cannot parse {t} as TIMESTAMP_SEC."))),
        LogicalType::TimestampTz => temporal::parse_timestamp(strip_quotes(t))
            .map(Value::TimestampTz)
            .ok_or_else(|| Error::conversion(format!("Cannot parse {t} as TIMESTAMP_TZ."))),
        LogicalType::Interval => temporal::parse_interval(strip_quotes(t)).map(Value::Interval),
        LogicalType::Uuid => scalar::parse_uuid(strip_quotes(t))
            .map(Value::Uuid)
            .ok_or_else(|| Error::conversion(format!("Cannot parse {t} as UUID."))),
        LogicalType::Blob => scalar::parse_blob(strip_quotes(t)).map(Value::Blob),
        // Structural failures (malformed text) report the C++ "not in range"
        // shape with the full input and type; value-level failures (a field
        // that fails its own cast, a NULL map key) propagate their wording.
        LogicalType::List(inner) | LogicalType::Array(inner, _) => parse_list(t, ty, inner),
        LogicalType::Struct(fields) => parse_struct(t, ty, fields),
        LogicalType::Map(kt, vt) => parse_map(t, ty, kt, vt),
        // The raw string, not the trimmed token: a STRING member keeps its
        // surrounding whitespace (`"  dfsa"` → the 6-char string).
        LogicalType::Union(variants) => parse_union(s, variants),
        // ANY is best-effort: try int, then double, then string.
        LogicalType::Any => Ok(parse_any(t)),
        other => Err(Error::not_implemented(format!(
            "cannot parse a value of type {other} from text"
        ))),
    }
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Strip the surrounding `open`/`close` brackets, returning the inner content.
fn unwrap_brackets(t: &str, open: char, close: char) -> Result<&str> {
    let t = t.trim();
    if t.starts_with(open) && t.ends_with(close) && t.len() >= 2 {
        Ok(&t[open.len_utf8()..t.len() - close.len_utf8()])
    } else {
        Err(Error::conversion(format!(
            "expected {open}…{close} but got {t}"
        )))
    }
}

/// The C++ structural nested-parse failure: `Cast failed. {text} is not in
/// {TYPE} range.` — emitted by the level whose own structure is malformed
/// (child value errors propagate untouched).
fn nested_range_error_at(text: &str, ty: &LogicalType) -> Error {
    Error::conversion(format!("Cast failed. {text} is not in {ty} range."))
}

/// Whether a nested-literal interior balances its brackets/braces (quote-aware).
/// An imbalance anywhere is *this* level's structural failure (C++ blames the
/// outer type for `[[231|4324]`).
fn brackets_balanced(s: &str) -> bool {
    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => {
                    depth -= 1;
                    if depth < 0 {
                        return false;
                    }
                }
                _ => {}
            },
        }
    }
    depth == 0
}

/// The C++ lenient element count for MALFORMED array text: no leading `[`
/// counts 0; an unterminated list counts only its top-level commas; a properly
/// closed one also counts the final element (an all-blank interior is 0).
/// Brackets/braces pair on a stack (a mismatched closer is data), quotes
/// shield. `None` = closes properly but with trailing junk (range error).
fn count_array_elements_lenient(t: &str) -> Option<u64> {
    let b = t.trim().as_bytes();
    if b.first() != Some(&b'[') {
        return Some(0);
    }
    let mut stack: Vec<u8> = vec![b']'];
    let mut count: u64 = 0;
    let mut interior_blank = true;
    let mut quote: Option<u8> = None;
    for (idx, &c) in b.iter().enumerate().skip(1) {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            b'\'' | b'"' => quote = Some(c),
            b'[' => stack.push(b']'),
            b'{' => stack.push(b'}'),
            b']' | b'}' if stack.last() == Some(&c) => {
                stack.pop();
                if stack.is_empty() {
                    if b[idx + 1..].iter().any(|x| !x.is_ascii_whitespace()) {
                        return None;
                    }
                    let total = if interior_blank && count == 0 {
                        0
                    } else {
                        count + 1
                    };
                    return Some(total);
                }
            }
            b',' if stack.len() == 1 => {
                count += 1;
                interior_blank = false;
            }
            _ => {
                if !c.is_ascii_whitespace() {
                    interior_blank = false;
                }
            }
        }
    }
    Some(count)
}

fn parse_list(t: &str, ty: &LogicalType, inner: &LogicalType) -> Result<Value> {
    let body = match unwrap_brackets(t, '[', ']') {
        Ok(b) if brackets_balanced(b) => b,
        // Malformed at *this* level (missing/unbalanced brackets) is this
        // type's range error; child failures below keep their own wording —
        // except a fixed-size ARRAY, which reports a wrong lenient element
        // count first (`[42,42` → "Expected: 2, Actual: 1.").
        _ => {
            if let LogicalType::Array(_, n) = ty {
                if let Some(actual) = count_array_elements_lenient(t) {
                    if actual != *n {
                        return Err(Error::conversion(format!(
                            "Each array should have fixed number of elements. \
                             Expected: {n}, Actual: {actual}."
                        )));
                    }
                }
            }
            return Err(nested_range_error_at(t, ty));
        }
    };
    let slots: Vec<&str> = if body.trim().is_empty() {
        Vec::new()
    } else {
        split_list_elements(body)
    };
    // A fixed-size ARRAY enforces its declared length BEFORE parsing any
    // element (C++ reports the count for `[42|42]` → DOUBLE[2], not the
    // unparseable element).
    if let LogicalType::Array(_, n) = ty {
        if slots.len() as u64 != *n {
            return Err(Error::conversion(format!(
                "Each array should have fixed number of elements. Expected: {n}, Actual: {}.",
                slots.len()
            )));
        }
    }
    // Whitespace around commas is structural, not part of the child literal;
    // an EMPTY slot (`[,[],x]`) is a NULL child (C++ renders it back as an
    // empty slot).
    let items: Vec<Value> = slots
        .into_iter()
        .map(|p| {
            let p = p.trim();
            if p.is_empty() {
                Ok(Value::Null)
            } else {
                parse_child_literal(p, inner)
            }
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::List(items))
}

/// Split a list body on its element commas. Unlike [`split_top_level`], a
/// quote only protects when it is the element's FIRST non-space character
/// (`['a,b', c]` → 2 elements but `[x'a,b']` → 2 elements split at the comma);
/// a start-quote that never closes retroactively becomes data (`[ 'a,b ]` →
/// `'a` and `b`). Brackets/braces nest as usual, shielded inside a live quote.
fn split_list_elements(body: &str) -> Vec<&str> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        let end = list_element_end(b, start, true)
            .or_else(|| list_element_end(b, start, false))
            .unwrap_or(b.len());
        out.push(&body[start..end]);
        if end >= b.len() {
            break;
        }
        start = end + 1;
        if start == b.len() {
            // A trailing comma leaves one final empty (NULL) slot.
            out.push("");
            break;
        }
    }
    out
}

/// The end (splitting comma or EOS) of the element starting at `start`.
/// With `honor_start_quote`, a leading quote enters quoted mode; `None` means
/// that quote never closed (the caller rescans it as plain data).
fn list_element_end(b: &[u8], start: usize, honor_start_quote: bool) -> Option<usize> {
    let mut i = start;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut quote: Option<u8> = None;
    if honor_start_quote && i < b.len() && (b[i] == b'\'' || b[i] == b'"') {
        quote = Some(b[i]);
        i += 1;
    }
    let mut depth = 0i32;
    while i < b.len() {
        let c = b[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else {
            match c {
                b'[' | b'{' | b'(' => depth += 1,
                b']' | b'}' | b')' => depth -= 1,
                b',' if depth == 0 => return Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    if quote.is_some() { None } else { Some(b.len()) }
}

/// Parse a LIST element / MAP key / MAP value. A STRING child keeps its quote
/// characters as *data* (audit W4/R6 — C++'s nested parser splits on structure
/// only: `CAST('["a","b"]' AS STRING[])` yields 3-char elements `"a"`), unlike a
/// struct's scalar STRING field, which strips (see `parse_struct`).
fn parse_child_literal(t: &str, ty: &LogicalType) -> Result<Value> {
    if matches!(ty, LogicalType::String) {
        let s = t.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("null") {
            return Ok(Value::Null);
        }
        return Ok(Value::String(s.to_string()));
    }
    parse_value_literal(t, ty)
}

/// Normalize the textual form of a list literal without assigning element types.
///
/// Bare `LOAD FROM` columns are still typed as STRING, but C++'s CSV sniffer
/// renders bracketed list-looking cells in canonical list form. This helper keeps
/// that narrow display parity without changing declared STRING parsing.
pub fn normalize_list_literal_text(s: &str) -> Option<String> {
    fn normalize(t: &str) -> Result<String> {
        let body = unwrap_brackets(t, '[', ']')?;
        if body.trim().is_empty() {
            return Ok("[]".to_string());
        }
        let items = split_top_level(body, ',')
            .into_iter()
            .map(|p| {
                let p = p.trim();
                if p.starts_with('[') && p.ends_with(']') {
                    normalize(p)
                } else {
                    Ok(strip_quotes(p).to_string())
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!("[{}]", items.join(",")))
    }

    let t = s.trim();
    if t.starts_with('[') && t.ends_with(']') {
        normalize(t).ok()
    } else {
        None
    }
}

/// Whether every quote opened in `s` closes again. Structs treat a dangling
/// quote as *their* structural failure (`{ c: 'fdsfs }`); lists are lenient.
fn quotes_terminated(s: &str) -> bool {
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None => {}
        }
    }
    quote.is_none()
}

/// The string→nested entry for the RUNTIME `cast()` function. Unlike a CSV
/// cell, a null-ish text is NOT null here: lists/structs/maps range-error on
/// it (raw text in the message) and a fixed-size ARRAY reports element count
/// 0, all matching the C++ cast path.
pub fn parse_string_cast(s: &str, ty: &LogicalType) -> Result<Value> {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("null") {
        return match ty {
            LogicalType::Array(_, n) => Err(Error::conversion(format!(
                "Each array should have fixed number of elements. Expected: {n}, Actual: 0."
            ))),
            LogicalType::Union(vs) => Err(Error::conversion(format!(
                "Could not convert to union type {}: {t}.",
                LogicalType::Union(vs.clone())
            ))),
            _ => Err(nested_range_error_at(s, ty)),
        };
    }
    parse_value_literal(s, ty)
}

fn parse_struct(t: &str, ty: &LogicalType, fields: &[(String, LogicalType)]) -> Result<Value> {
    let body = match unwrap_brackets(t, '{', '}') {
        Ok(b) if brackets_balanced(b) && quotes_terminated(b) => b,
        _ => return Err(nested_range_error_at(t, ty)),
    };
    // Collect supplied (key → raw value) pairs.
    let mut supplied: Vec<(String, &str)> = Vec::new();
    if !body.trim().is_empty() {
        for entry in split_top_level(body, ',') {
            let (k, v) =
                split_once_top_level(entry, ':').ok_or_else(|| nested_range_error_at(t, ty))?;
            let key = strip_quotes(k);
            // A key naming no declared field is a Parser error (C++ reports the
            // original spelling); matching itself is case-insensitive.
            if !fields.iter().any(|(n, _)| n.eq_ignore_ascii_case(key)) {
                return Err(Error::parser(format!("Invalid struct field name: {key}")));
            }
            supplied.push((key.to_ascii_lowercase(), v));
        }
    }
    // Assemble in declared field order (missing → NULL).
    let mut out = Vec::with_capacity(fields.len());
    for (name, fty) in fields {
        // A duplicated key keeps its LAST occurrence (C++ parse order).
        let raw = supplied
            .iter()
            .rev()
            .find(|(k, _)| *k == name.to_ascii_lowercase())
            .map(|(_, v)| *v);
        let val = match raw {
            // Trim surrounding whitespace around the field value (the STRING leaf
            // preserves interior spaces, so we trim the structural padding here).
            Some(v) => {
                let v = v.trim();
                let b = v.as_bytes();
                // A fully-quoted value strips its quote layer HERE and bypasses
                // the null check (`{c: 'null'}` is the string "null", `{c: '12'}`
                // is INT 12). A partial layer (`'ab' cd`) stays verbatim data.
                let quoted =
                    b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0];
                if quoted {
                    let inner = &v[1..v.len() - 1];
                    match fty {
                        LogicalType::String => Value::String(inner.to_string()),
                        _ => parse_value_literal(inner, fty)?,
                    }
                } else {
                    parse_value_literal(v, fty)?
                }
            }
            None => Value::Null,
        };
        out.push((name.clone(), val));
    }
    Ok(Value::Struct(out))
}

fn parse_map(t: &str, ty: &LogicalType, kt: &LogicalType, vt: &LogicalType) -> Result<Value> {
    // Like structs, a dangling quote is the map's own structural failure
    // (quotes stay *data* in keys/values, but still pair for splitting).
    let body = match unwrap_brackets(t, '{', '}') {
        Ok(b) if brackets_balanced(b) && quotes_terminated(b) => b,
        _ => return Err(nested_range_error_at(t, ty)),
    };
    let mut entries = Vec::new();
    if !body.trim().is_empty() {
        for entry in split_top_level(body, ',') {
            let (k, v) =
                split_once_top_level(entry, '=').ok_or_else(|| nested_range_error_at(t, ty))?;
            // Trim structural whitespace around each key/value token.
            let key = parse_child_literal(k.trim(), kt)?;
            // An empty key token is the NULL key (C++ CSV nested semantics),
            // which maps reject.
            if key.is_null() || k.trim().is_empty() {
                return Err(Error::conversion(
                    "Map does not allow null as key.".to_string(),
                ));
            }
            // All keys share the declared key type `kt`, so structural equality
            // detects duplicates (matches C++ string->map cast, a Conversion error).
            if entries.iter().any(|(k, _)| k == &key) {
                return Err(Error::conversion(
                    "Map does not allow duplicate keys.".to_string(),
                ));
            }
            entries.push((key, parse_child_literal(v.trim(), vt)?));
        }
    }
    Ok(Value::Map(entries))
}

fn parse_union(t: &str, variants: &[(String, LogicalType)]) -> Result<Value> {
    // The active member is just its value; try each declared variant in order
    // (C++ `CastString` to union: first member that parses wins), and tag the
    // result with the matched member.
    for (i, (_, vty)) in variants.iter().enumerate() {
        if let Ok(v) = parse_value_literal(t, vty) {
            return Ok(Value::Union {
                variants: variants.to_vec(),
                tag: i,
                value: Box::new(v),
            });
        }
    }
    Err(Error::conversion(format!(
        "Could not convert to union type {}: {t}.",
        LogicalType::Union(variants.to_vec())
    )))
}

fn parse_any(t: &str) -> Value {
    if let Ok(n) = t.parse::<i64>() {
        Value::Int64(n)
    } else if let Ok(x) = t.parse::<f64>() {
        Value::Double(x)
    } else {
        Value::String(strip_quotes(t).to_string())
    }
}

/// Split on the first top-level `sep`.
fn split_once_top_level(s: &str, sep: char) -> Option<(&str, &str)> {
    let parts = split_top_level(s, sep);
    if parts.len() < 2 {
        return None;
    }
    let first = parts[0];
    // Rejoin the remainder (it may legitimately contain `sep`, e.g. a time).
    let rest_start = first.len() + sep.len_utf8();
    Some((first, &s[rest_start..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_literals_trim_structural_padding_before_leaf_parse() {
        let string_list = LogicalType::List(Box::new(LogicalType::String));
        assert_eq!(
            parse_value_literal("[5, 6, 7, 8]", &string_list)
                .unwrap()
                .to_result_string(),
            "[5,6,7,8]"
        );
        assert_eq!(
            parse_value_literal("[0.1 ,8.8]", &string_list)
                .unwrap()
                .to_result_string(),
            "[0.1,8.8]"
        );

        let nested_string_list =
            LogicalType::List(Box::new(LogicalType::List(Box::new(LogicalType::String))));
        assert_eq!(
            parse_value_literal("[[], [5, 6]]", &nested_string_list)
                .unwrap()
                .to_result_string(),
            "[[],[5,6]]"
        );
    }

    #[test]
    fn direct_string_literal_padding_stays_verbatim() {
        assert_eq!(
            parse_value_literal("  keep me  ", &LogicalType::String).unwrap(),
            Value::String("  keep me  ".into())
        );
    }
}
