use crate::{
    AccessorKind, ColumnResolver, CompiledExpr,
    compile::{compile, is_literal_rooted, validate_static_cast},
};
use koko_common::{Error, LogicalType, Result};
use koko_function::{AggOp, BuiltinScalar};
use koko_ir::bound::BoundExpr;
use std::collections::HashMap;

/// A lifted aggregate and its compiled argument (`None` for `count(*)`).
#[derive(Debug, Clone)]
pub struct AggSpec {
    pub op: AggOp,
    pub distinct: bool,
    pub arg: Option<CompiledExpr>,
}

/// Compile an expression while lifting aggregate calls into `aggregates`.
pub fn compile_collect(
    bound: &BoundExpr,
    resolver: &dyn ColumnResolver,
    aggregates: &mut Vec<AggSpec>,
) -> Result<CompiledExpr> {
    match bound {
        BoundExpr::Literal(value) => Ok(CompiledExpr::Literal(value.clone())),
        BoundExpr::Parameter { name, .. } => Err(Error::binder(format!(
            "symbolic parameter ${name} cannot be compiled for execution"
        ))),
        BoundExpr::Column { col, .. } => Ok(CompiledExpr::Column(*col)),
        BoundExpr::Property { var, prop, .. } => {
            Ok(CompiledExpr::Column(resolver.column(*var, Some(prop))?))
        }
        BoundExpr::NodeRef { var, .. } => Ok(CompiledExpr::Column(
            resolver
                .value_column(*var)
                .map_or_else(|| resolver.column(*var, None), Ok)?,
        )),
        BoundExpr::ValueProperty { value, prop, .. } => Ok(CompiledExpr::ValueProperty {
            value: Box::new(compile_collect(value, resolver, aggregates)?),
            prop: prop.clone(),
        }),
        BoundExpr::ScalarVar { var, .. } => Ok(CompiledExpr::Column(resolver.column(*var, None)?)),
        BoundExpr::Subquery { id, .. } => Ok(CompiledExpr::Column(resolver.subquery_column(*id)?)),
        BoundExpr::SequenceCall { id, .. } => {
            Ok(CompiledExpr::Column(resolver.sequence_column(*id)?))
        }
        BoundExpr::Scalar { op, args, .. } => {
            let args = args
                .iter()
                .map(|argument| compile_collect(argument, resolver, aggregates))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Scalar { op: *op, args })
        }
        BoundExpr::Aggregate {
            op, distinct, arg, ..
        } => {
            let arg = arg
                .as_ref()
                .map(|argument| compile(argument, resolver))
                .transpose()?;
            let index = aggregates.len();
            aggregates.push(AggSpec {
                op: *op,
                distinct: *distinct,
                arg,
            });
            Ok(CompiledExpr::Agg(index))
        }
        BoundExpr::Cast { expr, target } => {
            let mut union_tag = None;
            if let LogicalType::Union(fields) = target {
                let source_type = expr.ty();
                if !matches!(
                    source_type,
                    LogicalType::Any | LogicalType::String | LogicalType::Union(_)
                ) {
                    union_tag = Some(
                        koko_common::types::union_min_cost_tag(&source_type, fields).ok_or_else(
                            || {
                                Error::conversion(format!(
                                    "Cannot cast from {source_type} to {target}, target type has no compatible field."
                                ))
                            },
                        )?,
                    );
                }
            }
            validate_static_cast(&expr.ty(), target)?;
            if let (
                LogicalType::Array(source, source_len),
                LogicalType::Array(target_item, target_len),
            ) = (&expr.ty(), target)
                && source_len != target_len
                && source == target_item
                && is_literal_rooted(expr)
            {
                return Err(Error::binder(format!(
                    "Cannot change literal expression data type from {} to {}.",
                    expr.ty(),
                    target
                )));
            }
            Ok(CompiledExpr::Cast {
                expr: Box::new(compile_collect(expr, resolver, aggregates)?),
                target: target.clone(),
                union_tag,
            })
        }
        BoundExpr::Call {
            function,
            called_name,
            args,
            ty,
        } => {
            if let (Some(kind), [argument]) =
                (AccessorKind::from_function(*function), args.as_slice())
            {
                let names = match kind {
                    AccessorKind::Label => resolver.table_names(),
                    AccessorKind::Id | AccessorKind::Offset => HashMap::new(),
                };
                return Ok(CompiledExpr::Accessor {
                    kind,
                    arg: Box::new(compile_collect(argument, resolver, aggregates)?),
                    names,
                });
            }
            if *function == BuiltinScalar::Typeof && args.len() == 1 {
                let ty = koko_function::scalarfn::typeof_type_name(&args[0].ty());
                let arg = Box::new(compile_collect(&args[0], resolver, aggregates)?);
                return Ok(CompiledExpr::TypeOf { ty, arg });
            }
            if *function == BuiltinScalar::UnionValue
                && args.len() == 1
                && let LogicalType::Union(variants) = ty
            {
                let arg = Box::new(compile_collect(&args[0], resolver, aggregates)?);
                return Ok(CompiledExpr::UnionValue {
                    variants: variants.clone(),
                    arg,
                });
            }
            let args = args
                .iter()
                .map(|argument| compile_collect(argument, resolver, aggregates))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Call {
                function: *function,
                called_name: called_name.clone(),
                args,
            })
        }
        BoundExpr::Udf { function, args, .. } => {
            let args = args
                .iter()
                .map(|argument| compile_collect(argument, resolver, aggregates))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Udf {
                function: std::sync::Arc::clone(function),
                args,
            })
        }
        BoundExpr::List { elems, .. } => {
            let items = elems
                .iter()
                .map(|element| compile_collect(element, resolver, aggregates))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::List(items))
        }
        BoundExpr::Struct { fields, .. } => {
            let fields = fields
                .iter()
                .map(|(name, expression)| {
                    Ok((
                        name.clone(),
                        compile_collect(expression, resolver, aggregates)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Struct(fields))
        }
        BoundExpr::LambdaVar { id, .. } => Ok(CompiledExpr::LambdaVar(*id)),
        BoundExpr::ListLambda {
            kind,
            list,
            params,
            body,
            ..
        } => Ok(CompiledExpr::ListLambda {
            kind: *kind,
            list: Box::new(compile_collect(list, resolver, aggregates)?),
            params: params.clone(),
            body: Box::new(compile_collect(body, resolver, aggregates)?),
        }),
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            let operand = operand
                .as_ref()
                .map(|operand| compile_collect(operand, resolver, aggregates))
                .transpose()?
                .map(Box::new);
            let branches = branches
                .iter()
                .map(|(condition, result)| {
                    Ok((
                        compile_collect(condition, resolver, aggregates)?,
                        compile_collect(result, resolver, aggregates)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let else_ = else_
                .as_ref()
                .map(|otherwise| compile_collect(otherwise, resolver, aggregates))
                .transpose()?
                .map(Box::new);
            Ok(CompiledExpr::Case {
                operand,
                branches,
                else_,
            })
        }
    }
}
