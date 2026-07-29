use super::query::ProjectionOutputRef;
use super::*;

pub(super) struct PreparedParameterState {
    pub(super) types: HashMap<String, LogicalType>,
    pub(super) error: Option<Error>,
}

/// Subquery syntax captured during expression binding and pattern-bound later.
pub(super) struct PendingSubquery {
    pub(super) id: usize,
    pub(super) kind: ast::SubqueryKind,
    pub(super) patterns: Vec<ast::PatternElement>,
    pub(super) where_clause: Option<ast::Expr>,
}

#[derive(Clone)]
pub(super) struct LambdaBinding {
    pub(super) name: String,
    pub(super) id: LambdaVarId,
    pub(super) ty: LogicalType,
}

impl Binder<'_, '_> {
    pub(super) fn bind_parameter(&self, name: &str) -> BoundExpr {
        if let Some(state) = self.prepared_parameters.as_deref() {
            let logical_type = state.types.get(name).cloned().unwrap_or(LogicalType::Any);
            BoundExpr::Parameter {
                name: name.to_string(),
                ty: logical_type,
            }
        } else {
            self.params
                .get(name)
                .map(|value| BoundExpr::Literal(value.clone()))
                .unwrap_or(BoundExpr::Literal(Value::Null))
        }
    }

    fn merge_parameter_type(&mut self, name: &str, target: &LogicalType) -> LogicalType {
        if *target == LogicalType::Any {
            return self
                .prepared_parameters
                .as_deref()
                .and_then(|state| state.types.get(name).cloned())
                .unwrap_or(LogicalType::Any);
        }
        let Some(state) = self.prepared_parameters.as_deref_mut() else {
            return target.clone();
        };
        let current = state.types.get(name).cloned().unwrap_or(LogicalType::Any);
        let merged = if current == LogicalType::Any {
            Some(target.clone())
        } else if current == *target {
            Some(current.clone())
        } else if is_numeric_type(&current) && is_numeric_type(target) {
            koko_common::types::common_numeric_type([&current, target])
        } else {
            None
        };
        match merged {
            Some(merged) => {
                state.types.insert(name.to_string(), merged.clone());
                merged
            }
            None => {
                if state.error.is_none() {
                    state.error = Some(Error::binder(format!(
                        "Parameter ${name} has conflicting type constraints {current} and {target}."
                    )));
                }
                current
            }
        }
    }

    pub(super) fn constrain_parameter(&mut self, expression: &mut BoundExpr, target: &LogicalType) {
        if let BoundExpr::Parameter { name, ty } = expression {
            *ty = self.merge_parameter_type(name, target);
        }
    }

    pub(super) fn capture_parameter_constraints(&mut self, expression: &BoundExpr) {
        match expression {
            BoundExpr::Cast { expr, target } => {
                if let BoundExpr::Parameter { name, .. } = expr.as_ref() {
                    self.merge_parameter_type(name, target);
                }
                self.capture_parameter_constraints(expr);
            }
            BoundExpr::Scalar { args, .. }
            | BoundExpr::Call { args, .. }
            | BoundExpr::List { elems: args, .. } => {
                for argument in args {
                    self.capture_parameter_constraints(argument);
                }
            }
            BoundExpr::Aggregate {
                arg: Some(argument),
                ..
            } => self.capture_parameter_constraints(argument),
            BoundExpr::ValueProperty { value, .. } => {
                self.capture_parameter_constraints(value);
            }
            BoundExpr::Struct { fields, .. } => {
                for (_, value) in fields {
                    self.capture_parameter_constraints(value);
                }
            }
            BoundExpr::ListLambda { list, body, .. } => {
                self.capture_parameter_constraints(list);
                self.capture_parameter_constraints(body);
            }
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                if let Some(operand) = operand {
                    self.capture_parameter_constraints(operand);
                }
                for (condition, result) in branches {
                    self.capture_parameter_constraints(condition);
                    self.capture_parameter_constraints(result);
                }
                if let Some(otherwise) = else_ {
                    self.capture_parameter_constraints(otherwise);
                }
            }
            _ => {}
        }
    }

    pub(super) fn coerce_to(
        &mut self,
        mut expression: BoundExpr,
        target: &LogicalType,
    ) -> BoundExpr {
        self.constrain_parameter(&mut expression, target);
        super::coerce_to(expression, target)
    }
}

impl<'catalog, 'bind> Binder<'catalog, 'bind> {
    pub(super) fn allocate_lambda_id(&mut self, name: &str) -> LambdaVarId {
        let id = LambdaVarId(self.next_lambda_id);
        self.next_lambda_id += 1;
        self.lambda_names.push(name.to_string());
        id
    }

    pub(super) fn lambda_binding(&self, name: &str) -> Option<(LambdaVarId, LogicalType)> {
        self.lambda_params
            .iter()
            .rev()
            .find(|binding| binding.name == name)
            .map(|binding| (binding.id, binding.ty.clone()))
    }

    pub(super) fn lambda_name(&self, id: LambdaVarId) -> String {
        self.lambda_names
            .get(id.0 as usize)
            .cloned()
            .unwrap_or_else(|| format!("_lambda{}", id.0))
    }

    /// Resolve a `CAST` target type, consulting the user-defined-type registry first.
    pub(super) fn resolve_cast_type(&self, s: &str) -> Result<LogicalType> {
        if let Some(ty) = self.catalog.user_type(s) {
            return Ok(ty);
        }
        LogicalType::from_cast_str_with(s, &|name| self.catalog.user_type(name))
    }

    /// Take the `nextval`/`currval` calls staged during this part's expression
    /// binding (ids are their indices), to attach to the part.
    pub(super) fn drain_sequence_calls(&mut self) -> Vec<BoundSequenceCall> {
        std::mem::take(&mut self.pending_sequence_calls)
    }

    /// Bind `nextval('seq')` / `currval('seq')`. The sequence name must be a string
    /// literal (it is resolved per row, and advancing it mutates catalog state, so
    /// the call is lifted to a per-row column rather than evaluated in the pure
    /// expression engine).
    pub(super) fn bind_sequence_call(
        &mut self,
        func: SequenceFn,
        args: &[ast::Expr],
    ) -> Result<BoundExpr> {
        let fname = match func {
            SequenceFn::NextVal => "nextval",
            SequenceFn::CurrVal => "currval",
        };
        if args.len() != 1 {
            return Err(Error::binder(format!(
                "{fname} expects exactly one argument (the sequence name)"
            )));
        }
        let ast::Expr::Literal(Value::String(seq_name)) = &args[0] else {
            return Err(Error::not_implemented(format!(
                "{fname} requires a string-literal sequence name"
            )));
        };
        let id = self.pending_sequence_calls.len();
        self.pending_sequence_calls.push(BoundSequenceCall {
            func,
            name: seq_name.clone(),
        });
        Ok(BoundExpr::SequenceCall {
            id,
            ty: LogicalType::Int64,
        })
    }

