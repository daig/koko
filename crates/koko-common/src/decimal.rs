//! Fixed-point `DECIMAL(precision, scale)` support: rendering, parsing, rescaling
//! (round-half-away-from-zero), and range checks.
//!
//! A decimal is an unscaled `i128` plus a `scale` (number of fractional digits);
//! `precision` is the maximum number of significant digits (≤ 38). The value
//! `unscaled = v` at scale `s` denotes `v / 10^s`. This mirrors the Kùzu/DuckDB
//! decimal layout and its byte-exact `toString` (always exactly `scale`
//! fractional digits).

/// The maximum decimal precision (digits). `10^38 - 1` is the largest magnitude
/// and still fits an `i128`.
pub const MAX_PRECISION: u8 = 38;

/// The default `DECIMAL` precision and scale when none is given (`CAST(x,
/// 'DECIMAL')`), matching DuckDB/Kùzu's `DECIMAL(18, 3)`.
pub const DEFAULT_PRECISION: u8 = 18;
pub const DEFAULT_SCALE: u8 = 3;

/// `10^n` as `i128` for `n <= 38` (panics above — callers keep `n` in range).
pub fn pow10(n: u8) -> i128 {
    let mut p: i128 = 1;
    for _ in 0..n {
        p *= 10;
    }
    p
}

/// The largest unscaled magnitude representable in `DECIMAL(precision, _)`:
/// `10^precision - 1`.
pub fn max_unscaled(precision: u8) -> i128 {
    pow10(precision) - 1
}

/// Whether `value` fits within `DECIMAL(precision, _)`.
pub fn fits(value: i128, precision: u8) -> bool {
    let m = max_unscaled(precision);
    value >= -m && value <= m
}

/// Result `(precision, scale)` of DECIMAL `+`/`-`, mirroring the C++
/// `DecimalAdd::resultingParams` exactly. Both operands are then rescaled to the
/// result scale before adding.
pub fn add_sub_params(p1: u8, s1: u8, p2: u8, s2: u8) -> (u8, u8) {
    let (p1, s1, p2, s2) = (p1 as i32, s1 as i32, p2 as i32, s2 as i32);
    let lim = MAX_PRECISION as i32;
    let max_scale = s1.max(s2);
    let max_int = (p1 - s1).max(p2 - s2);
    let p = lim.min(max_scale + max_int + 1);
    let mut s = p.min(max_scale);
    if max_int < lim.min(p) - s {
        s = p.min(lim) - max_int;
    }
    (p as u8, s as u8)
}

/// Result `(precision, scale)` of DECIMAL `*` (C++ `DecimalMultiply`). Returns
/// `None` if the result precision would exceed [`MAX_PRECISION`].
pub fn mul_params(p1: u8, s1: u8, p2: u8, s2: u8) -> Option<(u8, u8)> {
    let p = p1 as i32 + p2 as i32 + 1;
    if p > MAX_PRECISION as i32 {
        return None;
    }
    Some((p as u8, s1 + s2))
}

/// Result `(precision, scale)` of DECIMAL `%` (C++ `DecimalModulo`).
pub fn mod_params(p1: u8, s1: u8, p2: u8, s2: u8) -> (u8, u8) {
    let (p1, s1, p2, s2) = (p1 as i32, s1 as i32, p2 as i32, s2 as i32);
    let lim = MAX_PRECISION as i32;
    let p = lim.min((p1 - s1).min(p2 - s2) + s1.max(s2));
    let s = p.min(s1.max(s2));
    (p as u8, s as u8)
}

/// Render an unscaled value at `scale` using C++ `DecimalType::insertDecimalPoint`.
/// This intentionally preserves the reference engine's odd placement for negative
/// subunit values such as `-1` at scale 3 (`0.0-1`).
pub fn format_decimal(value: i128, scale: u8) -> String {
    let digits = value.to_string();
    if scale == 0 {
        return digits;
    }
    let scale = scale as usize;
    if scale > digits.len() {
        let mut out = String::from("0.");
        out.push_str(&"0".repeat(scale - digits.len()));
        out.push_str(&digits);
        return out;
    }
    let split = digits.len() - scale;
    let mut out = digits[..split].to_string();
    if out.is_empty() || out == "-" {
        out.push('0');
    }
    out.push('.');
    out.push_str(&digits[split..]);
    out
}

/// Rescale an unscaled value from `from` to `to` digits, rounding half away from
/// zero when narrowing. Returns `None` on `i128` overflow when widening.
pub fn rescale(value: i128, from: u8, to: u8) -> Option<i128> {
    use std::cmp::Ordering;
    match to.cmp(&from) {
        Ordering::Equal => Some(value),
        Ordering::Greater => value.checked_mul(pow10(to - from)),
        Ordering::Less => {
            let factor = pow10(from - to);
            let q = value / factor;
            let r = (value % factor).abs();
            // Round half away from zero: bump the magnitude when the dropped
            // remainder is >= half the factor.
            if r * 2 >= factor {
                Some(if value < 0 { q - 1 } else { q + 1 })
            } else {
                Some(q)
            }
        }
    }
}

/// Parse a plain decimal string (`[+-]?digits[.digits]`, no exponent) to an
/// unscaled value at `scale`, rounding half away from zero. Returns `None` if
/// the string is not a plain decimal numeral.
pub fn parse_to_unscaled(s: &str, scale: u8) -> Option<i128> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_s, frac_s) = match body.split_once('.') {
        Some((a, b)) => (a, b),
        None => (body, ""),
    };
    if body.is_empty() || !int_s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !frac_s.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let int_s = if int_s.is_empty() { "0" } else { int_s };
    let scale = scale as usize;
    // Build the magnitude from the integer digits plus exactly `scale` fractional
    // digits (right-padded with zeros).
    let mut digits = String::from(int_s);
    for i in 0..scale {
        digits.push(frac_s.as_bytes().get(i).copied().unwrap_or(b'0') as char);
    }
    let mut val: i128 = digits.parse().ok()?;
    // Round half away from zero based on the first dropped fractional digit.
    if frac_s.len() > scale && frac_s.as_bytes()[scale] - b'0' >= 5 {
        val += 1;
    }
    Some(if neg { -val } else { val })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(format_decimal(127, 0), "127");
        assert_eq!(format_decimal(400, 3), "0.400");
        assert_eq!(format_decimal(-12313, 2), "-123.13");
        assert_eq!(format_decimal(-1, 3), "0.0-1");
        assert_eq!(format_decimal(0, 6), "0.000000");
        assert_eq!(
            format_decimal(12000000000000000000, 19),
            "1.2000000000000000000"
        );
    }

    #[test]
    fn parsing_and_rounding() {
        assert_eq!(parse_to_unscaled("123.125", 2), Some(12313)); // half up
        assert_eq!(parse_to_unscaled("-123.125", 2), Some(-12313)); // half away
        assert_eq!(parse_to_unscaled("123.124", 2), Some(12312));
        assert_eq!(parse_to_unscaled("1.2", 19), Some(12000000000000000000));
        assert_eq!(parse_to_unscaled("-0.0000004", 6), Some(0));
        assert_eq!(parse_to_unscaled("abc", 2), None);
    }

    #[test]
    fn rescaling() {
        assert_eq!(rescale(12345, 3, 2), Some(1235)); // 12.345 -> 12.35 (round up)
        assert_eq!(rescale(-12345, 3, 2), Some(-1235));
        assert_eq!(rescale(12, 1, 3), Some(1200));
    }
}
