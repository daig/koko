//! `koko-expr` — compile [`koko_ir::bound::BoundExpr`]s into a column-indexed
//! evaluator over [`koko_common::DataChunk`]s.
//!
//! Binding resolves *names and types*; compilation resolves a property/variable
//! reference to a concrete **column index** in the runtime row layout (via a
//! [`ColumnResolver`]). Aggregates are lifted out into [`AggSpec`]s and replaced
//! by [`CompiledExpr::Agg`] placeholders, so the aggregate operator computes
//! them and the surrounding scalar expression reads the finalized values.
//!
//! Evaluation is per row over [`Value`]s; typed chunks remain the operator boundary.

mod aggregate;
mod compile;
mod eval;

pub use aggregate::{AggSpec, compile_collect};
pub use compile::{compile, eval_constant};
pub use eval::EvalState;
use koko_common::{LogicalType, Result, TableId, Value};
use koko_function::{BuiltinScalar, ScalarOp};
use koko_ir::bound::{LambdaKind, LambdaVarId, VarId};
use std::collections::HashMap;

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
    fn from_function(function: BuiltinScalar) -> Option<AccessorKind> {
        match function {
            BuiltinScalar::Id => Some(AccessorKind::Id),
            BuiltinScalar::Offset => Some(AccessorKind::Offset),
            BuiltinScalar::Label | BuiltinScalar::Labels => Some(AccessorKind::Label),
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
        function: BuiltinScalar,
        called_name: String,
        args: Vec<CompiledExpr>,
    },
    /// A connection-local native Rust callback retained by the compiled plan.
    Udf {
        function: std::sync::Arc<koko_common::RegisteredScalarFunction>,
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
        params: Vec<LambdaVarId>,
        body: Box<CompiledExpr>,
    },
    /// A reference to a lambda parameter (read from the lambda stack at eval).
    LambdaVar(LambdaVarId),
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
