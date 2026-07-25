//! Parser-backed, source-preserving syntax analysis for interactive tooling.
//!
//! This module is the only statement-boundary implementation in the project. It
//! performs a lexical source walk for comments, literals, and top-level
//! semicolons, then sends every non-empty segment through [`parse_statement`].
//! It never binds or executes source.

use crate::ast::{RegularQuery, Statement};
use crate::parse_statement;

/// A half-open UTF-8 byte span in the analyzed source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolSpan {
    pub start: usize,
    pub end: usize,
}

/// Coarse lexical classes used by editors and highlighters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTokenKind {
    Keyword,
    Identifier,
    Parameter,
    String,
    Number,
    Comment,
    Punctuation,
    Operator,
}

/// One source token, including comments and literal boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolToken {
    pub kind: ToolTokenKind,
    pub span: ToolSpan,
}

/// Whole-input completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSyntaxStatus {
    Empty,
    Incomplete,
    Complete,
    Invalid,
}

/// Syntactic statement family. Binding remains authoritative for semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatementClass {
    Query,
    DataDefinition,
    Graph,
    Transaction,
    Setting,
    Copy,
    ImportExport,
    Explain,
    Profile,
}

/// Expected presentation family available without binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutputClass {
    Rows,
    Status,
    Plan,
}

/// One non-empty logical statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStatement {
    pub span: ToolSpan,
    pub status: ToolSyntaxStatus,
    pub class: Option<ToolStatementClass>,
    pub output: Option<ToolOutputClass>,
}

/// A genuine source location produced by lexical or parser analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDiagnostic {
    pub message: String,
    pub span: Option<ToolSpan>,
}

/// Completion family at the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCursorKind {
    Keyword,
    Graph,
    NodeLabel,
    RelationshipLabel,
    Variable,
    Property,
    Function,
    Parameter,
    Setting,
    Path,
}

/// Conservative completion context and the source range to replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCursorContext {
    pub kind: ToolCursorKind,
    pub replacement: ToolSpan,
    pub prefix: String,
}

/// Immutable analysis of one source buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSyntaxAnalysis {
    pub status: ToolSyntaxStatus,
    pub tokens: Vec<ToolToken>,
    pub statements: Vec<ToolStatement>,
    pub diagnostic: Option<ToolDiagnostic>,
    pub cursor: Option<ToolCursorContext>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delimiter {
    Paren,
    Bracket,
    Brace,
}

#[derive(Debug)]
struct Scan {
    tokens: Vec<ToolToken>,
    boundaries: Vec<usize>,
    incomplete_at: Option<usize>,
    invalid_at: Option<usize>,
}

