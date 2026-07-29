use crate::bound::{
    BoundCreate, BoundDelete, BoundExpr, BoundSet, BoundTableFunc, CsvLoadOptions, PathSemantic,
    RecursiveFilter, RecursiveMode, SequenceFn, SubqueryKind, VarId,
};
use koko_common::{ExtendDir, TableId, file_resolver::FileFormat};

use super::layout::PropCol;

#[derive(Debug, Clone)]
pub struct ScanTable {
    pub table: TableId,
    pub prop_cols: Vec<PropCol>,
}

#[derive(Debug, Clone)]
pub struct ScanNode {
    pub var: VarId,
    pub id_col: usize,
    pub tables: Vec<ScanTable>,
}

#[derive(Debug, Clone)]
pub struct IndexScan {
    pub input: Option<Box<PlanOp>>,
    pub var: VarId,
    pub id_col: usize,
    pub table: TableId,
    pub prop_cols: Vec<PropCol>,
    pub pk_value: BoundExpr,
}

#[derive(Debug, Clone)]
pub struct RelBranch {
    pub rel_table: TableId,
    pub rel_prop_cols: Vec<PropCol>,
}

#[derive(Debug, Clone)]
pub enum ExtendTarget {
    New {
        to_id_col: usize,
        to_tables: Vec<ScanTable>,
    },
    Existing {
        filter_col: usize,
    },
}

#[derive(Debug, Clone)]
pub struct Extend {
    pub input: Box<PlanOp>,
    pub from_id_col: usize,
    pub dir: ExtendDir,
    pub rel_id_col: usize,
    pub branches: Vec<RelBranch>,
    pub target: ExtendTarget,
    pub carry_cols: Vec<usize>,
    pub factorize: bool,
}

#[derive(Debug, Clone)]
pub struct VarLengthExtend {
    pub input: Box<PlanOp>,
    pub from_id_col: usize,
    pub dir: ExtendDir,
    pub lower: u32,
    pub upper: u32,
    pub mode: RecursiveMode,
    pub semantic: PathSemantic,
    pub rel_tables: Vec<TableId>,
    pub rel_value_col: usize,
    pub build_value: bool,
    pub filter: Option<RecursiveFilter>,
    pub weight: Option<String>,
    pub in_named_path: bool,
    pub target: ExtendTarget,
    pub factorize: bool,
}

#[derive(Debug, Clone)]
pub struct PathSegmentPlan {
    pub rel: PathRel,
    pub to_node: VarId,
}

#[derive(Debug, Clone)]
pub enum PathRel {
    Recursive { value_col: usize },
    Single { rel: VarId },
}

#[derive(Debug, Clone)]
pub struct ProjectPath {
    pub input: Box<PlanOp>,
    pub path_col: usize,
    pub head: VarId,
    pub segments: Vec<PathSegmentPlan>,
}

#[derive(Debug, Clone)]
pub enum JoinKind {
    Inner,
    Left,
    Mark { mark_col: usize, kind: SubqueryKind },
}

#[derive(Debug, Clone)]
pub enum UnwindTarget {
    Scalar {
        col: usize,
    },
    Node {
        id_col: usize,
        prop_tables: Vec<ScanTable>,
    },
}

/// Data-only physical operator tree interpreted by `koko-processor`.
#[derive(Debug, Clone)]
pub enum PlanOp {
    SingleRow,
    InputScan,
    ScanTableFunc {
        func: BoundTableFunc,
        arg: Option<String>,
        cols: Vec<usize>,
    },
    LoadScan {
        cols: Vec<usize>,
        col_names: Vec<String>,
        path: String,
        paths: Vec<String>,
        format: FileFormat,
        options: CsvLoadOptions,
        bare: bool,
    },
    ScanNode(ScanNode),
    IndexScan(IndexScan),
    Extend(Box<Extend>),
    VarLengthExtend(Box<VarLengthExtend>),
    ProjectPath(Box<ProjectPath>),
    CrossProduct {
        left: Box<PlanOp>,
        left_width: usize,
        right: Box<PlanOp>,
        right_width: usize,
    },
    HashJoin {
        probe: Box<PlanOp>,
        build: Box<PlanOp>,
        probe_cols: (usize, usize),
        build_cols: (usize, usize),
        keys: Vec<(BoundExpr, BoundExpr)>,
        kind: JoinKind,
    },
    Filter {
        input: Box<PlanOp>,
        predicate: BoundExpr,
    },
    Unwind {
        input: Box<PlanOp>,
        list: BoundExpr,
        target: UnwindTarget,
    },
    Optional {
        input: Box<PlanOp>,
        pattern: Box<PlanOp>,
        new_cols: Vec<usize>,
    },
    Subquery {
        input: Box<PlanOp>,
        pattern: Box<PlanOp>,
        result_col: usize,
        kind: SubqueryKind,
    },
    SequenceCall {
        input: Box<PlanOp>,
        func: SequenceFn,
        name: String,
        result_col: usize,
    },
    MaterializeValues {
        input: Box<PlanOp>,
        items: Vec<MaterializeItem>,
    },
}

#[derive(Debug, Clone)]
pub struct MaterializeItem {
    pub id_col: usize,
    pub value_col: usize,
    pub is_node: bool,
}

#[derive(Debug, Clone)]
pub enum InputSlot {
    Scalar {
        col: usize,
    },
    Node {
        id_col: usize,
        prop_tables: Vec<ScanTable>,
    },
}

#[derive(Debug, Clone)]
pub enum UpdateOp {
    Create(BoundCreate),
    Set(BoundSet),
    Delete(BoundDelete),
    Merge(Box<MergePlan>),
}

#[derive(Debug, Clone)]
pub struct MergePlan {
    pub match_pattern: PlanOp,
    pub create: BoundCreate,
    pub on_create: BoundSet,
    pub on_match: BoundSet,
    pub key_node_vars: Vec<VarId>,
    pub suppress_dup: bool,
}
