//! A hand-written lexer for the Cypher subset.
//!
//! Produces a flat `Vec<Tok>` (terminated by [`Tok::Eof`]). Multi-character
//! comparison operators (`<>`, `<=`, `>=`) are combined here; the relationship
//! arrow pieces (`-`, `<`, `>`, `[`, `]`) are left as individual tokens and
//! assembled by the parser in pattern context, so `a > b` (arithmetic) and
//! `-[r]->` (pattern) never collide.

use koko_common::{Error, Result};

/// A lexical token.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Int(i128),
    /// An integer literal too large for `i128` but within `u128` (`UINT128`).
    UInt(u128),
    /// An integer literal too large even for `u128`; the binder turns it into
    /// the C++ conversion error.
    OverflowInt(String),
    Float(f64),
    Str(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Dot,
    /// `..` — list slice / range separator.
    DotDot,
    Colon,
    Semicolon,
    Pipe,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    /// `^` — the power operator.
    Caret,
    /// `&` — bitwise AND.
    Amp,
    /// `<<` / `>>` — bit shifts.
    ShiftL,
    ShiftR,
    /// `=~` — the regex-match operator.
    RegexMatch,
    /// `!` — postfix factorial (`!=` is rejected with its own hint).
    Bang,
    Dollar,
    Eof,
}

const EMPTY_TOKEN_NAME_ERROR: &str =
    "'' is not a valid token name. Token names cannot be empty or contain any null-bytes";

/// Tokenize `input`, or return a `Parser` error on an illegal character.
pub fn tokenize(input: &str) -> Result<Vec<Tok>> {
    Ok(tokenize_spanned(input)?.0)
}

/// A token stream plus each token's `(start, end)` byte span.
pub type SpannedTokens = (Vec<Tok>, Vec<(usize, usize)>);

