use crate::{AccessorKind, CompiledExpr};
use koko_common::{DataChunk, Error, LogicalType, Result, Value};
use koko_function::{
    ScalarOp, cast_value, eval_scalar, eval_scalar_func_with_context, oracle_hash::RandomState,
};
use koko_ir::bound::LambdaVarId;

/// Reusable, pipeline-local expression evaluation state.
#[derive(Debug, Default)]
pub struct EvalState {
    bindings: Vec<(LambdaVarId, Value)>,
}

impl EvalState {
    pub fn new() -> Self {
        Self::default()
    }

    fn lambda(&self, id: LambdaVarId) -> Value {
        self.bindings
            .iter()
            .rev()
            .find(|(candidate, _)| *candidate == id)
            .map_or(Value::Null, |(_, value)| value.clone())
    }

    /// Evaluate with temporary lambda bindings, restoring the stack on every ordinary exit.
    pub fn with_lambda_bindings<T>(
        &mut self,
        bindings: &[(LambdaVarId, Value)],
        evaluate: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let previous_len = self.bindings.len();
        self.bindings.extend(bindings.iter().cloned());
        let result = evaluate(self);
        self.bindings.truncate(previous_len);
        result
    }
}

impl CompiledExpr {
    /// Evaluate against a chunk row, with no aggregate context.
    pub fn eval(
        &self,
        chunk: &DataChunk,
        pos: usize,
        random: &RandomState,
        state: &mut EvalState,
    ) -> Result<Value> {
        self.eval_with_aggs(chunk, pos, &[], random, state)
    }

