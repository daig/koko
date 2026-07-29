use super::*;

pub(crate) struct CrossProductState<'a> {
    pub(crate) left: Box<Exec<'a>>,
    pub(crate) right: Box<Exec<'a>>,
    pub(crate) left_width: usize,
    pub(crate) right_width: usize,
    pub(crate) right_buf: Option<Vec<DataChunk>>,
    pub(crate) st: ExpandState,
}

pub(crate) struct HashJoinState<'a> {
    pub(crate) probe: Box<Exec<'a>>,
    pub(crate) build: Box<Exec<'a>>,
    pub(crate) probe_cols: (usize, usize),
    pub(crate) build_cols: (usize, usize),
    pub(crate) probe_keys: Vec<CompiledExpr>,
    pub(crate) build_keys: Vec<CompiledExpr>,
    pub(crate) table: Option<HashMap<JoinKey, Vec<Vec<Value>>>>,
    pub(crate) kind: JoinKind,
    pub(crate) st: ExpandState,
}

pub(crate) struct OptionalState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) pattern: &'a PlanOp,
    pub(crate) new_cols: &'a [usize],
    pub(crate) st: ExpandState,
}

pub(crate) struct SubqueryState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) pattern: &'a PlanOp,
    pub(crate) result_col: usize,
    pub(crate) kind: SubqueryKind,
    pub(crate) st: ExpandState,
}