/// Tokenize, also returning each token's `(start, end)` byte span — the parser
/// uses these for the C++-style `(line: N, offset: M)` + caret error blocks.
pub fn tokenize_spanned(input: &str) -> Result<SpannedTokens> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let n = bytes.len();
    let mut out = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();

    while i < n {
        let ch = input[i..].chars().next().expect("valid UTF-8 boundary");
        if is_cypher_whitespace(ch) {
            i += ch.len_utf8();
            continue;
        }
        let tok_start = i;
        let c = bytes[i];
        match c {
            // line comment
            b'/' if i + 1 < n && bytes[i + 1] == b'/' => {
                i += 2;
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // block comment
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i += 2;
                let mut terminated = false;
                while i + 1 < n {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        terminated = true;
                        break;
                    }
                    i += 1;
                }
                if !terminated {
                    return Err(Error::parser("unterminated block comment"));
                }
                i += 2;
            }
            b'(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            b')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            b'{' => {
                out.push(Tok::LBrace);
                i += 1;
            }
            b'}' => {
                out.push(Tok::RBrace);
                i += 1;
            }
            b'[' => {
                out.push(Tok::LBracket);
                i += 1;
            }
            b']' => {
                out.push(Tok::RBracket);
                i += 1;
            }
            b',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            // `..` (range/slice) before a single `.`.
            b'.' if i + 1 < n && bytes[i + 1] == b'.' => {
                out.push(Tok::DotDot);
                i += 2;
            }
            b'.' if !(i + 1 < n && bytes[i + 1].is_ascii_digit()) => {
                out.push(Tok::Dot);
                i += 1;
            }
            b':' => {
                out.push(Tok::Colon);
                i += 1;
            }
            b';' => {
                out.push(Tok::Semicolon);
                i += 1;
            }
            b'|' => {
                out.push(Tok::Pipe);
                i += 1;
            }
            b'+' => {
                out.push(Tok::Plus);
                i += 1;
            }
            b'-' => {
                out.push(Tok::Minus);
                i += 1;
            }
            b'*' => {
                out.push(Tok::Star);
                i += 1;
            }
            b'/' => {
                out.push(Tok::Slash);
                i += 1;
            }
            b'^' => {
                out.push(Tok::Caret);
                i += 1;
            }
            b'%' => {
                out.push(Tok::Percent);
                i += 1;
            }
            b'$' => {
                // A parameter name: alphanumeric (unicode included, like the
                // ANTLR symbolic-name rule — `$名前`, `$1` are valid) plus `_`.
                // A `$` with no name is the C++ invalid-input error whose
                // window runs from the statement start through the character
                // after the `$`, blamed on rule oC_RegularQuery.
                let name_start = i + 1;
                let mut j = name_start;
                for ch in input[name_start..].chars() {
                    if ch.is_alphanumeric() || ch == '_' {
                        j += ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if j == name_start {
                    let next = input[name_start..].chars().next().map_or(0, char::len_utf8);
                    let end = (name_start + next).min(n);
                    let window = &input[..end];
                    return Err(decorated_error(
                        input,
                        &format!("Invalid input <{window}>: expected rule oC_RegularQuery"),
                        name_start,
                        end.max(name_start + 1),
                    ));
                }
                out.push(Tok::Dollar);
                spans.push((tok_start, name_start));
                out.push(Tok::Ident(input[name_start..j].to_string()));
                spans.push((name_start, j));
                i = j;
            }
            b'=' => {
                if i + 1 < n && bytes[i + 1] == b'~' {
                    out.push(Tok::RegexMatch);
                    i += 2;
                } else {
                    out.push(Tok::Eq);
                    i += 1;
                }
            }
            b'<' => {
                if i + 1 < n && bytes[i + 1] == b'=' {
                    out.push(Tok::Le);
                    i += 2;
                } else if i + 1 < n && bytes[i + 1] == b'>' {
                    out.push(Tok::Neq);
                    i += 2;
                } else if i + 1 < n && bytes[i + 1] == b'<' {
                    out.push(Tok::ShiftL);
                    i += 2;
                } else {
                    out.push(Tok::Lt);
                    i += 1;
                }
            }
            b'>' => {
                if i + 1 < n && bytes[i + 1] == b'=' {
                    out.push(Tok::Ge);
                    i += 2;
                } else if i + 1 < n && bytes[i + 1] == b'>' {
                    out.push(Tok::ShiftR);
                    i += 2;
                } else {
                    out.push(Tok::Gt);
                    i += 1;
                }
            }
            b'&' => {
                out.push(Tok::Amp);
                i += 1;
            }
            b'\'' | b'"' => {
                let (s, next) = lex_string(input, i)?;
                out.push(Tok::Str(s));
                i = next;
            }
            _ if c.is_ascii_digit()
                || (c == b'.' && i + 1 < n && bytes[i + 1].is_ascii_digit()) =>
            {
                let (tok, next) = lex_number(input, i)?;
                out.push(tok);
                i = next;
            }
            _ if c == b'_' || c.is_ascii_alphabetic() => {
                let start = i;
                i += 1;
                while i < n && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                out.push(Tok::Ident(input[start..i].to_string()));
            }
            // A backtick-quoted identifier: `` `any chars` `` → a plain Ident.
            _ if c == b'`' => {
                let start = i + 1;
                let mut j = start;
                while j < n && bytes[j] != b'`' {
                    j += 1;
                }
                if j >= n {
                    return Err(Error::parser(
                        "unterminated backtick-quoted identifier".to_string(),
                    ));
                }
                let name = &input[start..j];
                if name.is_empty() || name.as_bytes().contains(&0) {
                    return Err(decorated_error(input, EMPTY_TOKEN_NAME_ERROR, i, j + 1));
                }
                out.push(Tok::Ident(name.to_string()));
                i = j + 1;
            }
            b'!' if i + 1 < n && bytes[i + 1] == b'=' => {
                // C++ rejects `!=` at the lexer with a dedicated hint.
                return Err(decorated_error(
                    input,
                    "Unknown operation '!=' (you probably meant to use '<>', which is the \
                     operator for inequality testing.)",
                    i,
                    i + 2,
                ));
            }
            b'!' => {
                out.push(Tok::Bang);
                i += 1;
            }
            _ => {
                // An unrecognized character is the ANTLR invalid-input error:
                // the window runs from the previous token's end (whitespace
                // included — the oracle shows `< —>` for an em-dash) through
                // the offending character, blamed on rule iC_Statements.
                let ch = input[i..].chars().next().expect("valid UTF-8 boundary");
                let prev_end = spans.last().map(|&(_, e)| e).unwrap_or(0);
                let window = &input[prev_end..i + ch.len_utf8()];
                return Err(decorated_error(
                    input,
                    &format!("Invalid input <{window}>: expected rule iC_Statements"),
                    i,
                    i + ch.len_utf8(),
                ));
            }
        }
        while spans.len() < out.len() {
            spans.push((tok_start, i));
        }
    }

    out.push(Tok::Eof);
    spans.push((n, n));
    Ok((out, spans))
}

