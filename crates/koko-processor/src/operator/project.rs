use super::*;

pub(crate) struct MaterializeValuesState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) items: &'a [MaterializeItem],
}

// ---------------------------------------------------------------------------
// Result production (projection / aggregation)
// ---------------------------------------------------------------------------

pub(crate) fn produce_results<'a>(
    projection: &BoundProjection,
    root: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
) -> Result<ExecResult> {
    let column_names = projection_column_names(projection);
    let output = if projection.has_aggregates() {
        // Aggregation/grouping is a pipeline breaker: drain fully.
        aggregate(projection, root, ctx)?
    } else {
        // Without `ORDER BY`/`DISTINCT` a `LIMIT` lets us stop pulling once
        // `SKIP + LIMIT` rows are collected (the streaming early-termination win);
        // sort/dedup need every row, so they drain fully.
        let early = if projection.order_by.is_empty() && !projection.distinct {
            match fold_skip_limit(projection.limit.as_ref())? {
                Some(limit) => Some(
                    fold_skip_limit(projection.skip.as_ref())?.unwrap_or(0) as usize
                        + limit as usize,
                ),
                None => None,
            }
        } else {
            None
        };
        project(projection, root, ctx, early)?
    };
    finish_projection(output, column_names, projection, ctx.memory)
}

pub(crate) fn finish_projection(
    output: OutputBuffer,
    column_names: Vec<String>,
    projection: &BoundProjection,
    memory: &QueryMemory,
) -> Result<ExecResult> {
    let order_ascending = projection
        .order_by
        .iter()
        .map(|(_, ascending)| *ascending)
        .collect::<Vec<_>>();
    let skip = fold_skip_limit(projection.skip.as_ref())?.unwrap_or(0) as usize;
    let limit = fold_skip_limit(projection.limit.as_ref())?.map(|value| value as usize);
    output.finish(
        column_names,
        projection.distinct,
        &order_ascending,
        skip,
        limit,
        memory,
    )
}

/// Compile each projection item for the non-aggregate path.
pub(crate) enum ItemExec {
    Var(VarId),
    Scalar(CompiledExpr),
}

pub(crate) fn project<'a>(
    projection: &BoundProjection,
    root: &mut Exec<'a>,
    ctx: &OperatorContext<'a>,
    early_target: Option<usize>,
) -> Result<OutputBuffer> {
    let mut eval = EvalState::new();
    let layout = ctx.layout;
    let resolver = LayoutResolver(layout);
    let items: Vec<ItemExec> = projection
        .items
        .iter()
        .map(|item| match item {
            ProjItem::Var { var, .. } => Ok(ItemExec::Var(*var)),
            ProjItem::Scalar { expr, .. } => Ok(ItemExec::Scalar(compile(expr, &resolver)?)),
        })
        .collect::<Result<_>>()?;
    let deep_types = output_deep_types(projection);

    // ORDER BY keys: an output-column reference, an input expression, or a
    // post-projection expression over already-produced output columns.
    enum OrderExec {
        Output(usize),
        Expr(CompiledExpr),
        Post(BoundExpr),
    }
    let orders: Vec<(OrderExec, bool)> = projection
        .order_by
        .iter()
        .map(|(key, ascending)| {
            let execution = match key {
                OrderKey::Output(index) => OrderExec::Output(*index),
                OrderKey::Expr(expr) => OrderExec::Expr(compile(expr, &resolver)?),
                OrderKey::PostProjection(expr) => OrderExec::Post(expr.clone()),
            };
            Ok((execution, *ascending))
        })
        .collect::<Result<_>>()?;

    let mut output = OutputBuffer::new(
        projection_column_types(projection, layout),
        !orders.is_empty(),
    );
    'pull: while let Some(chunk) = root.next_chunk(ctx, &mut eval)? {
        for position in chunk.sel.iter() {
            let mut values = Vec::with_capacity(items.len());
            for item in &items {
                values.push(match item {
                    ItemExec::Var(var) => {
                        assemble_var(*var, layout, &chunk, position, EntityReader::from_ctx(ctx))?
                    }
                    ItemExec::Scalar(expr) => expr.eval(&chunk, position, ctx.random, &mut eval)?,
                });
            }
            let order_keys = orders
                .iter()
                .map(|(execution, _)| match execution {
                    OrderExec::Output(index) => Ok(values[*index].clone()),
                    OrderExec::Expr(expr) => expr.eval(&chunk, position, ctx.random, &mut eval),
                    OrderExec::Post(expr) => eval_output_expr(expr, &values, ctx.random),
                })
                .collect::<Result<_>>()?;
            deep_materialize_values(&mut values, &deep_types, ctx)?;
            ctx.memory.charge(output.push(values, order_keys))?;
            // Early-termination: stop once `SKIP + LIMIT` rows are collected.
            if early_target.is_some_and(|target| output.len() >= target) {
                break 'pull;
            }
        }
    }
    Ok(output)
}

/// Materialize a stored property into layout column type `ty`: a promoted
/// polymorphic column (heterogeneous multi-label property, e.g. INT64+DOUBLE →
/// DOUBLE) casts each table's raw value up; homogeneous columns pass through.
pub(crate) fn promote_prop(v: Value, ty: &LogicalType) -> Value {
    if v.is_null() || matches!(ty, LogicalType::Any) || v.logical_type() == *ty {
        return v;
    }
    let raw = v.clone();
    cast_value(&v, ty).unwrap_or(raw)
}