/// Analyze a complete source buffer. An out-of-range or non-character cursor is
/// reported as an invalid analysis rather than rounded to another byte.
pub fn analyze(source: &str, cursor: Option<usize>) -> ToolSyntaxAnalysis {
    let cursor_error =
        cursor.filter(|&offset| offset > source.len() || !source.is_char_boundary(offset));
    let scan = scan(source);
    let spans = statement_spans(source, &scan);
    let mut statements = Vec::with_capacity(spans.len());
    let mut first_error = None;

    for span in spans {
        let text = &source[span.start..span.end];
        match parse_statement(text) {
            Ok(statement) => {
                let (class, output) = classify_statement(&statement);
                statements.push(ToolStatement {
                    span,
                    status: ToolSyntaxStatus::Complete,
                    class: Some(class),
                    output: Some(output),
                });
            }
            Err(error) => {
                let incomplete = scan
                    .incomplete_at
                    .is_some_and(|offset| offset >= span.start)
                    || looks_incomplete(source, span, &scan.tokens);
                let status = if incomplete {
                    ToolSyntaxStatus::Incomplete
                } else {
                    ToolSyntaxStatus::Invalid
                };
                if first_error.is_none() {
                    first_error = Some(ToolDiagnostic {
                        message: error.to_string(),
                        span: parser_error_span(span, &scan.tokens),
                    });
                }
                statements.push(ToolStatement {
                    span,
                    status,
                    class: None,
                    output: None,
                });
            }
        }
    }

    let status = if cursor_error.is_some() || scan.invalid_at.is_some() {
        ToolSyntaxStatus::Invalid
    } else if scan.incomplete_at.is_some()
        || statements
            .iter()
            .any(|statement| statement.status == ToolSyntaxStatus::Incomplete)
    {
        ToolSyntaxStatus::Incomplete
    } else if statements
        .iter()
        .any(|statement| statement.status == ToolSyntaxStatus::Invalid)
    {
        ToolSyntaxStatus::Invalid
    } else if statements.is_empty() {
        ToolSyntaxStatus::Empty
    } else {
        ToolSyntaxStatus::Complete
    };

    let diagnostic = if let Some(offset) = cursor_error {
        Some(ToolDiagnostic {
            message: "cursor is not on a UTF-8 character boundary".to_string(),
            span: Some(ToolSpan {
                start: offset.min(source.len()),
                end: offset.min(source.len()),
            }),
        })
    } else if let Some(offset) = scan.invalid_at {
        Some(ToolDiagnostic {
            message: "mismatched closing delimiter".to_string(),
            span: Some(char_span(source, offset)),
        })
    } else if let Some(offset) = scan.incomplete_at {
        Some(ToolDiagnostic {
            message: "incomplete input".to_string(),
            span: Some(ToolSpan {
                start: offset,
                end: source.len(),
            }),
        })
    } else {
        first_error
    };

    ToolSyntaxAnalysis {
        status,
        cursor: cursor.and_then(|offset| {
            (offset <= source.len() && source.is_char_boundary(offset))
                .then(|| cursor_context(source, offset, &scan.tokens))
        }),
        tokens: scan.tokens,
        statements,
        diagnostic,
    }
}

fn scan(source: &str) -> Scan {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut boundaries = vec![0];
    let mut delimiters: Vec<(Delimiter, usize)> = Vec::new();
    let mut incomplete_at = None;
    let mut invalid_at = None;
    let mut index = 0;

    while index < bytes.len() {
        let character = source[index..].chars().next().expect("valid UTF-8");
        if character.is_whitespace() {
            index += character.len_utf8();
            continue;
        }
        let start = index;
        let kind = match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                ToolTokenKind::Comment
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                let mut closed = false;
                while index + 1 < bytes.len() {
                    if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        index += 2;
                        closed = true;
                        break;
                    }
                    index += source[index..]
                        .chars()
                        .next()
                        .expect("valid UTF-8")
                        .len_utf8();
                }
                if !closed {
                    index = bytes.len();
                    incomplete_at.get_or_insert(start);
                }
                ToolTokenKind::Comment
            }
            quote @ (b'\'' | b'"' | b'`') => {
                index += 1;
                let mut closed = false;
                while index < bytes.len() {
                    if bytes[index] == b'\\' && quote != b'`' {
                        index += 1;
                        if index < bytes.len() {
                            index += source[index..]
                                .chars()
                                .next()
                                .expect("valid UTF-8")
                                .len_utf8();
                        }
                    } else if bytes[index] == quote {
                        index += 1;
                        closed = true;
                        break;
                    } else {
                        index += source[index..]
                            .chars()
                            .next()
                            .expect("valid UTF-8")
                            .len_utf8();
                    }
                }
                if !closed {
                    incomplete_at.get_or_insert(start);
                }
                if quote == b'`' {
                    ToolTokenKind::Identifier
                } else {
                    ToolTokenKind::String
                }
            }
            b'$' => {
                index += 1;
                while index < bytes.len() {
                    let current = source[index..].chars().next().expect("valid UTF-8");
                    if current == '_' || current.is_alphanumeric() {
                        index += current.len_utf8();
                    } else {
                        break;
                    }
                }
                ToolTokenKind::Parameter
            }
            byte if byte == b'_' || byte.is_ascii_alphabetic() => {
                index += 1;
                while index < bytes.len()
                    && (bytes[index] == b'_' || bytes[index].is_ascii_alphanumeric())
                {
                    index += 1;
                }
                if is_keyword(&source[start..index]) {
                    ToolTokenKind::Keyword
                } else {
                    ToolTokenKind::Identifier
                }
            }
            byte if byte.is_ascii_digit() => {
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric()
                        || matches!(bytes[index], b'.' | b'_' | b'+' | b'-'))
                {
                    index += 1;
                }
                ToolTokenKind::Number
            }
            b'(' | b'[' | b'{' => {
                delimiters.push((delimiter(bytes[index]), index));
                index += 1;
                ToolTokenKind::Punctuation
            }
            b')' | b']' | b'}' => {
                let closing = delimiter(bytes[index]);
                if delimiters
                    .last()
                    .is_some_and(|(opening, _)| *opening == closing)
                {
                    delimiters.pop();
                } else {
                    invalid_at.get_or_insert(index);
                }
                index += 1;
                ToolTokenKind::Punctuation
            }
            b';' => {
                index += 1;
                if delimiters.is_empty() {
                    boundaries.push(index);
                }
                ToolTokenKind::Punctuation
            }
            b',' | b'.' | b':' => {
                index += 1;
                ToolTokenKind::Punctuation
            }
            _ => {
                index += character.len_utf8();
                while index < bytes.len()
                    && matches!(bytes[index], b'=' | b'<' | b'>' | b'!' | b'~')
                {
                    index += 1;
                }
                ToolTokenKind::Operator
            }
        };
        tokens.push(ToolToken {
            kind,
            span: ToolSpan { start, end: index },
        });
    }

    if let Some((_, offset)) = delimiters.first().copied() {
        incomplete_at.get_or_insert(offset);
    }
    if boundaries.last().copied() != Some(source.len()) {
        boundaries.push(source.len());
    }
    Scan {
        tokens,
        boundaries,
        incomplete_at,
        invalid_at,
    }
}

