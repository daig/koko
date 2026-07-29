//! `koko-function` — the function library: scalar operators, aggregates, and
//! Cypher value comparison/ordering.
//!
//! Scalar implementations operate on [`Value`] arguments; the expression layer
//! applies them over typed vectors. Null propagation follows three-valued logic.

use koko_common::temporal;
use koko_common::{Error, IntKind, LogicalType, Result, Value, value_payload_bytes};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;

const MICROS_PER_DAY: i64 = 86_400_000_000;

pub mod catalog_data;
pub mod digest;
pub mod oracle_hash;
pub mod scalar;
pub mod scalarfn;
pub use scalar::{
    BuiltinDescriptor, BuiltinFunction, BuiltinScalar, CastTarget, CatalogTypeId, DigestAlgorithm,
    FunctionCatalogEntry, FunctionCatalogKind, OverloadDescriptor, RoundMode, resolve_builtin,
    resolve_builtin_scalar,
};
pub use scalarfn::{
    aggregate_signature_error, eval as eval_scalar_func,
    eval_with_context as eval_scalar_func_with_context,
    scalar_result_type as scalar_func_result_type,
};

mod aggregate;
mod cast;
mod compare;
mod operator;

pub use aggregate::{AggOp, AggState, agg_result_type};
pub use cast::{cast_value, parse_csv_cell};
pub use compare::{ValueKey, comparison_common_type, comparison_comparable, cypher_cmp, order_cmp};
pub use operator::{ScalarOp, eval_scalar, scalar_result_type};

#[cfg(test)]
mod tests {
    use super::*;

