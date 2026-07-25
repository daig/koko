//! Minimal ordered JSON used by schemaless graphs.

use crate::{Error, Result, Value};
use std::fmt::Write;

/// An owned JSON value. Object members retain their source/insertion order.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// Parse one complete UTF-8 JSON value.
    pub fn parse(input: &str) -> Result<Self> {
        let mut parser = Parser {
            input: input.as_bytes(),
            position: 0,
        };
        let value = parser.value()?;
        parser.whitespace();
        if parser.position != parser.input.len() {
            return Err(Error::conversion(format!(
                "Invalid JSON at byte {}.",
                parser.position
            )));
        }
        Ok(value)
    }

    /// Serialize compact JSON while preserving object-member order.
    pub fn render(&self) -> String {
        let mut output = String::new();
        self.render_into(&mut output);
        output
    }

    /// Convert a native query value into the JSON subset used by dynamic properties.
    pub fn from_value(value: &Value) -> Result<Self> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Int64(value) => Self::Int(*value as i128),
            Value::IntX { value, .. } => Self::Int(*value),
            Value::UInt128(value) => Self::Int(i128::try_from(*value).map_err(|_| {
                Error::conversion("UINT128 value is outside the JSON integer range.")
            })?),
            Value::Double(value) => Self::Float(*value),
            Value::Float(value) => Self::Float(*value as f64),
            Value::String(value) => Self::String(value.clone()),
            Value::List(values) => Self::Array(
                values
                    .iter()
                    .map(Self::from_value)
                    .collect::<Result<Vec<_>>>()?,
            ),
            Value::Struct(fields) => Self::Object(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), Self::from_value(value)?)))
                    .collect::<Result<Vec<_>>>()?,
            ),
            Value::Json(value) => value.clone(),
            other => {
                return Err(Error::conversion(format!(
                    "Cannot convert {} to JSON.",
                    other.logical_type()
                )));
            }
        })
    }

    /// Convert JSON scalars and containers to their closest native query values.
    pub fn to_value(&self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(*value),
            Self::Int(value) => i64::try_from(*value).map_or(
                Value::IntX {
                    value: *value,
                    kind: crate::IntKind::I128,
                },
                Value::Int64,
            ),
            Self::Float(value) => Value::Double(*value),
            Self::String(value) => Value::String(value.clone()),
            Self::Array(values) => Value::List(values.iter().map(Self::to_value).collect()),
            Self::Object(fields) => Value::Struct(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), value.to_value()))
                    .collect(),
            ),
        }
    }

    /// Approximate owned heap bytes for tracked-memory accounting.
    pub fn owned_bytes(&self) -> u64 {
        match self {
            Self::Null | Self::Bool(_) | Self::Int(_) | Self::Float(_) => 0,
            Self::String(value) => value.capacity() as u64,
            Self::Array(values) => {
                (values.capacity() * std::mem::size_of::<Self>()) as u64
                    + values.iter().map(Self::owned_bytes).sum::<u64>()
            }
            Self::Object(fields) => {
                (fields.capacity() * std::mem::size_of::<(String, Self)>()) as u64
                    + fields
                        .iter()
                        .map(|(name, value)| name.capacity() as u64 + value.owned_bytes())
                        .sum::<u64>()
            }
        }
    }

    fn render_into(&self, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::Int(value) => {
                let _ = write!(output, "{value}");
            }
            Self::Float(value) => {
                if value.is_finite() {
                    let _ = write!(output, "{value}");
                } else {
                    output.push_str("null");
                }
            }
            Self::String(value) => render_string(value, output),
            Self::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    value.render_into(output);
                }
                output.push(']');
            }
            Self::Object(fields) => {
                output.push('{');
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    render_string(name, output);
                    output.push(':');
                    value.render_into(output);
                }
                output.push('}');
            }
        }
    }
}

