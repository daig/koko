use koko_common::ExtendDir;
use koko_function::{AggOp, BuiltinScalar};
use koko_ir::bound::{BoundExpr, VarId};
use std::collections::HashSet;

/// Record variables whose full node or relationship value an expression consumes.
pub(super) fn collect_value_consumed_vars(
    expression: &BoundExpr,
    in_consumer: bool,
    output: &mut HashSet<VarId>,
) {
    match expression {
        BoundExpr::NodeRef { var, .. } => {
            if in_consumer {
                output.insert(*var);
            }
        }
        BoundExpr::Call { function, args, .. } => {
            let consumes = !matches!(
                function,
                BuiltinScalar::Id
                    | BuiltinScalar::Offset
                    | BuiltinScalar::Label
                    | BuiltinScalar::Labels
                    | BuiltinScalar::Typeof
                    | BuiltinScalar::Keys
            );
            args.iter()
                .for_each(|argument| collect_value_consumed_vars(argument, consumes, output));
        }
        BoundExpr::List { elems, .. } => elems
            .iter()
            .for_each(|element| collect_value_consumed_vars(element, true, output)),
        BoundExpr::Struct { fields, .. } => fields
            .iter()
            .for_each(|(_, value)| collect_value_consumed_vars(value, true, output)),
        BoundExpr::ValueProperty { value, .. } => {
            collect_value_consumed_vars(value, true, output);
        }
        BoundExpr::ListLambda { list, body, .. } => {
            collect_value_consumed_vars(list, true, output);
            collect_value_consumed_vars(body, true, output);
        }
        BoundExpr::Aggregate {
            op: AggOp::Collect,
            arg: Some(argument),
            ..
        } => collect_value_consumed_vars(argument, true, output),
        BoundExpr::Aggregate {
            arg: Some(argument),
            ..
        } => collect_value_consumed_vars(argument, in_consumer, output),
        BoundExpr::Scalar { args, .. } => args
            .iter()
            .for_each(|argument| collect_value_consumed_vars(argument, in_consumer, output)),
        BoundExpr::Cast { expr, .. } => collect_value_consumed_vars(expr, true, output),
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            if let Some(operand) = operand {
                collect_value_consumed_vars(operand, in_consumer, output);
            }
            for (condition, result) in branches {
                collect_value_consumed_vars(condition, in_consumer, output);
                collect_value_consumed_vars(result, in_consumer, output);
            }
            if let Some(otherwise) = else_ {
                collect_value_consumed_vars(otherwise, in_consumer, output);
            }
        }
        _ => {}
    }
}

pub(super) fn collect_expr_vars(expression: &BoundExpr, output: &mut HashSet<VarId>) {
    match expression {
        BoundExpr::Literal(_)
        | BoundExpr::Parameter { .. }
        | BoundExpr::Column { .. }
        | BoundExpr::LambdaVar { .. }
        | BoundExpr::Subquery { .. }
        | BoundExpr::SequenceCall { .. } => {}
        BoundExpr::Property { var, .. }
        | BoundExpr::NodeRef { var, .. }
        | BoundExpr::ScalarVar { var, .. } => {
            output.insert(*var);
        }
        BoundExpr::ValueProperty { value, .. } => collect_expr_vars(value, output),
        BoundExpr::Cast { expr, .. } => collect_expr_vars(expr, output),
        BoundExpr::Scalar { args, .. }
        | BoundExpr::Call { args, .. }
        | BoundExpr::Udf { args, .. }
        | BoundExpr::List { elems: args, .. } => {
            for argument in args {
                collect_expr_vars(argument, output);
            }
        }
        BoundExpr::Aggregate { arg, .. } => {
            if let Some(argument) = arg {
                collect_expr_vars(argument, output);
            }
        }
        BoundExpr::Struct { fields, .. } => {
            for (_, value) in fields {
                collect_expr_vars(value, output);
            }
        }
        BoundExpr::ListLambda { list, body, .. } => {
            collect_expr_vars(list, output);
            collect_expr_vars(body, output);
        }
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            if let Some(operand) = operand {
                collect_expr_vars(operand, output);
            }
            for (condition, result) in branches {
                collect_expr_vars(condition, output);
                collect_expr_vars(result, output);
            }
            if let Some(otherwise) = else_ {
                collect_expr_vars(otherwise, output);
            }
        }
    }
}

pub(super) fn fwd_or_both(directed: bool) -> ExtendDir {
    if directed {
        ExtendDir::Forward
    } else {
        ExtendDir::Both
    }
}

pub(super) fn bwd_or_both(directed: bool) -> ExtendDir {
    if directed {
        ExtendDir::Backward
    } else {
        ExtendDir::Both
    }
}

pub(super) fn collect_sequence_ids(expression: &BoundExpr) -> HashSet<usize> {
    fn walk(expression: &BoundExpr, output: &mut HashSet<usize>) {
        match expression {
            BoundExpr::SequenceCall { id, .. } => {
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

    let mut output = HashSet::new();
    walk(expression, &mut output);
    output
}
