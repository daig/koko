//! Physical plan and row-layout data contracts.

mod layout;
mod operator;
mod query;

pub use layout::{LayoutProp, PropCol, RowLayout, VarColKind, VarColumns};
pub use operator::{
    Extend, ExtendTarget, IndexScan, InputSlot, JoinKind, MaterializeItem, MergePlan, PathRel,
    PathSegmentPlan, PlanOp, ProjectPath, RelBranch, ScanNode, ScanTable, UnwindTarget, UpdateOp,
    VarLengthExtend,
};
pub use query::{PartPlan, QueryPlan, RegularPlan};
