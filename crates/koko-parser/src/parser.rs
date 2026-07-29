//! Recursive-descent statement/DDL/pattern parsing plus precedence-climbing
//! expression parsing, over the [`Tok`] stream.

use crate::ast::*;
use crate::lexer::Tok;
use koko_common::{Error, IntKind, Result, Value};
mod ddl;
mod expression;
mod pattern;
mod query;
mod statement;

/// Parse a single Cypher statement (an optional trailing `;` is allowed).
pub fn parse_statement(input: &str) -> Result<Statement> {
    let (toks, spans) = crate::lexer::tokenize_spanned(input)?;
    let mut p = Parser {
        toks,
        pos: 0,
        src: input.to_string(),
        spans,
        last_return_tok: None,
        allow_pipe: true,
        proj_item_start: None,
        in_query: false,
    };
    let stmt = p.statement()?;
    p.eat(&Tok::Semicolon);
    // Trailing input after a completed query: when it *starts a new clause*
    // and the query ended with a RETURN, C++ blames that keyword. Anything
    // else (e.g. an unparsed operator) keeps the generic error.
    let trailing_clause = matches!(
        p.peek(),
        Tok::Ident(w) if matches!(
            w.to_ascii_uppercase().as_str(),
            "MATCH" | "OPTIONAL" | "UNWIND" | "WITH" | "RETURN" | "CREATE" | "MERGE"
                | "SET" | "DELETE" | "DETACH" | "CALL" | "LOAD" | "UNION"
        )
    );
    if p.peek() != &Tok::Eof {
        if trailing_clause {
            if let Some(idx) = p.last_return_tok {
                let (start, end) = p.spans.get(idx).copied().unwrap_or((0, 0));
                return Err(crate::lexer::decorated_error(
                    &p.src,
                    "RETURN can only be used at the end of the query",
                    start,
                    end,
                ));
            }
        }
        // Any other unconsumed input is the ANTLR invalid-input error: the
        // window runs from the previous token's end (whitespace included)
        // through the offending token, blamed on rule iC_Statements.
        let prev_end = p
            .pos
            .checked_sub(1)
            .and_then(|i| p.spans.get(i))
            .map(|&(_, e)| e)
            .unwrap_or(0);
        let (cs, ce) = p
            .spans
            .get(p.pos)
            .copied()
            .unwrap_or((p.src.len(), p.src.len()));
        let window = &p.src[prev_end..ce];
        return Err(crate::lexer::decorated_error(
            &p.src,
            &format!("Invalid input <{window}>: expected rule iC_Statements"),
            cs,
            ce,
        ));
    }
    Ok(stmt)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    /// The source text and per-token byte spans, for the C++-style decorated
    /// parser errors (`(line: N, offset: M)` + caret block).
    src: String,
    spans: Vec<(usize, usize)>,
    /// Token index of the last `RETURN` keyword that opened a return clause —
    /// trailing input after a completed query blames it (C++ "RETURN can only
    /// be used at the end of the query").
    last_return_tok: Option<usize>,
    /// Bare `|` is bitwise-or in expressions, but a depth-zero separator inside
    /// recursive-rel lambdas and list comprehensions. Those parsers suppress it;
    /// a parenthesized nested expression re-enables it.
    allow_pipe: bool,
    /// Token index where the current projection item began (RETURN/WITH item
    /// lists set this) — the ANTLR oC_ProjectionItem error windows from here.
    proj_item_start: Option<usize>,
    /// Whether a query clause has been consumed — mid-statement ANTLR errors
    /// blame rule oC_SingleQuery once inside one, oC_RegularQuery before.
    in_query: bool,
}

const MULTIPLICITIES: &[&str] = &["MANY_MANY", "MANY_ONE", "ONE_MANY", "ONE_ONE"];
const EMPTY_TOKEN_NAME_ERROR: &str =
    "'' is not a valid token name. Token names cannot be empty or contain any null-bytes";

impl Parser {
    // ---- token cursor helpers ----

