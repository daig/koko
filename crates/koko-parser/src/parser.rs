//! Recursive-descent statement/DDL/pattern parsing plus precedence-climbing
//! expression parsing, over the [`Tok`] stream.

use crate::ast::*;
use crate::lexer::Tok;
use koko_common::{Error, IntKind, Result, Value};

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
    /// Bare `|` is bitwise-or in expressions, but a *separator* inside a
    /// recursive-rel lambda — suppressed there at depth 0 (any nested
    /// paren/bracket/brace re-enables it).
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

    // ---- statement dispatch ----

    fn statement(&mut self) -> Result<Statement> {
        if self.at_kw("EXPLAIN") || self.at_kw("PROFILE") {
            let profile = self.at_kw("PROFILE");
            self.advance();
            if !profile {
                self.eat_kw("LOGICAL");
            }
            let inner = Box::new(self.statement()?);
            return Ok(Statement::Explain { inner, profile });
        }
        if self.at_kw("CREATE") {
            if self.at_kw_ahead(1, "GRAPH") {
                return self.create_graph().map(Statement::CreateGraph);
            }
            if self.at_kw_ahead(1, "INDEX")
                || (self.at_kw_ahead(1, "HASH") && self.at_kw_ahead(2, "INDEX"))
                || (self.at_kw_ahead(1, "ART") && self.at_kw_ahead(2, "INDEX"))
            {
                return self.create_index().map(Statement::CreateIndex);
            }
            if self.at_kw_ahead(1, "NODE") && self.at_kw_ahead(2, "TABLE") {
                return self.create_node_table();
            }
            if self.at_kw_ahead(1, "REL") && self.at_kw_ahead(2, "TABLE") {
                return self.create_rel_table();
            }
            if self.at_kw_ahead(1, "SEQUENCE") {
                return self.create_sequence().map(Statement::CreateSequence);
            }
            if self.at_kw_ahead(1, "TYPE") {
                return self.create_type().map(Statement::CreateType);
            }
            if self.at_kw_ahead(1, "MACRO") {
                return self.create_macro().map(Statement::CreateMacro);
            }
        }
        if self.at_kw("USE") && self.at_kw_ahead(1, "GRAPH") {
            self.advance();
            self.advance();
            return Ok(Statement::UseGraph {
                name: self.ident()?,
            });
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "GRAPH") {
            self.advance();
            self.advance();
            let if_exists = self.parse_if_exists();
            return Ok(Statement::DropGraph {
                name: self.ident()?,
                if_exists,
            });
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "INDEX") {
            self.advance();
            self.advance();
            let if_exists = self.parse_if_exists();
            return Ok(Statement::DropIndex(DropIndex {
                name: self.ident()?,
                if_exists,
            }));
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "TABLE") {
            return self.drop_table().map(Statement::DropTable);
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "SEQUENCE") {
            return self.drop_sequence().map(Statement::DropSequence);
        }
        if self.at_kw("DROP") && self.at_kw_ahead(1, "MACRO") {
            return self.drop_macro();
        }
        if self.at_kw("COMMENT") && self.at_kw_ahead(1, "ON") {
            return self.comment_statement().map(Statement::Comment);
        }
        if self.at_kw("ALTER") && self.at_kw_ahead(1, "TABLE") {
            return self.alter_statement().map(Statement::Alter);
        }
        if self.at_kw("COPY") {
            if self.peek_at(1) == &Tok::LParen {
                return self.copy_to_statement().map(Statement::CopyTo);
            }
            return self.copy_statement().map(Statement::Copy);
        }
        if self.at_kw("EXPORT") {
            return self
                .export_database_statement()
                .map(Statement::ExportDatabase);
        }
        if self.at_kw("IMPORT") {
            return self
                .import_database_statement()
                .map(Statement::ImportDatabase);
        }
        if self.at_kw("BEGIN")
            || self.at_kw("COMMIT")
            || self.at_kw("ROLLBACK")
            || self.at_kw("CHECKPOINT")
        {
            return self.transaction_statement().map(Statement::Transaction);
        }
        if self.at_kw("CALL") {
            return self.call_statement();
        }
        self.regular_query().map(Statement::Query)
    }

    fn create_graph(&mut self) -> Result<CreateGraph> {
        self.expect_kw("CREATE")?;
        self.expect_kw("GRAPH")?;
        let if_not_exists = self.parse_if_not_exists();
        let name = self.ident()?;
        let kind = if self.eat_kw("ANY") {
            GraphKind::Any
        } else {
            GraphKind::Typed
        };
        Ok(CreateGraph {
            name,
            if_not_exists,
            kind,
        })
    }

    fn create_index(&mut self) -> Result<CreateIndex> {
        self.expect_kw("CREATE")?;
        let index_type = if self.eat_kw("HASH") {
            IndexType::Hash
        } else if self.eat_kw("ART") {
            IndexType::Art
        } else {
            IndexType::Hash
        };
        self.expect_kw("INDEX")?;
        let name = self.ident()?;
        let if_not_exists = self.parse_if_not_exists();
        self.expect_kw("FOR")?;
        self.expect(&Tok::LParen)?;
        let variable = self.ident()?;
        self.expect(&Tok::Colon)?;
        let table = self.ident()?;
        self.expect(&Tok::RParen)?;
        self.expect_kw("ON")?;
        self.expect(&Tok::LParen)?;
        let mut properties = Vec::new();
        loop {
            let property_variable = self.ident()?;
            if property_variable != variable {
                return Err(Error::parser(format!(
                    "index property variable {property_variable} does not match {variable}"
                )));
            }
            self.expect(&Tok::Dot)?;
            properties.push(self.ident()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        let mut options = Vec::new();
        if self.eat_kw("OPTIONS") {
            self.expect(&Tok::LBrace)?;
            if self.peek() != &Tok::RBrace {
                loop {
                    let key = self.ident()?;
                    self.expect(&Tok::Eq)?;
                    options.push((key, self.parse_expr()?));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Tok::RBrace)?;
        }
        Ok(CreateIndex {
            name,
            if_not_exists,
            index_type,
            variable,
            table,
            properties,
            options,
        })
    }

    /// `BEGIN [TRANSACTION] [READ ONLY|READ WRITE]` / `COMMIT` / `ROLLBACK` / `CHECKPOINT`.
    fn transaction_statement(&mut self) -> Result<TxnOp> {
        if self.eat_kw("COMMIT") {
            return Ok(TxnOp::Commit);
        }
        if self.eat_kw("ROLLBACK") {
            return Ok(TxnOp::Rollback);
        }
        if self.eat_kw("CHECKPOINT") {
            return Ok(TxnOp::Checkpoint);
        }
        self.expect_kw("BEGIN")?;
        self.eat_kw("TRANSACTION");
        let read_only = if self.eat_kw("READ") {
            // `READ ONLY` ⇒ read-only; `READ WRITE` ⇒ writable.
            !self.eat_kw("WRITE") && {
                self.eat_kw("ONLY");
                true
            }
        } else {
            false
        };
        Ok(TxnOp::Begin { read_only })
    }

    /// `CALL <key> = <value>` (config) or `CALL current_setting('<key>')` (read).
    fn call_statement(&mut self) -> Result<Statement> {
        self.expect_kw("CALL")?;
        let name = self.ident()?;
        if self.eat(&Tok::Eq) {
            let value = self.parse_expr()?;
            return Ok(Statement::Call(CallStmt::SetConfig { key: name, value }));
        }
        if self.eat(&Tok::LParen) {
            let func = Self::table_func_by_name(&name);
            if let Some(func) = func {
                let (arg, extra_args) = self.table_func_args(&name, func)?;
                self.expect(&Tok::RParen)?;
                let yield_items = self.parse_yield_items()?;
                // Route to the in-query SCAN path (the table function feeds the
                // query pipeline, so the surrounding query can filter / project /
                // aggregate) iff a `YIELD`/`WHERE`/`WITH`/another clause follows,
                // or a `RETURN` of something other than `*`. A bare `CALL f()` /
                // `CALL f() RETURN *` / `RETURN * ORDER BY` stays the standalone
                // short-circuit below. This mirrors Kùzu's split of
                // `iC_InQueryCall` from `iC_StandaloneCall`.
                let route_to_query = !yield_items.is_empty()
                    || self.at_kw("WHERE")
                    || self.at_kw("WITH")
                    || self.at_kw("CALL")
                    || self.at_kw("MATCH")
                    || self.at_kw("UNWIND")
                    || (self.at_kw("RETURN") && self.peek_at(1) != &Tok::Star);
                if route_to_query {
                    let where_clause = if self.eat_kw("WHERE") {
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    let clause = ReadingClause::TableFuncScan(TableFuncScanClause {
                        func,
                        arg,
                        extra_args,
                        yield_items,
                        where_clause,
                    });
                    // Continue parsing the rest of the query pipeline (further
                    // CALLs, WITH parts, RETURN) with this scan as the first
                    // reading clause.
                    let single = self.single_query_from(vec![clause])?;
                    return Ok(Statement::Query(RegularQuery {
                        singles: vec![single],
                        union_all: Vec::new(),
                    }));
                }
                // Standalone form: `RETURN *` and `ORDER BY <col>` are accepted and
                // ignored (the `.test` runner sorts result rows before comparing);
                // its presence is recorded — C++ rejects a bare `CALL f()` for
                // non-standalone table functions.
                let has_return = if self.eat_kw("RETURN") {
                    self.eat(&Tok::Star);
                    true
                } else {
                    false
                };
                if self.eat_kw("ORDER") {
                    self.expect_kw("BY")?;
                    let _ = self.ident()?;
                }
                return Ok(Statement::Call(CallStmt::TableFunc {
                    func,
                    arg,
                    extra_args,
                    has_return,
                }));
            }
            // A CALL of a non-table function name is the C++ binder error,
            // echoing the name as typed.
            return Err(Error::binder(format!(
                "{name} is not a table or algorithm function."
            )));
        }
        Err(Error::parser(format!(
            "expected `=` or `(` after CALL {name}"
        )))
    }

    /// `single_query (UNION [ALL] single_query)*`.
    fn regular_query(&mut self) -> Result<RegularQuery> {
        let mut singles = vec![self.single_query()?];
        let mut union_all = Vec::new();
        while self.eat_kw("UNION") {
            union_all.push(self.eat_kw("ALL"));
            singles.push(self.single_query()?);
        }
        Ok(RegularQuery { singles, union_all })
    }

    // ---- COPY ----

    fn copy_to_statement(&mut self) -> Result<CopyToStatement> {
        self.expect_kw("COPY")?;
        self.expect(&Tok::LParen)?;
        let query = self.regular_query()?;
        self.expect(&Tok::RParen)?;
        self.expect_kw("TO")?;
        let path = match self.advance() {
            Tok::Str(path) => path,
            other => {
                return Err(Error::parser(format!(
                    "expected a quoted file path after COPY ... TO, found {other:?}"
                )));
            }
        };
        let options = if self.peek() == &Tok::LParen {
            let options = self.load_options()?;
            if options.is_empty() {
                return Err(Error::parser(
                    "COPY ... TO option list cannot be empty.".to_string(),
                ));
            }
            options
        } else {
            Vec::new()
        };
        Ok(CopyToStatement {
            query,
            path,
            options,
        })
    }

    fn export_database_statement(&mut self) -> Result<ExportDatabaseStatement> {
        self.expect_kw("EXPORT")?;
        self.expect_kw("DATABASE")?;
        let path = match self.advance() {
            Tok::Str(path) => path,
            other => {
                return Err(Error::parser(format!(
                    "expected a quoted directory path after EXPORT DATABASE, found {other:?}"
                )));
            }
        };
        let options = if self.peek() == &Tok::LParen {
            let options = self.load_options()?;
            if options.is_empty() {
                return Err(Error::parser(
                    "EXPORT DATABASE option list cannot be empty.".to_string(),
                ));
            }
            options
        } else {
            Vec::new()
        };
        Ok(ExportDatabaseStatement { path, options })
    }

    fn import_database_statement(&mut self) -> Result<ImportDatabaseStatement> {
        self.expect_kw("IMPORT")?;
        self.expect_kw("DATABASE")?;
        let path = match self.advance() {
            Tok::Str(path) => path,
            other => {
                return Err(Error::parser(format!(
                    "expected a quoted directory path after IMPORT DATABASE, found {other:?}"
                )));
            }
        };
        Ok(ImportDatabaseStatement { path })
    }

    fn copy_statement(&mut self) -> Result<CopyStatement> {
        self.expect_kw("COPY")?;
        let table = self.ident()?;
        // Optional partial column list `COPY t(a, b) FROM` (possibly empty).
        let columns = if self.eat(&Tok::LParen) {
            let mut cols = Vec::new();
            if self.peek() != &Tok::RParen {
                loop {
                    cols.push(self.ident()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Tok::RParen)?;
            Some(cols)
        } else {
            None
        };
        self.expect_kw("FROM")?;
        // A parenthesized QUERY source: `COPY t FROM (LOAD …/MATCH …/UNWIND …)`.
        if self.peek() == &Tok::LParen
            && matches!(&self.peek_at(1), Tok::Ident(w) if matches!(
                w.to_ascii_uppercase().as_str(),
                "LOAD" | "MATCH" | "UNWIND" | "OPTIONAL" | "RETURN" | "WITH" | "CALL"
            ))
        {
            self.advance(); // (
            let query = self.regular_query()?;
            self.expect(&Tok::RParen)?;
            let options = if self.peek() == &Tok::LParen {
                self.load_options()?
            } else {
                Vec::new()
            };
            return Ok(CopyStatement {
                table,
                columns,
                file_path: String::new(),
                extra_files: Vec::new(),
                by_column: false,
                source_query: Some(query),
                options,
            });
        }
        // A table-function source `COPY t FROM TABLE_INFO('x')` scans the
        // function like `CALL … RETURN *`; any other bare identifier is the
        // C++ scope error.
        if let Tok::Ident(x) = self.peek().clone() {
            if let (Some(func), true) = (
                Self::table_func_by_name(&x),
                self.peek_at(1) == &Tok::LParen,
            ) {
                self.advance(); // name
                self.advance(); // (
                let (arg, extra_args) = self.table_func_args(&x, func)?;
                self.expect(&Tok::RParen)?;
                let options = if self.peek() == &Tok::LParen {
                    self.load_options()?
                } else {
                    Vec::new()
                };
                let clause = ReadingClause::TableFuncScan(TableFuncScanClause {
                    func,
                    arg,
                    extra_args,
                    yield_items: Vec::new(),
                    where_clause: None,
                });
                let query = RegularQuery {
                    singles: vec![SingleQuery {
                        parts: Vec::new(),
                        reading: vec![clause],
                        updating: Vec::new(),
                        ret: Some(ReturnClause {
                            distinct: false,
                            items: vec![ProjectionItem::Star],
                            order_by: Vec::new(),
                            skip: None,
                            limit: None,
                        }),
                    }],
                    union_all: Vec::new(),
                };
                return Ok(CopyStatement {
                    table,
                    columns,
                    file_path: String::new(),
                    extra_files: Vec::new(),
                    by_column: false,
                    source_query: Some(query),
                    options,
                });
            }
            return Err(Error::binder(format!("Variable {x} is not in scope.")));
        }
        // A single path, or a multi-file list `("a", "b")` / `["a", "b"]`.
        let mut files = Vec::new();
        let close = match self.peek() {
            Tok::LParen => Some(Tok::RParen),
            Tok::LBracket => Some(Tok::RBracket),
            _ => None,
        };
        if let Some(close) = close {
            self.advance();
            loop {
                match self.advance() {
                    Tok::Str(s) => files.push(s),
                    other => {
                        return Err(Error::parser(format!(
                            "expected a quoted file path after COPY ... FROM, found {other:?}"
                        )));
                    }
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&close)?;
        } else {
            match self.advance() {
                Tok::Str(s) => files.push(s),
                other => {
                    return Err(Error::parser(format!(
                        "expected a quoted file path after COPY ... FROM, found {other:?}"
                    )));
                }
            }
        }
        let by_column = if self.at_kw("BY") {
            self.advance();
            self.expect_kw("COLUMN")?;
            true
        } else {
            false
        };
        let options = if self.peek() == &Tok::LParen {
            self.load_options()?
        } else {
            Vec::new()
        };
        let file_path = files.remove(0);
        Ok(CopyStatement {
            table,
            columns,
            file_path,
            extra_files: files,
            by_column,
            source_query: None,
            options,
        })
    }

    // ---- LOAD FROM ----

    /// `LOAD [WITH HEADERS (col TYPE, …)] FROM "<path>" [(key = value, …)]`.
    fn load_from_clause(&mut self) -> Result<LoadFromClause> {
        self.expect_kw("LOAD")?;
        let headers = if self.at_kw("WITH") && self.at_kw_ahead(1, "HEADERS") {
            self.advance(); // WITH
            self.advance(); // HEADERS
            self.expect(&Tok::LParen)?;
            let mut cols = Vec::new();
            loop {
                let name = self.ident()?;
                let ty = self.parse_type_name()?;
                cols.push((name, ty));
                if self.eat(&Tok::Comma) {
                    continue;
                }
                break;
            }
            self.expect(&Tok::RParen)?;
            Some(cols)
        } else {
            None
        };
        self.expect_kw("FROM")?;
        // C++ rejects a parenthesized subquery source for LOAD outright.
        if self.peek() == &Tok::LParen {
            return Err(Error::binder(
                "LOAD FROM subquery is not supported.".to_string(),
            ));
        }
        // A single quoted path (possibly a glob), or a `["a", "b"]` file list.
        let (path, extra_paths) = if self.eat(&Tok::LBracket) {
            let mut files = Vec::new();
            if self.peek() != &Tok::RBracket {
                loop {
                    match self.advance() {
                        Tok::Str(s) => files.push(s),
                        other => {
                            return Err(Error::parser(format!(
                                "expected a quoted file path in LOAD FROM [...], found {other:?}"
                            )));
                        }
                    }
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Tok::RBracket)?;
            let mut it = files.into_iter();
            let first = it.next().ok_or_else(|| {
                Error::parser("LOAD FROM [] requires at least one file".to_string())
            })?;
            (first, it.collect())
        } else {
            match self.advance() {
                Tok::Str(s) => (s, Vec::new()),
                other => {
                    return Err(Error::parser(format!(
                        "expected a quoted file path after LOAD ... FROM, found {other:?}"
                    )));
                }
            }
        };
        let options = if self.peek() == &Tok::LParen {
            self.load_options()?
        } else {
            Vec::new()
        };
        // A `WHERE` immediately after the file filters the loaded rows (a `WHERE`
        // after a following `MATCH` is consumed by `match_clause` instead).
        let where_clause = if self.eat_kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(LoadFromClause {
            headers,
            path,
            extra_paths,
            options,
            where_clause,
        })
    }

    /// `( key [=] value, … )` — a CSV reader option list. A bare key is `true`
    /// (matching C++'s `iC_Option`), and `null_strings` can carry a string list.
    fn load_options(&mut self) -> Result<Vec<(String, LoadOptVal)>> {
        self.expect(&Tok::LParen)?;
        let mut opts = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                let key = self.ident()?;
                let val = if self.eat(&Tok::Eq) {
                    self.load_option_value()?
                } else if self.peek() == &Tok::Comma || self.peek() == &Tok::RParen {
                    LoadOptVal::Bool(true)
                } else {
                    self.load_option_value()?
                };
                opts.push((key, val));
                if self.eat(&Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(opts)
    }

    fn load_option_value(&mut self) -> Result<LoadOptVal> {
        if self.peek() == &Tok::LBracket {
            return self.load_option_list();
        }
        match self.advance() {
            Tok::Str(s) => Ok(LoadOptVal::Str(s)),
            Tok::Int(n) => i64::try_from(n)
                .map(LoadOptVal::Int)
                .map_err(|_| Error::parser(format!("CSV option integer {n} out of INT64 range"))),
            Tok::Float(x) => Ok(LoadOptVal::Float(x)),
            Tok::Ident(s) if s.eq_ignore_ascii_case("true") => Ok(LoadOptVal::Bool(true)),
            Tok::Ident(s) if s.eq_ignore_ascii_case("false") => Ok(LoadOptVal::Bool(false)),
            other => Err(Error::parser(format!(
                "expected a string/int/bool/list CSV option value, found {other:?}"
            ))),
        }
    }

    fn load_option_list(&mut self) -> Result<LoadOptVal> {
        self.expect(&Tok::LBracket)?;
        let mut vals = Vec::new();
        if self.peek() != &Tok::RBracket {
            loop {
                vals.push(self.load_option_value()?);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Tok::RBracket)?;
        Ok(LoadOptVal::List(vals))
    }

    // ---- DDL ----

    fn parse_if_not_exists(&mut self) -> bool {
        if self.at_kw("IF") && self.at_kw_ahead(1, "NOT") && self.at_kw_ahead(2, "EXISTS") {
            self.advance();
            self.advance();
            self.advance();
            true
        } else {
            false
        }
    }

    fn parse_if_exists(&mut self) -> bool {
        if self.at_kw("IF") && self.at_kw_ahead(1, "EXISTS") {
            self.advance();
            self.advance();
            true
        } else {
            false
        }
    }

    /// `DROP TABLE [IF EXISTS] <name>`.
    fn drop_table(&mut self) -> Result<DropTable> {
        self.expect_kw("DROP")?;
        self.expect_kw("TABLE")?;
        let if_exists = self.parse_if_exists();
        let name = self.ident()?;
        Ok(DropTable { name, if_exists })
    }

    /// `DROP SEQUENCE [IF EXISTS] <name>`.
    fn drop_sequence(&mut self) -> Result<DropSequence> {
        self.expect_kw("DROP")?;
        self.expect_kw("SEQUENCE")?;
        let if_exists = self.parse_if_exists();
        let name = self.ident()?;
        Ok(DropSequence { name, if_exists })
    }

    /// `CREATE TYPE <name> AS <type>`.
    fn create_type(&mut self) -> Result<CreateType> {
        self.expect_kw("CREATE")?;
        self.expect_kw("TYPE")?;
        let name = self.ident()?;
        self.expect_kw("AS")?;
        let type_name = self.parse_type_name()?;
        Ok(CreateType { name, type_name })
    }

    /// `CREATE MACRO <name>(<args>) AS <body>`. Parameters are positional
    /// identifiers, optionally followed by `name := <default>` defaults
    /// (the `:=` lexes as `Colon` then `Eq`).
    fn create_macro(&mut self) -> Result<CreateMacro> {
        self.expect_kw("CREATE")?;
        self.expect_kw("MACRO")?;
        let name = self.ident()?;
        self.expect(&Tok::LParen)?;
        let mut positional = Vec::new();
        let mut defaults = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                let arg = self.ident()?;
                if self.eat(&Tok::Colon) {
                    self.expect(&Tok::Eq)?;
                    defaults.push((arg, self.parse_expr()?));
                } else {
                    positional.push(arg);
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen)?;
        self.expect_kw("AS")?;
        // A macro body that fails to parse is the C++ ANTLR error: the invalid
        // window runs from the body's first token through the offending one,
        // blamed on rule oC_ComparisonExpression with a caret under the
        // offending token.
        let body_start = self.pos;
        let body = match self.parse_expr() {
            Ok(e) => Box::new(e),
            Err(_) => return Err(self.invalid_input("oC_ComparisonExpression", body_start)),
        };
        // Trailing tokens after the body expression fail the same way (e.g.
        // `AS exists match ...` — `exists` parses as a variable, then `match`
        // cannot follow).
        if !matches!(self.peek(), Tok::Eof | Tok::Semicolon) {
            return Err(self.invalid_input("oC_ComparisonExpression", body_start));
        }
        Ok(CreateMacro {
            name,
            positional,
            defaults,
            body,
        })
    }

    /// `DROP MACRO [IF EXISTS] <name>`.
    fn drop_macro(&mut self) -> Result<Statement> {
        self.expect_kw("DROP")?;
        self.expect_kw("MACRO")?;
        let if_exists = self.parse_if_exists();
        let name = self.ident()?;
        Ok(Statement::DropMacro { name, if_exists })
    }

    /// `COMMENT ON TABLE <name> IS '<text>'`.
    fn comment_statement(&mut self) -> Result<CommentStmt> {
        self.expect_kw("COMMENT")?;
        self.expect_kw("ON")?;
        self.expect_kw("TABLE")?;
        let table = self.ident()?;
        self.expect_kw("IS")?;
        let comment = match self.advance() {
            Tok::Str(s) => s,
            other => {
                return Err(Error::parser(format!(
                    "COMMENT ON ... IS expects a quoted string, found {other:?}"
                )));
            }
        };
        Ok(CommentStmt { table, comment })
    }

    /// A signed integer in a `CREATE SEQUENCE` option (`-5`, `9223372036854775807`),
    /// returned at `i128` width so the binder can range-check it against `INT64`.
    fn parse_seq_int(&mut self) -> Result<i128> {
        let neg = self.eat(&Tok::Minus);
        if !neg {
            self.eat(&Tok::Plus);
        }
        let mag: i128 = match self.advance() {
            Tok::Int(v) => v,
            Tok::UInt(v) => v as i128,
            other => {
                return Err(Error::parser(format!(
                    "expected an integer in SEQUENCE option, found {other:?}"
                )));
            }
        };
        Ok(if neg { -mag } else { mag })
    }

    /// `CREATE SEQUENCE [IF NOT EXISTS] <name> [START [WITH] n] [INCREMENT [BY] n]
    /// [MINVALUE n | NO MINVALUE] [MAXVALUE n | NO MAXVALUE] [CYCLE | NO CYCLE]`.
    fn create_sequence(&mut self) -> Result<CreateSequence> {
        self.expect_kw("CREATE")?;
        self.expect_kw("SEQUENCE")?;
        let if_not_exists = self.parse_if_not_exists();
        let name = self.ident()?;
        let mut seq = CreateSequence {
            name,
            if_not_exists,
            start: None,
            increment: None,
            min_value: None,
            max_value: None,
            cycle: false,
        };
        loop {
            if self.eat_kw("START") {
                self.eat_kw("WITH");
                seq.start = Some(self.parse_seq_int()?);
            } else if self.eat_kw("INCREMENT") {
                self.eat_kw("BY");
                seq.increment = Some(self.parse_seq_int()?);
            } else if self.eat_kw("MINVALUE") {
                seq.min_value = Some(self.parse_seq_int()?);
            } else if self.eat_kw("MAXVALUE") {
                seq.max_value = Some(self.parse_seq_int()?);
            } else if self.eat_kw("CYCLE") {
                seq.cycle = true;
            } else if self.at_kw("NO") {
                // `NO MINVALUE` / `NO MAXVALUE` / `NO CYCLE` — all select the default.
                self.advance();
                if self.eat_kw("CYCLE") {
                    seq.cycle = false;
                } else if self.eat_kw("MINVALUE") || self.eat_kw("MAXVALUE") {
                    // default min/max: left as `None` for the binder.
                } else {
                    return Err(Error::parser(
                        "expected MINVALUE, MAXVALUE, or CYCLE after NO".to_string(),
                    ));
                }
            } else {
                break;
            }
        }
        Ok(seq)
    }

    /// `ALTER TABLE <name> (ADD … | DROP … | RENAME …)`.
    fn alter_statement(&mut self) -> Result<AlterStatement> {
        self.expect_kw("ALTER")?;
        self.expect_kw("TABLE")?;
        let table = self.ident()?;
        let op = if self.eat_kw("ADD") {
            let if_not_exists = self.parse_if_not_exists();
            if self.eat_kw("FROM") {
                // `ADD [IF NOT EXISTS] FROM x TO y` — a rel-group endpoint pair.
                let from = self.ident()?;
                self.expect_kw("TO")?;
                let to = self.ident()?;
                AlterOp::AddFromTo {
                    from,
                    to,
                    if_not_exists,
                }
            } else {
                let name = self.ident()?;
                let type_name = self.parse_type_name()?;
                let default = if self.eat_kw("DEFAULT") {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                AlterOp::AddProperty {
                    name,
                    type_name,
                    default,
                    if_not_exists,
                }
            }
        } else if self.eat_kw("DROP") {
            let if_exists = self.parse_if_exists();
            if self.eat_kw("FROM") {
                // `DROP [IF EXISTS] FROM x TO y` — drop a rel-group endpoint pair.
                let from = self.ident()?;
                self.expect_kw("TO")?;
                let to = self.ident()?;
                AlterOp::DropFromTo {
                    from,
                    to,
                    if_exists,
                }
            } else {
                let name = self.ident()?;
                AlterOp::DropProperty { name, if_exists }
            }
        } else if self.eat_kw("RENAME") {
            // `RENAME TO <new>` renames the table; `RENAME [COLUMN] <old> TO <new>`
            // renames a property.
            if self.eat_kw("TO") {
                AlterOp::RenameTable { new: self.ident()? }
            } else {
                self.eat_kw("COLUMN");
                let old = self.ident()?;
                self.expect_kw("TO")?;
                let new = self.ident()?;
                AlterOp::RenameProperty { old, new }
            }
        } else {
            return Err(Error::parser(format!(
                "expected ADD, DROP, or RENAME after ALTER TABLE {table}"
            )));
        };
        Ok(AlterStatement { table, op })
    }

    fn create_node_table(&mut self) -> Result<Statement> {
        self.expect_kw("CREATE")?;
        self.expect_kw("NODE")?;
        self.expect_kw("TABLE")?;
        let if_not_exists = self.parse_if_not_exists();
        let name = self.ident()?;
        // `CREATE NODE TABLE <name> AS <query>` (CTAS) — no column list.
        if self.eat_kw("AS") {
            let query = self.regular_query()?;
            return Ok(Statement::CreateTableAs(CreateTableAs {
                name,
                is_node: true,
                pairs: Vec::new(),
                storage_direction: None,
                if_not_exists,
                query,
            }));
        }
        self.expect(&Tok::LParen)?;
        let mut columns = Vec::new();
        let mut primary_key: Option<String> = None;
        loop {
            if self.at_kw("PRIMARY") {
                // Trailing form: `PRIMARY KEY(col)`. Restating the same column
                // as an inline PK is accepted (C++); a different one is not.
                self.advance();
                self.expect_kw("KEY")?;
                self.expect(&Tok::LParen)?;
                let col = self.ident()?;
                if primary_key
                    .as_ref()
                    .is_some_and(|p| !p.eq_ignore_ascii_case(&col))
                {
                    return Err(Error::parser("Found multiple primary keys.".to_string()));
                }
                primary_key = Some(col);
                self.expect(&Tok::RParen)?;
            } else {
                let col = self.column_def()?;
                // Inline form: `col TYPE PRIMARY [KEY]`.
                if self.at_kw("PRIMARY") {
                    self.advance();
                    let _ = self.eat_kw("KEY");
                    if primary_key.is_some() {
                        return Err(Error::parser("Found multiple primary keys.".to_string()));
                    }
                    primary_key = Some(col.name.clone());
                }
                columns.push(col);
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        let (_, storage, format) = if self.eat_kw("WITH") {
            self.table_storage_options()?
        } else {
            (None, None, None)
        };
        let primary_key =
            primary_key.ok_or_else(|| Error::parser("Can not find primary key.".to_string()))?;
        Ok(Statement::CreateNodeTable(CreateNodeTable {
            name,
            columns,
            primary_key,
            if_not_exists,
            storage,
            format,
        }))
    }

    fn create_rel_table(&mut self) -> Result<Statement> {
        self.expect_kw("CREATE")?;
        self.expect_kw("REL")?;
        self.expect_kw("TABLE")?;
        // `CREATE REL TABLE GROUP <name>(…)` — `GROUP` is an optional alias keyword
        // (the C++ `transformCreateRelGroup` ignores it): every rel table is a rel
        // group, so eating it unconditionally is exact parity. `GROUP` is reserved,
        // so a table literally named `group` is impossible.
        self.eat_kw("GROUP");
        let if_not_exists = self.parse_if_not_exists();
        let name = self.ident()?;
        self.expect(&Tok::LParen)?;
        // One or more `FROM x TO y` node-table pairs, then property columns.
        let mut pairs = Vec::new();
        self.expect_kw("FROM")?;
        let from = self.ident()?;
        self.expect_kw("TO")?;
        pairs.push((from, self.ident()?));
        let mut columns = Vec::new();
        let mut multiplicity = koko_common::RelMultiplicity::default();
        while self.eat(&Tok::Comma) {
            // An additional `FROM x TO y` pair (a multi-pair rel table).
            if self.eat_kw("FROM") {
                let f = self.ident()?;
                self.expect_kw("TO")?;
                pairs.push((f, self.ident()?));
                continue;
            }
            // A multiplicity keyword (e.g. MANY_ONE) anywhere in the comma list:
            // any lone identifier in this slot (no column type follows) is a
            // multiplicity *candidate*; an unknown one is the C++ binder error
            // ("Cannot bind MANY_LOT as relationship multiplicity."), not a
            // parse failure.
            if let Tok::Ident(s) = self.peek() {
                if matches!(self.peek_at(1), Tok::Comma | Tok::RParen) {
                    if !MULTIPLICITIES.iter().any(|m| s.eq_ignore_ascii_case(m)) {
                        return Err(Error::binder(format!(
                            "Cannot bind {s} as relationship multiplicity."
                        )));
                    }
                    multiplicity = koko_common::RelMultiplicity::from_keyword(s);
                    self.advance();
                    continue;
                }
            }
            columns.push(self.column_def()?);
        }
        self.expect(&Tok::RParen)?;
        let (storage_direction, storage, format) = if self.eat_kw("WITH") {
            self.table_storage_options()?
        } else {
            (None, None, None)
        };
        // `CREATE REL TABLE <name> (FROM a TO b) AS <query>` (CTAS) — only the
        // FROM-TO pairs precede `AS`; the schema comes from the query.
        if self.eat_kw("AS") {
            let query = self.regular_query()?;
            return Ok(Statement::CreateTableAs(CreateTableAs {
                name,
                is_node: false,
                pairs,
                storage_direction,
                if_not_exists,
                query,
            }));
        }
        Ok(Statement::CreateRelTable(CreateRelTable {
            name,
            pairs,
            columns,
            if_not_exists,
            multiplicity,
            storage_direction,
            storage,
            format,
        }))
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.ident()?;
        let type_name = self.parse_type_name()?;
        let default = if self.eat_kw("DEFAULT") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(ColumnDef {
            name,
            type_name,
            default,
        })
    }

    /// Read a DDL/cast type expression, reconstructing it as a canonical string
    /// for the binder's type parser: a base name, then any `[N]`/`[]` list
    /// suffixes and/or a parameter group `(...)` (for `STRUCT`/`MAP`/`UNION`/
    /// `DECIMAL`), recursively.
    fn parse_type_name(&mut self) -> Result<String> {
        let mut s = self.ident()?;
        loop {
            match self.peek() {
                Tok::LBracket => {
                    self.advance();
                    s.push('[');
                    if let Tok::Int(n) = self.peek() {
                        s.push_str(&n.to_string());
                        self.advance();
                    }
                    self.expect(&Tok::RBracket)?;
                    s.push(']');
                }
                Tok::LParen => {
                    self.advance();
                    s.push('(');
                    self.reconstruct_type_group(&mut s)?;
                }
                _ => break,
            }
        }
        Ok(s)
    }

    /// Append a balanced `(...)` group's contents (already past the `(`) to `s`,
    /// with spacing that keeps `STRUCT(field TYPE, …)` re-parseable.
    fn reconstruct_type_group(&mut self, s: &mut String) -> Result<()> {
        let mut depth = 1;
        while depth > 0 {
            match self.advance() {
                Tok::LParen => {
                    s.push('(');
                    depth += 1;
                }
                Tok::RParen => {
                    s.push(')');
                    depth -= 1;
                }
                Tok::LBracket => s.push('['),
                Tok::RBracket => s.push(']'),
                Tok::Comma => s.push(','),
                Tok::Ident(name) => {
                    if s.ends_with(|c: char| c.is_alphanumeric() || c == ']' || c == ')') {
                        s.push(' ');
                    }
                    s.push_str(&name);
                }
                Tok::Int(n) => {
                    if s.ends_with(|c: char| c.is_alphanumeric()) {
                        s.push(' ');
                    }
                    s.push_str(&n.to_string());
                }
                Tok::Eof => return Err(Error::parser("unterminated type parameter list")),
                other => {
                    return Err(Error::parser(format!("unexpected {other:?} in type")));
                }
            }
        }
        Ok(())
    }

    fn table_storage_options(
        &mut self,
    ) -> Result<(Option<String>, Option<String>, Option<String>)> {
        self.expect(&Tok::LParen)?;
        let mut storage_direction = None;
        let mut storage = None;
        let mut format = None;
        if self.peek() != &Tok::RParen {
            loop {
                let key = self.ident()?;
                let val = if self.eat(&Tok::Eq) {
                    self.table_option_value()?
                } else {
                    String::new()
                };
                if key.eq_ignore_ascii_case("storage_direction") {
                    storage_direction = Some(val);
                } else if key.eq_ignore_ascii_case("storage") {
                    storage = Some(val);
                } else if key.eq_ignore_ascii_case("format") {
                    format = Some(val);
                }
                if self.eat(&Tok::Comma) {
                    continue;
                }
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok((storage_direction, storage, format))
    }

    fn table_option_value(&mut self) -> Result<String> {
        match self.advance() {
            Tok::Str(s) | Tok::Ident(s) => Ok(s),
            Tok::Int(n) => Ok(n.to_string()),
            other => Err(Error::parser(format!(
                "expected a string/int/identifier table option value, found {other:?}"
            ))),
        }
    }

    // ---- queries ----

    /// `YIELD col [AS alias] (, col [AS alias])*` — empty when no YIELD.
    fn parse_yield_items(&mut self) -> Result<Vec<(String, Option<String>)>> {
        let mut items = Vec::new();
        if !self.eat_kw("YIELD") {
            return Ok(items);
        }
        loop {
            let col = self.ident()?;
            let alias = if self.eat_kw("AS") {
                Some(self.ident()?)
            } else {
                None
            };
            items.push((col, alias));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(items)
    }

    /// Parse a CALL'd table function's argument list (already inside the
    /// parens) and enforce its arity with the C++ binder error, echoing the
    /// name as typed.
    /// A short rendering of a rejected table-function argument (C++ echoes the
    /// expression as typed).
    fn table_arg_name(e: &Expr) -> String {
        match e {
            Expr::Property { base, name } => {
                format!("{}.{name}", Self::table_arg_name(base))
            }
            Expr::Variable(v) => v.clone(),
            Expr::Function { name, .. } => format!("{}()", name.to_uppercase()),
            _ => "expression".to_string(),
        }
    }

    /// The C++ expression-kind label for the argument rejection.
    fn table_arg_kind(e: &Expr) -> &'static str {
        match e {
            Expr::Property { .. } => "PROPERTY",
            Expr::Variable(_) => "VARIABLE",
            Expr::Function { .. } => "FUNCTION",
            _ => "EXPRESSION",
        }
    }

    /// Fold a constant expression argument of a TABLE function to its string
    /// rendering (C++ binds these as real expressions; the corpus uses simple
    /// pure string functions like `upper("person")`).
    fn fold_const_str(e: &Expr) -> Option<String> {
        match e {
            Expr::Literal(Value::String(s)) => Some(s.clone()),
            Expr::Literal(v) => Some(v.to_result_string()),
            Expr::Function { name, args, .. } => {
                let vals: Vec<String> = args
                    .iter()
                    .map(Self::fold_const_str)
                    .collect::<Option<_>>()?;
                match name.to_ascii_lowercase().as_str() {
                    "upper" | "ucase" if vals.len() == 1 => Some(vals[0].to_uppercase()),
                    "lower" | "lcase" if vals.len() == 1 => Some(vals[0].to_lowercase()),
                    "concat" => Some(vals.concat()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn table_func_args(
        &mut self,
        name: &str,
        func: TableFunc,
    ) -> Result<(Option<String>, Vec<String>)> {
        let mut args = Vec::new();
        loop {
            let a = match self.peek().clone() {
                Tok::Str(a) => {
                    self.advance();
                    a
                }
                // A non-string literal still counts as the argument — its
                // rendering feeds the function's own validation
                // (show_connection(123) → "…only be called on a rel table!").
                Tok::Int(n) => {
                    self.advance();
                    n.to_string()
                }
                Tok::Float(f) => {
                    self.advance();
                    f.to_string()
                }
                // A constant EXPRESSION argument folds at parse
                // (show_connection(upper("person")) → "PERSON").
                Tok::Ident(_) => {
                    let e = self.parse_expr()?;
                    match Self::fold_const_str(&e) {
                        Some(v) => v,
                        // A non-constant argument is the C++ argument-kind
                        // rejection (show_connection(a.fName)).
                        None => {
                            return Err(Error::binder(format!(
                                "{} has type {} but LITERAL,PARAMETER,PATTERN was expected.",
                                Self::table_arg_name(&e),
                                Self::table_arg_kind(&e)
                            )));
                        }
                    }
                }
                _ => break,
            };
            args.push(a);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let expected = match func {
            TableFunc::TableInfo
            | TableFunc::ShowConnection
            | TableFunc::StorageInfo
            | TableFunc::CurrentSetting
            | TableFunc::StatsInfo => 1,
            TableFunc::CacheArrayColumn => 2,
            _ => 0,
        };
        if args.len() != expected {
            let actual = vec!["STRING"; args.len()].join(",");
            let exp = vec!["STRING"; expected].join(",");
            return Err(Error::binder(format!(
                "Function {name} did not receive correct arguments:\nActual:   ({actual})\nExpected: ({exp})"
            )));
        }
        let mut it = args.into_iter();
        let first = it.next();
        Ok((first, it.collect()))
    }

    /// Map a CALL'd name to its table function, if it is one.
    fn table_func_by_name(name: &str) -> Option<TableFunc> {
        match name.to_ascii_lowercase().as_str() {
            "show_sequences" => Some(TableFunc::ShowSequences),
            "show_tables" => Some(TableFunc::ShowTables),
            "table_info" => Some(TableFunc::TableInfo),
            "show_macros" => Some(TableFunc::ShowMacros),
            "show_functions" => Some(TableFunc::ShowFunctions),
            "db_version" => Some(TableFunc::DbVersion),
            "show_indexes" => Some(TableFunc::ShowIndexes),
            "show_warnings" => Some(TableFunc::ShowWarnings),
            "show_connection" => Some(TableFunc::ShowConnection),
            "storage_info" => Some(TableFunc::StorageInfo),
            "stats_info" => Some(TableFunc::StatsInfo),
            "show_official_extensions" => Some(TableFunc::ShowOfficialExtensions),
            "current_setting" => Some(TableFunc::CurrentSetting),
            "bm_info" => Some(TableFunc::BmInfo),
            "show_loaded_extensions" => Some(TableFunc::ShowLoadedExtensions),
            "_cache_array_column_locally" => Some(TableFunc::CacheArrayColumn),
            "clear_warnings" => Some(TableFunc::ClearWarnings),
            _ => None,
        }
    }

    /// A mid-query `CALL fn(['arg']) [YIELD …] [WHERE …]` reading clause.
    fn call_reading_clause(&mut self) -> Result<ReadingClause> {
        self.expect_kw("CALL")?;
        let name = self.ident()?;
        self.expect(&Tok::LParen)?;
        let func = Self::table_func_by_name(&name).ok_or_else(|| {
            Error::binder(format!("{name} is not a table or algorithm function."))
        })?;
        let (arg, extra_args) = self.table_func_args(&name, func)?;
        self.expect(&Tok::RParen)?;
        let yield_items = self.parse_yield_items()?;
        let where_clause = if self.eat_kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(ReadingClause::TableFuncScan(TableFuncScanClause {
            func,
            arg,
            extra_args,
            yield_items,
            where_clause,
        }))
    }

    fn single_query(&mut self) -> Result<SingleQuery> {
        self.single_query_from(Vec::new())
    }

    fn single_query_from(&mut self, seeded: Vec<ReadingClause>) -> Result<SingleQuery> {
        // Accumulate reading/updating clauses; each `WITH` closes a query part and
        // starts a new one. The trailing clauses + optional `RETURN` form the
        // final part.
        let seeded_any = !seeded.is_empty();
        let mut parts: Vec<QueryPart> = Vec::new();
        let mut reading: Vec<ReadingClause> = seeded;
        let mut updating: Vec<UpdatingClause> = Vec::new();
        loop {
            if self.at_kw("MATCH")
                || self.at_kw("UNWIND")
                || self.at_kw("SET")
                || self.at_kw("MERGE")
                || self.at_kw("DELETE")
                || (self.at_kw("OPTIONAL") && self.at_kw_ahead(1, "MATCH"))
            {
                self.in_query = true;
            }
            if self.at_kw("MATCH") || (self.at_kw("OPTIONAL") && self.at_kw_ahead(1, "MATCH")) {
                reading.push(ReadingClause::Match(self.match_clause()?));
            } else if self.at_kw("UNWIND") {
                reading.push(ReadingClause::Unwind(self.unwind_clause()?));
            } else if self.at_kw("LOAD") {
                reading.push(ReadingClause::LoadFrom(self.load_from_clause()?));
            } else if self.at_kw("CALL")
                && matches!(self.peek_at(1), Tok::Ident(_))
                && self.peek_at(2) == &Tok::LParen
            {
                reading.push(self.call_reading_clause()?);
            } else if self.at_kw("CREATE")
                || self.at_kw("SET")
                || self.at_kw("DELETE")
                || self.at_kw("MERGE")
                || (self.at_kw("DETACH") && self.at_kw_ahead(1, "DELETE"))
            {
                updating.push(self.updating_clause()?);
            } else if self.at_kw("WITH") {
                let with = self.with_clause()?;
                parts.push(QueryPart {
                    reading: std::mem::take(&mut reading),
                    updating: std::mem::take(&mut updating),
                    with,
                });
            } else {
                break;
            }
        }
        let ret = if self.at_kw("RETURN") {
            Some(self.return_clause()?)
        } else {
            None
        };
        if !seeded_any
            && parts.is_empty()
            && reading.is_empty()
            && updating.is_empty()
            && ret.is_none()
        {
            return Err(Error::parser(format!(
                "expected a MATCH, CREATE, or RETURN clause but found {:?}",
                self.peek()
            )));
        }
        Ok(SingleQuery {
            parts,
            reading,
            updating,
            ret,
        })
    }

    fn match_clause(&mut self) -> Result<MatchClause> {
        let optional = self.eat_kw("OPTIONAL");
        self.expect_kw("MATCH")?;
        let patterns = self.pattern_list()?;
        let where_clause = if self.eat_kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        // An optional `HINT <join-tree>` (C++ `oC_Match`'s trailing `iC_Hint`,
        // after the pattern and optional WHERE). It only constrains join order,
        // which our single-order planner ignores and the `.test` runner sorts
        // away — so parsing and discarding it is result-correct.
        let hint = if self.eat_kw("HINT") {
            Some(self.parse_join_hint()?)
        } else {
            None
        };
        Ok(MatchClause {
            patterns,
            optional,
            where_clause,
            hint,
        })
    }

    /// Consume and discard a join-order HINT tree (`<var>`, `<tree> JOIN <tree>`,
    /// `<tree> MULTI_JOIN <var>…`, `( <tree> )`). Loop-based so it is immune to the
    /// grammar's JOIN-associativity ambiguity. The planner ignores join order.
    fn parse_join_hint(&mut self) -> Result<JoinHint> {
        let mut tree = self.parse_join_atom()?;
        loop {
            if self.eat_kw("JOIN") {
                let rhs = self.parse_join_atom()?;
                tree = JoinHint::Join(Box::new(tree), Box::new(rhs));
            } else if self.eat_kw("MULTI_JOIN") {
                let mut rels = vec![self.ident()?];
                while self.eat_kw("MULTI_JOIN") {
                    rels.push(self.ident()?);
                }
                tree = JoinHint::MultiJoin(Box::new(tree), rels);
            } else {
                break;
            }
        }
        Ok(tree)
    }

    /// One node of a join-order HINT tree: a parenthesized sub-tree or a (dotted)
    /// schema name.
    fn parse_join_atom(&mut self) -> Result<JoinHint> {
        if self.eat(&Tok::LParen) {
            let t = self.parse_join_hint()?;
            self.expect(&Tok::RParen)?;
            Ok(t)
        } else {
            let mut name = self.ident()?;
            while self.eat(&Tok::Dot) {
                name = self.ident()?;
            }
            Ok(JoinHint::Var(name))
        }
    }

    fn unwind_clause(&mut self) -> Result<UnwindClause> {
        self.expect_kw("UNWIND")?;
        let expr = self.parse_expr()?;
        self.expect_kw("AS")?;
        let var = self.ident()?;
        Ok(UnwindClause { expr, var })
    }

    /// Parse one updating clause: `CREATE …`, `SET …`, `[DETACH] DELETE …`, or `MERGE …`.
    fn updating_clause(&mut self) -> Result<UpdatingClause> {
        if self.at_kw("CREATE") {
            self.advance();
            let patterns = self.pattern_list()?;
            Ok(UpdatingClause::Create(CreateClause { patterns }))
        } else if self.at_kw("SET") {
            Ok(UpdatingClause::Set(self.set_clause()?))
        } else if self.at_kw("MERGE") {
            Ok(UpdatingClause::Merge(self.merge_clause()?))
        } else {
            Ok(UpdatingClause::Delete(self.delete_clause()?))
        }
    }

    /// `SET item (, item)*`.
    fn set_clause(&mut self) -> Result<SetClause> {
        self.expect_kw("SET")?;
        Ok(SetClause {
            items: self.set_items()?,
        })
    }

    /// `MERGE <pattern> (ON CREATE SET … | ON MATCH SET …)*`.
    fn merge_clause(&mut self) -> Result<MergeClause> {
        self.expect_kw("MERGE")?;
        let patterns = self.pattern_list()?;
        let mut on_create = Vec::new();
        let mut on_match = Vec::new();
        // Zero-or-more merge actions ACCUMULATE in source order (C++ grammar
        // allows repeats and the binder applies all — audit W8; the old
        // assignment silently dropped every clause but the last).
        while self.eat_kw("ON") {
            if self.eat_kw("CREATE") {
                self.expect_kw("SET")?;
                on_create.extend(self.set_items()?);
            } else {
                self.expect_kw("MATCH")?;
                self.expect_kw("SET")?;
                on_match.extend(self.set_items()?);
            }
        }
        Ok(MergeClause {
            patterns,
            on_create,
            on_match,
        })
    }

    /// One or more `SET` assignments: `var.prop = expr` or `var = expr`. The
    /// Neo4j-ism `+=` is a parse error, matching C++ (which has no `+=`; its
    /// `SET n = {…}` already preserves unlisted properties — audit §3.3, ledger
    /// "set-plus-equals").
    fn set_items(&mut self) -> Result<Vec<SetItem>> {
        let mut items = Vec::new();
        loop {
            let var = self.ident()?;
            let target = if self.eat(&Tok::Dot) {
                let name = self.ident()?;
                SetTarget::Property { var, name }
            } else {
                SetTarget::Var(var)
            };
            if self.peek() != &Tok::Eq {
                // A bare `SET a.name` (no value) is the ANTLR invalid-input
                // error at whatever token follows, windowed from the start.
                return Err(self.invalid_input("oC_SingleQuery", 0));
            }
            self.expect(&Tok::Eq)?;
            let value = self.parse_expr()?;
            items.push(SetItem { target, value });
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(items)
    }

    /// `[DETACH] DELETE expr (, expr)*`.
    fn delete_clause(&mut self) -> Result<DeleteClause> {
        let detach = self.eat_kw("DETACH");
        self.expect_kw("DELETE")?;
        let mut exprs = vec![self.parse_expr()?];
        while self.eat(&Tok::Comma) {
            exprs.push(self.parse_expr()?);
        }
        Ok(DeleteClause { exprs, detach })
    }

    fn pattern_list(&mut self) -> Result<Vec<PatternElement>> {
        let mut elems = vec![self.pattern_element()?];
        // Continue on `, (…)` or a named `, p = (…)`.
        while self.peek() == &Tok::Comma
            && (self.peek_at(1) == &Tok::LParen
                || (matches!(self.peek_at(1), Tok::Ident(_)) && self.peek_at(2) == &Tok::Eq))
        {
            self.advance();
            elems.push(self.pattern_element()?);
        }
        Ok(elems)
    }

    fn pattern_element(&mut self) -> Result<PatternElement> {
        // A named path: `p = (…)-[…]->(…)`.
        let name = if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Eq {
            let n = self.ident()?;
            self.expect(&Tok::Eq)?;
            Some(n)
        } else {
            None
        };
        let head = self.node_pattern()?;
        let mut chains = Vec::new();
        while matches!(self.peek(), Tok::Lt | Tok::Minus) {
            let rel = self.rel_pattern()?;
            let node = self.node_pattern()?;
            chains.push((rel, node));
        }
        Ok(PatternElement { name, head, chains })
    }

    fn node_pattern(&mut self) -> Result<NodePattern> {
        self.expect(&Tok::LParen)?;
        let var = if matches!(self.peek(), Tok::Ident(_)) {
            Some(self.ident()?)
        } else {
            None
        };
        let labels = self.label_list()?;
        // A parameter where the property map would sit is the ANTLR invalid-
        // input error, windowed from the statement start through the `$`.
        if self.peek() == &Tok::Dollar {
            return Err(self.invalid_input("oC_SingleQuery", 0));
        }
        let properties = if self.peek() == &Tok::LBrace {
            self.property_map()?
        } else {
            Vec::new()
        };
        self.expect(&Tok::RParen)?;
        Ok(NodePattern {
            var,
            labels,
            properties,
        })
    }

    /// Parse a label set introduced by `:` with labels separated by any run of
    /// `:` / `|` (Kùzu accepts `:A:B`, `:A|B`, and mixed `:A|:B`, all denoting a
    /// multi-label / polymorphic node or relationship). Empty when no leading `:`.
    fn label_list(&mut self) -> Result<Vec<String>> {
        let mut labels = Vec::new();
        if self.eat(&Tok::Colon) {
            labels.push(self.ident()?);
            loop {
                let mut sep = false;
                while self.eat(&Tok::Colon) || self.eat(&Tok::Pipe) {
                    sep = true;
                }
                if !sep || !matches!(self.peek(), Tok::Ident(_)) {
                    break;
                }
                labels.push(self.ident()?);
            }
        }
        Ok(labels)
    }

    fn rel_pattern(&mut self) -> Result<RelPattern> {
        let has_left = self.eat(&Tok::Lt);
        self.expect(&Tok::Minus)?;
        let (var, labels, recursive, properties) = if self.eat(&Tok::LBracket) {
            let var = if matches!(self.peek(), Tok::Ident(_)) {
                Some(self.ident()?)
            } else {
                None
            };
            let labels = self.label_list()?;
            // A parameter or a bare `..` range here is the ANTLR invalid-input
            // error, windowed from the statement start through the bad token.
            if matches!(self.peek(), Tok::Dollar) {
                return Err(self.invalid_input("oC_SingleQuery", 0));
            }
            if matches!(self.peek(), Tok::DotDot) {
                return Err(self.invalid_input("oC_SingleQuery", 0));
            }
            // `*[mode|semantic] [lo][..[hi]]` variable-length quantifier.
            let mut recursive = if self.peek() == &Tok::Star {
                Some(self.recursive_quantifier()?)
            } else {
                None
            };
            let properties = if self.peek() == &Tok::LBrace {
                self.property_map()?
            } else {
                Vec::new()
            };
            // A recursive rel may carry a per-step lambda `(r, n | …)`.
            if let Some(info) = &mut recursive {
                if self.peek() == &Tok::LParen {
                    info.lambda = Some(self.recursive_lambda()?);
                }
            }
            // An unexpected token where the rel bracket should close is the
            // ANTLR invalid-input error (`[:LIKES*-2]` → `Invalid input
            // <…LIKES*->: expected rule oC_SingleQuery`), not a bare
            // "expected RBracket" — C++ never emits the latter.
            if self.peek() != &Tok::RBracket {
                return Err(self.invalid_input("oC_SingleQuery", 0));
            }
            self.advance();
            (var, labels, recursive, properties)
        } else {
            (None, Vec::new(), None, Vec::new())
        };
        self.expect(&Tok::Minus)?;
        let has_right = self.eat(&Tok::Gt);
        let direction = match (has_left, has_right) {
            (true, false) => Direction::Left,
            (false, true) => Direction::Right,
            _ => Direction::Both,
        };
        Ok(RelPattern {
            var,
            labels,
            direction,
            properties,
            recursive,
        })
    }

    /// Parse a `*` variable-length quantifier: an optional mode/semantic keyword
    /// (`SHORTEST` / `ALL SHORTEST` / `TRAIL` / `ACYCLIC` / `WALK`) then optional
    /// `lo`, `..`, `hi` bounds. `*` ⇒ `(None, None)`; `*N` ⇒ exact `(N, N)`.
    fn recursive_quantifier(&mut self) -> Result<RecursiveInfo> {
        self.expect(&Tok::Star)?;
        let mut mode = RecursiveMode::All;
        let mut semantic = PathSemantic::Walk;
        let mut weight_col = None;
        if self.eat_kw("ALL") {
            // `ALL SHORTEST` or `ALL WSHORTEST(weightCol)`.
            if self.eat_kw("WSHORTEST") {
                mode = RecursiveMode::AllWShortest;
                weight_col = Some(self.wshortest_weight_col()?);
            } else {
                self.expect_kw("SHORTEST")?;
                mode = RecursiveMode::AllShortest;
            }
        } else if self.eat_kw("WSHORTEST") {
            mode = RecursiveMode::WShortest;
            weight_col = Some(self.wshortest_weight_col()?);
        } else if self.eat_kw("SHORTEST") {
            mode = RecursiveMode::Shortest;
        } else if self.eat_kw("TRAIL") {
            semantic = PathSemantic::Trail;
        } else if self.eat_kw("ACYCLIC") {
            semantic = PathSemantic::Acyclic;
        } else if self.eat_kw("WALK") {
            semantic = PathSemantic::Walk;
        }
        let lo = self.opt_bound();
        let (lower, upper) = if self.eat(&Tok::DotDot) {
            (lo, self.opt_bound())
        } else {
            (lo, lo) // bare `*` ⇒ (None, None); `*N` ⇒ exact (N, N)
        };
        Ok(RecursiveInfo {
            bounds: (lower, upper),
            mode,
            semantic,
            lambda: None,
            weight_col,
        })
    }

    /// The `(weightCol)` parenthesized rel-property name of a WSHORTEST spec.
    fn wshortest_weight_col(&mut self) -> Result<String> {
        self.expect(&Tok::LParen)?;
        let col = self.ident()?;
        self.expect(&Tok::RParen)?;
        Ok(col)
    }

    /// Read an optional non-negative integer bound (a length-quantifier endpoint).
    fn opt_bound(&mut self) -> Option<u32> {
        if let Tok::Int(v) = *self.peek() {
            if (0..=u32::MAX as i128).contains(&v) {
                self.advance();
                return Some(v as u32);
            }
        }
        None
    }

    /// Parse a recursive-rel lambda `(relVar, nodeVar | [WHERE pred]
    /// [| {relProj,…}, {nodeProj,…}])`.
    fn recursive_lambda(&mut self) -> Result<RecursiveLambda> {
        self.expect(&Tok::LParen)?;
        let rel_var = self.ident()?;
        self.expect(&Tok::Comma)?;
        let node_var = self.ident()?;
        self.expect(&Tok::Pipe)?;
        let mut predicate = None;
        if self.eat_kw("WHERE") {
            let saved = self.allow_pipe;
            self.allow_pipe = false;
            let pred = self.parse_expr();
            self.allow_pipe = saved;
            predicate = Some(pred?);
        }
        // Optional projection pair `{relProj}, {nodeProj}`. It follows a *second*
        // `|` when a `WHERE` preceded it (`… | WHERE p | {…}, {…}`), or directly
        // (no extra pipe) when there was no `WHERE` (`… | {…}, {…}`).
        let (mut rel_projection, mut node_projection) = (None, None);
        let has_projection = if predicate.is_some() {
            self.eat(&Tok::Pipe)
        } else {
            self.peek() == &Tok::LBrace
        };
        if has_projection {
            rel_projection = Some(self.brace_expr_list()?);
            self.expect(&Tok::Comma)?;
            node_projection = Some(self.brace_expr_list()?);
        }
        self.expect(&Tok::RParen)?;
        Ok(RecursiveLambda {
            rel_var,
            node_var,
            predicate,
            rel_projection,
            node_projection,
        })
    }

    /// Parse `{ expr, expr, … }` (possibly empty) as a list of expressions —
    /// the projection list inside a recursive-rel lambda.
    fn brace_expr_list(&mut self) -> Result<Vec<Expr>> {
        self.expect(&Tok::LBrace)?;
        let mut exprs = Vec::new();
        if self.peek() != &Tok::RBrace {
            loop {
                exprs.push(self.parse_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RBrace)?;
        Ok(exprs)
    }

    fn property_map(&mut self) -> Result<Vec<(String, Expr)>> {
        self.expect(&Tok::LBrace)?;
        let mut props = Vec::new();
        if self.peek() != &Tok::RBrace {
            loop {
                let key = self.ident()?;
                self.expect(&Tok::Colon)?;
                let val = self.parse_expr()?;
                props.push((key, val));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RBrace)?;
        Ok(props)
    }

    fn return_clause(&mut self) -> Result<ReturnClause> {
        self.last_return_tok = Some(self.pos);
        self.expect_kw("RETURN")?;
        self.projection_tail()
    }

    /// `WITH <projection> [WHERE pred]` — shares the projection body with RETURN,
    /// plus an optional trailing post-projection filter.
    fn with_clause(&mut self) -> Result<WithClause> {
        self.expect_kw("WITH")?;
        let projection = self.projection_tail()?;
        let where_clause = if self.eat_kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(WithClause {
            projection,
            where_clause,
        })
    }

    /// The body shared by `RETURN` and `WITH` (the leading keyword is already
    /// consumed): `[DISTINCT] items [ORDER BY …] [SKIP n] [LIMIT n]`.
    /// Whether the tokens from here form `Ident (Dot Ident)+ Dot Star` — a
    /// struct-field spread over a property chain (`a.state.*`), as opposed to
    /// the bare `a.*` handled above.
    fn dot_star_chain_ahead(&self) -> bool {
        if !matches!(self.peek(), Tok::Ident(_)) {
            return false;
        }
        // Consume `Ident`, then one or more `Dot Ident`, and require a final
        // `Dot Star`. At least one property (`Dot Ident`) must precede the star.
        let mut i = 1usize;
        let mut props = 0usize;
        while self.peek_at(i) == &Tok::Dot && matches!(self.peek_at(i + 1), Tok::Ident(_)) {
            i += 2;
            props += 1;
        }
        props >= 1 && self.peek_at(i) == &Tok::Dot && self.peek_at(i + 1) == &Tok::Star
    }

    fn projection_tail(&mut self) -> Result<ReturnClause> {
        let distinct = self.eat_kw("DISTINCT");
        let mut items = Vec::new();
        loop {
            self.proj_item_start = Some(self.pos);
            if self.peek() == &Tok::Star {
                self.advance();
                items.push(ProjectionItem::Star);
            } else if matches!(self.peek(), Tok::Ident(_))
                && self.peek_at(1) == &Tok::Dot
                && self.peek_at(2) == &Tok::Star
            {
                // `a.*` — all properties of variable `a`. It is a projection
                // item, not an expression atom: anything continuing an
                // expression after it is the C++ binder error.
                let var = self.ident()?;
                self.advance(); // `.`
                self.advance(); // `*`
                if !matches!(
                    self.peek(),
                    Tok::Comma
                        | Tok::Semicolon
                        | Tok::Eof
                        | Tok::Ident(_)
                        | Tok::RBrace
                        // Closing a parenthesized subquery: COPY t FROM (… RETURN a.*)
                        | Tok::RParen
                ) {
                    return Err(Error::binder(format!(
                        "Cannot bind {var}.* as a single property expression."
                    )));
                }
                items.push(ProjectionItem::AllProperties(var));
            } else if self.dot_star_chain_ahead() {
                // `a.b.c.*` — spread the FIELDS of the struct expression `a.b.c`.
                let mut expr = Expr::Variable(self.ident()?);
                loop {
                    self.advance(); // `.`
                    // The token before the terminating `*` is the last field.
                    if self.peek() == &Tok::Star {
                        self.advance();
                        break;
                    }
                    let name = self.ident()?;
                    expr = Expr::Property {
                        base: Box::new(expr),
                        name,
                    };
                }
                items.push(ProjectionItem::AllStructFields(expr));
            } else {
                let expr = self.parse_expr()?;
                let alias = if self.eat_kw("AS") {
                    Some(self.ident()?)
                } else {
                    None
                };
                items.push(ProjectionItem::Expr { expr, alias });
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                let e = self.parse_expr()?;
                let asc = if self.eat_kw("DESC") {
                    false
                } else {
                    self.eat_kw("ASC");
                    true
                };
                order_by.push((e, asc));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let skip = if self.eat_kw("SKIP") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let limit = if self.eat_kw("LIMIT") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(ReturnClause {
            distinct,
            items,
            order_by,
            skip,
            limit,
        })
    }

    // ---- expressions (precedence climbing) ----

    fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr> {
        let first = self.parse_xor()?;
        if !self.at_kw("OR") {
            return Ok(first);
        }
        let mut terms = vec![first];
        while self.eat_kw("OR") {
            terms.push(self.parse_xor()?);
        }
        Ok(Expr::Or(terms))
    }

    fn parse_xor(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while self.eat_kw("XOR") {
            let rhs = self.parse_and()?;
            lhs = Expr::Xor(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr> {
        let first = self.parse_not()?;
        if !self.at_kw("AND") {
            return Ok(first);
        }
        let mut terms = vec![first];
        while self.eat_kw("AND") {
            terms.push(self.parse_not()?);
        }
        Ok(Expr::And(terms))
    }

    fn parse_not(&mut self) -> Result<Expr> {
        if self.eat_kw("NOT") {
            Ok(Expr::Not(Box::new(self.parse_not()?)))
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr> {
        let cmp_start = self.pos;
        let lhs = self.parse_bitwise_or()?;
        let op = match self.peek() {
            Tok::Eq => CmpOp::Eq,
            Tok::Neq => CmpOp::Ne,
            Tok::Lt => CmpOp::Lt,
            Tok::Le => CmpOp::Le,
            Tok::Gt => CmpOp::Gt,
            Tok::Ge => CmpOp::Ge,
            _ => return Ok(lhs),
        };
        self.advance();
        let rhs = self.parse_bitwise_or()?;
        // A third comparison operator (`a = b = c`) is rejected like C++, with
        // a single caret at the start of the whole comparison expression.
        if matches!(
            self.peek(),
            Tok::Eq | Tok::Neq | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge
        ) {
            let (start, _) = self.spans.get(cmp_start).copied().unwrap_or((0, 0));
            return Err(crate::lexer::decorated_error(
                &self.src,
                "Non-binary comparison (e.g. a=b=c) is not supported",
                start,
                start + 1,
            ));
        }
        Ok(Expr::Comparison {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        })
    }

    /// `|` / `&` / `<<` `>>` — the bitwise tiers between comparison and
    /// addition (`1 = 2 & 1` compares `1` with `2 & 1`). Each desugars to its
    /// catalog function.
    fn parse_bitwise_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_bitwise_and()?;
        while self.allow_pipe && self.eat(&Tok::Pipe) {
            let rhs = self.parse_bitwise_and()?;
            lhs = fn_call("bitwise_or", vec![lhs, rhs]);
        }
        Ok(lhs)
    }

    fn parse_bitwise_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_bit_shift()?;
        while self.eat(&Tok::Amp) {
            let rhs = self.parse_bit_shift()?;
            lhs = fn_call("bitwise_and", vec![lhs, rhs]);
        }
        Ok(lhs)
    }

    fn parse_bit_shift(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_additive()?;
        loop {
            let name = match self.peek() {
                Tok::ShiftL => "bitshift_left",
                Tok::ShiftR => "bitshift_right",
                _ => break,
            };
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = fn_call(name, vec![lhs, rhs]);
        }
        Ok(lhs)
    }

    fn parse_additive(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => ArithOp::Add,
                Tok::Minus => ArithOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Arithmetic {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_multiplicative(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_power()?;
        loop {
            let op = match self.peek() {
                Tok::Star => ArithOp::Mul,
                Tok::Slash => ArithOp::Div,
                Tok::Percent => ArithOp::Mod,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_power()?;
            lhs = Expr::Arithmetic {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `^` — the power operator: binds tighter than `* / %`, left-associative
    /// (`4 ^ 6 ^ 3` = `(4^6)^3`, per the tck Precedence2 values), and desugars
    /// to the POWER function (DOUBLE result).
    fn parse_power(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_string_list_null()?;
        while self.eat(&Tok::Caret) {
            let rhs = self.parse_string_list_null()?;
            lhs = fn_call("pow", vec![lhs, rhs]);
        }
        Ok(lhs)
    }

    /// The string/list/null operator tier, between `^` and unary minus.
    /// Exactly one alternative may follow the operand (the grammar allows
    /// `IN` to chain, but not e.g. `=~ … IN …`): a string operator
    /// (`STARTS WITH` / `ENDS WITH` / `CONTAINS` / `=~`), `IN`, or
    /// `IS [NOT] NULL`. Postfix `!` (factorial) wraps the operand first,
    /// after any unary minuses (`-3 !` is `factorial(-3)`).
    fn parse_string_list_null(&mut self) -> Result<Expr> {
        let mut e = self.parse_unary()?;
        if self.eat(&Tok::Bang) {
            e = fn_call("factorial", vec![e]);
        }
        if self.at_kw("STARTS") {
            self.advance();
            self.expect_kw("WITH")?;
            let rhs = self.parse_unary()?;
            return Ok(fn_call("starts_with", vec![e, rhs]));
        }
        if self.at_kw("ENDS") {
            self.advance();
            self.expect_kw("WITH")?;
            let rhs = self.parse_unary()?;
            return Ok(fn_call("ends_with", vec![e, rhs]));
        }
        if self.eat_kw("CONTAINS") {
            let rhs = self.parse_unary()?;
            return Ok(fn_call("contains", vec![e, rhs]));
        }
        if self.eat(&Tok::RegexMatch) {
            let rhs = self.parse_unary()?;
            return Ok(fn_call("regexp_full_match", vec![e, rhs]));
        }
        if self.at_kw("IN") {
            while self.eat_kw("IN") {
                let rhs = self.parse_unary()?;
                e = fn_call("list_contains", vec![rhs, e]);
            }
            return Ok(e);
        }
        if !self.eat_kw("IS") {
            return Ok(e);
        }
        if self.eat_kw("NOT") {
            self.expect_kw("NULL")?;
            Ok(Expr::IsNotNull(Box::new(e)))
        } else {
            self.expect_kw("NULL")?;
            Ok(Expr::IsNull(Box::new(e)))
        }
    }

    fn parse_unary(&mut self) -> Result<Expr> {
        if self.eat(&Tok::Minus) {
            let inner = self.parse_unary()?;
            // A negated beyond-u128 literal keeps its raw text, minus included
            // (the binder's conversion error quotes it and names INT128).
            if let Expr::OverflowInt(text) = &inner {
                return Ok(Expr::OverflowInt(format!("-{text}")));
            }
            // Fold `-<2^127>` into the INT128 minimum. The magnitude `2^127` overflows
            // `i128`, so it is lexed as a `UINT128` literal, but its negation is the valid
            // `INT128` minimum — a `UINT128` value otherwise cannot be negated. Matches
            // C++ recognizing the negative literal; every other literal negates normally.
            if let Expr::Literal(Value::UInt128(u)) = &inner {
                if *u == i128::MIN.unsigned_abs() {
                    return Ok(Expr::Literal(Value::IntX {
                        value: i128::MIN,
                        kind: IntKind::I128,
                    }));
                }
                // Any other negated UINT128-magnitude literal is out of range
                // for every signed width: the binder's conversion error names
                // UINT128 with the full negative text.
                return Ok(Expr::OverflowInt(format!("-{u}")));
            }
            // Fold a negated wide literal back to INT64 when it fits (audit V7):
            // `9223372036854775808` lexes as INT128 (it overflows i64), but its
            // negation is exactly INT64_MIN — C++ types `-9223372036854775808` as
            // INT64, so downstream arithmetic overflow-checks in INT64 range.
            if let Expr::Literal(Value::IntX {
                value,
                kind: IntKind::I128,
            }) = &inner
            {
                let neg = value.checked_neg();
                if let Some(n) = neg.and_then(|n| i64::try_from(n).ok()) {
                    return Ok(Expr::Literal(Value::Int64(n)));
                }
                if let Some(n) = neg {
                    return Ok(Expr::Literal(Value::IntX {
                        value: n,
                        kind: IntKind::I128,
                    }));
                }
            }
            Ok(Expr::Negate(Box::new(inner)))
        } else if self.eat(&Tok::Plus) {
            self.parse_unary()
        } else {
            self.parse_postfix()
        }
    }

    /// Apply postfix property lookups (`.name`), `[index]` (→ `list_extract`),
    /// and `[from..to]` / `[from:to]` (→ `list_slice`) operators to an atom.
    /// `from`/`to` may be omitted.
    fn parse_postfix(&mut self) -> Result<Expr> {
        let mut e = self.parse_atom()?;
        let mut indexed = false;
        loop {
            if self.eat(&Tok::LBracket) {
                indexed = true;
                let is_sep = |p: &Self| matches!(p.peek(), Tok::DotDot | Tok::Colon);
                let from = if is_sep(self) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                if is_sep(self) {
                    self.advance(); // the `..` / `:` slice separator
                    let to = if self.peek() == &Tok::RBracket {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    self.expect(&Tok::RBracket)?;
                    e = Expr::Function {
                        name: "list_slice".to_string(),
                        distinct: false,
                        args: vec![
                            e,
                            from.unwrap_or(Expr::Literal(Value::Int64(1))),
                            to.unwrap_or(Expr::Literal(Value::Int64(i64::MAX))),
                        ],
                        arg_names: Vec::new(),
                    };
                } else {
                    self.expect(&Tok::RBracket)?;
                    e = Expr::Function {
                        name: "list_extract".to_string(),
                        distinct: false,
                        args: vec![e, from.ok_or_else(|| Error::parser("empty list index"))?],
                        arg_names: Vec::new(),
                    };
                }
            } else if self.peek() == &Tok::Dot {
                // C++ property lookups attach to ATOMS only — indexing lives a
                // tier above, so `.` after `[…]` is the ANTLR mismatched-input
                // error (`RETURN [{a:1}][1].a` → caret on the dot).
                if indexed {
                    let (cs, ce) = self.spans.get(self.pos).copied().unwrap_or((0, 0));
                    return Err(crate::lexer::decorated_error(
                        &self.src,
                        "mismatched input '.' expecting {<EOF>, ';', SP}",
                        cs,
                        ce,
                    ));
                }
                self.advance();
                if !matches!(self.peek(), Tok::Ident(_)) {
                    // `5.` / `x.` with no property name: the ANTLR error
                    // windows from the input start through the token AFTER
                    // the dot, with the caret on that token.
                    let (cs, ce) = self
                        .spans
                        .get(self.pos)
                        .copied()
                        .unwrap_or((self.src.len(), self.src.len()));
                    let window = &self.src[..ce];
                    return Err(crate::lexer::decorated_error(
                        &self.src,
                        &format!("Invalid input <{window}>: expected rule oC_RegularQuery"),
                        cs,
                        ce,
                    ));
                }
                let name = self.ident()?;
                e = Expr::Property {
                    base: Box::new(e),
                    name,
                };
            } else {
                break;
            }
        }
        Ok(e)
    }

    fn parse_atom(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                // A literal that fits INT64 is INT64; a wider one is INT128.
                let v = match i64::try_from(n) {
                    Ok(n64) => Value::Int64(n64),
                    Err(_) => Value::IntX {
                        value: n,
                        kind: IntKind::I128,
                    },
                };
                Ok(Expr::Literal(v))
            }
            Tok::UInt(u) => {
                self.advance();
                // Beyond i128 → a UINT128 literal.
                Ok(Expr::Literal(Value::UInt128(u)))
            }
            Tok::OverflowInt(text) => {
                self.advance();
                Ok(Expr::OverflowInt(text))
            }
            Tok::Float(x) => {
                self.advance();
                Ok(Expr::Literal(Value::Double(x)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::String(s)))
            }
            Tok::Star => {
                self.advance();
                Ok(Expr::Star)
            }
            Tok::Dollar => {
                self.advance();
                Ok(Expr::Parameter(self.ident()?))
            }
            Tok::LBracket => {
                self.advance();
                // A pattern comprehension `[(a)-[:R]->(b) | expr]` (C++ parses
                // these; a WHERE inside is the ANTLR oC_ProjectionItem error).
                if self.peek() == &Tok::LParen {
                    let save = self.pos;
                    if let Ok(pattern) = self.pattern_element() {
                        if self.at_kw("WHERE") {
                            let ws = self.spans.get(self.pos).copied().unwrap_or((0, 0));
                            let item_start = self
                                .proj_item_start
                                .and_then(|i| self.spans.get(i))
                                .map(|&(s, _)| s)
                                .unwrap_or(0);
                            let window = &self.src[item_start..ws.1];
                            return Err(crate::lexer::decorated_error(
                                &self.src,
                                &format!(
                                    "Invalid input <{window}>: expected rule oC_ProjectionItem"
                                ),
                                ws.0,
                                ws.1,
                            ));
                        }
                        let projection = if self.eat(&Tok::Pipe) {
                            Some(Box::new(self.parse_expr()?))
                        } else {
                            None
                        };
                        if self.eat(&Tok::RBracket) {
                            return Ok(Expr::PatternComprehension {
                                pattern,
                                projection,
                            });
                        }
                    }
                    self.pos = save;
                }
                // No list-comprehension production: like C++, `[x IN l | p]`
                // parses as a list literal over the IN/`|` operators, so the
                // binder rejects the unbound variable (`x is not in scope`).
                let mut items = Vec::new();
                if self.peek() != &Tok::RBracket {
                    loop {
                        // An empty slot (before a comma or the closing bracket,
                        // incl. a trailing comma) is a NULL hole: `[1,,2]`,`[x,]`.
                        if matches!(self.peek(), Tok::Comma | Tok::RBracket) {
                            items.push(Expr::Literal(Value::Null));
                        } else {
                            items.push(self.parse_expr()?);
                        }
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                // The retired comprehension's WHERE form (`[x IN l WHERE p]`)
                // is the ANTLR invalid-input error, windowed from the input
                // start with the carets on WHERE.
                if self.at_kw("WHERE") {
                    let (ws, we) = self.spans.get(self.pos).copied().unwrap_or((0, 0));
                    let window = &self.src[..we];
                    return Err(crate::lexer::decorated_error(
                        &self.src,
                        &format!("Invalid input <{window}>: expected rule oC_RegularQuery"),
                        ws,
                        we,
                    ));
                }
                self.expect(&Tok::RBracket)?;
                Ok(Expr::List(items))
            }
            Tok::LBrace => {
                // Struct literal `{field: expr, …}`. Field keys are bare idents
                // or string literals. An EMPTY `{}` in expression position is
                // the C++ ANTLR invalid-input error (window from the statement
                // start through the `}`); property maps parse elsewhere and DO
                // allow `{}`.
                self.advance();
                if self.peek() == &Tok::RBrace {
                    let (cs, ce) = self
                        .spans
                        .get(self.pos)
                        .copied()
                        .unwrap_or((self.src.len(), self.src.len()));
                    let window = &self.src[..ce];
                    return Err(crate::lexer::decorated_error(
                        &self.src,
                        &format!("Invalid input <{window}>: expected rule oC_RegularQuery"),
                        cs,
                        ce,
                    ));
                }
                let mut fields = Vec::new();
                if self.peek() != &Tok::RBrace {
                    loop {
                        let key = match self.peek().clone() {
                            Tok::Ident(k) => {
                                self.advance();
                                k
                            }
                            Tok::Str(k) => {
                                self.advance();
                                k
                            }
                            other => {
                                return Err(Error::parser(format!(
                                    "expected a struct field name, found {other:?}"
                                )));
                            }
                        };
                        if self.peek() != &Tok::Colon {
                            // `{name, fName}` — a field without a value is the
                            // ANTLR invalid-input error at the offending token.
                            let rule = if self.in_query {
                                "oC_SingleQuery"
                            } else {
                                "oC_RegularQuery"
                            };
                            return Err(self.invalid_input(rule, 0));
                        }
                        self.expect(&Tok::Colon)?;
                        fields.push((key, self.parse_expr()?));
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBrace)?;
                Ok(Expr::Struct(fields))
            }
            Tok::LParen => {
                // A path pattern `(a)-[:R]->(b)` is a valid C++ atom (used as
                // a WHERE predicate); only treated as one when a rel chain
                // follows, so plain parenthesized expressions stay expressions.
                let save = self.pos;
                if let Ok(pattern) = self.pattern_element() {
                    if !pattern.chains.is_empty() {
                        return Ok(Expr::PatternComprehension {
                            pattern,
                            projection: None,
                        });
                    }
                }
                self.pos = save;
                self.advance();
                let saved = self.allow_pipe;
                self.allow_pipe = true;
                let e = self.parse_expr();
                self.allow_pipe = saved;
                let e = e?;
                self.expect(&Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(name) => {
                self.advance();
                if name.is_empty() || name.as_bytes().contains(&0) {
                    return Err(Error::parser(EMPTY_TOKEN_NAME_ERROR));
                }
                if name.eq_ignore_ascii_case("true") {
                    Ok(Expr::Literal(Value::Bool(true)))
                } else if name.eq_ignore_ascii_case("false") {
                    Ok(Expr::Literal(Value::Bool(false)))
                } else if name.eq_ignore_ascii_case("null") {
                    Ok(Expr::Literal(Value::Null))
                } else if name.eq_ignore_ascii_case("case") {
                    self.parse_case()
                } else if (name.eq_ignore_ascii_case("exists")
                    || name.eq_ignore_ascii_case("count"))
                    && self.peek() == &Tok::LBrace
                {
                    self.subquery(&name)
                } else if self.peek() == &Tok::LParen {
                    self.function_call(name)
                } else {
                    Ok(Expr::Variable(name))
                }
            }
            other => Err(Error::parser(format!(
                "unexpected token {other:?} in expression"
            ))),
        }
    }

    /// Parse `EXISTS { MATCH … [WHERE …] }` / `COUNT { … }` (the keyword is
    /// consumed; the next token is `{`).
    fn subquery(&mut self, kw: &str) -> Result<Expr> {
        let kind = if kw.eq_ignore_ascii_case("exists") {
            SubqueryKind::Exists
        } else {
            SubqueryKind::Count
        };
        self.expect(&Tok::LBrace)?;
        self.expect_kw("MATCH")?;
        let patterns = self.pattern_list()?;
        let where_clause = if self.eat_kw("WHERE") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect(&Tok::RBrace)?;
        Ok(Expr::Subquery {
            kind,
            patterns,
            where_clause,
        })
    }

    /// Parse a `CASE … END` expression (the leading `CASE` is already consumed).
    fn parse_case(&mut self) -> Result<Expr> {
        // A simple CASE has an operand before the first WHEN.
        let operand = if self.at_kw("WHEN") {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut when_thens = Vec::new();
        while self.eat_kw("WHEN") {
            let cond = self.parse_expr()?;
            self.expect_kw("THEN")?;
            let res = self.parse_expr()?;
            when_thens.push((cond, res));
        }
        if when_thens.is_empty() {
            return Err(Error::parser("CASE requires at least one WHEN branch"));
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_kw("END")?;
        Ok(Expr::Case {
            operand,
            when_thens,
            else_,
        })
    }

    fn function_call(&mut self, name: String) -> Result<Expr> {
        self.expect(&Tok::LParen)?;
        // List-predicate quantifiers `ANY|ALL|NONE|SINGLE(x IN list WHERE p)`:
        // desugar to the lambda list function; a missing WHERE is the ANTLR
        // invalid-input error at the closing paren (C++ grammar requires it).
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "any" | "all" | "none" | "single"
        ) && matches!(self.peek(), Tok::Ident(_))
            && self.at_kw_ahead(1, "IN")
        {
            let var = self.ident()?;
            self.expect_kw("IN")?;
            let list = self.parse_expr()?;
            if !self.eat_kw("WHERE") {
                return Err(self.invalid_input("oC_RegularQuery", 0));
            }
            let pred = self.parse_expr()?;
            self.expect(&Tok::RParen)?;
            return Ok(Expr::Function {
                name,
                distinct: false,
                args: vec![
                    list,
                    Expr::Lambda {
                        params: vec![var],
                        body: Box::new(pred),
                    },
                ],
                arg_names: vec![None, None],
            });
        }
        let distinct = self.eat_kw("DISTINCT");
        let mut args = Vec::new();
        let mut arg_names: Vec<Option<String>> = Vec::new();
        if self.peek() == &Tok::Star {
            self.advance();
            args.push(Expr::Star);
            arg_names.push(None);
        } else if self.peek() != &Tok::RParen {
            loop {
                let (nm, arg) = self.parse_call_arg()?;
                args.push(arg);
                arg_names.push(nm);
                // `CAST(expr AS TYPE)` → normalize the type to a string-literal arg
                // so it shares the binder path with `CAST(expr, "TYPE")`.
                if self.eat_kw("AS") {
                    let ty = self.parse_type_name()?;
                    args.push(Expr::Literal(Value::String(ty)));
                    arg_names.push(None);
                    break;
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(Expr::Function {
            name,
            distinct,
            args,
            arg_names,
        })
    }

    /// A function argument. Returns the optional argument *name* (the `name` of a
    /// `name := expr` named argument, e.g. `union_value(a := 1)`) and the argument
    /// expression, which may itself be a lambda `x -> body` / `(x,y) -> body`.
    fn parse_call_arg(&mut self) -> Result<(Option<String>, Expr)> {
        // Named argument `name := expr`. The `:=` lexes as `Colon` then `Eq` (as in
        // a `CREATE MACRO` default). Only a bare identifier may be named; disambiguate
        // from a lambda (`x -> …`, lexed `Minus Gt`) and a plain expression by looking
        // at the two tokens after the identifier.
        if let Tok::Ident(nm) = self.peek() {
            if self.peek_at(1) == &Tok::Colon && self.peek_at(2) == &Tok::Eq {
                let nm = nm.clone();
                self.advance(); // ident
                self.advance(); // Colon
                self.advance(); // Eq
                return Ok((Some(nm), self.parse_expr()?));
            }
        }
        if let Some(params) = self.try_lambda_params() {
            let body = Box::new(self.parse_expr()?);
            return Ok((None, Expr::Lambda { params, body }));
        }
        Ok((None, self.parse_expr()?))
    }

    /// If the upcoming tokens are a lambda parameter list followed by `->`,
    /// consume them and return the params; otherwise leave the cursor untouched.
    fn try_lambda_params(&mut self) -> Option<Vec<String>> {
        let start = self.pos;
        let params = if let Tok::Ident(name) = self.peek().clone() {
            self.advance();
            vec![name]
        } else if self.peek() == &Tok::LParen {
            self.advance();
            let mut ps = Vec::new();
            loop {
                match self.peek().clone() {
                    Tok::Ident(n) => {
                        self.advance();
                        ps.push(n);
                    }
                    _ => {
                        self.pos = start;
                        return None;
                    }
                }
                if self.eat(&Tok::Comma) {
                    continue;
                }
                break;
            }
            if !self.eat(&Tok::RParen) {
                self.pos = start;
                return None;
            }
            ps
        } else {
            return None;
        };
        // Require the `->` arrow (lexed as Minus, Gt).
        if self.peek() == &Tok::Minus && self.peek_at(1) == &Tok::Gt {
            self.advance();
            self.advance();
            Some(params)
        } else {
            self.pos = start;
            None
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
