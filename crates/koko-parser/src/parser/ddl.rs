use super::*;

impl Parser {
    pub(super) fn create_graph(&mut self) -> Result<CreateGraph> {
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

    pub(super) fn create_index(&mut self) -> Result<CreateIndex> {
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
    pub(super) fn transaction_statement(&mut self) -> Result<TxnOp> {
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
    pub(super) fn call_statement(&mut self) -> Result<Statement> {
        self.expect_kw("CALL")?;
        let name = self.ident()?;
        if self.eat(&Tok::Eq) {
            let value = self.parse_expr()?;
            return Ok(Statement::Call(CallStmt::SetConfig { key: name, value }));
        }
        self.expect(&Tok::LParen)
            .map_err(|_| Error::parser(format!("expected `=` or `(` after CALL {name}")))?;
        let args = self.call_args()?;
        self.expect(&Tok::RParen)?;
        let yield_items = self.parse_yield_items()?;

        // A composable form enters the ordinary reading-clause pipeline. A
        // function with no projection remains a standalone call; the binder
        // later accepts only functions explicitly declared standalone.
        let route_to_query = !yield_items.is_empty()
            || self.at_kw("WHERE")
            || self.at_kw("WITH")
            || self.at_kw("CALL")
            || self.at_kw("MATCH")
            || self.at_kw("UNWIND")
            || self.at_kw("LOAD")
            || self.at_kw("RETURN")
            || (self.at_kw("OPTIONAL") && self.at_kw_ahead(1, "MATCH"));
        if route_to_query {
            let where_clause = if self.eat_kw("WHERE") {
                Some(self.parse_expr()?)
            } else {
                None
            };
            let clause = ReadingClause::Call(CallClause {
                name,
                args,
                yield_items,
                where_clause,
            });
            let single = self.single_query_from(vec![clause])?;
            return Ok(Statement::Query(RegularQuery {
                singles: vec![single],
                union_all: Vec::new(),
            }));
        }
        Ok(Statement::Call(CallStmt::Function(CallClause {
            name,
            args,
            yield_items,
            where_clause: None,
        })))
    }

    pub(super) fn copy_to_statement(&mut self) -> Result<CopyToStatement> {
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

    pub(super) fn export_database_statement(&mut self) -> Result<ExportDatabaseStatement> {
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

    pub(super) fn import_database_statement(&mut self) -> Result<ImportDatabaseStatement> {
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

    pub(super) fn copy_statement(&mut self) -> Result<CopyStatement> {
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
        // A function source `COPY t FROM TABLE_INFO('x')` is parsed generically;
        // the binder resolves its function kind and arguments.
        if let Tok::Ident(name) = self.peek().clone() {
            if self.peek_at(1) == &Tok::LParen {
                self.advance();
                self.advance();
                let args = self.call_args()?;
                self.expect(&Tok::RParen)?;
                let options = if self.peek() == &Tok::LParen {
                    self.load_options()?
                } else {
                    Vec::new()
                };
                let clause = ReadingClause::Call(CallClause {
                    name,
                    args,
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
            return Err(Error::binder(format!("Variable {name} is not in scope.")));
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
    pub(super) fn load_from_clause(&mut self) -> Result<LoadFromClause> {
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
    pub(super) fn load_options(&mut self) -> Result<Vec<(String, LoadOptVal)>> {
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

    pub(super) fn load_option_value(&mut self) -> Result<LoadOptVal> {
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

    pub(super) fn load_option_list(&mut self) -> Result<LoadOptVal> {
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

    pub(super) fn parse_if_not_exists(&mut self) -> bool {
        if self.at_kw("IF") && self.at_kw_ahead(1, "NOT") && self.at_kw_ahead(2, "EXISTS") {
            self.advance();
            self.advance();
            self.advance();
            true
        } else {
            false
        }
    }

    pub(super) fn parse_if_exists(&mut self) -> bool {
        if self.at_kw("IF") && self.at_kw_ahead(1, "EXISTS") {
            self.advance();
            self.advance();
            true
        } else {
            false
        }
    }

    /// `DROP TABLE [IF EXISTS] <name>`.
    pub(super) fn drop_table(&mut self) -> Result<DropTable> {
        self.expect_kw("DROP")?;
        self.expect_kw("TABLE")?;
        let if_exists = self.parse_if_exists();
        let name = self.ident()?;
        Ok(DropTable { name, if_exists })
    }

    /// `DROP SEQUENCE [IF EXISTS] <name>`.
    pub(super) fn drop_sequence(&mut self) -> Result<DropSequence> {
        self.expect_kw("DROP")?;
        self.expect_kw("SEQUENCE")?;
        let if_exists = self.parse_if_exists();
        let name = self.ident()?;
        Ok(DropSequence { name, if_exists })
    }

    /// `CREATE TYPE <name> AS <type>`.
    pub(super) fn create_type(&mut self) -> Result<CreateType> {
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
    pub(super) fn create_macro(&mut self) -> Result<CreateMacro> {
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
    pub(super) fn drop_macro(&mut self) -> Result<Statement> {
        self.expect_kw("DROP")?;
        self.expect_kw("MACRO")?;
        let if_exists = self.parse_if_exists();
        let name = self.ident()?;
        Ok(Statement::DropMacro { name, if_exists })
    }

    /// `COMMENT ON TABLE <name> IS '<text>'`.
    pub(super) fn comment_statement(&mut self) -> Result<CommentStmt> {
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
    pub(super) fn parse_seq_int(&mut self) -> Result<i128> {
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
    pub(super) fn create_sequence(&mut self) -> Result<CreateSequence> {
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
    pub(super) fn alter_statement(&mut self) -> Result<AlterStatement> {
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

    pub(super) fn create_node_table(&mut self) -> Result<Statement> {
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

    pub(super) fn create_rel_table(&mut self) -> Result<Statement> {
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

    pub(super) fn column_def(&mut self) -> Result<ColumnDef> {
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
    pub(super) fn parse_type_name(&mut self) -> Result<String> {
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
    pub(super) fn reconstruct_type_group(&mut self, s: &mut String) -> Result<()> {
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

    pub(super) fn table_storage_options(
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

    pub(super) fn table_option_value(&mut self) -> Result<String> {
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
    pub(super) fn parse_yield_items(&mut self) -> Result<Vec<(String, Option<String>)>> {
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

    /// Parse a typed `CALL` argument list while already inside its parentheses.
    fn call_args(&mut self) -> Result<Vec<Expr>> {
        let mut args = Vec::new();
        if self.peek_at(0) == &Tok::RParen {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            if !self.eat(&Tok::Comma) {
                return Ok(args);
            }
        }
    }

    /// A mid-query `CALL fn(args) [YIELD …] [WHERE …]` reading clause.
    pub(super) fn call_reading_clause(&mut self) -> Result<ReadingClause> {
        self.expect_kw("CALL")?;
        let name = self.ident()?;
        self.expect(&Tok::LParen)?;
        let args = self.call_args()?;
        self.expect(&Tok::RParen)?;
        let yield_items = self.parse_yield_items()?;
        let where_clause = if self.eat_kw("WHERE") {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(ReadingClause::Call(CallClause {
            name,
            args,
            yield_items,
            where_clause,
        }))
    }
}