pub(crate) fn eval_output_expr(
    expr: &BoundExpr,
    values: &[Value],
    random: &RandomState,
) -> Result<Value> {
    match expr {
        BoundExpr::Literal(v) => Ok(v.clone()),
        BoundExpr::Parameter { name, .. } => Err(Error::binder(format!(
            "symbolic parameter ${name} cannot be evaluated during execution"
        ))),
        BoundExpr::Column { col, .. } => values
            .get(*col)
            .cloned()
            .ok_or_else(|| Error::runtime(format!("ORDER BY output column {col} is out of range"))),
        BoundExpr::ValueProperty { value, prop, .. } => {
            let base = eval_output_expr(value, values, random)?;
            eval_scalar_func_with_context(
                BuiltinScalar::StructExtract,
                "struct_extract",
                &[base, Value::String(prop.clone())],
                random,
            )
        }
        BoundExpr::Scalar { op, args, .. } => {
            let vals = args
                .iter()
                .map(|a| eval_output_expr(a, values, random))
                .collect::<Result<Vec<_>>>()?;
            eval_scalar(*op, &vals)
        }
        BoundExpr::Cast { expr, target } => {
            let v = eval_output_expr(expr, values, random)?;
            // C++ resolves STRUCT→STRUCT casts against the DECLARED source
            // type: a field-name mismatch reports the static shapes even when
            // a field's value is NULL (cast_value only sees value-level types).
            if let (LogicalType::Struct(sf), LogicalType::Struct(tf)) = (&expr.ty(), target) {
                let names_match = sf.len() == tf.len()
                    && sf
                        .iter()
                        .zip(tf)
                        .all(|((sn, _), (tn, _))| sn.eq_ignore_ascii_case(tn));
                if !names_match {
                    return Err(Error::conversion(format!(
                        "Unsupported casting function from {} to {}.",
                        expr.ty(),
                        target
                    )));
                }
            }
            cast_value(&v, target)
        }
        BoundExpr::Call { function, args, .. }
            if *function == BuiltinScalar::Typeof && args.len() == 1 =>
        {
            let _ = eval_output_expr(&args[0], values, random)?;
            Ok(Value::String(koko_function::scalarfn::typeof_type_name(
                &args[0].ty(),
            )))
        }
        // `union_value` is constructed here (not in the scalar evaluator): the active
        // member's *name* lives in the bound UNION type, not in the payload value, so
        // the tagged value is built by pairing the bound type with the evaluated arg.
        BoundExpr::Call {
            function, args, ty, ..
        } if *function == BuiltinScalar::UnionValue && args.len() == 1 => {
            let payload = eval_output_expr(&args[0], values, random)?;
            match ty {
                LogicalType::Union(variants) => Ok(Value::Union {
                    variants: variants.clone(),
                    tag: 0,
                    value: Box::new(payload),
                }),
                _ => Ok(payload),
            }
        }
        BoundExpr::Call {
            function,
            called_name,
            args,
            ..
        } => {
            let vals = args
                .iter()
                .map(|a| eval_output_expr(a, values, random))
                .collect::<Result<Vec<_>>>()?;
            eval_scalar_func_with_context(*function, called_name, &vals, random)
        }
        BoundExpr::Udf { function, args, .. } => {
            let values = args
                .iter()
                .map(|argument| eval_output_expr(argument, values, random))
                .collect::<Result<Vec<_>>>()?;
            function.invoke(&values)
        }
        BoundExpr::List { elems, .. } => elems
            .iter()
            .map(|e| eval_output_expr(e, values, random))
            .collect::<Result<Vec<_>>>()
            .map(Value::List),
        BoundExpr::Struct { fields, .. } => fields
            .iter()
            .map(|(k, e)| Ok((k.clone(), eval_output_expr(e, values, random)?)))
            .collect::<Result<Vec<_>>>()
            .map(Value::Struct),
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            let operand_val = operand
                .as_ref()
                .map(|o| eval_output_expr(o, values, random))
                .transpose()?;
            for (cond, res) in branches {
                let cv = eval_output_expr(cond, values, random)?;
                let matched = match &operand_val {
                    // C++ rule: a NULL when-value matches ANY operand; a NULL
                    // operand matches nothing else.
                    Some(ov) => match (ov.is_null(), cv.is_null()) {
                        (_, true) => true,
                        (true, false) => false,
                        (false, false) => cypher_cmp(ov, &cv) == Some(std::cmp::Ordering::Equal),
                    },
                    None => cv == Value::Bool(true),
                };
                if matched {
                    return eval_output_expr(res, values, random);
                }
            }
            else_
                .as_ref()
                .map(|e| eval_output_expr(e, values, random))
                .unwrap_or(Ok(Value::Null))
        }
        BoundExpr::Property { .. }
        | BoundExpr::NodeRef { .. }
        | BoundExpr::ScalarVar { .. }
        | BoundExpr::Aggregate { .. }
        | BoundExpr::ListLambda { .. }
        | BoundExpr::LambdaVar { .. }
        | BoundExpr::Subquery { .. }
        | BoundExpr::SequenceCall { .. } => Err(Error::binder(
            "ORDER BY expression cannot be evaluated over projected output".to_string(),
        )),
    }
}

pub(crate) fn order_keys_from_output(
    projection: &BoundProjection,
    values: &[Value],
    random: &RandomState,
) -> Result<Vec<Value>> {
    projection
        .order_by
        .iter()
        .map(|(k, _)| match k {
            OrderKey::Output(i) => Ok(values[*i].clone()),
            OrderKey::PostProjection(e) => eval_output_expr(e, values, random),
            OrderKey::Expr(_) => Err(Error::not_implemented(
                "ORDER BY an expression over aggregated results is not supported in this phase"
                    .to_string(),
            )),
        })
        .collect()
}
