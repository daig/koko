use super::*;

impl Parser {
    /// `single_query (UNION [ALL] single_query)*`.
    pub(super) fn regular_query(&mut self) -> Result<RegularQuery> {
        let mut singles = vec![self.single_query()?];
        let mut union_all = Vec::new();
        while self.eat_kw("UNION") {
            union_all.push(self.eat_kw("ALL"));
            singles.push(self.single_query()?);
        }
        Ok(RegularQuery { singles, union_all })
    }

    pub(super) fn single_query(&mut self) -> Result<SingleQuery> {
        self.single_query_from(Vec::new())
    }

    pub(super) fn single_query_from(&mut self, seeded: Vec<ReadingClause>) -> Result<SingleQuery> {
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

    pub(super) fn match_clause(&mut self) -> Result<MatchClause> {
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
    pub(super) fn parse_join_hint(&mut self) -> Result<JoinHint> {
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
    pub(super) fn parse_join_atom(&mut self) -> Result<JoinHint> {
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

    pub(super) fn unwind_clause(&mut self) -> Result<UnwindClause> {
        self.expect_kw("UNWIND")?;
        let expr = self.parse_expr()?;
        self.expect_kw("AS")?;
        let var = self.ident()?;
        Ok(UnwindClause { expr, var })
    }

    /// Parse one updating clause: `CREATE …`, `SET …`, `[DETACH] DELETE …`, or `MERGE …`.
    pub(super) fn updating_clause(&mut self) -> Result<UpdatingClause> {
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
    pub(super) fn set_clause(&mut self) -> Result<SetClause> {
        self.expect_kw("SET")?;
        Ok(SetClause {
            items: self.set_items()?,
        })
    }

    /// `MERGE <pattern> (ON CREATE SET … | ON MATCH SET …)*`.
    pub(super) fn merge_clause(&mut self) -> Result<MergeClause> {
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
    pub(super) fn set_items(&mut self) -> Result<Vec<SetItem>> {
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
    pub(super) fn delete_clause(&mut self) -> Result<DeleteClause> {
        let detach = self.eat_kw("DETACH");
        self.expect_kw("DELETE")?;
        let mut exprs = vec![self.parse_expr()?];
        while self.eat(&Tok::Comma) {
            exprs.push(self.parse_expr()?);
        }
        Ok(DeleteClause { exprs, detach })
    }

    pub(super) fn return_clause(&mut self) -> Result<ReturnClause> {
        self.last_return_tok = Some(self.pos);
        self.expect_kw("RETURN")?;
        self.projection_tail()
    }

    /// `WITH <projection> [WHERE pred]` — shares the projection body with RETURN,
    /// plus an optional trailing post-projection filter.
    pub(super) fn with_clause(&mut self) -> Result<WithClause> {
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
    pub(super) fn dot_star_chain_ahead(&self) -> bool {
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

    pub(super) fn projection_tail(&mut self) -> Result<ReturnClause> {
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
}