fn delimiter(byte: u8) -> Delimiter {
    match byte {
        b'(' | b')' => Delimiter::Paren,
        b'[' | b']' => Delimiter::Bracket,
        b'{' | b'}' => Delimiter::Brace,
        _ => unreachable!("delimiter byte"),
    }
}

fn statement_spans(source: &str, scan: &Scan) -> Vec<ToolSpan> {
    let mut spans = Vec::new();
    for window in scan.boundaries.windows(2) {
        let mut start = window[0];
        let mut end = window[1];
        if source.as_bytes().get(end.wrapping_sub(1)) == Some(&b';') {
            end -= 1;
        }
        while start < end {
            let character = source[start..].chars().next().expect("valid UTF-8");
            if !character.is_whitespace() {
                break;
            }
            start += character.len_utf8();
        }
        while start < end {
            let character = source[..end].chars().next_back().expect("valid UTF-8");
            if !character.is_whitespace() {
                break;
            }
            end -= character.len_utf8();
        }
        if start == end {
            continue;
        }
        let has_code = scan.tokens.iter().any(|token| {
            token.span.start >= start
                && token.span.end <= end
                && token.kind != ToolTokenKind::Comment
                && &source[token.span.start..token.span.end] != ";"
        });
        if has_code {
            spans.push(ToolSpan { start, end });
        }
    }
    spans
}

fn classify_statement(statement: &Statement) -> (ToolStatementClass, ToolOutputClass) {
    match statement {
        Statement::Explain { profile, .. } => (
            if *profile {
                ToolStatementClass::Profile
            } else {
                ToolStatementClass::Explain
            },
            ToolOutputClass::Plan,
        ),
        Statement::Query(query) => (
            ToolStatementClass::Query,
            if query_has_return(query) {
                ToolOutputClass::Rows
            } else {
                ToolOutputClass::Status
            },
        ),
        Statement::CreateGraph(_) | Statement::UseGraph { .. } | Statement::DropGraph { .. } => {
            (ToolStatementClass::Graph, ToolOutputClass::Status)
        }
        Statement::Transaction(_) => (ToolStatementClass::Transaction, ToolOutputClass::Status),
        Statement::Call(_) => (ToolStatementClass::Setting, ToolOutputClass::Rows),
        Statement::Copy(_) | Statement::CopyTo(_) => {
            (ToolStatementClass::Copy, ToolOutputClass::Status)
        }
        Statement::ExportDatabase(_) | Statement::ImportDatabase(_) => {
            (ToolStatementClass::ImportExport, ToolOutputClass::Status)
        }
        _ => (ToolStatementClass::DataDefinition, ToolOutputClass::Status),
    }
}