    pub(super) fn bind_property_expr(&mut self, base: &ast::Expr, name: &str) -> Result<BoundExpr> {
        if let ast::Expr::Variable(var) = base {
            if let Some((id, ty)) = self.lambda_binding(var) {
                return Ok(Self::property_extract_call(
                    BoundExpr::LambdaVar { id, ty },
                    name,
                    LogicalType::Any,
                ));
            }

            let id = self.lookup_var(var)?;
            let info = &self.vars[id.0 as usize];
            // A path / variable-length rel has no direct properties (C++ types
            // the rejection rather than reporting a missing property).
            if info.is_recursive() || matches!(info.kind, VarKind::Path { .. }) {
                return Err(Error::binder(format!(
                    "{var} has data type RECURSIVE_REL but (NODE,REL,STRUCT,ANY) was expected."
                )));
            }
            if !info.is_scalar() {
                // The internal identity columns are not user-accessible as
                // properties on *pattern* variables (use id()/label(); C++
                // reserved-name check). A value-backed node/rel exposes them as
                // ordinary value-struct fields (`UNWIND collect(a) AS d` →
                // `d._id` works).
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "_id" | "_label" | "_src" | "_dst" | "_nodes" | "_rels"
                ) {
                    if info.value_backed {
                        return Ok(Self::property_extract_call(
                            self.var_ref_expr(id),
                            name,
                            match name.to_ascii_lowercase().as_str() {
                                "_id" => LogicalType::InternalId,
                                _ => LogicalType::String,
                            },
                        ));
                    }
                    return Err(Error::binder(format!(
                        "{name} is reserved for system usage. External access is not allowed."
                    )));
                }
                if let Some(prop) = info.property(name) {
                    return Ok(BoundExpr::Property {
                        var: id,
                        prop: prop.name.clone(),
                        ty: prop.ty.clone(),
                    });
                }
                // `a.rowid` / `e.rowid` — the internal offset (C++ rowid
                // pseudo-property), i.e. offset(id(x)).
                if name.eq_ignore_ascii_case("rowid") {
                    let synth = ast::Expr::Function {
                        name: "offset".to_string(),
                        distinct: false,
                        args: vec![ast::Expr::Function {
                            name: "id".to_string(),
                            distinct: false,
                            args: vec![ast::Expr::Variable(var.clone())],
                            arg_names: vec![None],
                        }],
                        arg_names: vec![None],
                    };
                    return self.bind_expr(&synth);
                }
                if self.is_any_var(id) {
                    let data = info
                        .property("data")
                        .expect("ANY variables carry the hidden data column");
                    return Ok(BoundExpr::ValueProperty {
                        value: Box::new(BoundExpr::Property {
                            var: id,
                            prop: data.name.clone(),
                            ty: data.ty.clone(),
                        }),
                        prop: name.to_string(),
                        ty: LogicalType::Any,
                    });
                }
                return Err(Error::binder(format!(
                    "Cannot find property {name} for {var}."
                )));
            }

            return self.bind_value_property(self.var_ref_expr(id), name, var);
        }

