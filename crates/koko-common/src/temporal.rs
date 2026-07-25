//! DATE / TIMESTAMP / INTERVAL representations, parsing, and formatting.
//!
//! - DATE is days since the Unix epoch (1970-01-01).
//! - TIMESTAMP is microseconds since the epoch.
//! - INTERVAL is `(months, days, micros)` (Cypher keeps the components separate).
//!
//! Rendering matches the C++ engine (see `docs/cpp-reference/02-value-formatting.md`):
//! `YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS[.ffffff-trimmed]`, and the
//! `<n> years <n> months <n> days HH:MM:SS` interval form.

use crate::error::{Error, Result};

const MICROS_PER_SEC: i64 = 1_000_000;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// An interval: months, days, and sub-day microseconds, kept separate (a month
/// is not a fixed number of days in Cypher).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Interval {
    pub months: i32,
    pub days: i32,
    pub micros: i64,
}

impl Interval {
    /// A normalized total-micros estimate (30-day months) used only for
    /// ordering/equality of intervals.
    pub fn cmp_micros(&self) -> i128 {
        self.months as i128 * 30 * MICROS_PER_DAY as i128
            + self.days as i128 * MICROS_PER_DAY as i128
            + self.micros as i128
    }
}

// --- civil <-> days (Howard Hinnant's algorithms) ---

/// Day-of-week for a day count since the Unix epoch: `0 = Sunday … 6 = Saturday`
/// (1970-01-01 was a Thursday).
pub fn day_of_week(days: i32) -> usize {
    ((days as i64).rem_euclid(7) as usize + 4) % 7
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// --- DATE ---

pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days as i64);
    if y > 0 {
        format!("{y:04}-{m:02}-{d:02}")
    } else {
        // Year ≤ 0 is rendered as (1 - year) with a BC suffix.
        format!("{:04}-{:02}-{:02} (BC)", 1 - y, m, d)
    }
}

pub fn parse_date(s: &str) -> Option<i32> {
    let s = s.trim();
    // C++ rejects a leading '-' ('-0001-01-01' is a parse error), and accepts
    // `/` as the separator ('2020/1/1') but not mixed forms.
    if s.starts_with('-') {
        return None;
    }
    let body = s;
    let sep = if body.contains('/') && !body.contains('-') {
        '/'
    } else {
        '-'
    };
    let mut it = body.splitn(3, sep);
    let y: i64 = it.next()?.trim().parse().ok()?;
    let m: i64 = it.next()?.trim().parse().ok()?;
    let d: i64 = it.next()?.trim().parse().ok()?;
    // Strict component validation (audit V2): C++ rejects out-of-range months
    // and days rather than rolling them over — date('2020-02-30') is a parse
    // error, not 2020-03-01.
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d) as i32)
}

/// Days in `m` of year `y` (proleptic Gregorian leap rule).
pub fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

// --- TIME-of-day helpers ---

fn format_time(micros_in_day: i64) -> String {
    let secs = micros_in_day / MICROS_PER_SEC;
    let frac = micros_in_day % MICROS_PER_SEC;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut out = format!("{h:02}:{m:02}:{s:02}");
    append_frac(&mut out, frac);
    out
}

/// Append `.ffffff` (trailing zeros trimmed) if `frac` (micros) is non-zero.
fn append_frac(out: &mut String, frac: i64) {
    if frac != 0 {
        let mut f = format!("{frac:06}");
        while f.ends_with('0') {
            f.pop();
        }
        out.push('.');
        out.push_str(&f);
    }
}