fn query_has_return(query: &RegularQuery) -> bool {
    query.singles.iter().any(|single| {
        single.ret.is_some()
            || single
                .parts
                .iter()
                .any(|part| !part.with.projection.items.is_empty())
    })
}

fn looks_incomplete(source: &str, span: ToolSpan, tokens: &[ToolToken]) -> bool {
    let Some(last) = tokens.iter().rev().find(|token| {
        token.span.start >= span.start
            && token.span.end <= span.end
            && token.kind != ToolTokenKind::Comment
    }) else {
        return false;
    };
    let text = source[last.span.start..last.span.end].to_ascii_uppercase();
    matches!(last.kind, ToolTokenKind::Operator)
        || matches!(
            text.as_str(),
            "," | "."
                | ":"
                | "$"
                | "RETURN"
                | "WHERE"
                | "WITH"
                | "AS"
                | "ORDER"
                | "BY"
                | "SKIP"
                | "LIMIT"
                | "UNWIND"
                | "SET"
                | "CALL"
                | "USE"
                | "GRAPH"
                | "FROM"
                | "TO"
        )
}

fn parser_error_span(span: ToolSpan, tokens: &[ToolToken]) -> Option<ToolSpan> {
    tokens
        .iter()
        .rev()
        .find(|token| {
            token.span.start >= span.start
                && token.span.end <= span.end
                && token.kind != ToolTokenKind::Comment
        })
        .map(|token| token.span)
        .or(Some(ToolSpan {
            start: span.end,
            end: span.end,
        }))
}

fn cursor_context(source: &str, cursor: usize, tokens: &[ToolToken]) -> ToolCursorContext {
    let current = tokens.iter().find(|token| {
        token.span.start <= cursor
            && cursor <= token.span.end
            && token.kind != ToolTokenKind::Comment
    });
    let replacement = current
        .filter(|token| {
            matches!(
                token.kind,
                ToolTokenKind::Identifier | ToolTokenKind::Keyword | ToolTokenKind::Parameter
            )
        })
        .map_or(
            ToolSpan {
                start: cursor,
                end: cursor,
            },
            |token| ToolSpan {
                start: token.span.start,
                end: cursor.min(token.span.end),
            },
        );
    let prefix = source[replacement.start..replacement.end].to_string();
    let before: Vec<&ToolToken> = tokens
        .iter()
        .filter(|token| token.span.end <= replacement.start && token.kind != ToolTokenKind::Comment)
        .collect();
    let previous = before
        .last()
        .map(|token| &source[token.span.start..token.span.end]);
    let previous_upper = previous.map(str::to_ascii_uppercase);
    let prior_upper = before
        .iter()
        .rev()
        .nth(1)
        .map(|token| source[token.span.start..token.span.end].to_ascii_uppercase());

    let kind = if prefix.starts_with('$') || previous == Some("$") {
        ToolCursorKind::Parameter
    } else if previous == Some(".") {
        ToolCursorKind::Property
    } else if previous == Some(":") {
        let relationship = before.iter().rev().skip(1).find_map(|token| {
            let text = &source[token.span.start..token.span.end];
            (text == "[" || text == "(").then_some(text)
        }) == Some("[");
        if relationship {
            ToolCursorKind::RelationshipLabel
        } else {
            ToolCursorKind::NodeLabel
        }
    } else if previous_upper.as_deref() == Some("GRAPH")
        || (previous_upper.as_deref() == Some("USE") && prior_upper.as_deref() != Some("CALL"))
    {
        ToolCursorKind::Graph
    } else if previous_upper.as_deref() == Some("CALL") {
        ToolCursorKind::Setting
    } else if before.iter().rev().take(4).any(|token| {
        matches!(
            source[token.span.start..token.span.end]
                .to_ascii_uppercase()
                .as_str(),
            "FROM" | "TO" | "DATABASE"
        )
    }) {
        ToolCursorKind::Path
    } else if current.is_some_and(|token| {
        token.kind == ToolTokenKind::Identifier
            && tokens.iter().any(|next| {
                next.span.start >= token.span.end && &source[next.span.start..next.span.end] == "("
            })
    }) {
        ToolCursorKind::Function
    } else if previous_upper.as_deref().is_some_and(|keyword| {
        matches!(
            keyword,
            "MATCH" | "WITH" | "RETURN" | "WHERE" | "UNWIND" | "AS"
        )
    }) {
        ToolCursorKind::Variable
    } else {
        ToolCursorKind::Keyword
    };

    ToolCursorContext {
        kind,
        replacement,
        prefix,
    }
}

