use crate::{ColumnResolver, CompiledExpr, EvalState, aggregate::compile_collect};
use koko_common::{DataChunk, Error, LogicalType, Result, TableId, Value};
use koko_function::oracle_hash::RandomState;
use koko_ir::bound::{BoundExpr, VarId};
use std::collections::HashMap;

fn literal_rooted(expression: &BoundExpr) -> bool {
    match expression {
        BoundExpr::Literal(_) => true,
        BoundExpr::Cast { expr, .. } => literal_rooted(expr),
        BoundExpr::List { elems, .. } => elems.iter().all(literal_rooted),
        _ => false,
    }
}

pub(super) fn validate_static_cast(source: &LogicalType, target: &LogicalType) -> Result<()> {
    use LogicalType::*;

    fn resolved(logical_type: &LogicalType) -> LogicalType {
        match logical_type {
            Any => LogicalType::Int64,
            List(item) => List(Box::new(resolved(item))),
            Array(item, length) => Array(Box::new(resolved(item)), *length),
            Struct(fields) => Struct(
                fields
                    .iter()
                    .map(|(name, logical_type)| (name.clone(), resolved(logical_type)))
                    .collect(),
            ),
            Map(key, value) => Map(Box::new(resolved(key)), Box::new(resolved(value))),
            other => other.clone(),
        }
    }

    let full_error = || {
        Err(Error::conversion(format!(
            "Unsupported casting function from {} to {}.",
            resolved(source),
            resolved(target)
        )))
    };
    match (source, target) {
        (Any | String | Union(_), _) | (_, Any | String | Union(_)) => Ok(()),
        (Struct(source_fields), Struct(target_fields)) => {
            let names_match = source_fields.len() == target_fields.len()
                && source_fields.iter().zip(target_fields).all(
                    |((source_name, _), (target_name, _))| {
                        source_name.eq_ignore_ascii_case(target_name)
                    },
                );
            if !names_match {
                return full_error();
            }
            for ((_, source_type), (_, target_type)) in source_fields.iter().zip(target_fields) {
                validate_static_cast(source_type, target_type)?;
            }
            Ok(())
        }
        (Struct(_), List(_) | Array(_, _) | Map(_, _)) => {
            let kind = if matches!(target, Map(_, _)) {
                "MAP"
            } else {
                "LIST"
            };
            Err(Error::conversion(format!(
                "Unsupported casting function from STRUCT to {kind}."
            )))
        }
        (Struct(_), _) => full_error(),
        (Array(source_item, source_length), Array(target_item, target_length))
            if source_length != target_length =>
        {
            if source_item == target_item {
                Ok(())
            } else {
                full_error()
            }
        }
        (List(source_item) | Array(source_item, _), List(target_item) | Array(target_item, _)) => {
            validate_static_cast(source_item, target_item)
        }
        (List(_) | Array(_, _), Struct(_) | Map(_, _)) => {
            let kind = if matches!(target, Map(_, _)) {
                "MAP"
            } else {
                "STRUCT"
            };
            Err(Error::conversion(format!(
                "Unsupported casting function from LIST to {kind}."
            )))
        }
        (List(_) | Array(_, _), _) => full_error(),
        (Map(source_key, source_value), Map(target_key, target_value)) => {
            validate_static_cast(source_key, target_key)?;
            validate_static_cast(source_value, target_value)
        }
        _ => Ok(()),
    }
}

/// Compile an aggregate-free expression.
pub fn compile(bound: &BoundExpr, resolver: &dyn ColumnResolver) -> Result<CompiledExpr> {
    let mut aggregates = Vec::new();
    let expression = compile_collect(bound, resolver, &mut aggregates)?;
    if !aggregates.is_empty() {
        return Err(Error::Raw(
            "Cannot evaluate expression with type AGGREGATE_FUNCTION.".to_string(),
        ));
    }
    Ok(expression)
}

pub(super) fn is_literal_rooted(expression: &BoundExpr) -> bool {
    literal_rooted(expression)
}

/// Compile and evaluate an expression that cannot read row columns.
pub fn eval_constant(expression: &BoundExpr) -> Result<Value> {
    struct NoColumns;

    impl ColumnResolver for NoColumns {
        fn column(&self, _: VarId, _: Option<&str>) -> Result<usize> {
            Err(Error::binder(
                "a DEFAULT value must be constant (it cannot reference columns)".to_string(),
            ))
        }

        fn subquery_column(&self, _: usize) -> Result<usize> {
            Err(Error::binder(
                "a DEFAULT value cannot contain a subquery".to_string(),
            ))
        }

        fn sequence_column(&self, _: usize) -> Result<usize> {
            Err(Error::binder(
                "a DEFAULT value cannot contain a nested sequence call".to_string(),
            ))
        }

        fn table_names(&self) -> HashMap<TableId, String> {
            HashMap::new()
        }
    }

    let compiled = compile(expression, &NoColumns)?;
    let chunk = DataChunk::new(&[]);
    let mut state = EvalState::new();
    compiled.eval(&chunk, 0, &RandomState::default(), &mut state)
}