/// Build the C++-style decorated parser error: the message with
/// `(line: N, offset: M)` appended, the offending line quoted, and a caret run
/// under the offending span (offset and width in characters; the pad is
/// offset + 1 to sit inside the opening quote).
pub(crate) fn decorated_error(input: &str, msg: &str, start: usize, end: usize) -> Error {
    let line = input[..start].matches('\n').count() + 1;
    let line_start = input[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = input[line_start..]
        .find('\n')
        .map_or(input.len(), |i| line_start + i);
    let stmt = &input[line_start..line_end];
    let offset = input[line_start..start].chars().count();
    let width = input[start..end.min(line_end)].chars().count().max(1);
    Error::parser(format!(
        "{msg} (line: {line}, offset: {offset})\n\"{stmt}\"\n{:pad$}{carets}",
        "",
        pad = offset + 1,
        carets = "^".repeat(width)
    ))
}

/// Lex a single- or double-quoted string starting at `start` (the quote byte).
fn lex_string(input: &str, start: usize) -> Result<(String, usize)> {
    let bytes = input.as_bytes();
    let quote = bytes[start];
    let mut i = start + 1;
    let mut s = String::new();
    while i < bytes.len() {
        let c = bytes[i];
        if c == quote {
            return Ok((s, i + 1));
        }
        if c == b'\\' {
            if i + 1 >= bytes.len() {
                return Err(Error::parser("unterminated string literal"));
            }
            match bytes[i + 1] {
                b'\\' | b'\'' | b'"' => {
                    s.push(bytes[i + 1] as char);
                    i += 2;
                }
                b'b' | b'B' => {
                    s.push('\u{0008}');
                    i += 2;
                }
                b'f' | b'F' => {
                    s.push('\u{000C}');
                    i += 2;
                }
                b'n' | b'N' => {
                    s.push('\n');
                    i += 2;
                }
                b'r' | b'R' => {
                    s.push('\r');
                    i += 2;
                }
                b't' | b'T' => {
                    s.push('\t');
                    i += 2;
                }
                b'x' | b'X' => {
                    validate_hex_escape(input, i, 2, start)?;
                    // C++ keeps byte escapes as source text so BLOB('\xAA') can
                    // later consume the post-lex string as a hex-coded byte.
                    s.push_str(&input[i..i + 4]);
                    i += 4;
                }
                b'u' => {
                    s.push(decode_unicode_escape(input, i, 4, start)?);
                    i += 6;
                }
                b'U' => {
                    s.push(decode_unicode_escape(input, i, 8, start)?);
                    i += 10;
                }
                _ => {
                    // An invalid escape makes the whole string token
                    // unmatchable in the C++ ANTLR lexer: the error window
                    // runs from the input start through the OPENING quote,
                    // with the caret on that quote.
                    return Err(decorated_error(
                        input,
                        &format!(
                            "Invalid input <{}>: expected rule oC_RegularQuery",
                            &input[..start + 1]
                        ),
                        start,
                        start + 1,
                    ));
                }
            }
        } else {
            // Copy a full UTF-8 char.
            let ch_len = utf8_len(c);
            s.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    Err(Error::parser("unterminated string literal"))
}

fn is_cypher_whitespace(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t'
            | '\n'
            | '\u{000B}'
            | '\u{000C}'
            | '\r'
            | '\u{001C}'
            | '\u{001D}'
            | '\u{001E}'
            | '\u{001F}'
            | '\u{1680}'
            | '\u{180E}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{00A0}'
            | '\u{2007}'
            | '\u{202F}'
    )
}

fn validate_hex_escape(input: &str, slash: usize, digits: usize, quote: usize) -> Result<()> {
    let start = slash + 2;
    let end = start + digits;
    let bytes = input.as_bytes();
    if end > bytes.len() || !bytes[start..end].iter().all(|b| b.is_ascii_hexdigit()) {
        // Same ANTLR shape as any invalid escape: the string token fails to
        // match, windowed through the opening quote.
        return Err(decorated_error(
            input,
            &format!(
                "Invalid input <{}>: expected rule oC_RegularQuery",
                &input[..quote + 1]
            ),
            quote,
            quote + 1,
        ));
    }
    Ok(())
}

fn decode_unicode_escape(input: &str, slash: usize, digits: usize, quote: usize) -> Result<char> {
    validate_hex_escape(input, slash, digits, quote)?;
    let start = slash + 2;
    let end = start + digits;
    let code = u32::from_str_radix(&input[start..end], 16)
        .map_err(|_| Error::parser("invalid unicode escape in string literal"))?;
    char::from_u32(code).ok_or_else(|| Error::parser("invalid unicode escape in string literal"))
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Lex an integer or floating-point literal.
fn lex_number(input: &str, start: usize) -> Result<(Tok, usize)> {
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut i = start;
    let mut is_float = false;
    while i < n && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // A `.` is a decimal point only when a digit follows; otherwise it belongs to
    // a following token (`..` slice, or a member access) and the number is an int.
    if i + 1 < n && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
        is_float = true;
        i += 1;
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < n && (bytes[i] == b'e' || bytes[i] == b'E') {
        is_float = true;
        i += 1;
        if i < n && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        while i < n && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    let text = &input[start..i];
    if is_float {
        let v: f64 = text
            .parse()
            .map_err(|_| Error::parser(format!("invalid float literal {text}")))?;
        Ok((Tok::Float(v), i))
    } else if let Ok(v) = text.parse::<i128>() {
        Ok((Tok::Int(v), i))
    } else if let Ok(v) = text.parse::<u128>() {
        // Beyond i128 but within u128 → a UINT128 literal.
        Ok((Tok::UInt(v), i))
    } else {
        // Beyond u128: kept as raw text — the binder rejects it with the C++
        // Conversion cast error (naming INT128 when negated, else UINT128).
        Ok((Tok::OverflowInt(text.to_string()), i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_basics() {
        let toks = tokenize("MATCH (a:Person) WHERE a.age >= 30 RETURN a.name").unwrap();
        assert_eq!(toks[0], Tok::Ident("MATCH".into()));
        assert!(toks.contains(&Tok::Ge));
        assert!(toks.contains(&Tok::Colon));
        assert_eq!(*toks.last().unwrap(), Tok::Eof);
    }

    #[test]
    fn lex_numbers_and_strings() {
        let toks = tokenize("RETURN 1, -2, 3.5, 'hi', \"x\"").unwrap();
        assert!(toks.contains(&Tok::Int(1)));
        assert!(toks.contains(&Tok::Float(3.5)));
        assert!(toks.contains(&Tok::Str("hi".into())));
        assert!(toks.contains(&Tok::Str("x".into())));
    }

    #[test]
    fn neq_le_ge() {
        let toks = tokenize("a <> b <= c >= d").unwrap();
        assert!(toks.contains(&Tok::Neq));
        assert!(toks.contains(&Tok::Le));
        assert!(toks.contains(&Tok::Ge));
    }

    #[test]
    fn cpp_compat_string_escapes() {
        let toks = tokenize(r#"RETURN '\b\f\n\r\t\\\'\"\xAA\u00FC\U0001F600'"#).unwrap();
        assert!(toks.contains(&Tok::Str("\u{0008}\u{000C}\n\r\t\\'\"\\xAAü😀".into())));
    }

    #[test]
    fn cpp_compat_rejects_unterminated_block_comment() {
        assert!(tokenize("RETURN 1 /* no end").is_err());
    }

    #[test]
    fn cpp_compat_rejects_empty_backtick_identifier() {
        assert!(tokenize("MATCH (a:``) RETURN *").is_err());
    }

    #[test]
    fn cpp_compat_unicode_whitespace() {
        let toks = tokenize("RETURN\u{00A0}1\u{202F}+\u{3000}2").unwrap();
        assert!(toks.contains(&Tok::Int(1)));
        assert!(toks.contains(&Tok::Plus));
        assert!(toks.contains(&Tok::Int(2)));
    }
}
