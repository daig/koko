use super::{PlanBuilder, collect_expr_vars};
use koko_common::Result;
use koko_ir::{
    bound::{BoundUpdate, VarId},
    plan::{MergePlan, PlanOp, UpdateOp},
};
use std::collections::HashSet;

impl PlanBuilder<'_> {
    /// Plan one updating clause and allocate columns for created or merged values.
    pub(super) fn plan_update(&mut self, update: &BoundUpdate) -> Result<UpdateOp> {
        match update {
            BoundUpdate::Create(create) => {
                for node in &create.nodes {
                    if self.layout.try_var(node.var).is_none() {
                        self.alloc_node(node.var);
                    }
                }
                for relationship in &create.rels {
                    if let Some(variable) = relationship.var
                        && self.layout.try_var(variable).is_none()
                    {
                        let allocation = self.alloc_rel(variable);
                        self.register_rel(variable, allocation.id_col, allocation.props);
                    }
                }
                Ok(UpdateOp::Create(create.clone()))
            }
            BoundUpdate::Set(set) => Ok(UpdateOp::Set(set.clone())),
            BoundUpdate::Delete(delete) => Ok(UpdateOp::Delete(delete.clone())),
            BoundUpdate::Merge(merge) => {
                let mut key_node_vars = Vec::new();
                for &variable in &merge.match_.node_vars {
                    if self.bound.contains(&variable) && !key_node_vars.contains(&variable) {
                        key_node_vars.push(variable);
                    }
                }
                let suppress_dup = merge.on_create.items.is_empty()
                    && merge.on_match.items.is_empty()
                    && merge.create.rels.is_empty()
                    && {
                        let mut key_vars: HashSet<VarId> = key_node_vars.iter().copied().collect();
                        for node in &merge.create.nodes {
                            key_vars.insert(node.var);
                            for (_, expression) in &node.props {
                                collect_expr_vars(expression, &mut key_vars);
                            }
                        }
                        self.layout
                            .var_ids()
                            .all(|variable| key_vars.contains(&variable))
                    };
                let mut pattern = self.build_match(
                    &merge.match_,
                    merge.filter.as_ref(),
                    Some(PlanOp::InputScan),
                )?;
                if let Some(predicate) = &merge.filter {
                    pattern = PlanOp::Filter {
                        input: Box::new(pattern),
                        predicate: predicate.clone(),
                    };
                }
                Ok(UpdateOp::Merge(Box::new(MergePlan {
                    match_pattern: pattern,
                    create: merge.create.clone(),
                    on_create: merge.on_create.clone(),
                    on_match: merge.on_match.clone(),
                    key_node_vars,
                    suppress_dup,
                })))
            }
        }
    }
}