    fn peek(&self) -> &Tok {
        &self.toks[self.pos]
    }
    fn peek_at(&self, k: usize) -> &Tok {
        self.toks.get(self.pos + k).unwrap_or(&Tok::Eof)
    }
    /// The C++ ANTLR-style invalid-input error: the source window from
    /// `window_start_tok` through the current (offending) token, the expected
    /// rule name, and a caret run under the offending token.
    fn invalid_input(&self, rule: &str, window_start_tok: usize) -> Error {
        let (ws, _) = self.spans.get(window_start_tok).copied().unwrap_or((0, 0));
        let (cs, ce) = self
            .spans
            .get(self.pos)
            .copied()
            .unwrap_or((self.src.len(), self.src.len()));
        let window = self.src[ws..ce].trim_end();
        crate::lexer::decorated_error(
            &self.src,
            &format!("Invalid input <{window}>: expected rule {rule}"),
            cs,
            ce,
        )
    }
    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: &Tok) -> Result<()> {
        if self.peek() == t {
            self.advance();
            Ok(())
        } else {
            Err(Error::parser(format!(
                "expected {t:?} but found {:?}",
                self.peek()
            )))
        }
    }
    fn at_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    fn at_kw_ahead(&self, k: usize, kw: &str) -> bool {
        matches!(self.peek_at(k), Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.at_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(Error::parser(format!(
                "expected keyword {kw} but found {:?}",
                self.peek()
            )))
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.advance() {
            Tok::Ident(s) if s.is_empty() || s.as_bytes().contains(&0) => {
                Err(Error::parser(EMPTY_TOKEN_NAME_ERROR))
            }
            Tok::Ident(s) => Ok(s),
            other => Err(Error::parser(format!(
                "expected an identifier, found {other:?}"
            ))),
        }
    }
}

/// A desugared operator call (`IN` → `list_contains`, `!` → `factorial`, …).
fn fn_call(name: &str, args: Vec<Expr>) -> Expr {
    let arg_names = vec![None; args.len()];
    Expr::Function {
        name: name.to_string(),
        distinct: false,
        args,
        arg_names,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn return_expr(sql: &str) -> Expr {
        let Statement::Query(q) = parse_statement(sql).unwrap() else {
            panic!("expected query")
        };
        let ProjectionItem::Expr { expr, .. } = &q.singles[0].ret.as_ref().unwrap().items[0] else {
            panic!("expected expression projection")
        };
        expr.clone()
    }

    #[test]
    fn cpp_compat_comparison_rhs_can_be_is_null() {
        assert_eq!(
            return_expr("RETURN False = True IS NULL"),
            Expr::Comparison {
                op: CmpOp::Eq,
                lhs: Box::new(Expr::Literal(Value::Bool(false))),
                rhs: Box::new(Expr::IsNull(Box::new(Expr::Literal(Value::Bool(true))))),
            }
        );
    }

    #[test]
    fn parser_rejects_empty_identifier_token() {
        let mut parser = Parser {
            toks: vec![Tok::Ident(String::new()), Tok::Eof],
            pos: 0,
            src: String::new(),
            spans: vec![(0, 0), (0, 0)],
            last_return_tok: None,
            allow_pipe: true,
            proj_item_start: None,
            in_query: false,
        };
        assert!(parser.ident().is_err());
    }
    #[test]
    fn parses_copy_to_with_query_path_and_options() {
        let Statement::CopyTo(copy) =
            parse_statement("COPY (RETURN 1 AS id) TO 'out.csv' (header=true, delim='|')").unwrap()
        else {
            panic!("expected COPY TO")
        };
        assert_eq!(copy.path, "out.csv");
        assert_eq!(copy.query.singles.len(), 1);
        assert_eq!(
            copy.options,
            vec![
                ("header".to_string(), LoadOptVal::Bool(true)),
                ("delim".to_string(), LoadOptVal::Str("|".to_string())),
            ]
        );
    }

    #[test]
    fn parses_export_and_import_database_forms() {
        let Statement::ExportDatabase(export) =
            parse_statement("EXPORT DATABASE 'snapshot' (format='csv', header=true)").unwrap()
        else {
            panic!("expected EXPORT DATABASE")
        };
        assert_eq!(export.path, "snapshot");
        assert_eq!(export.options.len(), 2);

        let Statement::ImportDatabase(import) =
            parse_statement("IMPORT DATABASE 'snapshot'").unwrap()
        else {
            panic!("expected IMPORT DATABASE")
        };
        assert_eq!(import.path, "snapshot");
    }

    #[test]
    fn interchange_syntax_rejects_missing_paths_and_import_options() {
        assert!(matches!(
            parse_statement("COPY (RETURN 1) TO out.csv"),
            Err(Error::Parser(_))
        ));
        assert!(matches!(
            parse_statement("EXPORT DATABASE"),
            Err(Error::Parser(_))
        ));
        assert!(matches!(
            parse_statement("IMPORT DATABASE 'snapshot' (format='csv')"),
            Err(Error::Parser(_))
        ));
        assert!(matches!(
            parse_statement("IMPORT DATABASE 'one', 'two'"),
            Err(Error::Parser(_))
        ));
        assert!(matches!(
            parse_statement("COPY (RETURN 1) TO 'out.csv' ()"),
            Err(Error::Parser(_))
        ));
        assert!(matches!(
            parse_statement("EXPORT DATABASE 'snapshot' ()"),
            Err(Error::Parser(_))
        ));
    }
}