fn render_string(value: &str, output: &mut String) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character < '\u{20}' => {
                let _ = write!(output, "\\u{:04x}", character as u32);
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

struct Parser<'a> {
    input: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn value(&mut self) -> Result<JsonValue> {
        self.whitespace();
        match self.peek() {
            Some(b'n') => {
                self.keyword(b"null")?;
                Ok(JsonValue::Null)
            }
            Some(b't') => {
                self.keyword(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.keyword(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'"') => self.string().map(JsonValue::String),
            Some(b'[') => self.array(),
            Some(b'{') => self.object(),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.invalid()),
        }
    }

    fn array(&mut self) -> Result<JsonValue> {
        self.position += 1;
        let mut values = Vec::new();
        self.whitespace();
        if self.consume(b']') {
            return Ok(JsonValue::Array(values));
        }
        loop {
            values.push(self.value()?);
            self.whitespace();
            if self.consume(b']') {
                return Ok(JsonValue::Array(values));
            }
            self.expect(b',')?;
        }
    }

    fn object(&mut self) -> Result<JsonValue> {
        self.position += 1;
        let mut fields = Vec::new();
        self.whitespace();
        if self.consume(b'}') {
            return Ok(JsonValue::Object(fields));
        }
        loop {
            self.whitespace();
            let name = self.string()?;
            self.whitespace();
            self.expect(b':')?;
            fields.push((name, self.value()?));
            self.whitespace();
            if self.consume(b'}') {
                return Ok(JsonValue::Object(fields));
            }
            self.expect(b',')?;
        }
    }

    fn number(&mut self) -> Result<JsonValue> {
        let start = self.position;
        self.consume(b'-');
        if self.consume(b'0') {
            if self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                return Err(self.invalid());
            }
        } else {
            self.digits()?;
        }
        let mut float = false;
        if self.consume(b'.') {
            float = true;
            self.digits()?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            float = true;
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            self.digits()?;
        }
        let text =
            std::str::from_utf8(&self.input[start..self.position]).map_err(|_| self.invalid())?;
        if float {
            text.parse::<f64>()
                .map(JsonValue::Float)
                .map_err(|_| self.invalid())
        } else {
            text.parse::<i128>()
                .map(JsonValue::Int)
                .map_err(|_| self.invalid())
        }
    }

    fn digits(&mut self) -> Result<()> {
        let start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        if self.position == start {
            Err(self.invalid())
        } else {
            Ok(())
        }
    }

    fn string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut output = String::new();
        let mut segment = self.position;
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.invalid());
            };
            match byte {
                b'"' => {
                    output.push_str(
                        std::str::from_utf8(&self.input[segment..self.position])
                            .map_err(|_| self.invalid())?,
                    );
                    self.position += 1;
                    return Ok(output);
                }
                b'\\' => {
                    output.push_str(
                        std::str::from_utf8(&self.input[segment..self.position])
                            .map_err(|_| self.invalid())?,
                    );
                    self.position += 1;
                    let escaped = self.peek().ok_or_else(|| self.invalid())?;
                    self.position += 1;
                    match escaped {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{08}'),
                        b'f' => output.push('\u{0c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => output.push(self.unicode_escape()?),
                        _ => return Err(self.invalid()),
                    }
                    segment = self.position;
                }
                byte if byte < 0x20 => return Err(self.invalid()),
                _ => self.position += 1,
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char> {
        let first = self.hex_quad()?;
        let scalar = if (0xd800..=0xdbff).contains(&first) {
            self.expect(b'\\')?;
            self.expect(b'u')?;
            let second = self.hex_quad()?;
            if !(0xdc00..=0xdfff).contains(&second) {
                return Err(self.invalid());
            }
            0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
        } else {
            first
        };
        char::from_u32(scalar).ok_or_else(|| self.invalid())
    }

    fn hex_quad(&mut self) -> Result<u32> {
        if self.position + 4 > self.input.len() {
            return Err(self.invalid());
        }
        let mut value = 0;
        for _ in 0..4 {
            let digit = self.input[self.position];
            self.position += 1;
            value = value * 16
                + match digit {
                    b'0'..=b'9' => u32::from(digit - b'0'),
                    b'a'..=b'f' => u32::from(digit - b'a' + 10),
                    b'A'..=b'F' => u32::from(digit - b'A' + 10),
                    _ => return Err(self.invalid()),
                };
        }
        Ok(value)
    }

    fn keyword(&mut self, keyword: &[u8]) -> Result<()> {
        if self.input.get(self.position..self.position + keyword.len()) == Some(keyword) {
            self.position += keyword.len();
            Ok(())
        } else {
            Err(self.invalid())
        }
    }

    fn whitespace(&mut self) {
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.consume(byte) {
            Ok(())
        } else {
            Err(self.invalid())
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }

    fn invalid(&self) -> Error {
        Error::conversion(format!("Invalid JSON at byte {}.", self.position))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_json_round_trips_and_rejects_trailing_input() {
        let value = JsonValue::parse(r#"{"z":1,"a":[true,null,"x\n\u03bb"]}"#).unwrap();
        assert_eq!(value.render(), "{\"z\":1,\"a\":[true,null,\"x\\nλ\"]}");
        assert!(JsonValue::parse("{} trailing").is_err());
    }
}
