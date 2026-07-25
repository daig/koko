//! UUID and BLOB representation helpers (format + parse).
//!
//! UUID is a 128-bit value rendered canonically `8-4-4-4-12` lowercase. BLOB is
//! a byte string rendered with printable ASCII as-is and everything else as
//! `\xHH` (uppercase), matching the C++ engine.

use crate::{Error, Result};

/// Render a 128-bit UUID in canonical lowercase form.
pub fn format_uuid(v: u128) -> String {
    let h = format!("{v:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Parse a UUID into a 128-bit value, mirroring C++ `UUID::fromString`: an
/// optional single `{…}` bracket pair, `-` separators skipped anywhere, every
/// other character must be a hex digit, and exactly 32 hex digits are required
/// (so spaces or stray characters are rejected, unlike a blanket hex filter).
pub fn parse_uuid(s: &str) -> Option<u128> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let num_brackets = usize::from(bytes[0] == b'{');
    if num_brackets == 1 && bytes[bytes.len() - 1] != b'}' {
        return None;
    }
    let mut result: u128 = 0;
    let mut count = 0u32;
    for &c in &bytes[num_brackets..bytes.len() - num_brackets] {
        if c == b'-' {
            continue;
        }
        let digit = (c as char).to_digit(16)?;
        if count >= 32 {
            return None;
        }
        result = (result << 4) | digit as u128;
        count += 1;
    }
    (count == 32).then_some(result)
}

fn is_regular_blob_char(b: u8) -> bool {
    (32..=126).contains(&b) && b != b'\\' && b != b'\'' && b != b'"'
}

/// Render a byte string: printable ASCII verbatim, else `\xHH` (uppercase).
pub fn format_blob(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        if is_regular_blob_char(b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("\\x{b:02X}"));
        }
    }
    s
}

/// Parse a blob string into bytes, mirroring C++ `Blob::fromString` /
/// `validateHexCode`: every `\` must introduce a `\xHH` hex escape, all other
/// bytes must be ASCII (≤ 127). The lexer has already collapsed string-level
/// escapes, so this operates on the post-lex bytes exactly like the C++ engine.
pub fn parse_blob(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < len {
        let b = bytes[i];
        if b == b'\\' {
            // A backslash must begin a full `\xHH` escape (4 bytes).
            if i + 4 > len {
                return Err(Error::conversion(
                    "Invalid hex escape code encountered in string -> blob conversion: \
                     unterminated escape code at end of string"
                        .to_string(),
                ));
            }
            let hi = (bytes[i + 2] as char).to_digit(16);
            let lo = (bytes[i + 3] as char).to_digit(16);
            if bytes[i + 1] != b'x' || hi.is_none() || lo.is_none() {
                return Err(Error::conversion(format!(
                    "Invalid hex escape code encountered in string -> blob conversion: {}",
                    String::from_utf8_lossy(&bytes[i..i + 4])
                )));
            }
            out.push((hi.unwrap() * 16 + lo.unwrap()) as u8);
            i += 4;
        } else if b <= 127 {
            out.push(b);
            i += 1;
        } else {
            return Err(Error::conversion(
                "Invalid byte encountered in STRING -> BLOB conversion. All non-ascii characters \
                 must be escaped with hex codes (e.g. \\xAA)"
                    .to_string(),
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_roundtrip() {
        let s = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
        let v = parse_uuid(s).unwrap();
        assert_eq!(format_uuid(v), s);
        // Uppercase input parses to the same value.
        assert_eq!(parse_uuid("A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11"), Some(v));
    }

    #[test]
    fn blob_format() {
        assert_eq!(format_blob(&[0xAA]), "\\xAA");
        assert_eq!(format_blob(b"Hello"), "Hello");
        assert_eq!(format_blob(&[0xAB, 0xCD]), "\\xAB\\xCD");
    }
}
