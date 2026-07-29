use super::*;

pub(crate) struct FilterState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) predicate: CompiledExpr,
}
