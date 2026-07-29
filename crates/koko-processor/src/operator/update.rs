use super::*;

pub(crate) struct SequenceCallState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) func: SequenceFn,
    pub(crate) name: &'a str,
    pub(crate) result_col: usize,
    pub(crate) st: ExpandState,
}
