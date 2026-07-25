//! `koko-expr` — compile [`BoundExpr`]s into a column-indexed evaluator over
//! [`DataChunk`]s.
//!
//! Binding resolves *names and types*; compilation resolves a property/variable
//! reference to a concrete **column index** in the runtime row layout (via a
//! [`ColumnResolver`]). Aggregates are lifted out into [`AggSpec`]s and replaced
//! by [`CompiledExpr::Agg`] placeholders, so the aggregate operator computes
//! them and the surrounding scalar expression reads the finalized values.
//!
//! Evaluation is per-row over [`Value`]s (P0; vectorization is a P3 concern).

use koko_binder::{BoundExpr, LambdaKind, VarId};
use koko_common::{DataChunk, Error, LogicalType, Result, TableId, Value};
use koko_function::{
    AggOp, ScalarOp, cast_value, eval_scalar, eval_scalar_func_with_context,
    oracle_hash::RandomState,
};
use std::cell::RefCell;
use std::collections::HashMap;

// Lambda parameter bindings, as a stack so nested lambdas compose. Single-thread
// per query in P1 (vectorized parallelism is a P3 concern); each thread gets its
// own stack, so this is correct even if execution is later parallelized by row.
thread_local! {
    static LAMBDA_STACK: RefCell<Vec<(String, Value)>> = const { RefCell::new(Vec::new()) };
}

fn lambda_get(name: &str) -> Value {
    LAMBDA_STACK.with(|s| {
        s.borrow()
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Null)
    })
}

/// Resolves a `(variable, optional property)` reference to a column index in the
/// current row layout. `None` property ⇒ the variable's internal-id column.
pub trait ColumnResolver {
    fn column(&self, var: VarId, prop: Option<&str>) -> Result<usize>;
    /// The column holding `var`'s *materialized* node/rel value, when one was
    /// planned (audit V12 seam); `None` keeps the bare-internal-id column.
    fn value_column(&self, _var: VarId) -> Option<usize> {
        None
    }
    /// Resolve a lifted subquery (by id) to the column holding its per-row result.
    fn subquery_column(&self, id: usize) -> Result<usize>;
    /// Resolve a lifted `nextval`/`currval` (by id) to its per-row result column.
    fn sequence_column(&self, id: usize) -> Result<usize>;
    /// Every table id → its name, for compiling `label()`/`labels()`.
    fn table_names(&self) -> HashMap<TableId, String>;
}

/// A node/rel accessor function (`id`/`offset`/`label`/`labels`). These take a
/// node or relationship value (carried as its internal id) and read identity /
/// schema off it; `label`/`labels` resolve the table id to a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessorKind {
    Id,
    Offset,
    Label,
}

impl AccessorKind {
    fn from_name(name: &str) -> Option<AccessorKind> {
        match name {
            "id" => Some(AccessorKind::Id),
            "offset" => Some(AccessorKind::Offset),
            // `labels` is an alias of `label` (scalar STRING) in the C++ oracle —
            // audit V8; the ledgered parity decision is docs/DIVERGENCES.md.
            "label" | "labels" => Some(AccessorKind::Label),
            _ => None,
        }
    }
}

