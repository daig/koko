use super::*;

impl Parser {
    pub(super) fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_or()
    }

    pub(super) fn parse_or(&mut self) -> Result<Expr> {
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

    pub(super) fn parse_xor(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_and()?;
        while self.eat_kw("XOR") {
            let rhs = self.parse_and()?;
            lhs = Expr::Xor(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    pub(super) fn parse_and(&mut self) -> Result<Expr> {
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

    pub(super) fn parse_not(&mut self) -> Result<Expr> {
        if self.eat_kw("NOT") {
            Ok(Expr::Not(Box::new(self.parse_not()?)))
        } else {
            self.parse_comparison()
        }
    }

    pub(super) fn parse_comparison(&mut self) -> Result<Expr> {
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
    pub(super) fn parse_bitwise_or(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_bitwise_and()?;
        while self.allow_pipe && self.eat(&Tok::Pipe) {
            let rhs = self.parse_bitwise_and()?;
            lhs = fn_call("bitwise_or", vec![lhs, rhs]);
        }
        Ok(lhs)
    }

    pub(super) fn parse_bitwise_and(&mut self) -> Result<Expr> {
        let mut lhs = self.parse_bit_shift()?;
        while self.eat(&Tok::Amp) {
            let rhs = self.parse_bit_shift()?;
            lhs = fn_call("bitwise_and", vec![lhs, rhs]);
        }
        Ok(lhs)
    }

    pub(super) fn parse_bit_shift(&mut self) -> Result<Expr> {
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

    pub(super) fn parse_additive(&mut self) -> Result<Expr> {
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

    pub(super) fn parse_multiplicative(&mut self) -> Result<Expr> {
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
    pub(super) fn parse_power(&mut self) -> Result<Expr> {
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
    pub(super) fn parse_string_list_null(&mut self) -> Result<Expr> {
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

    pub(super) fn parse_unary(&mut self) -> Result<Expr> {
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
    pub(super) fn parse_postfix(&mut self) -> Result<Expr> {
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

    pub(super) fn parse_atom(&mut self) -> Result<Expr> {
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
                // Koko reserves `[var IN …]` for list comprehensions. Parenthesize
                // a membership expression to keep it as a list-literal element.
                if matches!(self.peek(), Tok::Ident(_)) && self.at_kw_ahead(1, "IN") {
                    return self.parse_list_comprehension();
                }
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

    /// Parse `[var IN list [WHERE predicate] [| projection]]` after the leading
    /// `[` has been consumed. A top-level `|` delimits the projection; nested
    /// expressions can still use bitwise OR by parenthesizing it.
    pub(super) fn parse_list_comprehension(&mut self) -> Result<Expr> {
        let var = self.ident()?;
        self.expect_kw("IN")?;
        let list = Box::new(self.parse_expr_before_pipe()?);
        let predicate = if self.eat_kw("WHERE") {
            Some(Box::new(self.parse_expr_before_pipe()?))
        } else {
            None
        };
        let projection = if self.eat(&Tok::Pipe) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect(&Tok::RBracket)?;
        Ok(Expr::ListComprehension {
            var,
            list,
            predicate,
            projection,
        })
    }

    /// Parse an expression while reserving a depth-zero `|` for the enclosing
    /// list-comprehension grammar, restoring parser state on success or error.
    fn parse_expr_before_pipe(&mut self) -> Result<Expr> {
        let saved = self.allow_pipe;
        self.allow_pipe = false;
        let result = self.parse_expr();
        self.allow_pipe = saved;
        result
    }

    /// Parse `EXISTS { MATCH … [WHERE …] }` / `COUNT { … }` (the keyword is
    /// consumed; the next token is `{`).
    pub(super) fn subquery(&mut self, kw: &str) -> Result<Expr> {
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
    pub(super) fn parse_case(&mut self) -> Result<Expr> {
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

    pub(super) fn function_call(&mut self, name: String) -> Result<Expr> {
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
    pub(super) fn parse_call_arg(&mut self) -> Result<(Option<String>, Expr)> {
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
    pub(super) fn try_lambda_params(&mut self) -> Option<Vec<String>> {
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
