//! Interactive trailing-backslash continuation normalization.

use koko::tooling::{SourceSpan, TokenKind, analyze_cypher};
use std::borrow::Cow;

/// Normalize editor-only continuation markers and report whether the final physical line requests
/// another line.
///
/// A marker is a standalone `\` operator token followed only by whitespace on its physical line.
/// Canonical tooling tokenization keeps backslashes inside strings, identifiers, and comments out of
/// this set. Meta-command lines remain byte-preserved.
pub(crate) fn normalize_continuations(input: &str) -> (Cow<'_, str>, bool) {
    if !input.as_bytes().contains(&b'\\') {
        return (Cow::Borrowed(input), false);
    }
    let markers = continuation_markers(input);
    let requests_more = markers
        .last()
        .is_some_and(|marker| input[marker.end()..].chars().all(char::is_whitespace));
    if markers.is_empty() {
        return (Cow::Borrowed(input), false);
    }

    let mut output = String::with_capacity(input.len() - markers.len());
    let mut copied = 0;
    for marker in markers {
        output.push_str(&input[copied..marker.start()]);
        copied = marker.end();
    }
    output.push_str(&input[copied..]);
    (Cow::Owned(output), requests_more)
}

fn continuation_markers(input: &str) -> Vec<SourceSpan> {
    analyze_cypher(input, None)
        .tokens()
        .iter()
        .filter_map(|token| {
            let span = token.span();
            (token.kind() == TokenKind::Operator
                && &input[span.start()..span.end()] == "\\"
                && line_suffix_is_whitespace(input, span.end())
                && !line_is_meta_command(input, span.start()))
            .then_some(span)
        })
        .collect()
}

fn line_suffix_is_whitespace(input: &str, offset: usize) -> bool {
    input[offset..]
        .split(['\n', '\r'])
        .next()
        .is_none_or(|suffix| suffix.chars().all(char::is_whitespace))
}

fn line_is_meta_command(input: &str, offset: usize) -> bool {
    let line_start = input[..offset]
        .rfind(['\n', '\r'])
        .map_or(0, |newline| newline + 1);
    input[line_start..offset].trim_start().starts_with(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use koko::tooling::SyntaxStatus;

    #[test]
    fn removes_code_markers_but_retains_physical_newlines() {
        let source = "MATCH (n) \\  \nWHERE n.id = 1\\\r\nRETURN n \\  ";
        let (normalized, requests_more) = normalize_continuations(source);
        assert_eq!(normalized, "MATCH (n)   \nWHERE n.id = 1\r\nRETURN n   ");
        assert!(requests_more);
        assert_eq!(
            analyze_cypher(&normalized, None).status(),
            SyntaxStatus::Complete
        );
    }

    #[test]
    fn ignores_strings_identifiers_comments_and_meta_commands() {
        for source in [
            "RETURN 'unterminated\\",
            "RETURN \"unterminated\\",
            "RETURN `unterminated\\",
            "RETURN 1 // \\",
            "RETURN 1 /* \\",
            ":read C:\\",
            "  :output C:\\",
        ] {
            let (normalized, requests_more) = normalize_continuations(source);
            assert_eq!(normalized, source, "changed {source:?}");
            assert!(!requests_more, "continued {source:?}");
        }
    }

    #[test]
    fn marker_is_fragment_agnostic_and_never_joins_tokens() {
        let (expression, _) = normalize_continuations("RETURN 1 + \\\n2");
        assert_eq!(expression, "RETURN 1 + \n2");
        assert_eq!(
            analyze_cypher(&expression, None).status(),
            SyntaxStatus::Complete
        );

        let (identifier, _) = normalize_continuations("RETURN per\\\nson");
        assert_eq!(identifier, "RETURN per\nson");
        assert_eq!(
            analyze_cypher(&identifier, None).status(),
            SyntaxStatus::Invalid
        );
    }

    #[test]
    fn mixed_paste_preserves_meta_path_backslashes() {
        let source = "RETURN 1 \\\n:read C:\\\nRETURN 2 \\";
        let (normalized, requests_more) = normalize_continuations(source);
        assert_eq!(normalized, "RETURN 1 \n:read C:\\\nRETURN 2 ");
        assert!(requests_more);
    }
}