fn char_span(source: &str, start: usize) -> ToolSpan {
    let end = source[start..]
        .chars()
        .next()
        .map_or(start, |character| start + character.len_utf8());
    ToolSpan { start, end }
}

/// Canonical completion vocabulary recognized by this tooling lexer.
pub const CYPHER_KEYWORDS: &[&str] = &[
    "ALTER",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "BEGIN",
    "BY",
    "CALL",
    "CASE",
    "CHECKPOINT",
    "COMMENT",
    "COMMIT",
    "COPY",
    "CREATE",
    "DATABASE",
    "DELETE",
    "DESC",
    "DETACH",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXISTS",
    "EXPLAIN",
    "EXPORT",
    "FALSE",
    "FROM",
    "GRAPH",
    "IF",
    "IMPORT",
    "IN",
    "INDEX",
    "IS",
    "LIMIT",
    "LOAD",
    "MACRO",
    "MATCH",
    "MERGE",
    "NODE",
    "NOT",
    "NULL",
    "ON",
    "OPTIONAL",
    "OR",
    "ORDER",
    "PRIMARY",
    "PROFILE",
    "READ",
    "REL",
    "REMOVE",
    "RENAME",
    "RETURN",
    "ROLLBACK",
    "SEQUENCE",
    "SET",
    "SKIP",
    "TABLE",
    "THEN",
    "TO",
    "TRANSACTION",
    "TRUE",
    "TYPE",
    "UNION",
    "UNWIND",
    "USE",
    "WHEN",
    "WHERE",
    "WITH",
    "WRITE",
];

fn is_keyword(text: &str) -> bool {
    CYPHER_KEYWORDS.contains(&text.to_ascii_uppercase().as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_only_top_level_semicolons() {
        let source = "RETURN ';' AS x; /* ; */ RETURN [1, 2][0] AS y;; // tail\n";
        let analysis = analyze(source, None);
        assert_eq!(analysis.status, ToolSyntaxStatus::Complete);
        assert_eq!(analysis.statements.len(), 2);
        assert!(
            analysis
                .tokens
                .iter()
                .any(|token| token.kind == ToolTokenKind::Comment)
        );
    }

    #[test]
    fn distinguishes_empty_incomplete_and_invalid() {
        assert_eq!(
            analyze(" ; // nothing\n", None).status,
            ToolSyntaxStatus::Empty
        );
        assert_eq!(
            analyze("RETURN ('x'", None).status,
            ToolSyntaxStatus::Incomplete
        );
        assert_eq!(analyze("RETURN )", None).status, ToolSyntaxStatus::Invalid);
    }

    #[test]
    fn spans_are_utf8_byte_offsets() {
        let source = "RETURN 'é' AS café";
        let analysis = analyze(source, Some(source.len()));
        assert_eq!(
            analysis.statements[0].span,
            ToolSpan {
                start: 0,
                end: source.len()
            }
        );
        let literal = analysis
            .tokens
            .iter()
            .find(|token| token.kind == ToolTokenKind::String)
            .unwrap();
        assert_eq!(&source[literal.span.start..literal.span.end], "'é'");
    }
}
