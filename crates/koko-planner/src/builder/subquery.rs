use super::{DECORRELATE_MIN_PROBE_ROWS, PlanBuilder};
use koko_common::{LogicalType, Result};
use koko_ir::{
    bound::{BoundExpr, BoundSubquery, SubqueryKind},
    plan::{JoinKind, PlanOp},
};

/// Plan one lifted subquery onto `root`, decorrelating only when the cost gate allows it.
pub(super) fn plan_subquery(
    builder: &mut PlanBuilder<'_>,
    root: PlanOp,
    subquery: &BoundSubquery,
    result_column: usize,
    all: &[BoundSubquery],
) -> Result<PlanOp> {
    let inner_ids = subquery
        .where_predicate
        .as_ref()
        .map(|predicate| {
            collect_subquery_ids(predicate)
                .into_iter()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let correlated = (inner_ids.is_empty()
        && crate::cost::plan_card(&root, builder.stats) >= DECORRELATE_MIN_PROBE_ROWS)
        .then(|| builder.correlated_nodes(&subquery.match_, subquery.where_predicate.as_ref()))
        .flatten();
    if let Some(correlation) = correlated {
        let kind = JoinKind::Mark {
            mark_col: result_column,
            kind: subquery.kind,
        };
        return builder.build_decorrelated_join(root, &correlation, &subquery.match_, kind);
    }

    let mut pattern = builder.build_match(
        &subquery.match_,
        subquery.where_predicate.as_ref(),
        Some(PlanOp::InputScan),
    )?;
    for id in inner_ids {
        let inner = &all[id];
        let logical_type = match inner.kind {
            SubqueryKind::Exists => LogicalType::Bool,
            SubqueryKind::Count => LogicalType::Int64,
        };
        let column = builder.layout.add_subquery_column(id, logical_type);
        pattern = plan_subquery(builder, pattern, inner, column, all)?;
    }
    if let Some(predicate) = &subquery.where_predicate {
        pattern = PlanOp::Filter {
            input: Box::new(pattern),
            predicate: predicate.clone(),
        };
    }
    Ok(PlanOp::Subquery {
        input: Box::new(root),
        pattern: Box::new(pattern),
        result_col: result_column,
        kind: subquery.kind,
    })
}

/// Lifted subquery IDs referenced by an expression.
pub(super) fn collect_subquery_ids(expression: &BoundExpr) -> std::collections::HashSet<usize> {
    fn walk(expression: &BoundExpr, output: &mut std::collections::HashSet<usize>) {
        match expression {
            BoundExpr::Subquery { id, .. } => {
                output.insert(*id);
            }
            BoundExpr::Scalar { args, .. }
            | BoundExpr::Call { args, .. }
            | BoundExpr::List { elems: args, .. } => {
                args.iter().for_each(|argument| walk(argument, output));
            }
            BoundExpr::Cast { expr, .. } => walk(expr, output),
            BoundExpr::ValueProperty { value, .. } => walk(value, output),
            BoundExpr::Struct { fields, .. } => {
                fields.iter().for_each(|(_, value)| walk(value, output));
            }
            BoundExpr::ListLambda { list, body, .. } => {
                walk(list, output);
                walk(body, output);
            }
            BoundExpr::Aggregate {
                arg: Some(argument),
                ..
            } => walk(argument, output),
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                if let Some(operand) = operand {
                    walk(operand, output);
                }
                for (condition, result) in branches {
                    walk(condition, output);
                    walk(result, output);
                }
                if let Some(otherwise) = else_ {
                    walk(otherwise, output);
                }
            }
            _ => {}
        }
    }

    let mut output = std::collections::HashSet::new();
    walk(expression, &mut output);
    output
}
