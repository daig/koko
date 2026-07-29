use super::*;

impl Parser {
    pub(super) fn pattern_list(&mut self) -> Result<Vec<PatternElement>> {
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

    pub(super) fn pattern_element(&mut self) -> Result<PatternElement> {
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

    pub(super) fn node_pattern(&mut self) -> Result<NodePattern> {
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
    pub(super) fn label_list(&mut self) -> Result<Vec<String>> {
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

    pub(super) fn rel_pattern(&mut self) -> Result<RelPattern> {
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
    pub(super) fn recursive_quantifier(&mut self) -> Result<RecursiveInfo> {
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
    pub(super) fn wshortest_weight_col(&mut self) -> Result<String> {
        self.expect(&Tok::LParen)?;
        let col = self.ident()?;
        self.expect(&Tok::RParen)?;
        Ok(col)
    }

    /// Read an optional non-negative integer bound (a length-quantifier endpoint).
    pub(super) fn opt_bound(&mut self) -> Option<u32> {
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
    pub(super) fn recursive_lambda(&mut self) -> Result<RecursiveLambda> {
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
    pub(super) fn brace_expr_list(&mut self) -> Result<Vec<Expr>> {
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

    pub(super) fn property_map(&mut self) -> Result<Vec<(String, Expr)>> {
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
}