        let value = self.bind_expr(base)?;
        let display = expr_name(base);
        self.bind_value_property(value, name, &display)
    }

    pub(super) fn bind_value_property(
        &self,
        value: BoundExpr,
        name: &str,
        display: &str,
    ) -> Result<BoundExpr> {
        let value_ty = value.ty();
        // Node/rel *values* expose their identity struct fields directly
        // (`d._id`, `e2._SRC` — case-insensitive, like C++ value structs).
        if matches!(value_ty, LogicalType::Node(_) | LogicalType::Rel(_)) {
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(), "_id" | "_label" | "_src" | "_dst") {
                let ty = match lower.as_str() {
                    "_id" | "_src" | "_dst" => LogicalType::InternalId,
                    _ => LogicalType::String,
                };
                return Ok(Self::property_extract_call(value, &lower, ty));
            }
        }
        match value_ty {
            // The sentinel table id (nodes()/rels() elements — the concrete
            // table is only known per value) resolves properties at eval.
            LogicalType::Node(t) if t.0 == u64::MAX => Ok(BoundExpr::ValueProperty {
                value: Box::new(value),
                prop: name.to_string(),
                ty: LogicalType::Any,
            }),
            LogicalType::Rel(t) if t.0 == u64::MAX => Ok(BoundExpr::ValueProperty {
                value: Box::new(value),
                prop: name.to_string(),
                ty: LogicalType::Any,
            }),
            LogicalType::Node(t) => {
                let ty = self
                    .catalog
                    .node_table(t)
                    .and_then(|nt| nt.column(name))
                    .map(|column| column.logical_type().clone())
                    .ok_or_else(|| {
                        Error::binder(format!("Cannot find property {name} for {display}."))
                    })?;
                Ok(BoundExpr::ValueProperty {
                    value: Box::new(value),
                    prop: name.to_string(),
                    ty,
                })
            }
            LogicalType::Rel(t) => {
                let ty = self
                    .catalog
                    .rel_table(t)
                    .and_then(|rt| rt.column(name))
                    .map(|column| column.logical_type().clone())
                    .ok_or_else(|| {
                        Error::binder(format!("Cannot find property {name} for {display}."))
                    })?;
                Ok(BoundExpr::ValueProperty {
                    value: Box::new(value),
                    prop: name.to_string(),
                    ty,
                })
            }
            LogicalType::Struct(fields) => {
                let ty = fields
                    .iter()
                    .find(|(field, _)| field.eq_ignore_ascii_case(name))
                    .map(|(_, ty)| ty.clone())
                    .ok_or_else(|| Error::binder(format!("Invalid struct field name: {name}.")))?;
                Ok(Self::property_extract_call(value, name, ty))
            }
            LogicalType::Map(_, value_ty) => {
                Ok(Self::property_extract_call(value, name, *value_ty))
            }
            LogicalType::Any => Ok(Self::property_extract_call(value, name, LogicalType::Any)),
            other => Err(Error::binder(format!(
                "{display} has data type {other} but (NODE,REL,STRUCT,ANY) was expected."
            ))),
        }
    }

    pub(super) fn property_extract_call(
        value: BoundExpr,
        name: &str,
        ty: LogicalType,
    ) -> BoundExpr {
        BoundExpr::Call {
            function: BuiltinScalar::StructExtract,
            called_name: "struct_extract".to_string(),
            args: vec![value, BoundExpr::Literal(Value::String(name.to_string()))],
            ty,
        }
    }

    pub(super) fn bind_expr(&mut self, e: &ast::Expr) -> Result<BoundExpr> {
        // C++ nullifies the WHOLE expression containing an unbound parameter
        // (oracle: `coalesce($x, 5)` and `$x IS NULL` both yield NULL, not 5 /
        // True) — the parameter's ANY type swallows the tree.
        if self.has_unbound_param(e) {
            return Ok(BoundExpr::Literal(Value::Null));
        }
        let result = match e {
            ast::Expr::PatternComprehension {
                pattern,
                projection,
            } => self.bind_pattern_expression(pattern, projection.as_deref()),
            ast::Expr::Literal(v) => Ok(BoundExpr::Literal(v.clone())),
            ast::Expr::Variable(name) => {
                // A lambda parameter in scope resolves to a LambdaVar (not a
                // pattern variable / column).
                if let Some((id, ty)) = self.lambda_binding(name) {
                    return Ok(BoundExpr::LambdaVar { id, ty });
                }
                let var = self.lookup_var(name)?;
                let info = &self.vars[var.0 as usize];
                if info.is_scalar() {
                    Ok(BoundExpr::ScalarVar {
                        var,
                        ty: info.scalar_type(),
                    })
                } else {
                    Ok(BoundExpr::NodeRef {
                        var,
                        ty: self.var_value_type(var),
                    })
                }
            }
            ast::Expr::OverflowInt(text) => Err(overflow_int_error(text)),
            ast::Expr::Property { base, name } => self.bind_property_expr(base, name),
            ast::Expr::Parameter(name) => Ok(self.bind_parameter(name)),
            ast::Expr::Function {
                name,
                distinct,
                args,
                arg_names,
            } => self.bind_function(name, *distinct, args, arg_names),
            ast::Expr::And(terms) => self.bind_bool(ScalarOp::And, terms),
            ast::Expr::Or(terms) => self.bind_bool(ScalarOp::Or, terms),
            ast::Expr::Xor(a, b) => self.bind_scalar(ScalarOp::Xor, &[a.as_ref(), b.as_ref()]),
            ast::Expr::Not(a) => self.bind_scalar(ScalarOp::Not, &[a.as_ref()]),
            ast::Expr::Comparison { op, lhs, rhs } => {
                self.bind_scalar(cmp_op(*op), &[lhs.as_ref(), rhs.as_ref()])
            }
            ast::Expr::Arithmetic { op, lhs, rhs } => {
                self.bind_scalar(arith_op(*op), &[lhs.as_ref(), rhs.as_ref()])
            }
            ast::Expr::Negate(a) => self.bind_scalar(ScalarOp::Neg, &[a.as_ref()]),
            ast::Expr::IsNull(a) => self.bind_scalar(ScalarOp::IsNull, &[a.as_ref()]),
            ast::Expr::IsNotNull(a) => self.bind_scalar(ScalarOp::IsNotNull, &[a.as_ref()]),
            ast::Expr::List(items) => {
                let bound = items
                    .iter()
                    .map(|e| self.bind_expr(e))
                    .collect::<Result<Vec<_>>>()?;
                // Element type = the C++ `LIST_CREATION` combine (lattice, then the
                // mixed-type STRING / first-element fallback; empty or all-NULL
                // literals default to `INT64[]`) — see `list_literal_elem_type`.
                let elem_ty = list_literal_elem_type(&bound);
                // Each element must implicitly cast to the combined element type —
                // C++ `ListCreationFunction::bindFunc` casts every argument to the
                // combined type via `implicitCastIfNecessary` (POSITIONAL for
                // nested literals: struct/union member names adopt the combined
                // type's), so an incompatible element is a *bind-time*
                // rejection, not a deferred runtime cast failure.
                let mut bound = bound;
                for elem in &mut bound {
                    adopt_struct_field_names(elem, &elem_ty);
                    adopt_union_member_names(elem, &elem_ty);
                }
                for (item, elem) in items.iter().zip(&bound) {
                    assignable_or_err(elem, &elem_ty, &expr_name(item))?;
                }
                let elems = bound
                    .into_iter()
                    .map(|expression| self.coerce_to(expression, &elem_ty))
                    .collect();
                Ok(BoundExpr::List {
                    elems,
                    ty: LogicalType::List(Box::new(elem_ty)),
                })
            }
            ast::Expr::Struct(fields) => {
                check_struct_field_dups(fields)?;
                let bfields = fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), self.bind_expr(v)?)))
                    .collect::<Result<Vec<_>>>()?;
                // An all-NULL field types as STRING (C++ ANY-default) in the
                // struct literal's declared type.
                let ty = LogicalType::Struct(
                    bfields
                        .iter()
                        .map(|(k, v)| {
                            let t = match v.ty() {
                                LogicalType::Any => LogicalType::String,
                                t => t,
                            };
                            (k.clone(), t)
                        })
                        .collect(),
                );
                Ok(BoundExpr::Struct {
                    fields: bfields,
                    ty,
                })
            }
            ast::Expr::Lambda { .. } => Err(Error::binder(
                "a lambda is only valid as an argument to a list function".to_string(),
            )),
            ast::Expr::ListComprehension {
                var,
                list,
                predicate,
                projection,
            } => self.bind_comprehension(var, list, predicate.as_deref(), projection.as_deref()),
            ast::Expr::Case {
                operand,
                when_thens,
                else_,
            } => self.bind_case(operand.as_deref(), when_thens, else_.as_deref()),
            ast::Expr::Star => Err(Error::binder(
                "'*' is only valid as the argument of count(*) or in RETURN *".to_string(),
            )),
            ast::Expr::Subquery {
                kind,
                patterns,
                where_clause,
            } => {
                // Stage the subquery (its pattern is bound later, in `&mut`
                // context — see `drain_subqueries`) and reference it by id.
                let id = self.subquery_count;
                self.subquery_count += 1;
                self.pending_subqueries.push(PendingSubquery {
                    id,
                    kind: *kind,
                    patterns: patterns.clone(),
                    where_clause: where_clause.as_deref().cloned(),
                });
                let ty = match kind {
                    ast::SubqueryKind::Exists => LogicalType::Bool,
                    ast::SubqueryKind::Count => LogicalType::Int64,
                };
                Ok(BoundExpr::Subquery { id, ty })
            }
        };
        if let Ok(expression) = &result {
            self.capture_parameter_constraints(expression);
        }
        result
    }

    /// Bind the patterns of subqueries staged during this part's expression
    /// binding. Each binds in the current scope (outer variables visible) plus
    /// fresh inner variables that are then dropped from scope. Returns them
    /// ordered by id. A NESTED subquery (staged while binding another's WHERE)
    /// binds depth-first, while its parent's inner variables are still in
    /// scope (it references them).
    pub(super) fn drain_subqueries(&mut self) -> Result<Vec<BoundSubquery>> {
        let pending = std::mem::take(&mut self.pending_subqueries);
        let mut out: Vec<Option<BoundSubquery>> = (0..pending.len()).map(|_| None).collect();
        for ps in pending {
            self.bind_one_subquery(ps, &mut out)?;
        }
        // Ids are part-local (index into this part's `subqueries`); reset for the
        // next part.
        self.subquery_count = 0;
        Ok(out
            .into_iter()
            .map(|o| o.expect("every id filled"))
            .collect())
    }

    /// Bind one staged subquery; any subqueries staged while binding its WHERE
    /// bind recursively BEFORE this one's inner scope is dropped.
    pub(super) fn bind_one_subquery(
        &mut self,
        ps: PendingSubquery,
        out: &mut Vec<Option<BoundSubquery>>,
    ) -> Result<()> {
        let saved_scope = self.scope.clone();
        let mut match_ = BoundMatch::default();
        let mut preds = Vec::new();
        for pe in &ps.patterns {
            self.bind_match_pattern(pe, &mut match_, &mut preds, None)?;
        }
        if let Some(w) = &ps.where_clause {
            preds.push(self.bind_expr(w)?);
        }
        let nested = std::mem::take(&mut self.pending_subqueries);
        for n in nested {
            if n.id >= out.len() {
                out.resize(n.id + 1, None);
            }
            self.bind_one_subquery(n, out)?;
        }
        // Inner variables are scoped to the subquery.
        self.scope = saved_scope;
        let kind = match ps.kind {
            ast::SubqueryKind::Exists => SubqueryKind::Exists,
            ast::SubqueryKind::Count => SubqueryKind::Count,
        };
        if ps.id >= out.len() {
            out.resize(ps.id + 1, None);
        }
        out[ps.id] = Some(BoundSubquery {
            kind,
            match_,
            where_predicate: combine_and(preds),
        });
        Ok(())
    }

    /// Bind `list_transform`/`list_filter`/`list_reduce` (a list plus a lambda).
    pub(super) fn bind_list_lambda(
        &mut self,
        kind: LambdaKind,
        args: &[ast::Expr],
    ) -> Result<BoundExpr> {
        if args.len() != 2 {
            return Err(Error::binder(
                "list lambda functions take a list and a lambda".to_string(),
            ));
        }
        let list = self.bind_expr(&args[0])?;
        let (params, body_ast) = match &args[1] {
            ast::Expr::Lambda { params, body } => (params.clone(), body.as_ref()),
            other => {
                let fn_name = match kind {
                    LambdaKind::Transform => "LIST_TRANSFORM",
                    LambdaKind::Filter => "LIST_FILTER",
                    LambdaKind::Reduce => "LIST_REDUCE",
                };
                return Err(Error::binder(format!(
                    "The second argument of {fn_name} should be a lambda expression but got {}.",
                    ast_expr_kind(other)
                )));
            }
        };
        let want = if kind == LambdaKind::Reduce { 2 } else { 1 };
        if params.len() != want {
            return Err(Error::binder(format!(
                "this lambda expects {want} parameter(s) but got {}",
                params.len()
            )));
        }
        // An untyped (NULL) list argument is the C++ binder type error — the
        // expression name of a NULL literal renders empty, giving the
        // verbatim ` has data type ANY but LIST was expected.`
        if list.ty() == LogicalType::Any {
            return Err(Error::binder(format!(
                "{} has data type ANY but LIST was expected.",
                expr_name(&args[0])
            )));
        }
        let elem_ty = match list.ty() {
            LogicalType::List(inner) | LogicalType::Array(inner, _) => *inner,
            _ => LogicalType::Any,
        };
        let param_ids = params
            .iter()
            .map(|parameter| self.allocate_lambda_id(parameter))
            .collect::<Vec<_>>();
        let depth = self.lambda_params.len();
        self.lambda_params
            .extend(
                params
                    .iter()
                    .zip(&param_ids)
                    .map(|(parameter, id)| LambdaBinding {
                        name: parameter.clone(),
                        id: *id,
                        ty: elem_ty.clone(),
                    }),
            );
        let body = self.bind_expr(body_ast);
        self.lambda_params.truncate(depth);
        let body = body?;
        // A filter lambda must produce BOOL (C++ bindFunc validation).
        if kind == LambdaKind::Filter && !matches!(body.ty(), LogicalType::Bool | LogicalType::Any)
        {
            return Err(Error::binder(
                "LIST_FILTER requires the result type of lambda expression be BOOL.".to_string(),
            ));
        }
        let ty = match kind {
            LambdaKind::Transform => LogicalType::List(Box::new(body.ty())),
            LambdaKind::Filter => LogicalType::List(Box::new(elem_ty)),
            LambdaKind::Reduce => body.ty(),
        };
        Ok(BoundExpr::ListLambda {
            kind,
            list: Box::new(list),
            params: param_ids,
            body: Box::new(body),
            ty,
        })
    }

    /// Desugar `[var IN list [WHERE pred] [| proj]]` into filter/transform lambdas.
    pub(super) fn bind_comprehension(
        &mut self,
        var: &str,
        list: &ast::Expr,
        predicate: Option<&ast::Expr>,
        projection: Option<&ast::Expr>,
    ) -> Result<BoundExpr> {
        let list = self.bind_expr(list)?;
        let elem_ty = match list.ty() {
            LogicalType::List(inner) | LogicalType::Array(inner, _) => *inner,
            _ => LogicalType::Any,
        };
        let param_id = self.allocate_lambda_id(var);
        let depth = self.lambda_params.len();
        self.lambda_params.push(LambdaBinding {
            name: var.to_string(),
            id: param_id,
            ty: elem_ty.clone(),
        });
        let result = self.bind_comprehension_stages(list, elem_ty, param_id, predicate, projection);
        self.lambda_params.truncate(depth);
        result
    }

    fn bind_comprehension_stages(
        &mut self,
        mut list: BoundExpr,
        elem_ty: LogicalType,
        param_id: LambdaVarId,
        predicate: Option<&ast::Expr>,
        projection: Option<&ast::Expr>,
    ) -> Result<BoundExpr> {
        if let Some(predicate) = predicate {
            let body = self.bind_expr(predicate)?;
            if !matches!(body.ty(), LogicalType::Bool | LogicalType::Any) {
                return Err(Error::binder(
                    "LIST_FILTER requires the result type of lambda expression be BOOL."
                        .to_string(),
                ));
            }
            list = BoundExpr::ListLambda {
                kind: LambdaKind::Filter,
                list: Box::new(list),
                params: vec![param_id],
                body: Box::new(body),
                ty: LogicalType::List(Box::new(elem_ty)),
            };
        }
        if let Some(projection) = projection {
            let body = self.bind_expr(projection)?;
            let ty = LogicalType::List(Box::new(body.ty()));
            list = BoundExpr::ListLambda {
                kind: LambdaKind::Transform,
                list: Box::new(list),
                params: vec![param_id],
                body: Box::new(body),
                ty,
            };
        }
        Ok(list)
    }

    /// Bind a `CASE`. The operand (if any) is kept so the simple form can use
    /// null-safe equality at runtime; the searched form has no operand and each
    /// condition is a boolean predicate.
    pub(super) fn bind_case(
        &mut self,
        operand: Option<&ast::Expr>,
        when_thens: &[(ast::Expr, ast::Expr)],
        else_: Option<&ast::Expr>,
    ) -> Result<BoundExpr> {
        let bound_operand = operand
            .map(|o| self.bind_expr(o))
            .transpose()?
            .map(Box::new);
        let branches = when_thens
            .iter()
            .map(|(cond, res)| Ok((self.bind_expr(cond)?, self.bind_expr(res)?)))
            .collect::<Result<Vec<_>>>()?;
        let belse = else_.map(|e| self.bind_expr(e)).transpose()?.map(Box::new);
        // Result type: the LAST non-`Any` THEN type (else the ELSE type) — oracle:
        // `THEN 2.5 … THEN 1` is INT64, `THEN 1 … THEN 'a'` is STRING. Every other
        // THEN/ELSE must be implicitly castable to it (validated in order — a
        // mismatch is the C++ "Implicit cast is not supported" error naming the
        // branch), and in the operand form each WHEN value must likewise cast to
        // the operand's type (checked after the branches, matching C++ order).
        let ty = branches
            .iter()
            .rev()
            .map(|(_, r)| r.ty())
            .find(|t| *t != LogicalType::Any)
            .or_else(|| {
                belse
                    .as_ref()
                    .map(|e| e.ty())
                    .filter(|t| *t != LogicalType::Any)
            })
            .unwrap_or(LogicalType::Any);
        for ((_, r), (_, r_ast)) in branches.iter().zip(when_thens) {
            assignable_or_err(r, &ty, &expr_name(r_ast))?;
        }
        if let (Some(b), Some(e_ast)) = (&belse, else_) {
            assignable_or_err(b, &ty, &expr_name(e_ast))?;
        }
        if let Some(op) = &bound_operand {
            let op_ty = op.ty();
            for ((c, _), (c_ast, _)) in branches.iter().zip(when_thens) {
                assignable_or_err(c, &op_ty, &expr_name(c_ast))?;
            }
        }
        let op_ty = bound_operand.as_ref().map(|o| o.ty());
        let branches = branches
            .into_iter()
            .map(|(c, r)| {
                let c = match &op_ty {
                    Some(target) => self.coerce_to(c, target),
                    None => c,
                };
                (c, self.coerce_to(r, &ty))
            })
            .collect();
        let belse = belse.map(|expression| Box::new(self.coerce_to(*expression, &ty)));
        Ok(BoundExpr::Case {
            operand: bound_operand,
            branches,
            else_: belse,
            ty,
        })
    }

    pub(super) fn bind_bool(&mut self, op: ScalarOp, terms: &[ast::Expr]) -> Result<BoundExpr> {
        let mut args = terms
            .iter()
            .map(|term| self.bind_expr(term))
            .collect::<Result<Vec<_>>>()?;
        for (term, argument) in terms.iter().zip(&mut args) {
            self.constrain_parameter(argument, &LogicalType::Bool);
            bool_arg_or_err(term, argument)?;
        }
        Ok(BoundExpr::Scalar {
            op,
            args,
            ty: LogicalType::Bool,
        })
    }

    pub(super) fn bind_scalar(
        &mut self,
        op: ScalarOp,
        raw_args: &[&ast::Expr],
    ) -> Result<BoundExpr> {
        let mut args = raw_args
            .iter()
            .map(|a| self.bind_expr(a))
            .collect::<Result<Vec<_>>>()?;
        if op.is_arithmetic() && args.len() == 2 {
            let left = args[0].ty();
            let right = args[1].ty();
            if left == LogicalType::Any && right != LogicalType::Any {
                args[0] = self.coerce_to(args[0].clone(), &right);
            } else if right == LogicalType::Any && left != LogicalType::Any {
                args[1] = self.coerce_to(args[1].clone(), &left);
            }
        }
        if op.is_comparison() && args.len() == 2 {
            let (l, r) = (args[0].ty(), args[1].ty());
            if !koko_function::comparison_comparable(&l, &r) {
                // C++ folds literal-argument calls while binding them, so a
                // constant operand's own error (`date(2012)` → the date-parse
                // Conversion error) surfaces BEFORE the comparison check.
                for a in &args {
                    if let Some(Err(e)) = try_const_eval(a) {
                        return Err(e);
                    }
                }
                return Err(Error::binder(format!(
                    "Type Mismatch: Cannot compare types {l} and {r}"
                )));
            }
            let common = koko_function::comparison_common_type(&l, &r);
            if let Some(common) = common {
                args = args
                    .into_iter()
                    .map(|argument| self.coerce_to(argument, &common))
                    .collect();
            }
        }
        if matches!(op, ScalarOp::Xor | ScalarOp::Not) {
            for (expr, argument) in raw_args.iter().zip(&mut args) {
                self.constrain_parameter(argument, &LogicalType::Bool);
                bool_arg_or_err(expr, argument)?;
            }
        }
        let arg_types: Vec<LogicalType> = args.iter().map(|a| a.ty()).collect();
        let ty = koko_function::scalar_result_type(op, &arg_types)?;
        Ok(BoundExpr::Scalar { op, args, ty })
    }

    pub(super) fn bind_cast(&mut self, args: &[ast::Expr]) -> Result<BoundExpr> {
        if args.len() != 2 {
            return Err(Error::binder(
                "cast expects (expression, type) arguments".to_string(),
            ));
        }
        let target = match &args[1] {
            ast::Expr::Literal(Value::String(s)) => self.resolve_cast_type(s)?,
            _ => {
                return Err(Error::binder(
                    "the second argument to cast must be a type-name string".to_string(),
                ));
            }
        };
        let expr = self.bind_expr(&args[0])?;
        Ok(BoundExpr::Cast {
            expr: Box::new(expr),
            target,
        })
    }

    /// Bind `union_value(tag := v)` / `struct_pack(f := v, …)` — the named-argument
    /// constructors. Returns `Some` when `name` is one of them, so both function-bind
    /// paths (`bind_function` and `bind_output_function`) share one implementation.
    /// `bind_arg` binds a sub-expression in the caller's scope.
    pub(super) fn bind_named_ctor(
        &mut self,
        name: &str,
        args: &[ast::Expr],
        arg_names: &[Option<String>],
        scope: Option<&HashMap<String, ProjectionOutputRef>>,
    ) -> Result<Option<BoundExpr>> {
        let arg_name = |i: usize| arg_names.get(i).and_then(|o| o.clone());
        if name.eq_ignore_ascii_case("union_value") {
            if args.len() != 1 {
                return Err(Error::binder(
                    "Function UNION_VALUE takes exactly one argument.".to_string(),
                ));
            }
            let tag = arg_name(0).ok_or_else(|| {
                Error::binder(
                    "Function UNION_VALUE requires a named argument, e.g. union_value(tag := value)."
                        .to_string(),
                )
            })?;
            // C++ casts an ANY-typed argument (e.g. an untyped NULL) to STRING before
            // building the single-member union type `(tag, argType)`.
            let mut val = self.bind_named_argument(&args[0], scope)?;
            if val.ty() == LogicalType::Any {
                val = self.coerce_to(val, &LogicalType::String);
            }
            let field_ty = val.ty();
            return Ok(Some(BoundExpr::Call {
                function: BuiltinScalar::UnionValue,
                called_name: "union_value".to_string(),
                args: vec![val],
                ty: LogicalType::Union(vec![(tag, field_ty)]),
            }));
        }
        if name.eq_ignore_ascii_case("struct_pack") {
            let mut fields = Vec::with_capacity(args.len());
            for (i, a) in args.iter().enumerate() {
                // An explicit `field := value` name wins; otherwise C++ infers the
                // field name from a variable/property expression, and errors on
                // anything else (e.g. a literal).
                let fname = match arg_name(i) {
                    Some(n) => n,
                    None => match a {
                        ast::Expr::Variable(v) => v.clone(),
                        ast::Expr::Property { name, .. } => name.clone(),
                        _ => {
                            return Err(Error::binder(format!(
                                "Cannot infer field name for {}.",
                                expr_name(a)
                            )));
                        }
                    },
                };
                fields.push((fname, self.bind_named_argument(a, scope)?));
            }
            check_struct_field_dups(&fields)?;
            let ty = LogicalType::Struct(fields.iter().map(|(k, v)| (k.clone(), v.ty())).collect());
            return Ok(Some(BoundExpr::Struct { fields, ty }));
        }
        Ok(None)
    }

    pub(super) fn bind_named_argument(
        &mut self,
        argument: &ast::Expr,
        scope: Option<&HashMap<String, ProjectionOutputRef>>,
    ) -> Result<BoundExpr> {
        match scope {
            Some(scope) => self.bind_order_expr_in_output_scope(argument, scope),
            None => self.bind_expr(argument),
        }
    }

    /// Bind `keys(node|rel)` → a constant `LIST(STRING)` of the value's property
    /// names (the multi-label union, in schema order), matching C++'s bind-time
    /// rewrite (`struct/keys_function.cpp`). `keys(NULL)` → `NULL`. Returns `None`
    /// when `name` isn't `keys`, so both function-bind paths share it.
    pub(super) fn bind_keys(
        &mut self,
        name: &str,
        args: &[ast::Expr],
    ) -> Result<Option<BoundExpr>> {
        if !name.eq_ignore_ascii_case("keys") {
            return Ok(None);
        }
        if args.len() != 1 {
            return Err(Error::binder(
                "Function KEYS takes exactly one argument.".to_string(),
            ));
        }
        // C++ rewrites a null-literal argument to a null literal.
        if matches!(&args[0], ast::Expr::Literal(Value::Null)) {
            return Ok(Some(BoundExpr::Literal(Value::Null)));
        }
        // A node/rel *variable* carries its resolved (multi-label unioned) property
        // list — the same source `RETURN a.*` expands (schema order). This is the
        // corpus path; anything else falls back to the bound value's single-table type.
        let names: Vec<String> = if let ast::Expr::Variable(v) = &args[0] {
            let var = self.lookup_var(v)?;
            let info = &self.vars[var.0 as usize];
            if info.is_scalar() {
                return Err(Error::binder(format!(
                    "Variable {v} is not a node or relationship."
                )));
            }
            info.properties.iter().map(|p| p.name.clone()).collect()
        } else {
            let bound = self.bind_expr(&args[0])?;
            match bound.ty() {
                LogicalType::Node(t) => self.node_props(t).into_iter().map(|p| p.name).collect(),
                LogicalType::Rel(t) => self.rel_props(t).into_iter().map(|p| p.name).collect(),
                LogicalType::Any => return Ok(Some(BoundExpr::Literal(Value::Null))),
                other => {
                    return Err(koko_function::scalarfn::signature_error(
                        "keys",
                        &[other],
                        &["(NODE)", "(REL)"],
                    ));
                }
            }
        };
        let elems = names
            .into_iter()
            .map(|n| BoundExpr::Literal(Value::String(n)))
            .collect();
        Ok(Some(BoundExpr::List {
            elems,
            ty: LogicalType::List(Box::new(LogicalType::String)),
        }))
    }

    pub(super) fn bind_scalar_udf(
        &mut self,
        function: std::sync::Arc<koko_common::RegisteredScalarFunction>,
        arguments: Vec<BoundExpr>,
    ) -> Result<BoundExpr> {
        if arguments.len() != function.parameter_types.len() {
            return Err(Error::binder(format!(
                "Scalar function {} expects {} arguments but received {}.",
                function.name,
                function.parameter_types.len(),
                arguments.len()
            )));
        }
        for (position, (argument, expected)) in
            arguments.iter().zip(&function.parameter_types).enumerate()
        {
            if !assignable(&argument.ty(), expected) {
                return Err(Error::binder(format!(
                    "Scalar function {} argument {} has type {}, expected {}.",
                    function.name,
                    position + 1,
                    argument.ty(),
                    expected
                )));
            }
        }
        let arguments = arguments
            .into_iter()
            .zip(&function.parameter_types)
            .map(|(argument, expected)| self.coerce_to(argument, expected))
            .collect();
        let ty = function.result_type.clone();
        Ok(BoundExpr::Udf {
            function,
            args: arguments,
            ty,
        })
    }

    pub(super) fn bind_function(
        &mut self,
        name: &str,
        distinct: bool,
        args: &[ast::Expr],
        arg_names: &[Option<String>],
    ) -> Result<BoundExpr> {
        let registered_function = resolve_builtin(name).map(|descriptor| descriptor.function);
        let builtin = match registered_function {
            Some(BuiltinFunction::Scalar(function)) => Some(function),
            _ => None,
        };
        let bindable_builtin = resolve_builtin_scalar(name);
        if builtin == Some(BuiltinScalar::CastFunction) {
            return self.bind_cast(args);
        }
        // `cost(e)` is defined only for a (ALL) WSHORTEST recursive rel — a
        // plain or unweighted recursive rel is a bind-time error.
        if builtin == Some(BuiltinScalar::Cost) {
            if let [ast::Expr::Variable(v)] = args {
                let weighted = self.scope.get(v).is_some_and(|&var| {
                    matches!(
                        &self.vars[var.0 as usize].kind,
                        VarKind::Rel { recursive: Some(spec), .. } if spec.weight.is_some()
                    )
                });
                if !weighted {
                    return Err(Error::binder(format!(
                        "Cost function is not defined for {v}"
                    )));
                }
            }
        }
        if let Some(bound) = self.bind_named_ctor(name, args, arg_names, None)? {
            return Ok(bound);
        }
        if let Some(b) = self.bind_keys(name, args)? {
            return Ok(b);
        }
        if let Some(func) = sequence_fn(name) {
            return self.bind_sequence_call(func, args);
        }
        if let Some(BuiltinFunction::Aggregate(op)) = registered_function {
            // percentileDisc(x, p): the percentile is a constant second
            // argument, folded into the op.
            if let AggOp::PercentileDisc(_) = op {
                if args.len() != 2 {
                    return Err(Error::binder(format!(
                        "aggregate {name} expects a value and a percentile"
                    )));
                }
                let arg = self.bind_expr(&args[0])?;
                let pexpr = self.bind_expr(&args[1])?;
                let p = match try_const_eval(&pexpr) {
                    Some(Ok(v)) => v.as_f64().unwrap_or(0.5),
                    Some(Err(e)) => return Err(e),
                    None => {
                        return Err(Error::binder(
                            "the percentile argument must be a constant".to_string(),
                        ));
                    }
                };
                let ty = arg.ty();
                return Ok(BoundExpr::Aggregate {
                    op: AggOp::PercentileDisc(p.to_bits()),
                    distinct,
                    arg: Some(Box::new(arg)),
                    ty,
                });
            }
            if args.len() == 1 && args[0] == ast::Expr::Star {
                if op != AggOp::Count {
                    return Err(Error::binder(format!(
                        "{name}(*) is not a valid aggregate."
                    )));
                }
                return Ok(BoundExpr::Aggregate {
                    op: AggOp::CountStar,
                    distinct,
                    arg: None,
                    ty: LogicalType::Int64,
                });
            }
            if args.len() != 1 {
                return Err(Error::binder(format!(
                    "aggregate {name} expects exactly one argument"
                )));
            }
            let arg = self.bind_expr(&args[0])?;
            if arg.contains_aggregate() {
                let display = format!(
                    "{}({})",
                    name.to_uppercase(),
                    args.iter().map(expr_name).collect::<Vec<_>>().join(",")
                );
                return Err(Error::binder(format!(
                    "Expression {display} contains nested aggregation."
                )));
            }
            // The catalog aggregate gate (C++ matchAggregateFunction): exact
            // type-ID matching, no casts; failures carry the full DISTINCT-
            // annotated overload block.
            if let Some(err) = koko_function::aggregate_signature_error(name, &[arg.ty()], distinct)
            {
                return Err(err);
            }
            // SUM/AVG require a numeric argument (mirrors the scalar-arithmetic
            // operand check); otherwise they would silently produce 0.
            if matches!(op, AggOp::Sum | AggOp::Avg) {
                let at = arg.ty();
                if !at.is_numeric() && at != LogicalType::Any {
                    return Err(Error::binder(format!(
                        "Function {name} expects a numeric argument but got {at}."
                    )));
                }
            }
            let ty = koko_function::agg_result_type(op, &arg.ty())?;
            return Ok(BoundExpr::Aggregate {
                op,
                distinct,
                arg: Some(Box::new(arg)),
                ty,
            });
        }
        let lk = match builtin {
            Some(BuiltinScalar::ListTransform) => Some(LambdaKind::Transform),
            Some(BuiltinScalar::ListFilter) => Some(LambdaKind::Filter),
            Some(BuiltinScalar::ListReduce) => Some(LambdaKind::Reduce),
            _ => None,
        };
        if let Some(kind) = lk {
            return self.bind_list_lambda(kind, args);
        }
        // START_NODE/END_NODE are C++ REWRITE functions: over a rel PATTERN
        // variable they rewrite to its bound endpoint node variables (per-row
        // for undirected patterns); rel VALUES fall through to the scalar
        // eval over materialized endpoints.
        if matches!(
            builtin,
            Some(BuiltinScalar::StartNode | BuiltinScalar::EndNode)
        ) && let [ast::Expr::Variable(rv)] = args
        {
            if let Ok(id) = self.lookup_var(rv) {
                if let VarKind::Rel {
                    src,
                    dst,
                    recursive: None,
                    ..
                } = &self.vars[id.0 as usize].kind
                {
                    let target = if builtin == Some(BuiltinScalar::StartNode) {
                        *src
                    } else {
                        *dst
                    };
                    return Ok(self.var_ref_expr(target));
                }
            }
        }
        // List-predicate quantifiers `ANY|ALL|NONE|SINGLE (x IN list WHERE p)`
        // (parser-desugared to `name(list, x -> p)`): bound as a size
        // comparison over LIST_FILTER, matching the C++ rewrite.
        if let Some(
            quant @ (BuiltinScalar::Any
            | BuiltinScalar::All
            | BuiltinScalar::None
            | BuiltinScalar::Single),
        ) = builtin
        {
            if args.len() == 2 && matches!(&args[1], ast::Expr::Lambda { .. }) {
                let filtered = self.bind_list_lambda(LambdaKind::Filter, args)?;
                let size_of = |e: BoundExpr| BoundExpr::Call {
                    function: BuiltinScalar::Size,
                    called_name: "size".to_string(),
                    args: vec![e],
                    ty: LogicalType::Int64,
                };
                let int_lit = |n: i64| BoundExpr::Literal(Value::Int64(n));
                let (op, lhs, rhs) = match quant {
                    BuiltinScalar::Any => (ScalarOp::Gt, size_of(filtered), int_lit(0)),
                    BuiltinScalar::None => (ScalarOp::Eq, size_of(filtered), int_lit(0)),
                    BuiltinScalar::Single => (ScalarOp::Eq, size_of(filtered), int_lit(1)),
                    _ => {
                        // The outer pattern admits only `All` after the three
                        // explicit variants above.
                        let whole = self.bind_expr(&args[0])?;
                        (ScalarOp::Eq, size_of(filtered), size_of(whole))
                    }
                };
                return Ok(BoundExpr::Scalar {
                    op,
                    args: vec![lhs, rhs],
                    ty: LogicalType::Bool,
                });
            }
        }
        if let Some(function) = self
            .session_config
            .scalar_udfs
            .get(&name.to_ascii_lowercase())
            .cloned()
        {
            if distinct {
                return Err(Error::binder(format!(
                    "DISTINCT is not valid for scalar function {}.",
                    function.name
                )));
            }
            if args
                .iter()
                .any(|argument| matches!(argument, ast::Expr::Lambda { .. }))
            {
                return Err(Error::binder(format!(
                    "{} does not support lambda input.",
                    function.name.to_uppercase()
                )));
            }
            let bound = args
                .iter()
                .map(|argument| self.bind_expr(argument))
                .collect::<Result<Vec<_>>>()?;
            return self.bind_scalar_udf(function, bound);
        }
        if let Some(function) = bindable_builtin {
            // A lambda argument to a non-lambda function is the C++ binder error
            // (the lambda functions were dispatched above).
            if args.iter().any(|a| matches!(a, ast::Expr::Lambda { .. })) {
                return Err(Error::binder(format!(
                    "{} does not support lambda input.",
                    name.to_uppercase()
                )));
            }
            let mut bound: Vec<BoundExpr> = args
                .iter()
                .map(|a| self.bind_expr(a))
                .collect::<Result<_>>()?;
            // NULL-coalescing functions return the common supertype of their args,
            // and each arg is coerced to it (so coalesce(1, 1.5) is DOUBLE and
            // yields 1.000000). greatest/least keep their exact DATE/TIMESTAMP
            // signatures and are validated by koko-function.
            let lname = name.to_ascii_lowercase();
            // `properties(list, key)` binds with C++'s three checks: the key is
            // a literal string; the list holds node/rel values; and (when the
            // element tables are knowable) the key names an existing property.
            if function == BuiltinScalar::Properties && args.len() == 2 {
                if !matches!(&args[1], ast::Expr::Literal(Value::String(_))) {
                    return Err(Error::binder(
                        "Expected literal input as the second argument for PROPERTIES()."
                            .to_string(),
                    ));
                }
                let t0 = bound[0].ty();
                let elem_ok = match &t0 {
                    LogicalType::List(inner) | LogicalType::Array(inner, _) => matches!(
                        inner.as_ref(),
                        LogicalType::Node(_)
                            | LogicalType::Rel(_)
                            | LogicalType::RecursiveRel
                            | LogicalType::Any
                    ),
                    _ => false,
                };
                if !elem_ok {
                    return Err(Error::binder(format!(
                        "Cannot extract properties from {t0}."
                    )));
                }
                if let ast::Expr::Literal(Value::String(key)) = &args[1] {
                    let lower = key.to_ascii_lowercase();
                    if !matches!(lower.as_str(), "_id" | "_label" | "_src" | "_dst")
                        && !self.catalog.any_table_has_property(key)
                    {
                        return Err(Error::binder(format!("Invalid property name: {key}.")));
                    }
                }
            }
            if matches!(function, BuiltinScalar::Coalesce | BuiltinScalar::Ifnull) {
                // C++ `coalesce.cpp` bindFunc: result type = the combined arg type
                // (ANY, i.e. all-null, defaults to STRING), and every arg is then bound
                // to that type. An arg that can't *implicitly* cast to it is a bind-time
                // error (`implicitCastIfNecessary`), not a deferred runtime conversion —
                // e.g. `coalesce(1, "hello")` → `expected INT64`.
                // C++ falls back to STRING only when the type pair has NO max
                // in the lattice (coalesce(1, true) → STRING '1'); a pair WITH
                // a max keeps it, and a non-castable arg is then the bind error
                // below (coalesce(1, "hello") errors — corpus-pinned).
                let mut common = match common_type_strict(bound.iter().map(|a| a.ty())) {
                    Some(t) => t,
                    None => LogicalType::String,
                };
                if common == LogicalType::Any {
                    common = LogicalType::String;
                }
                for (arg, b) in args.iter().zip(bound.iter()) {
                    if !assignable(&b.ty(), &common) {
                        return Err(Error::binder(format!(
                            "Expression {} has data type {} but expected {}. Implicit cast is not supported.",
                            expr_name(arg),
                            b.ty().name(),
                            common.name()
                        )));
                    }
                }
                bound = bound
                    .into_iter()
                    .map(|argument| self.coerce_to(argument, &common))
                    .collect();
            }
            // regexp_replace's optional 4th (options) argument must be a
            // STRING literal (C++ bindFunc checks: non-literal first, then
            // the literal's type).
            if function == BuiltinScalar::RegexpReplace && args.len() == 4 {
                match &args[3] {
                    ast::Expr::Literal(Value::String(opt)) => {
                        if opt != "g" {
                            return Err(Error::binder(
                                "regex_replace can only support global replace option: g."
                                    .to_string(),
                            ));
                        }
                    }
                    ast::Expr::Literal(v) => {
                        return Err(Error::binder(format!(
                            "{} has data type {} but STRING was expected.",
                            expr_name(&args[3]),
                            v.logical_type().name()
                        )));
                    }
                    other => {
                        return Err(Error::binder(format!(
                            "{} has type {} but LITERAL was expected.",
                            expr_name(other),
                            ast_expr_kind(other)
                        )));
                    }
                }
            }
            // The signature gate runs on the PRE-coercion types so its error
            // renders the original Actual line (C++ matchFunction order:
            // `list_to_string([1,2,3], 0)` shows `(INT64[],INT64)`).
            let pre_types: Vec<LogicalType> = bound.iter().map(|a| a.ty()).collect();
            koko_function::scalarfn::signature_gate(name, &pre_types)?;
            if matches!(function, BuiltinScalar::Greatest | BuiltinScalar::Least) {
                if let Some(common) = koko_common::types::common_numeric_type(pre_types.iter()) {
                    bound = bound
                        .into_iter()
                        .map(|argument| self.coerce_to(argument, &common))
                        .collect();
                }
            }
            // C++ binds the element argument of these list functions to the
            // list's child type: a non-castable element is the assignment
            // error (list_contains) or the bespoke bind error (append/prepend).
            if bound.len() == 2 {
                if let LogicalType::List(elem) | LogicalType::Array(elem, _) = bound[0].ty() {
                    // An empty list literal statically casts to any list type
                    // (LIST_APPEND([], '5') is fine — the result is STRING[]).
                    let empty_list =
                        matches!(&bound[0], BoundExpr::List { elems, .. } if elems.is_empty());
                    let ety = bound[1].ty();
                    let concrete =
                        !empty_list && *elem != LogicalType::Any && ety != LogicalType::Any;
                    if concrete && function == BuiltinScalar::ListContains {
                        // A pair with no comparison combine at all is the
                        // bespoke C++ error, child type first.
                        if !koko_function::comparison_comparable(&elem, &ety) {
                            return Err(Error::binder(format!(
                                "Cannot compare {elem} and {ety} in list_contains function."
                            )));
                        }
                        // Otherwise C++ unifies (child, element) on the
                        // comparison lattice and blames whichever side can't
                        // cast: list_contains(['1','2'], 2) blames the
                        // STRING[] list expecting INT64[];
                        // list_contains([1,2,3], '2') blames the '2'.
                        let common = koko_function::comparison_common_type(&elem, &ety)
                            .unwrap_or_else(|| (*elem).clone());
                        if !assignable(&elem, &common) {
                            return Err(Error::binder(format!(
                                "Expression {} has data type {} but expected {}. \
                                 Implicit cast is not supported.",
                                expr_name(&args[0]),
                                LogicalType::List(elem.clone()),
                                LogicalType::List(Box::new(common))
                            )));
                        }
                        if !assignable(&ety, &common) {
                            return Err(Error::binder(format!(
                                "Expression {} has data type {} but expected {}. \
                                 Implicit cast is not supported.",
                                expr_name(&args[1]),
                                ety.name(),
                                common.name()
                            )));
                        }
                    }
                    if concrete
                        && !assignable(&ety, &elem)
                        && matches!(
                            function,
                            BuiltinScalar::ListAppend | BuiltinScalar::ListPrepend
                        )
                    {
                        return Err(Error::binder(format!(
                            "Cannot bind {} with parameter type {} and {}.",
                            name.to_uppercase(),
                            bound[0].ty(),
                            ety
                        )));
                    }
                }
            }
            // An empty list literal adopts the OTHER list argument's element
            // type (C++ canCastStatically): LIST_CAT(['7','3'], []) binds as
            // STRING[] × STRING[] instead of a bind-time mismatch.
            if bound.len() == 2 {
                for i in 0..2 {
                    if let Some(child) = bound[1 - i].ty().list_child().cloned() {
                        if let BoundExpr::List { elems, ty } = &mut bound[i] {
                            if elems.is_empty() {
                                *ty = LogicalType::List(Box::new(child));
                            }
                        }
                    }
                }
            }
            bound = coerce_string_params(&lname, bound);
            let arg_types: Vec<LogicalType> = bound.iter().map(|a| a.ty()).collect();
            let ty = koko_function::scalar_func_result_type(function, name, &arg_types)?;
            // With `disable_map_key_check=false`, map() validates its keys at
            // eval (NULL keys reject) — routed via the internal checked name.
            let lname = if lname == "map" && !self.disable_map_key_check {
                "map_checked".to_string()
            } else {
                lname
            };
            return Ok(BoundExpr::Call {
                function,
                called_name: lname,
                args: bound,
                ty,
            });
        }
        // An unrecognized function name (not a built-in scalar/aggregate/lambda,
        // sequence fn, cast, or — after macro expansion — a macro).
        Err(unknown_function_error(name))
    }

    pub(super) fn any_data_expr(
        &mut self,
        properties: &[(String, ast::Expr)],
    ) -> Result<BoundExpr> {
        let mut fields = properties
            .iter()
            .map(|(name, expression)| Ok((name.clone(), self.bind_expr(expression)?)))
            .collect::<Result<Vec<_>>>()?;
        fields.reverse();
        let ty = LogicalType::Struct(
            fields
                .iter()
                .map(|(name, expression)| (name.clone(), expression.ty()))
                .collect(),
        );
        Ok(BoundExpr::Struct { fields, ty })
    }

    pub(super) fn inline_predicates(
        &mut self,
        var: VarId,
        properties: &[(String, ast::Expr)],
        predicates: &mut Vec<BoundExpr>,
    ) -> Result<()> {
        for (k, e) in properties {
            let info = &self.vars[var.0 as usize];
            let (lhs, prop_ty) = if let Some(prop) = info.property(k) {
                (
                    BoundExpr::Property {
                        var,
                        prop: prop.name.clone(),
                        ty: prop.ty.clone(),
                    },
                    prop.ty.clone(),
                )
            } else if self.is_any_var(var) {
                let data = info
                    .property("data")
                    .expect("ANY variables carry the hidden data column");
                (
                    BoundExpr::ValueProperty {
                        value: Box::new(BoundExpr::Property {
                            var,
                            prop: data.name.clone(),
                            ty: data.ty.clone(),
                        }),
                        prop: k.clone(),
                        ty: LogicalType::Any,
                    },
                    LogicalType::Any,
                )
            } else {
                return Err(Error::binder(format!(
                    "Cannot find property {k} for {}.",
                    info.name
                )));
            };
            let rhs = self.bind_expr(e)?;
            let rhs = self.coerce_to(rhs, &prop_ty);
            predicates.push(BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args: vec![lhs, rhs],
                ty: LogicalType::Bool,
            });
        }
        Ok(())
    }
}