    /// Evaluate a filter predicate, specializing internal-ID equality and inequality.
    pub fn eval_predicate(
        &self,
        chunk: &DataChunk,
        pos: usize,
        random: &RandomState,
        state: &mut EvalState,
    ) -> Result<bool> {
        let internal_id_columns = match self {
            CompiledExpr::Scalar { op, args } if matches!(op, ScalarOp::Eq | ScalarOp::Ne) => {
                match args.as_slice() {
                    [left, right] => Self::internal_id_column(left)
                        .zip(Self::internal_id_column(right))
                        .map(|(left, right)| (*op, left, right)),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some((op, left_col, right_col)) = internal_id_columns {
            return Ok(
                match (
                    chunk.columns[left_col].get_value(pos),
                    chunk.columns[right_col].get_value(pos),
                ) {
                    (Value::InternalId(left), Value::InternalId(right)) => {
                        if op == ScalarOp::Eq {
                            left == right
                        } else {
                            left != right
                        }
                    }
                    (Value::Null, _) | (_, Value::Null) => false,
                    _ => self.eval(chunk, pos, random, state)?.as_bool() == Some(true),
                },
            );
        }
        Ok(self.eval(chunk, pos, random, state)?.as_bool() == Some(true))
    }

    /// Evaluate with extra lambda-parameter bindings in scope.
    pub fn eval_with_bindings(
        &self,
        chunk: &DataChunk,
        pos: usize,
        bindings: &[(LambdaVarId, Value)],
        random: &RandomState,
        state: &mut EvalState,
    ) -> Result<Value> {
        state.with_lambda_bindings(bindings, |state| {
            self.eval_with_aggs(chunk, pos, &[], random, state)
        })
    }

    /// Evaluate against a chunk row, reading finalized aggregates from `aggregates`.
    pub fn eval_with_aggs(
        &self,
        chunk: &DataChunk,
        pos: usize,
        aggregates: &[Value],
        random: &RandomState,
        state: &mut EvalState,
    ) -> Result<Value> {
        match self {
            CompiledExpr::Literal(value) => Ok(value.clone()),
            CompiledExpr::Column(column) => Ok(chunk.columns[*column].get_value(pos)),
            CompiledExpr::Scalar { op, args } => match args.as_slice() {
                [] => eval_scalar(*op, &[]),
                [argument] => {
                    let value = argument.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                    eval_scalar(*op, std::slice::from_ref(&value))
                }
                [left, right] => {
                    let values = [
                        left.eval_with_aggs(chunk, pos, aggregates, random, state)?,
                        right.eval_with_aggs(chunk, pos, aggregates, random, state)?,
                    ];
                    eval_scalar(*op, &values)
                }
                [first, second, third] => {
                    let values = [
                        first.eval_with_aggs(chunk, pos, aggregates, random, state)?,
                        second.eval_with_aggs(chunk, pos, aggregates, random, state)?,
                        third.eval_with_aggs(chunk, pos, aggregates, random, state)?,
                    ];
                    eval_scalar(*op, &values)
                }
                _ => {
                    let mut values = Vec::with_capacity(args.len());
                    for argument in args {
                        values
                            .push(argument.eval_with_aggs(chunk, pos, aggregates, random, state)?);
                    }
                    eval_scalar(*op, &values)
                }
            },
            CompiledExpr::Agg(index) => Ok(aggregates[*index].clone()),
            CompiledExpr::Cast {
                expr,
                target,
                union_tag,
            } => {
                let value = expr.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                if let (Some(tag), LogicalType::Union(fields)) = (union_tag, target) {
                    if value.is_null() {
                        return Ok(Value::Null);
                    }
                    let inner = cast_value(&value, &fields[*tag].1)?;
                    return Ok(Value::Union {
                        variants: fields.clone(),
                        tag: *tag,
                        value: Box::new(inner),
                    });
                }
                cast_value(&value, target)
            }
            CompiledExpr::TypeOf { ty, arg } => {
                let _ = arg.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                Ok(Value::String(ty.clone()))
            }
            CompiledExpr::UnionValue { variants, arg } => {
                let payload = arg.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                Ok(Value::Union {
                    variants: variants.clone(),
                    tag: 0,
                    value: Box::new(payload),
                })
            }
            CompiledExpr::Call {
                function,
                called_name,
                args,
            } => {
                let mut values = Vec::with_capacity(args.len());
                for argument in args {
                    values.push(argument.eval_with_aggs(chunk, pos, aggregates, random, state)?);
                }
                eval_scalar_func_with_context(*function, called_name, &values, random)
            }
            CompiledExpr::Udf { function, args } => {
                let mut values = Vec::with_capacity(args.len());
                for argument in args {
                    values.push(argument.eval_with_aggs(chunk, pos, aggregates, random, state)?);
                }
                function.invoke(&values)
            }
            CompiledExpr::Accessor { kind, arg, names } => {
                let value = arg.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                let id = match &value {
                    Value::Null => return Ok(Value::Null),
                    Value::InternalId(id) => *id,
                    Value::Node(node) => node.id,
                    Value::Rel(relationship) => relationship.id,
                    other => {
                        return Err(Error::runtime(format!(
                            "a node/rel accessor expects a node or relationship, got {}",
                            other.logical_type()
                        )));
                    }
                };
                Ok(match kind {
                    AccessorKind::Id => Value::InternalId(id),
                    AccessorKind::Offset => Value::Int64(id.offset.0 as i64),
                    AccessorKind::Label => names
                        .get(&id.table_id)
                        .map_or(Value::Null, |name| Value::String(name.clone())),
                })
            }
            CompiledExpr::ValueProperty { value, prop } => {
                let value = value.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                let find = |properties: &[(String, Value)]| {
                    properties
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(prop))
                        .map_or(Value::Null, |(_, value)| value.clone())
                };
                Ok(match &value {
                    Value::Node(node) => find(&node.props),
                    Value::Rel(relationship) => find(&relationship.props),
                    Value::Json(koko_common::JsonValue::Object(fields)) => fields
                        .iter()
                        .find(|(name, _)| name == prop)
                        .map_or(Value::Null, |(_, value)| value.to_value()),
                    _ => Value::Null,
                })
            }
            CompiledExpr::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(item.eval_with_aggs(chunk, pos, aggregates, random, state)?);
                }
                Ok(Value::List(values))
            }
            CompiledExpr::Struct(fields) => {
                let mut values = Vec::with_capacity(fields.len());
                for (name, expression) in fields {
                    values.push((
                        name.clone(),
                        expression.eval_with_aggs(chunk, pos, aggregates, random, state)?,
                    ));
                }
                Ok(Value::Struct(values))
            }
            CompiledExpr::LambdaVar(id) => Ok(state.lambda(*id)),
            CompiledExpr::ListLambda {
                kind,
                list,
                params,
                body,
            } => {
                let list = list.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                if list.is_null() {
                    return Ok(Value::Null);
                }
                let items = match list {
                    Value::List(items) => items,
                    other => {
                        return Err(Error::runtime(format!(
                            "list lambda expects a LIST, got {}",
                            other.logical_type()
                        )));
                    }
                };
                match kind {
                    koko_ir::bound::LambdaKind::Transform => {
                        let mut output = Vec::with_capacity(items.len());
                        for item in items {
                            let bindings = [(params[0], item)];
                            output.push(state.with_lambda_bindings(&bindings, |state| {
                                body.eval_with_aggs(chunk, pos, aggregates, random, state)
                            })?);
                        }
                        Ok(Value::List(output))
                    }
                    koko_ir::bound::LambdaKind::Filter => {
                        let mut output = Vec::new();
                        for item in items {
                            let bindings = [(params[0], item.clone())];
                            if state
                                .with_lambda_bindings(&bindings, |state| {
                                    body.eval_with_aggs(chunk, pos, aggregates, random, state)
                                })?
                                .as_bool()
                                == Some(true)
                            {
                                output.push(item);
                            }
                        }
                        Ok(Value::List(output))
                    }
                    koko_ir::bound::LambdaKind::Reduce => {
                        let mut items = items.into_iter();
                        let Some(mut accumulator) = items.next() else {
                            return Err(Error::runtime(
                                "Cannot execute list_reduce on an empty list.".to_string(),
                            ));
                        };
                        for item in items {
                            let bindings = [(params[0], accumulator), (params[1], item)];
                            accumulator = state.with_lambda_bindings(&bindings, |state| {
                                body.eval_with_aggs(chunk, pos, aggregates, random, state)
                            })?;
                        }
                        Ok(accumulator)
                    }
                }
            }
            CompiledExpr::Case {
                operand,
                branches,
                else_,
            } => {
                let operand_value = match operand {
                    Some(operand) => {
                        Some(operand.eval_with_aggs(chunk, pos, aggregates, random, state)?)
                    }
                    None => None,
                };
                for (condition, result) in branches {
                    let condition_value =
                        condition.eval_with_aggs(chunk, pos, aggregates, random, state)?;
                    let matched = match &operand_value {
                        Some(operand) => match (operand.is_null(), condition_value.is_null()) {
                            (_, true) => true,
                            (true, false) => false,
                            (false, false) => {
                                koko_function::cypher_cmp(operand, &condition_value)
                                    == Some(std::cmp::Ordering::Equal)
                            }
                        },
                        None => condition_value.as_bool() == Some(true),
                    };
                    if matched {
                        return result.eval_with_aggs(chunk, pos, aggregates, random, state);
                    }
                }
                match else_ {
                    Some(otherwise) => {
                        otherwise.eval_with_aggs(chunk, pos, aggregates, random, state)
                    }
                    None => Ok(Value::Null),
                }
            }
        }
    }

    fn internal_id_column(expression: &CompiledExpr) -> Option<usize> {
        match expression {
            CompiledExpr::Column(column) => Some(*column),
            CompiledExpr::Accessor {
                kind: AccessorKind::Id,
                arg,
                ..
            } => match arg.as_ref() {
                CompiledExpr::Column(column) => Some(*column),
                _ => None,
            },
            _ => None,
        }
    }
}