    /// Merging two partial accumulators (the parallel partitioned-aggregate combine)
    /// gives the same answer as feeding all values to one accumulator — and for
    /// `collect`, in the order the partials are merged.
    #[test]
    fn agg_merge_matches_single_accumulator() {
        // count(*): two morsels of 3 and 2 rows -> 5.
        let mut a = AggState::new(AggOp::CountStar, false);
        let mut b = AggState::new(AggOp::CountStar, false);
        for _ in 0..3 {
            a.update(&Value::Null);
        }
        for _ in 0..2 {
            b.update(&Value::Null);
        }
        a.merge(b);
        assert_eq!(a.finalize().unwrap(), Value::Int64(5));

        // integer sum: {1,2,3} + {4,5} == 15 (exact, associative).
        let mut a = AggState::new(AggOp::Sum, false);
        let mut b = AggState::new(AggOp::Sum, false);
        [1, 2, 3].iter().for_each(|&v| a.update(&Value::Int64(v)));
        [4, 5].iter().for_each(|&v| b.update(&Value::Int64(v)));
        a.merge(b);
        // SUM widens to INT128 (audit V3), so the merged result is IntX/I128.
        assert_eq!(
            a.finalize().unwrap(),
            Value::IntX {
                value: 15,
                kind: IntKind::I128
            }
        );

        // min / max take the combined extreme regardless of which partial held it.
        let mut a = AggState::new(AggOp::Min, false);
        let mut b = AggState::new(AggOp::Min, false);
        a.update(&Value::Int64(7));
        b.update(&Value::Int64(3));
        a.merge(b);
        assert_eq!(a.finalize().unwrap(), Value::Int64(3));

        // collect concatenates in merge order (morsel-index order at the call site).
        let mut a = AggState::new(AggOp::Collect, false);
        let mut b = AggState::new(AggOp::Collect, false);
        a.update(&Value::Int64(1));
        a.update(&Value::Int64(2));
        b.update(&Value::Int64(3));
        a.merge(b);
        assert_eq!(
            a.finalize().unwrap(),
            Value::List(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)])
        );
    }

    #[test]
    fn arithmetic_and_nulls() {
        assert_eq!(
            eval_scalar(ScalarOp::Add, &[Value::Int64(2), Value::Int64(3)]).unwrap(),
            Value::Int64(5)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Add, &[Value::Int64(2), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_scalar(ScalarOp::Mul, &[Value::Double(2.0), Value::Int64(3)]).unwrap(),
            Value::Double(6.0)
        );
        assert_eq!(
            scalar_result_type(
                ScalarOp::Mul,
                &[LogicalType::Decimal(4, 2), LogicalType::Int64]
            )
            .unwrap(),
            LogicalType::Decimal(9, 4)
        );
        assert_eq!(
            eval_scalar(
                ScalarOp::Mul,
                &[
                    Value::Decimal {
                        value: 123,
                        precision: 4,
                        scale: 2
                    },
                    Value::Int64(2)
                ]
            )
            .unwrap(),
            Value::Decimal {
                value: 24600,
                precision: 9,
                scale: 4
            }
        );
        assert!(eval_scalar(ScalarOp::Add, &[Value::Int64(i64::MAX), Value::Int64(1)]).is_err());
    }

    #[test]
    fn path_accessor_functions() {
        use koko_common::{InternalId, NodeValue, RecursiveRelValue, RelValue};
        let iid = |t: u64, o: u64| InternalId::new(koko_common::TableId(t), o);
        let rr = Value::RecursiveRel(Box::new(RecursiveRelValue {
            nodes: vec![NodeValue {
                id: iid(0, 0),
                label: "person".into(),
                props: vec![("fName".into(), Value::String("Alice".into()))],
            }],
            rels: vec![
                RelValue {
                    src: iid(0, 3),
                    dst: iid(0, 0),
                    id: iid(3, 9),
                    label: "knows".into(),
                    props: vec![],
                    src_node: None,
                    dst_node: None,
                },
                RelValue {
                    src: iid(0, 0),
                    dst: iid(0, 1),
                    id: iid(3, 0),
                    label: "knows".into(),
                    props: vec![],
                    src_node: None,
                    dst_node: None,
                },
            ],
            degenerate: false,
            cost: None,
            null_nodes: 0,
        }));
        // length = rel count.
        assert_eq!(
            eval_scalar_func("length", std::slice::from_ref(&rr)).unwrap(),
            Value::Int64(2)
        );
        // nodes(p) → LIST[NODE]; rels(p) → LIST[REL].
        let nodes = eval_scalar_func("nodes", std::slice::from_ref(&rr)).unwrap();
        assert_eq!(
            nodes.to_result_string(),
            "[{_ID: 0:0, _LABEL: person, fName: Alice}]"
        );
        let rels = eval_scalar_func("rels", std::slice::from_ref(&rr)).unwrap();
        assert!(matches!(rels, Value::List(ref v) if v.len() == 2));
        // properties(nodes(p), key): plain prop, _id, _label, and a missing prop.
        assert_eq!(
            eval_scalar_func(
                "properties",
                &[nodes.clone(), Value::String("fName".into())]
            )
            .unwrap()
            .to_result_string(),
            "[Alice]"
        );
        assert_eq!(
            eval_scalar_func("properties", &[rels.clone(), Value::String("_id".into())])
                .unwrap()
                .to_result_string(),
            "[3:9,3:0]"
        );
        assert_eq!(
            eval_scalar_func(
                "properties",
                &[nodes.clone(), Value::String("_Label".into())]
            )
            .unwrap()
            .to_result_string(),
            "[person]"
        );
        // A missing property renders empty (NULL).
        assert_eq!(
            eval_scalar_func("properties", &[nodes, Value::String("age".into())])
                .unwrap()
                .to_result_string(),
            "[]"
        );
    }

    #[test]
    fn size_rejects_graph_types() {
        use koko_common::TableId;
        // `size` accepts LIST/MAP/STRING but rejects NODE/REL/RECURSIVE_REL with
        // the C++ signature listing.
        assert!(
            scalar_func_result_type(
                BuiltinScalar::Size,
                "size",
                &[LogicalType::List(Box::new(LogicalType::Int64))]
            )
            .is_ok()
        );
        assert!(
            scalar_func_result_type(BuiltinScalar::Size, "size", &[LogicalType::String]).is_ok()
        );
        let err = scalar_func_result_type(
            BuiltinScalar::Size,
            "size",
            &[LogicalType::Node(TableId(0))],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Function SIZE did not receive correct arguments:")
        );
        assert!(err.to_string().contains("Actual:   (NODE)"));
        assert!(
            scalar_func_result_type(BuiltinScalar::Size, "size", &[LogicalType::RecursiveRel])
                .is_err()
        );
    }

    #[test]
    fn three_valued_logic() {
        assert_eq!(
            eval_scalar(ScalarOp::And, &[Value::Bool(false), Value::Null]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_scalar(ScalarOp::And, &[Value::Bool(true), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_scalar(ScalarOp::Or, &[Value::Bool(true), Value::Null]).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn comparisons() {
        assert_eq!(
            eval_scalar(ScalarOp::Gt, &[Value::Int64(5), Value::Int64(3)]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Lt, &[Value::Int64(5), Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            eval_scalar(ScalarOp::Eq, &[Value::Double(3.0), Value::Int64(3)]).unwrap(),
            Value::Bool(true)
        );
        let nan = Value::Double(f64::NAN);
        assert_eq!(
            eval_scalar(ScalarOp::Eq, &[nan.clone(), nan.clone()]).unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Ne, &[nan.clone(), nan.clone()]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_scalar(ScalarOp::Lt, &[nan, Value::Double(0.0)]).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn aggregates() {
        let mut s = AggState::new(AggOp::Sum, false);
        for v in [Value::Int64(10), Value::Null, Value::Int64(5)] {
            s.update(&v);
        }
        // SUM widens to INT128 (audit V3).
        assert_eq!(
            s.finalize().unwrap(),
            Value::IntX {
                value: 15,
                kind: IntKind::I128
            }
        );

        let mut s = AggState::new(AggOp::Sum, false);
        s.update(&Value::Null);
        assert_eq!(s.finalize().unwrap(), Value::Null);

        let mut s = AggState::new(AggOp::Avg, false);
        for v in [Value::Int64(35), Value::Int64(40)] {
            s.update(&v);
        }
        assert_eq!(s.finalize().unwrap(), Value::Double(37.5));

        let mut s = AggState::new(AggOp::CountStar, false);
        for v in [Value::Null, Value::Int64(1)] {
            s.update(&v);
        }
        assert_eq!(s.finalize().unwrap(), Value::Int64(2));

        let mut s = AggState::new(AggOp::Count, true);
        for v in [Value::Int64(1), Value::Int64(1), Value::Int64(2)] {
            s.update(&v);
        }
        assert_eq!(s.finalize().unwrap(), Value::Int64(2));
    }

    #[test]
    fn sum_widens_past_the_argument_width() {
        // C++ SUM(INT64) -> INT128 (audit V3): two i64::MAX values sum exactly,
        // where the old accumulator raised an invented overflow error.
        let mut s = AggState::new(AggOp::Sum, false);
        s.update(&Value::Int64(i64::MAX));
        s.update(&Value::Int64(i64::MAX));
        assert_eq!(
            s.finalize().unwrap(),
            Value::IntX {
                value: 2 * (i64::MAX as i128),
                kind: IntKind::I128
            }
        );

        // SUM(UINT*) -> UINT128: no silent i128 wrap (two 2^127-1 values).
        let big = (1u128 << 127) - 1;
        let mut s = AggState::new(AggOp::Sum, false);
        s.update(&Value::UInt128(big));
        s.update(&Value::UInt128(big));
        assert_eq!(s.finalize().unwrap(), Value::UInt128(big * 2));
    }

    #[test]
    fn group_key_agrees_with_equality() {
        // 1 and 1.0 compare equal, so they must key equal; -0.0 == 0.0 likewise.
        assert_eq!(
            ValueKey::from_value(&Value::Int64(1)),
            ValueKey::from_value(&Value::Double(1.0))
        );
        assert_eq!(
            ValueKey::from_value(&Value::Double(0.0)),
            ValueKey::from_value(&Value::Double(-0.0))
        );
        assert_ne!(
            ValueKey::from_value(&Value::Decimal {
                value: 10_000_000_000_000_000_001,
                precision: 38,
                scale: 0
            }),
            ValueKey::from_value(&Value::Decimal {
                value: 10_000_000_000_000_000_002,
                precision: 38,
                scale: 0
            })
        );
    }
    #[test]
    fn numeric_extrema_overload_rejects_other_families() {
        let rejected = [
            vec![LogicalType::String, LogicalType::String],
            vec![LogicalType::Bool, LogicalType::Bool],
            vec![
                LogicalType::List(Box::new(LogicalType::Int64)),
                LogicalType::List(Box::new(LogicalType::Int64)),
            ],
            vec![
                LogicalType::Map(Box::new(LogicalType::String), Box::new(LogicalType::Int64)),
                LogicalType::Map(Box::new(LogicalType::String), Box::new(LogicalType::Int64)),
            ],
            vec![
                LogicalType::Node(koko_common::TableId(0)),
                LogicalType::Node(koko_common::TableId(0)),
            ],
            vec![LogicalType::Int64, LogicalType::Date],
        ];
        for args in rejected {
            assert!(
                scalarfn::signature_gate("greatest", &args).is_err(),
                "unexpectedly accepted {args:?}"
            );
            assert!(
                scalarfn::signature_gate("least", &args).is_err(),
                "unexpectedly accepted {args:?}"
            );
        }
    }

    #[test]
    fn casts_struct_values_to_ordered_json() {
        let value = Value::Struct(vec![("answer".to_string(), Value::Int64(42))]);
        assert_eq!(
            cast_value(&value, &LogicalType::Json).unwrap(),
            Value::Json(koko_common::JsonValue::Object(vec![(
                "answer".to_string(),
                koko_common::JsonValue::Int(42),
            )]))
        );
    }
}