/// A compiled scalar expression: leaves are literals, column reads, or
/// finalized-aggregate reads.
#[derive(Debug, Clone)]
pub enum CompiledExpr {
    Literal(Value),
    Column(usize),
    Scalar {
        op: ScalarOp,
        args: Vec<CompiledExpr>,
    },
    /// Read the finalized value of aggregate `#i` for the current group.
    Agg(usize),
    Cast {
        expr: Box<CompiledExpr>,
        target: LogicalType,
        /// For a UNION target: the member selected at compile time from the
        /// *static* source type (C++ `bindCastToUnionFunction` picks the
        /// min-cast-cost field at bind time — the static type matters for
        /// sources whose runtime value erases it, e.g. TIMESTAMP flavors).
        /// `None` resolves dynamically in `cast_value` (STRING parses, UNION
        /// remaps, ANY inspects the runtime value).
        union_tag: Option<usize>,
    },
    TypeOf {
        ty: String,
        arg: Box<CompiledExpr>,
    },
    /// `union_value(tag := v)` — construct a single-member tagged UNION value. The
    /// member name/type (`variants`) is captured from the bound type at compile time
    /// (it is not present in the payload value); the payload is `arg`.
    UnionValue {
        variants: Vec<(String, LogicalType)>,
        arg: Box<CompiledExpr>,
    },
    Call {
        name: String,
        args: Vec<CompiledExpr>,
    },
    /// A connection-local native Rust callback retained by the compiled plan.
    Udf {
        function: std::sync::Arc<koko_common::ScalarUdf>,
        args: Vec<CompiledExpr>,
    },
    /// A node/rel accessor. `names` (table id → name) is embedded for
    /// `label`/`labels` since the evaluator has no catalog; empty otherwise.
    Accessor {
        kind: AccessorKind,
        arg: Box<CompiledExpr>,
        names: HashMap<TableId, String>,
    },
    /// Read property `prop` off a node/rel *value* (read by name at runtime).
    ValueProperty {
        value: Box<CompiledExpr>,
        prop: String,
    },
    List(Vec<CompiledExpr>),
    /// A struct literal `{field: expr, …}`.
    Struct(Vec<(String, CompiledExpr)>),
    /// A higher-order list operation; `body` reads `params` via
    /// [`CompiledExpr::LambdaVar`].
    ListLambda {
        kind: LambdaKind,
        list: Box<CompiledExpr>,
        params: Vec<String>,
        body: Box<CompiledExpr>,
    },
    /// A reference to a lambda parameter (read from the lambda stack at eval).
    LambdaVar(String),
    /// `CASE`: the first matching branch yields its result; otherwise `else_` (or
    /// `NULL`). With `operand` set (simple CASE) a branch matches when its
    /// condition is null-safe-equal to the operand; without (searched CASE) when
    /// its condition is `true`. Evaluated in order, short-circuiting.
    Case {
        operand: Option<Box<CompiledExpr>>,
        branches: Vec<(CompiledExpr, CompiledExpr)>,
        else_: Option<Box<CompiledExpr>>,
    },
}

/// A lifted aggregate: its operator, distinctness, and compiled argument
/// (`None` for `count(*)`).
#[derive(Debug, Clone)]
pub struct AggSpec {
    pub op: AggOp,
    pub distinct: bool,
    pub arg: Option<CompiledExpr>,
}

impl CompiledExpr {
    /// Evaluate against a chunk row, with no aggregate context.
    pub fn eval(&self, chunk: &DataChunk, pos: usize, random: &RandomState) -> Result<Value> {
        self.eval_with_aggs(chunk, pos, &[], random)
    }