/// `strict` = a clock time-of-day (timestamps): hour 0-23, minute/second 0-59
/// (audit V2 — C++ rejects '25:00' / '10:60:00'). Non-strict keeps the interval
/// semantics where hours may exceed a day (`48:24:11`).
fn parse_time_impl(t: &str, strict: bool) -> Option<i64> {
    // Strip a trailing timezone (`Z`, `+00:00`, `-05` …); the time itself never
    // starts with one of these, so cut at the first occurrence.
    let end = t
        .bytes()
        .position(|b| b == b'Z' || b == b'z' || b == b'+' || b == b'-')
        .unwrap_or(t.len());
    // The suffix must actually be a timezone: `Z`, or `±DD[:DD]` (strict mode
    // rejects `-XX:DD`-style garbage like C++).
    if strict {
        let mut tz = t[end..].trim();
        // Optional leading `Z`, then an optional `±DD[:DD]` offset
        // (`Z+00:00` appears in the corpus datasets).
        if tz.starts_with(['z', 'Z']) {
            tz = &tz[1..];
        }
        let tz_ok = tz.is_empty()
            || (tz.starts_with(['+', '-'])
                && tz[1..].split(':').all(|part| {
                    !part.is_empty() && part.len() <= 2 && part.bytes().all(|b| b.is_ascii_digit())
                })
                && tz[1..].split(':').count() <= 2);
        if !tz_ok {
            return None;
        }
    }
    let t = t[..end].trim();
    let (hms, frac) = match t.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (t, None),
    };
    let mut it = hms.split(':');
    let h: i64 = it.next()?.trim().parse().ok()?;
    let m: i64 = it.next()?.trim().parse().ok()?;
    let sec = it.next();
    // A fraction is only valid after an explicit seconds component
    // (`08:23.005612` is rejected by C++).
    if strict && frac.is_some() && sec.is_none() {
        return None;
    }
    let s: i64 = sec.unwrap_or("0").trim().parse().ok()?;
    if strict && (!(0..=23).contains(&h) || !(0..=59).contains(&m) || !(0..=59).contains(&s)) {
        return None;
    }
    // Checked arithmetic: an out-of-range time (an absurd hour count from a
    // malformed/overflowing literal) yields `None` — a clean parse failure — rather
    // than a debug-build overflow panic. Legitimate intervals (e.g. `48:24:11`) are
    // many orders of magnitude below the i64 bound.
    let mut micros = h
        .checked_mul(3600)?
        .checked_add(m.checked_mul(60)?)?
        .checked_add(s)?
        .checked_mul(MICROS_PER_SEC)?;
    if let Some(f) = frac {
        let f6: String = f.chars().chain(std::iter::repeat('0')).take(6).collect();
        micros = micros.checked_add(f6.parse::<i64>().ok()?)?;
    }
    Some(micros)
}

// --- TIMESTAMP ---

pub fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let rem = micros.rem_euclid(MICROS_PER_DAY);
    format!("{} {}", format_date(days as i32), format_time(rem))
}

pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date_part, time_part) = match s.split_once([' ', 'T']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let days = parse_date(date_part)? as i64;
    let micros_in_day = match time_part {
        Some(t) => parse_time_impl(t.trim(), true)?,
        None => 0,
    };
    // A timezone designator normalizes the instant to UTC: UTC = local - offset.
    // Applied to the absolute timestamp so a cross-midnight offset wraps the date.
    let offset = time_part.map_or(0, |t| parse_tz_offset(t.trim()));
    Some(days * MICROS_PER_DAY + micros_in_day - offset)
}

/// Parse the trailing timezone designator of a time string into an offset in
/// micros (`0` if none). Forms: `Z`/`z` (UTC), `+HH[:MM[:SS]]`, `-HH[:MM[:SS]]`,
/// and `Z+HH:MM` (a `Z` immediately followed by an explicit offset).
fn parse_tz_offset(t: &str) -> i64 {
    let Some(pos) = t
        .bytes()
        .position(|b| b == b'Z' || b == b'z' || b == b'+' || b == b'-')
    else {
        return 0;
    };
    let desig = t[pos..].trim();
    let rest = desig.trim_start_matches(['Z', 'z']);
    if rest.is_empty() {
        return 0;
    }
    let (sign, body) = match rest.strip_prefix('-') {
        Some(r) => (-1i64, r),
        None => (1i64, rest.strip_prefix('+').unwrap_or(rest)),
    };
    let mut it = body.split(':');
    let hh: i64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    let mm: i64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    let ss: i64 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    sign * (hh * 3600 + mm * 60 + ss) * MICROS_PER_SEC
}

// --- INTERVAL ---

