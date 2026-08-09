//! Physical plan and row-layout data contracts.

mod layout;
mod operator;
mod query;

pub use layout::{LayoutProp, PropCol, RowLayout, VarColKind, VarColumns};
pub use operator::{
    Extend, ExtendTarget, GraphAlgorithmPlan, IndexScan, InputSlot, JoinKind, KCorePlan,
    LouvainPlan, MaterializeItem, MergePlan, PageRankPlan, PathRel, PathSegmentPlan, PlanOp,
    ProjectPath, RelBranch, ScanNode, ScanTable, StronglyConnectedComponentsPlan,
    TopologicalLevelsPlan, UnwindTarget, UpdateOp, VarLengthExtend, WeaklyConnectedComponentsPlan,
};
pub use query::{PartPlan, QueryPlan, RegularPlan};
