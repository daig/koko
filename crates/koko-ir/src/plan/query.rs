use super::{InputSlot, PlanOp, RowLayout, UpdateOp};

#[derive(Debug, Clone)]
pub struct PartPlan {
    pub root: PlanOp,
    pub layout: RowLayout,
    pub inputs: Vec<InputSlot>,
    pub update_ops: Vec<UpdateOp>,
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub parts: Vec<PartPlan>,
}

#[derive(Debug, Clone)]
pub struct RegularPlan {
    pub operands: Vec<QueryPlan>,
    pub distinct: bool,
}