pub fn format_interval(iv: &Interval) -> String {
    let mut parts: Vec<String> = Vec::new();
    if iv.months != 0 {
        let years = iv.months / 12;
        let months = iv.months % 12;
        if years != 0 {
            parts.push(format!("{years} year{}", plural(years)));
        }
        if months != 0 {
            parts.push(format!("{months} month{}", plural(months)));
        }
    }
    if iv.days != 0 {
        parts.push(format!("{} day{}", iv.days, plural(iv.days)));
    }
    if iv.micros != 0 {
        let neg = iv.micros < 0;
        let abs = iv.micros.unsigned_abs() as i64;
        let secs = abs / MICROS_PER_SEC;
        let frac = abs % MICROS_PER_SEC;
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        let mut t = String::new();
        if neg {
            t.push('-');
        }
        t.push_str(&format!("{h:02}:{m:02}:{s:02}"));
        append_frac(&mut t, frac);
        parts.push(t);
    }
    if parts.is_empty() {
        "00:00:00".to_string()
    } else {
        parts.join(" ")
    }
}

fn plural(n: i32) -> &'static str {
    if n != 1 { "s" } else { "" }
}

/// Parse an interval string — a faithful port of C++ `Interval::fromCString`
/// (interval_t.cpp): optional `@`, then repeated `<digits>[.frac] <specifier>`
/// groups, or one trailing `HH:MM:SS[.ffffff]` time component (≤9 hour digits)
/// that must consume the rest of the string. Errors carry the C++ messages,
/// including the sub-errors ("Field name is missing.", "parsing time",
/// "Unrecognized interval specifier string"). Negative components are not
/// accepted (C++ rejects a leading `-` as an unrecognized character).
pub fn parse_interval(s: &str) -> Result<Interval> {
    let b = s.as_bytes();
    let len = b.len();
    let given = || {
        Error::conversion(format!(
            "Error occurred during parsing interval. Given: \"{s}\"."
        ))
    };
    if len == 0 {
        return Err(Error::conversion(
            "Error occurred during parsing interval. Given empty string.".to_string(),
        ));
    }
    let mut iv = Interval {
        months: 0,
        days: 0,
        micros: 0,
    };
    let mut pos = 0usize;
    let mut found_any = false;
    if b[pos] == b'@' {
        pos += 1;
    }
    'groups: loop {
        // Skip spaces; the next char must start a number (or end the string).
        loop {
            if pos >= len {
                break 'groups;
            }
            let c = b[pos];
            if c.is_ascii_whitespace() {
                pos += 1;
            } else if c.is_ascii_digit() {
                break;
            } else {
                return Err(given());
            }
        }
        let start = pos;
        while pos < len && b[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos < len && b[pos] == b':' {
            // A time component consumes the remainder of the string.
            let rest = &s[start..];
            let micros = parse_interval_time(rest).ok_or_else(|| {
                Error::conversion(format!(
                    "Error occurred during parsing time. Given: \"{rest}\"."
                ))
            })?;
            iv.micros = iv
                .micros
                .checked_add(micros)
                .ok_or_else(interval_range_err)?;
            found_any = true;
            break 'groups;
        }
        // C++ parses the digits with CastString::operation — its own error.
        let number: i64 = s[start..pos].parse().map_err(|_| {
            Error::conversion(format!(
                "Cast failed. Could not convert \"{}\" to INT64.",
                &s[start..pos]
            ))
        })?;
        let mut fraction: i64 = 0;
        if pos < len && b[pos] == b'.' {
            pos += 1;
            let mut mult: i64 = 100_000;
            while pos < len && b[pos].is_ascii_digit() {
                if mult > 0 {
                    fraction += i64::from(b[pos] - b'0') * mult;
                }
                mult /= 10;
                pos += 1;
            }
        }
        while pos < len && b[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let id_start = pos;
        while pos < len && !b[pos].is_ascii_whitespace() {
            pos += 1;
        }
        let spec = &s[id_start..pos];
        if spec.is_empty() {
            return Err(Error::conversion(
                "Error occurred during parsing interval. Field name is missing.".to_string(),
            ));
        }
        apply_interval_specifier(&mut iv, spec, number, fraction)?;
        found_any = true;
    }
    if !found_any {
        return Err(given());
    }
    Ok(iv)
}

fn interval_range_err() -> Error {
    Error::overflow("Interval value is out of range".to_string())
}

fn interval_fraction_err() -> Error {
    Error::overflow("Interval fraction is out of range".to_string())
}

/// C++ `intervalTryAddition<int32_t>`: `target += input*multiplier`, then the
/// scaled fraction, with per-step overflow errors.
fn interval_add_i32(target: &mut i32, input: i64, multiplier: i64, fraction: i64) -> Result<()> {
    let addition = input
        .checked_mul(multiplier)
        .ok_or_else(interval_range_err)?;
    // C++ `intervalTryCastInteger` narrows via CastToInt32 — its own message.
    let base = i32::try_from(addition)
        .map_err(|_| Error::overflow(format!("Value {addition} is not within INT32 range")))?;
    *target = target.checked_add(base).ok_or_else(interval_range_err)?;
    if fraction != 0 {
        let add = (fraction * multiplier) / MICROS_PER_SEC;
        let base = i32::try_from(add).map_err(|_| interval_fraction_err())?;
        *target = target.checked_add(base).ok_or_else(interval_fraction_err)?;
    }
    Ok(())
}

/// C++ `intervalTryAddition<int64_t>`.
fn interval_add_i64(target: &mut i64, input: i64, multiplier: i64, fraction: i64) -> Result<()> {
    let addition = input
        .checked_mul(multiplier)
        .ok_or_else(interval_range_err)?;
    *target = target
        .checked_add(addition)
        .ok_or_else(interval_range_err)?;
    if fraction != 0 {
        let add = (fraction * multiplier) / MICROS_PER_SEC;
        *target = target.checked_add(add).ok_or_else(interval_fraction_err)?;
    }
    Ok(())
}

/// Apply one `<number>[.fraction] <specifier>` group — the C++ specifier alias
/// table (`tryGetDatePartSpecifier`) plus the per-part fraction spill rules.
fn apply_interval_specifier(
    iv: &mut Interval,
    spec: &str,
    number: i64,
    fraction: i64,
) -> Result<()> {
    const MICROS_PER_MSEC: i64 = 1_000;
    const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SEC;
    const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
    const DAYS_PER_WEEK: i64 = 7;
    const DAYS_PER_MONTH: i64 = 30;
    let lowered = spec.to_ascii_lowercase();
    match lowered.as_str() {
        "millennium" | "mil" | "millenniums" | "millennia" | "mils" | "millenium"
        | "milleniums" => interval_add_i32(&mut iv.months, number, 12_000, fraction),
        "century" | "cent" | "centuries" | "c" => {
            interval_add_i32(&mut iv.months, number, 1_200, fraction)
        }
        "decade" | "dec" | "decades" | "decs" => {
            interval_add_i32(&mut iv.months, number, 120, fraction)
        }
        "year" | "yr" | "y" | "years" | "yrs" => {
            interval_add_i32(&mut iv.months, number, 12, fraction)
        }
        "quarter" | "quarters" => {
            interval_add_i32(&mut iv.months, number, 3, fraction)?;
            // Reduce to fraction of a month.
            let fraction = (fraction * 3) % MICROS_PER_SEC;
            interval_add_i32(&mut iv.days, 0, DAYS_PER_MONTH, fraction)
        }
        "month" | "mon" | "months" | "mons" => {
            interval_add_i32(&mut iv.months, number, 1, 0)?;
            interval_add_i32(&mut iv.days, 0, DAYS_PER_MONTH, fraction)
        }
        "day" | "days" | "d" | "dayofmonth" => {
            interval_add_i32(&mut iv.days, number, 1, 0)?;
            interval_add_i64(&mut iv.micros, 0, MICROS_PER_DAY, fraction)
        }
        "week" | "weeks" | "w" | "weekofyear" => {
            interval_add_i32(&mut iv.days, number, DAYS_PER_WEEK, fraction)?;
            // Reduce to fraction of a day.
            let fraction = (fraction * DAYS_PER_WEEK) % MICROS_PER_SEC;
            interval_add_i64(&mut iv.micros, 0, MICROS_PER_DAY, fraction)
        }
        "hour" | "hr" | "hours" | "hrs" | "h" => {
            interval_add_i64(&mut iv.micros, number, MICROS_PER_HOUR, fraction)
        }
        "minute" | "min" | "minutes" | "mins" | "m" => {
            interval_add_i64(&mut iv.micros, number, MICROS_PER_MINUTE, fraction)
        }
        "second" | "sec" | "seconds" | "secs" | "s" => {
            interval_add_i64(&mut iv.micros, number, MICROS_PER_SEC, fraction)
        }
        "milliseconds" | "millisecond" | "ms" | "msec" | "msecs" | "msecond" | "mseconds" => {
            interval_add_i64(&mut iv.micros, number, MICROS_PER_MSEC, fraction)
        }
        "microseconds" | "microsecond" | "us" | "usec" | "usecs" | "usecond" | "useconds" => {
            // Round the fraction into whole microseconds.
            let number = number + (fraction * 2) / MICROS_PER_SEC;
            interval_add_i64(&mut iv.micros, number, 1, 0)
        }
        _ => Err(Error::conversion(format!(
            "Unrecognized interval specifier string: {lowered}."
        ))),
    }
}

/// C++ `Time::tryConvertInterval`: `H{1,9}:MM:SS[.ffffff]` (minutes/seconds in
/// `[0, 60)`), trailing spaces allowed, nothing else may follow.
fn parse_interval_time(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let len = b.len();
    let mut pos = 0usize;
    while pos < len && b[pos].is_ascii_whitespace() {
        pos += 1;
    }
    if pos >= len || !b[pos].is_ascii_digit() {
        return None;
    }
    let mut hour: i64 = 0;
    let mut digits = 9;
    while pos < len && b[pos].is_ascii_digit() {
        if digits == 0 {
            return None;
        }
        digits -= 1;
        hour = hour * 10 + i64::from(b[pos] - b'0');
        pos += 1;
    }
    if pos >= len || b[pos] != b':' {
        return None;
    }
    pos += 1;
    let min = parse_one_or_two_digits(b, &mut pos)?;
    if !(0..60).contains(&min) {
        return None;
    }
    if pos >= len || b[pos] != b':' {
        return None;
    }
    pos += 1;
    let sec = parse_one_or_two_digits(b, &mut pos)?;
    if !(0..60).contains(&sec) {
        return None;
    }
    let mut micros: i64 = 0;
    if pos < len && b[pos] == b'.' {
        pos += 1;
        let mut mult: i64 = 100_000;
        while pos < len && b[pos].is_ascii_digit() {
            if mult > 0 {
                micros += i64::from(b[pos] - b'0') * mult;
            }
            mult /= 10;
            pos += 1;
        }
    }
    while pos < len && b[pos].is_ascii_whitespace() {
        pos += 1;
    }
    if pos < len {
        return None;
    }
    Some(hour * 3_600 * MICROS_PER_SEC + min * 60 * MICROS_PER_SEC + sec * MICROS_PER_SEC + micros)
}

/// C++ `Date::parseDoubleDigit`: one or two digits.
fn parse_one_or_two_digits(b: &[u8], pos: &mut usize) -> Option<i64> {
    if *pos >= b.len() || !b[*pos].is_ascii_digit() {
        return None;
    }
    let mut v = i64::from(b[*pos] - b'0');
    *pos += 1;
    if *pos < b.len() && b[*pos].is_ascii_digit() {
        v = v * 10 + i64::from(b[*pos] - b'0');
        *pos += 1;
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_roundtrip() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(format_date(0), "1970-01-01");
        let d = parse_date("1900-01-01").unwrap();
        assert_eq!(format_date(d), "1900-01-01");
        let d = parse_date("1990-11-27").unwrap();
        assert_eq!(format_date(d), "1990-11-27");
    }

    #[test]
    fn timestamp_format() {
        let t = parse_timestamp("2011-08-20 11:25:30").unwrap();
        assert_eq!(format_timestamp(t), "2011-08-20 11:25:30");
        let t = parse_timestamp("1986-10-21 21:08:31.521").unwrap();
        assert_eq!(format_timestamp(t), "1986-10-21 21:08:31.521");
        // No fractional part → no decimals.
        let t = parse_timestamp("2020-01-01").unwrap();
        assert_eq!(format_timestamp(t), "2020-01-01 00:00:00");
    }

    #[test]
    fn interval_format_and_parse() {
        let iv = parse_interval("3 years 2 days 13:02:00").unwrap();
        assert_eq!(iv.months, 36);
        assert_eq!(iv.days, 2);
        assert_eq!(format_interval(&iv), "3 years 2 days 13:02:00");

        let iv = parse_interval("00:18:00.024").unwrap();
        assert_eq!(format_interval(&iv), "00:18:00.024");

        let iv = parse_interval("1 year").unwrap();
        assert_eq!(format_interval(&iv), "1 year");

        let iv = parse_interval("10 years 5 months 13:00:00.000024").unwrap();
        assert_eq!(format_interval(&iv), "10 years 5 months 13:00:00.000024");
    }
}