    /// Evaluate a filter predicate, specializing the ubiquitous internal-id
    /// equality/inequality shape so graph-pattern filters do not allocate argument
    /// vectors or dispatch through the dynamic scalar comparison machinery per row.
    pub fn eval_predicate(
        &self,
        chunk: &DataChunk,
        pos: usize,
        random: &RandomState,
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
                    // Cypher NULL comparisons yield NULL, which a filter rejects.
                    (Value::Null, _) | (_, Value::Null) => false,
                    _ => {
                        return Ok(self.eval(chunk, pos, random)?.as_bool() == Some(true));
                    }
                },
            );
        }
        Ok(self.eval(chunk, pos, random)?.as_bool() == Some(true))
    }

    /// Evaluate with extra lambda-parameter bindings in scope (e.g. a recursive
    /// pattern's current relationship/node values, referenced as `LambdaVar`s).
    /// The bindings are pushed onto the lambda stack for the duration of the call.
    pub fn eval_with_bindings(
        &self,
        chunk: &DataChunk,
        pos: usize,
        binds: &[(String, Value)],
        random: &RandomState,
    ) -> Result<Value> {
        let n = binds.len();
        LAMBDA_STACK.with(|s| s.borrow_mut().extend(binds.iter().cloned()));
        let r = self.eval_with_aggs(chunk, pos, &[], random);
        LAMBDA_STACK.with(|s| {
            let mut b = s.borrow_mut();
            let len = b.len();
            b.truncate(len - n);
        });
        r
    }

    /// Evaluate against a chunk row, reading finalized aggregates from `aggs`.
    pub fn eval_with_aggs(
        &self,
        chunk: &DataChunk,
        pos: usize,
        aggs: &[Value],
        random: &RandomState,
    ) -> Result<Value> {
        match self {
            CompiledExpr::Literal(v) => Ok(v.clone()),
            CompiledExpr::Column(i) => Ok(chunk.columns[*i].get_value(pos)),
            CompiledExpr::Scalar { op, args } => match args.as_slice() {
                [] => eval_scalar(*op, &[]),
                [arg] => {
                    let value = arg.eval_with_aggs(chunk, pos, aggs, random)?;
                    eval_scalar(*op, std::slice::from_ref(&value))
                }
                [left, right] => {
                    let values = [
                        left.eval_with_aggs(chunk, pos, aggs, random)?,
                        right.eval_with_aggs(chunk, pos, aggs, random)?,
                    ];
                    eval_scalar(*op, &values)
                }
                [first, second, third] => {
                    let values = [
                        first.eval_with_aggs(chunk, pos, aggs, random)?,
                        second.eval_with_aggs(chunk, pos, aggs, random)?,
                        third.eval_with_aggs(chunk, pos, aggs, random)?,
                    ];
                    eval_scalar(*op, &values)
                }
                _ => {
                    let values = args
                        .iter()
                        .map(|arg| arg.eval_with_aggs(chunk, pos, aggs, random))
                        .collect::<Result<Vec<_>>>()?;
                    eval_scalar(*op, &values)
                }
            },
            CompiledExpr::Agg(i) => Ok(aggs[*i].clone()),
            CompiledExpr::Cast {
                expr,
                target,
                union_tag,
            } => {
                let v = expr.eval_with_aggs(chunk, pos, aggs, random)?;
                if let (Some(tag), LogicalType::Union(fields)) = (union_tag, target) {
                    // A NULL casts to a NULL union (no tag) — unlike
                    // `union_value(a := NULL)`, which stays tagged.
                    if v.is_null() {
                        return Ok(Value::Null);
                    }
                    let inner = cast_value(&v, &fields[*tag].1)?;
                    return Ok(Value::Union {
                        variants: fields.clone(),
                        tag: *tag,
                        value: Box::new(inner),
                    });
                }
                cast_value(&v, target)
            }
            CompiledExpr::TypeOf { ty, arg } => {
                let _ = arg.eval_with_aggs(chunk, pos, aggs, random)?;
                Ok(Value::String(ty.clone()))
            }
            CompiledExpr::UnionValue { variants, arg } => {
                let payload = arg.eval_with_aggs(chunk, pos, aggs, random)?;
                Ok(Value::Union {
                    variants: variants.clone(),
                    tag: 0,
                    value: Box::new(payload),
                })
            }
            CompiledExpr::Call { name, args } => {
                let vals = args
                    .iter()
                    .map(|a| a.eval_with_aggs(chunk, pos, aggs, random))
                    .collect::<Result<Vec<_>>>()?;
                eval_scalar_func_with_context(name, &vals, random)
            }
            CompiledExpr::Udf { function, args } => {
                let values = args
                    .iter()
                    .map(|argument| argument.eval_with_aggs(chunk, pos, aggs, random))
                    .collect::<Result<Vec<_>>>()?;
                function.invoke(&values)
            }
            CompiledExpr::Accessor { kind, arg, names } => {
                let v = arg.eval_with_aggs(chunk, pos, aggs, random)?;
                // A node/rel is carried as its internal id (or a whole value).
                let id = match &v {
                    Value::Null => return Ok(Value::Null),
                    Value::InternalId(id) => *id,
                    Value::Node(n) => n.id,
                    Value::Rel(r) => r.id,
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
                        .map_or(Value::Null, |n| Value::String(n.clone())),
                })
            }
            CompiledExpr::ValueProperty { value, prop } => {
                let v = value.eval_with_aggs(chunk, pos, aggs, random)?;
                let find = |props: &[(String, Value)]| {
                    props
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(prop))
                        .map_or(Value::Null, |(_, val)| val.clone())
                };
                Ok(match &v {
                    Value::Node(n) => find(&n.props),
                    Value::Rel(r) => find(&r.props),
                    Value::Json(koko_common::JsonValue::Object(fields)) => fields
                        .iter()
                        .find(|(name, _)| name == prop)
                        .map_or(Value::Null, |(_, value)| value.to_value()),
                    _ => Value::Null,
                })
            }
            CompiledExpr::List(items) => {
                let vals = items
                    .iter()
                    .map(|a| a.eval_with_aggs(chunk, pos, aggs, random))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::List(vals))
            }
            CompiledExpr::Struct(fields) => {
                let vals = fields
                    .iter()
                    .map(|(k, e)| Ok((k.clone(), e.eval_with_aggs(chunk, pos, aggs, random)?)))
                    .collect::<Result<Vec<_>>>()?;
                Ok(Value::Struct(vals))
            }
            CompiledExpr::LambdaVar(name) => Ok(lambda_get(name)),
            CompiledExpr::ListLambda {
                kind,
                list,
                params,
                body,
            } => {
                let lv = list.eval_with_aggs(chunk, pos, aggs, random)?;
                if lv.is_null() {
                    return Ok(Value::Null);
                }
                let items = match lv {
                    Value::List(items) => items,
                    other => {
                        return Err(Error::runtime(format!(
                            "list lambda expects a LIST, got {}",
                            other.logical_type()
                        )));
                    }
                };
                // Evaluate `body` with `param` bound to `val`, popping afterward.
                let eval_body = |binds: Vec<(String, Value)>| -> Result<Value> {
                    let n = binds.len();
                    LAMBDA_STACK.with(|s| s.borrow_mut().extend(binds));
                    let r = body.eval_with_aggs(chunk, pos, aggs, random);
                    LAMBDA_STACK.with(|s| {
                        let mut b = s.borrow_mut();
                        let len = b.len();
                        b.truncate(len - n);
                    });
                    r
                };
                match kind {
                    LambdaKind::Transform => {
                        let mut out = Vec::with_capacity(items.len());
                        for it in items {
                            out.push(eval_body(vec![(params[0].clone(), it)])?);
                        }
                        Ok(Value::List(out))
                    }
                    LambdaKind::Filter => {
                        let mut out = Vec::new();
                        for it in items {
                            if eval_body(vec![(params[0].clone(), it.clone())])?.as_bool()
                                == Some(true)
                            {
                                out.push(it);
                            }
                        }
                        Ok(Value::List(out))
                    }
                    LambdaKind::Reduce => {
                        let mut iter = items.into_iter();
                        let Some(mut acc) = iter.next() else {
                            // Matches the C++ engine: reducing an empty list errors.
                            return Err(Error::runtime(
                                "Cannot execute list_reduce on an empty list.".to_string(),
                            ));
                        };
                        for it in iter {
                            acc =
                                eval_body(vec![(params[0].clone(), acc), (params[1].clone(), it)])?;
                        }
                        Ok(acc)
                    }
                }
            }
            CompiledExpr::Case {
                operand,
                branches,
                else_,
            } => {
                let operand_val = match operand {
                    Some(o) => Some(o.eval_with_aggs(chunk, pos, aggs, random)?),
                    None => None,
                };
                for (cond, res) in branches {
                    let cv = cond.eval_with_aggs(chunk, pos, aggs, random)?;
                    let matched = match &operand_val {
                        // Simple CASE, C++ rule: a NULL when-value matches ANY
                        // operand (oracle: CASE 1 WHEN NULL THEN 'a' → 'a');
                        // otherwise a NULL operand matches nothing.
                        Some(ov) => match (ov.is_null(), cv.is_null()) {
                            (_, true) => true,
                            (true, false) => false,
                            (false, false) => {
                                koko_function::cypher_cmp(ov, &cv)
                                    == Some(std::cmp::Ordering::Equal)
                            }
                        },
                        // Searched CASE: the condition must be exactly true.
                        None => cv.as_bool() == Some(true),
                    };
                    if matched {
                        return res.eval_with_aggs(chunk, pos, aggs, random);
                    }
                }
                match else_ {
                    Some(e) => e.eval_with_aggs(chunk, pos, aggs, random),
                    None => Ok(Value::Null),
                }
            }
        }
    }

    fn internal_id_column(expr: &CompiledExpr) -> Option<usize> {
        match expr {
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

/// Compile an aggregate-free expression. Errors if it contains an aggregate.
/// Bind-time cast-function resolution for STRUCT sources (C++ resolves nested
/// casts statically): STRUCT→STRUCT needs matching field-name sequences and
/// recursively resolvable fields (mismatch reported with the FULL shapes);
/// STRUCT→scalar has no function (full shapes); STRUCT→LIST/MAP renders the
/// generic kind names ("STRUCT to MAP"). STRING/ANY/UNION and non-struct
/// sources resolve dynamically at runtime.
/// Whether an expression is a bind-time literal (a literal, a literal list,
/// or a cast over one) — the shapes C++ folds into typed literals at bind.
fn literal_rooted(e: &BoundExpr) -> bool {
    match e {
        BoundExpr::Literal(_) => true,
        BoundExpr::Cast { expr, .. } => literal_rooted(expr),
        BoundExpr::List { elems, .. } => elems.iter().all(literal_rooted),
        _ => false,
    }
}

fn validate_static_cast(src: &LogicalType, dst: &LogicalType) -> Result<()> {
    use LogicalType::*;
    // Messages render bind-time ANY as INT64 (`[[]]` casts as INT64[][]).
    fn resolved(t: &LogicalType) -> LogicalType {
        match t {
            Any => LogicalType::Int64,
            List(i) => List(Box::new(resolved(i))),
            Array(i, n) => Array(Box::new(resolved(i)), *n),
            Struct(fs) => Struct(fs.iter().map(|(n, t)| (n.clone(), resolved(t))).collect()),
            Map(k, v) => Map(Box::new(resolved(k)), Box::new(resolved(v))),
            other => other.clone(),
        }
    }
    let full_err = || {
        Err(Error::conversion(format!(
            "Unsupported casting function from {} to {}.",
            resolved(src),
            resolved(dst)
        )))
    };
    match (src, dst) {
        // Dynamic sources/targets resolve at runtime (STRING parses; UNION has
        // its own member machinery; ANY inspects the value).
        (Any | String | Union(_), _) | (_, Any | String | Union(_)) => Ok(()),
        (Struct(sf), Struct(tf)) => {
            let names_match = sf.len() == tf.len()
                && sf
                    .iter()
                    .zip(tf)
                    .all(|((sn, _), (tn, _))| sn.eq_ignore_ascii_case(tn));
            if !names_match {
                return full_err();
            }
            for ((_, sty), (_, tty)) in sf.iter().zip(tf) {
                validate_static_cast(sty, tty)?;
            }
            Ok(())
        }
        // Kind mismatches against a nested target render generically
        // ("STRUCT to MAP", "INT8 to STRUCT" stays the runtime wording).
        (Struct(_), List(_) | Array(_, _) | Map(_, _)) => {
            let kind = if matches!(dst, Map(_, _)) {
                "MAP"
            } else {
                "LIST"
            };
            Err(Error::conversion(format!(
                "Unsupported casting function from STRUCT to {kind}."
            )))
        }
        (Struct(_), _) => full_err(),
        // ARRAY→ARRAY with different lengths has no function when the element
        // types differ; identical elements fall to the literal-retype path.
        (Array(sc, n), Array(tc, m)) if n != m => {
            if sc == tc {
                Ok(())
            } else {
                full_err()
            }
        }
        // LIST→ARRAY defers its element count to the runtime value; children
        // still resolve statically.
        (List(sc) | Array(sc, _), List(tc) | Array(tc, _)) => validate_static_cast(sc, tc),
        (List(_) | Array(_, _), Struct(_) | Map(_, _)) => {
            let kind = if matches!(dst, Map(_, _)) {
                "MAP"
            } else {
                "STRUCT"
            };
            Err(Error::conversion(format!(
                "Unsupported casting function from LIST to {kind}."
            )))
        }
        (List(_) | Array(_, _), _) => full_err(),
        (Map(sk, sv), Map(tk, tv)) => {
            validate_static_cast(sk, tk)?;
            validate_static_cast(sv, tv)
        }
        _ => Ok(()),
    }
}

pub fn compile(bound: &BoundExpr, resolver: &dyn ColumnResolver) -> Result<CompiledExpr> {
    let mut aggs = Vec::new();
    let e = compile_collect(bound, resolver, &mut aggs)?;
    if !aggs.is_empty() {
        return Err(Error::Raw(
            // C++ emits this prefix-less (ExpressionEvaluator toString path).
            "Cannot evaluate expression with type AGGREGATE_FUNCTION.".to_string(),
        ));
    }
    Ok(e)
}

/// Compile an expression that may contain aggregates, lifting each aggregate
/// into `aggs` and replacing it with [`CompiledExpr::Agg`].
pub fn compile_collect(
    bound: &BoundExpr,
    resolver: &dyn ColumnResolver,
    aggs: &mut Vec<AggSpec>,
) -> Result<CompiledExpr> {
    match bound {
        BoundExpr::Literal(v) => Ok(CompiledExpr::Literal(v.clone())),
        BoundExpr::Parameter { name, .. } => Err(Error::binder(format!(
            "symbolic parameter ${name} cannot be compiled for execution"
        ))),
        BoundExpr::Column { col, .. } => Ok(CompiledExpr::Column(*col)),
        BoundExpr::Property { var, prop, .. } => {
            Ok(CompiledExpr::Column(resolver.column(*var, Some(prop))?))
        }
        BoundExpr::NodeRef { var, .. } => Ok(CompiledExpr::Column(
            // Prefer the materialized value column when the planner allocated
            // one (audit V12): value consumers then see the full node/rel value
            // instead of a bare internal id.
            match resolver.value_column(*var) {
                Some(vc) => vc,
                None => resolver.column(*var, None)?,
            },
        )),
        BoundExpr::ValueProperty { value, prop, .. } => Ok(CompiledExpr::ValueProperty {
            value: Box::new(compile_collect(value, resolver, aggs)?),
            prop: prop.clone(),
        }),
        BoundExpr::ScalarVar { var, .. } => Ok(CompiledExpr::Column(resolver.column(*var, None)?)),
        // A lifted subquery reads its precomputed per-row result column.
        BoundExpr::Subquery { id, .. } => Ok(CompiledExpr::Column(resolver.subquery_column(*id)?)),
        // A lifted `nextval`/`currval` likewise reads its per-row result column.
        BoundExpr::SequenceCall { id, .. } => {
            Ok(CompiledExpr::Column(resolver.sequence_column(*id)?))
        }
        BoundExpr::Scalar { op, args, .. } => {
            let args = args
                .iter()
                .map(|a| compile_collect(a, resolver, aggs))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Scalar { op: *op, args })
        }
        BoundExpr::Aggregate {
            op, distinct, arg, ..
        } => {
            // The argument is guaranteed aggregate-free by the binder.
            let arg = match arg {
                Some(a) => Some(compile(a, resolver)?),
                None => None,
            };
            let idx = aggs.len();
            aggs.push(AggSpec {
                op: *op,
                distinct: *distinct,
                arg,
            });
            Ok(CompiledExpr::Agg(idx))
        }
        BoundExpr::Cast { expr, target } => {
            // A cast to UNION resolves its member from the *static* source type
            // (C++ `bindCastToUnionFunction` → `findUnionMinCostTag` at bind time):
            // no cost-defined member is an error here, before anything runs.
            // STRING sources go through the union string parser instead, and ANY
            // (NULL / parameters) and UNION sources resolve dynamically in
            // `cast_value` — all via `union_tag: None`.
            let mut union_tag = None;
            if let LogicalType::Union(fields) = target {
                let src_ty = expr.ty();
                if !matches!(
                    src_ty,
                    LogicalType::Any | LogicalType::String | LogicalType::Union(_)
                ) {
                    union_tag = Some(
                        koko_common::types::union_min_cost_tag(&src_ty, fields).ok_or_else(
                            || {
                                Error::conversion(format!(
                                    "Cannot cast from {src_ty} to {target}, target type has no \
                                     compatible field."
                                ))
                            },
                        )?,
                    );
                }
            }
            // C++ resolves nested cast functions against the DECLARED source
            // type at bind time, so shape mismatches error even when the
            // runtime value is NULL (`{a: 12}` → STRUCT(a…, b STRUCT(…)) then
            // recast: field b resolves STRUCT→INT8 before anything runs).
            validate_static_cast(&expr.ty(), target)?;
            // A literal-rooted ARRAY re-dimension over the SAME element type
            // is the C++ bind-time literal retype, which refuses length
            // changes with a Binder error (differing elements instead fail
            // cast resolution above).
            if let (LogicalType::Array(sc, n), LogicalType::Array(tc, m)) = (&expr.ty(), target) {
                if n != m && sc == tc && literal_rooted(expr) {
                    return Err(Error::binder(format!(
                        "Cannot change literal expression data type from {} to {}.",
                        expr.ty(),
                        target
                    )));
                }
            }
            Ok(CompiledExpr::Cast {
                expr: Box::new(compile_collect(expr, resolver, aggs)?),
                target: target.clone(),
                union_tag,
            })
        }
        BoundExpr::Call { name, args, ty } => {
            // Node/rel accessors are evaluated specially (label/labels need the
            // table-name map, which lives only here at compile time).
            if let (Some(kind), [arg]) = (AccessorKind::from_name(name), args.as_slice()) {
                let names = match kind {
                    AccessorKind::Label => resolver.table_names(),
                    AccessorKind::Id | AccessorKind::Offset => HashMap::new(),
                };
                return Ok(CompiledExpr::Accessor {
                    kind,
                    arg: Box::new(compile_collect(arg, resolver, aggs)?),
                    names,
                });
            }
            if name == "typeof" && args.len() == 1 {
                let ty = koko_function::scalarfn::typeof_type_name(&args[0].ty());
                let arg = Box::new(compile_collect(&args[0], resolver, aggs)?);
                return Ok(CompiledExpr::TypeOf { ty, arg });
            }
            // `union_value`'s member name lives in the bound UNION type, not the
            // payload — capture it here and build the tagged value at eval time.
            if name == "union_value" && args.len() == 1 {
                if let LogicalType::Union(variants) = ty {
                    let arg = Box::new(compile_collect(&args[0], resolver, aggs)?);
                    return Ok(CompiledExpr::UnionValue {
                        variants: variants.clone(),
                        arg,
                    });
                }
            }
            let args = args
                .iter()
                .map(|a| compile_collect(a, resolver, aggs))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Call {
                name: name.clone(),
                args,
            })
        }
        BoundExpr::Udf { function, args, .. } => {
            let args = args
                .iter()
                .map(|argument| compile_collect(argument, resolver, aggs))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Udf {
                function: std::sync::Arc::clone(function),
                args,
            })
        }
        BoundExpr::List { elems, .. } => {
            let items = elems
                .iter()
                .map(|a| compile_collect(a, resolver, aggs))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::List(items))
        }
        BoundExpr::Struct { fields, .. } => {
            let fields = fields
                .iter()
                .map(|(k, e)| Ok((k.clone(), compile_collect(e, resolver, aggs)?)))
                .collect::<Result<Vec<_>>>()?;
            Ok(CompiledExpr::Struct(fields))
        }
        BoundExpr::LambdaVar { name, .. } => Ok(CompiledExpr::LambdaVar(name.clone())),
        BoundExpr::ListLambda {
            kind,
            list,
            params,
            body,
            ..
        } => Ok(CompiledExpr::ListLambda {
            kind: *kind,
            list: Box::new(compile_collect(list, resolver, aggs)?),
            params: params.clone(),
            body: Box::new(compile_collect(body, resolver, aggs)?),
        }),
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            let operand = operand
                .as_ref()
                .map(|o| compile_collect(o, resolver, aggs))
                .transpose()?
                .map(Box::new);
            let branches = branches
                .iter()
                .map(|(c, r)| {
                    Ok((
                        compile_collect(c, resolver, aggs)?,
                        compile_collect(r, resolver, aggs)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let else_ = else_
                .as_ref()
                .map(|e| compile_collect(e, resolver, aggs))
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
