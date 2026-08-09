use super::*;

pub(crate) struct SingleRowState {
    pub(crate) done: bool,
}

pub(crate) struct InputScanState<'a> {
    pub(crate) chunks: &'a [DataChunk],
    pub(crate) idx: usize,
}

pub(crate) struct BufferedState {
    pub(crate) chunks: Vec<DataChunk>,
    pub(crate) idx: usize,
}

pub(crate) struct ScanNodeState<'a> {
    pub(crate) scan: &'a ScanNode,
    pub(crate) table_idx: usize,
    pub(crate) offset: u64,
    pub(crate) end: u64,
    pub(crate) single_table: bool,
    pub(crate) projected_columns: Vec<Vec<usize>>,
    pub(crate) external_reader: Option<IcebugNodeScan>,
}

pub(crate) struct IndexScanState<'a> {
    pub(crate) scan: &'a IndexScan,
    pub(crate) done: bool,
}

pub(crate) struct IndexLookupState<'a> {
    pub(crate) input: Box<Exec<'a>>,
    pub(crate) scan: &'a IndexScan,
    pub(crate) key: CompiledExpr,
    pub(crate) st: ExpandState,
}

pub(crate) struct TableFunctionScanState<'a> {
    pub(crate) call: &'a BoundTableFunctionCall,
    pub(crate) cols: &'a [usize],
    pub(crate) rows: Option<Vec<Vec<Value>>>,
    pub(crate) idx: usize,
}

pub(crate) struct LoadScanState<'a> {
    pub(crate) cols: &'a [usize],
    pub(crate) source: SourceLoadScan,
}
