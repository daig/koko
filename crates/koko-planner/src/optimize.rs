//! Logical-plan optimizer (P3 step 5): a post-planning rewrite stage over the
//! [`PlanOp`] tree, mirroring the C++ `Optimizer` pass pipeline.
//!
//! The first (and currently only) rule is **filter push-down**, including the
//! **primary-key point-lookup** rewrite — the Rust analog of
//! `FilterPushDownOptimizer` + its `PRIMARY_KEY_SCAN` replacement. It is a
//! predicate-set-carrying recursive visitor:
//!
//! - a [`PlanOp::Filter`] splits its predicate into conjuncts, adds them to the
//!   carried set, and drops itself (the conjuncts are now in flight);
//! - a [`PlanOp::CrossProduct`] partitions the carried conjuncts into those the
//!   left subtree can evaluate, those the right can, and the rest, pushing each
//!   side's into it (a single-variable conjunct thus sinks below the product —
//!   turning the O(n²) `MATCH (a),(b) WHERE a.pk=… AND b.pk=…` cross-product into
//!   two O(1) point lookups);
//! - a [`PlanOp::ScanNode`] over a single table pops a `var.<pk> = <expr>`
//!   equality and becomes an [`PlanOp::IndexScan`] (an in-memory `find_node_by_pk`
//!   probe); constants become leaf lookups, input-column expressions become
//!   index-nested-loop lookups, and leftovers are re-materialized as a `Filter`;
//! - row-preserving single-input operators (`Extend`, `Unwind`, …) push the
//!   conjuncts their input can evaluate down into it; `SequenceCall` is a barrier
//!   (pushing a filter below it would change how many times the sequence advances);
//! - every other operator re-materializes the carried conjuncts above itself.
//!
//! Because a bound predicate references *variables* (resolved to chunk columns
//! only at exec time), relocating a `Filter` never rewrites the predicate — moving
//! the node is enough. "Evaluable here" is decided structurally: a conjunct can be
//! pushed into a subtree iff every column it reads is produced by that subtree.

use crate::cost::{StatsMap, plan_card};
use koko_catalog::Catalog;
use koko_common::LogicalType;
use koko_function::{BuiltinScalar, ScalarOp};
use koko_ir::bound::{
    BoundExpr, BoundPart, BoundProjection, BoundQuery, BoundRegularQuery, OrderKey, ProjItem, VarId,
};
use koko_ir::plan::{
    Extend, ExtendTarget, GraphAlgorithmPlan, IndexScan, InputSlot, JoinKind, PartPlan, PathRel,
    PlanOp, QueryPlan, RegularPlan, RowLayout, ScanNode, ScanTable, UnwindTarget, VarLengthExtend,
};
use std::collections::HashSet;

/// Optimize every operand of a `UNION` plan in place.
pub fn optimize_regular(
    rq: &BoundRegularQuery,
    plan: &mut RegularPlan,
    catalog: &Catalog,
    stats: &StatsMap,
) {
    for (q, operand) in rq.operands.iter().zip(&mut plan.operands) {
        optimize(q, operand, catalog, stats);
    }
}

/// Optimize every part of a query plan in place. The bound `query` is threaded in so
/// the factorization pass sees each part's projection (its consumer columns), and
/// `stats` (the P3 cost model's input) drives the hash-join build-side choice.
pub fn optimize(query: &BoundQuery, plan: &mut QueryPlan, catalog: &Catalog, stats: &StatsMap) {
    for (bound_part, part) in query.parts.iter().zip(&mut plan.parts) {
        optimize_part(bound_part, part, catalog, stats);
    }
}

fn optimize_part(bound_part: &BoundPart, part: &mut PartPlan, catalog: &Catalog, stats: &StatsMap) {
    // The layout columns the carried-input scope (`PlanOp::InputScan`) replays.
    let mut input_cols = HashSet::new();
    for slot in &part.inputs {
        match slot {
            InputSlot::Scalar { col } => {
                input_cols.insert(*col);
            }
            InputSlot::Node {
                id_col,
                prop_tables,
            } => {
                input_cols.insert(*id_col);
                for table in prop_tables {
                    for pc in &table.prop_cols {
                        input_cols.insert(pc.col_index);
                    }
                }
            }
        }
    }
    let root = std::mem::replace(&mut part.root, PlanOp::SingleRow);
    let driver = FilterPushDown {
        catalog,
        layout: &part.layout,
        input_cols: &input_cols,
        stats,
    };
    part.root = driver.rewrite(root, Vec::new());

    // Projection pruning: relationship and node schemas can be wide, while analytical
    // patterns commonly need only connectivity. Do not gather property columns that no
    // operator or final projection reads. Updating parts stay conservative because their
    // mutation payloads are maintained outside the physical read tree.
    prune_unused_properties(bound_part, part);

    // Factorization (P3 step 6): on the rewritten and pruned tree, collapse any
    // fan-out suffix consumed only as a count.
    mark_factorization(bound_part, part);
}

/// Remove node/relationship property mappings that are provably unread by a read-only
/// part. The stable layout remains unchanged; pruned columns simply stay NULL in chunks.
fn prune_unused_properties(bound_part: &BoundPart, part: &mut PartPlan) {
    let Some(projection) = &bound_part.projection else {
        return;
    };
    if !part.update_ops.is_empty() {
        return;
    }
    let mut read = HashSet::new();
    collect_projection_reads(projection, &part.layout, &mut read);
    collect_plan_reads(&part.root, &part.layout, &mut read);
    prune_plan_properties(&mut part.root, &read);
    let mut needed = HashSet::new();
    collect_projection_reads(projection, &part.layout, &mut needed);
    annotate_extend_carry(&mut part.root, &needed, &read, &part.layout);
}

/// Backward liveness for the common unary scan/extend/filter pipeline. Branching
/// operators deliberately fall back to the global read set; this keeps the rewrite
/// conservative while allowing linear analytical paths to stop copying endpoint ids
/// immediately after their last consumer.
fn annotate_extend_carry(
    op: &mut PlanOp,
    needed_above: &HashSet<usize>,
    global_read: &HashSet<usize>,
    layout: &RowLayout,
) {
    match op {
        PlanOp::Filter { input, predicate } => {
            let mut needed = needed_above.clone();
            collect_expr_cols(layout, predicate, &mut needed);
            annotate_extend_carry(input, &needed, global_read, layout);
        }
        PlanOp::Extend(extend) => {
            let mut carry: Vec<_> = needed_above
                .iter()
                .copied()
                .filter(|column| *column < extend.rel_id_col)
                .collect();
            carry.sort_unstable();
            extend.carry_cols = carry;

            let mut needed = needed_above.clone();
            needed.remove(&extend.rel_id_col);
            for branch in &extend.branches {
                for property in &branch.rel_prop_cols {
                    needed.remove(&property.col_index);
                }
            }
            match &extend.target {
                ExtendTarget::New {
                    to_id_col,
                    to_tables,
                } => {
                    needed.remove(to_id_col);
                    for table in to_tables {
                        for property in &table.prop_cols {
                            needed.remove(&property.col_index);
                        }
                    }
                }
                ExtendTarget::Existing { filter_col } => {
                    needed.insert(*filter_col);
                }
            }
            needed.insert(extend.from_id_col);
            annotate_extend_carry(&mut extend.input, &needed, global_read, layout);
        }
        PlanOp::ProjectPath(path) => {
            let mut needed = needed_above.clone();
            needed.remove(&path.path_col);
            add_var_binding(layout, path.head, &mut needed);
            for segment in &path.segments {
                add_var_binding(layout, segment.to_node, &mut needed);
                match segment.rel {
                    PathRel::Recursive { value_col } => {
                        needed.insert(value_col);
                    }
                    PathRel::Single { rel } => add_var_binding(layout, rel, &mut needed),
                }
            }
            annotate_extend_carry(&mut path.input, &needed, global_read, layout);
        }
        PlanOp::Unwind { input, list, .. } => {
            let mut needed = needed_above.clone();
            collect_expr_cols(layout, list, &mut needed);
            annotate_extend_carry(input, &needed, global_read, layout);
        }
        PlanOp::SequenceCall {
            input, result_col, ..
        } => {
            let mut needed = needed_above.clone();
            needed.remove(result_col);
            annotate_extend_carry(input, &needed, global_read, layout);
        }
        PlanOp::MaterializeValues { input, items } => {
            let mut needed = needed_above.clone();
            for item in items {
                needed.remove(&item.value_col);
                needed.insert(item.id_col);
            }
            annotate_extend_carry(input, &needed, global_read, layout);
        }
        PlanOp::IndexScan(scan) => {
            if let Some(input) = &mut scan.input {
                annotate_extend_carry(input, global_read, global_read, layout);
            }
        }
        PlanOp::VarLengthExtend(extend) => {
            annotate_extend_carry(&mut extend.input, global_read, global_read, layout);
        }
        PlanOp::CrossProduct { left, right, .. } => {
            annotate_extend_carry(left, global_read, global_read, layout);
            annotate_extend_carry(right, global_read, global_read, layout);
        }
        PlanOp::HashJoin { probe, build, .. } => {
            annotate_extend_carry(probe, global_read, global_read, layout);
            annotate_extend_carry(build, global_read, global_read, layout);
        }
        PlanOp::Optional { input, pattern, .. } | PlanOp::Subquery { input, pattern, .. } => {
            annotate_extend_carry(input, global_read, global_read, layout);
            annotate_extend_carry(pattern, global_read, global_read, layout);
        }
        PlanOp::SingleRow
        | PlanOp::InputScan
        | PlanOp::ScanNode(_)
        | PlanOp::ScanTableFunc { .. }
        | PlanOp::ScanGraphAlgorithm(_)
        | PlanOp::LoadScan { .. } => {}
    }
}

fn prune_plan_properties(op: &mut PlanOp, read: &HashSet<usize>) {
    let prune_tables = |tables: &mut Vec<ScanTable>| {
        for table in tables {
            table
                .prop_cols
                .retain(|property| read.contains(&property.col_index));
        }
    };
    match op {
        PlanOp::ScanNode(scan) => prune_tables(&mut scan.tables),
        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::TopologicalLevels(scan)) => {
            prune_tables(&mut scan.node.tables);
        }
        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::KCoreDecomposition(scan)) => {
            prune_tables(&mut scan.node.tables);
        }
        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::WeaklyConnectedComponents(scan)) => {
            prune_tables(&mut scan.node.tables);
        }
        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::StronglyConnectedComponents(scan)) => {
            prune_tables(&mut scan.node.tables);
        }
        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::PageRank(scan)) => {
            prune_tables(&mut scan.node.tables);
        }
        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::Louvain(scan)) => {
            prune_tables(&mut scan.node.tables);
        }
        PlanOp::IndexScan(scan) => {
            scan.prop_cols
                .retain(|property| read.contains(&property.col_index));
            if let Some(input) = &mut scan.input {
                prune_plan_properties(input, read);
            }
        }
        PlanOp::Extend(extend) => {
            extend.carry_cols.retain(|column| read.contains(column));
            for branch in &mut extend.branches {
                branch
                    .rel_prop_cols
                    .retain(|property| read.contains(&property.col_index));
            }
            if let ExtendTarget::New { to_tables, .. } = &mut extend.target {
                prune_tables(to_tables);
            }
            prune_plan_properties(&mut extend.input, read);
        }
        PlanOp::VarLengthExtend(extend) => {
            if let ExtendTarget::New { to_tables, .. } = &mut extend.target {
                prune_tables(to_tables);
            }
            prune_plan_properties(&mut extend.input, read);
        }
        PlanOp::Unwind {
            input,
            target: UnwindTarget::Node { prop_tables, .. },
            ..
        } => {
            prune_tables(prop_tables);
            prune_plan_properties(input, read);
        }
        PlanOp::Unwind { input, .. }
        | PlanOp::Filter { input, .. }
        | PlanOp::SequenceCall { input, .. }
        | PlanOp::MaterializeValues { input, .. } => prune_plan_properties(input, read),
        PlanOp::ProjectPath(path) => prune_plan_properties(&mut path.input, read),
        PlanOp::CrossProduct { left, right, .. } => {
            prune_plan_properties(left, read);
            prune_plan_properties(right, read);
        }
        PlanOp::HashJoin { probe, build, .. } => {
            prune_plan_properties(probe, read);
            prune_plan_properties(build, read);
        }
        PlanOp::Optional { input, pattern, .. } | PlanOp::Subquery { input, pattern, .. } => {
            prune_plan_properties(input, read);
            prune_plan_properties(pattern, read);
        }
        PlanOp::SingleRow
        | PlanOp::InputScan
        | PlanOp::ScanTableFunc { .. }
        | PlanOp::LoadScan { .. } => {}
    }
}

/// Mark the collapsible (factorizable) fan-out extends in `part.root`.
///
/// Only runs for a part whose projection **aggregates** (the sole consumer of
/// multiplicity) and has **no updating clauses** (a write consumes the matched rows
/// by count, which multiplicity would corrupt). It then walks the root top-down: an
/// `Extend`/`VarLengthExtend` collapses iff every ancestor so far is a `Filter` or an
/// already-collapsed extend (`chain_ok`) **and** its introduced columns are never read
/// (`read_set`). Any other operator (fan-out extend, cross-product, unwind, optional,
/// subquery, sequence-call, project-path) clears `chain_ok` for its subtree — so a
/// fan-out is never *above* a collapse, which is why no operator besides the
/// factorizing extend and the aggregate needs to be multiplicity-aware.
fn mark_factorization(bound_part: &BoundPart, part: &mut PartPlan) {
    let Some(proj) = &bound_part.projection else {
        return;
    };
    if !proj.has_aggregates() || !part.update_ops.is_empty() {
        return;
    }
    let mut read = HashSet::new();
    collect_projection_reads(proj, &part.layout, &mut read);
    collect_plan_reads(&part.root, &part.layout, &mut read);
    mark_collapsible(&mut part.root, true, &read);
}

/// Every layout column read by a projection: a scalar item's expression references,
/// an `ORDER BY` expression's references, and a whole-variable item's binding.
fn collect_projection_reads(proj: &BoundProjection, layout: &RowLayout, out: &mut HashSet<usize>) {
    for item in &proj.items {
        match item {
            ProjItem::Scalar { expr, .. } => collect_expr_cols(layout, expr, out),
            ProjItem::Var { var, .. } => add_var_binding(layout, *var, out),
        }
    }
    for (key, _) in &proj.order_by {
        match key {
            // Output-scope references read projected columns, already covered above.
            OrderKey::Output(_) | OrderKey::PostProjection(_) => {}
            OrderKey::Expr(e) => collect_expr_cols(layout, e, out),
        }
    }
}

/// Every layout column read by an operator subtree: predicate/list/expression
/// references, the structural id columns an extend/path reads, and the references
/// inside correlated sub-patterns (so a collapsed var can never be one a subquery or
/// optional reads). Produced columns are *not* reads.
fn collect_plan_reads(op: &PlanOp, layout: &RowLayout, out: &mut HashSet<usize>) {
    match op {
        PlanOp::Filter { input, predicate } => {
            collect_expr_cols(layout, predicate, out);
            collect_plan_reads(input, layout, out);
        }
        PlanOp::Extend(e) => {
            out.insert(e.from_id_col);
            if let ExtendTarget::Existing { filter_col } = &e.target {
                out.insert(*filter_col);
            }
            collect_plan_reads(&e.input, layout, out);
        }
        PlanOp::VarLengthExtend(e) => {
            out.insert(e.from_id_col);
            if let ExtendTarget::Existing { filter_col } = &e.target {
                out.insert(*filter_col);
            }
            if let Some(f) = &e.filter {
                if let Some(p) = &f.rel_pred {
                    collect_expr_cols(layout, p, out);
                }
                if let Some(p) = &f.node_pred {
                    collect_expr_cols(layout, p, out);
                }
            }
            collect_plan_reads(&e.input, layout, out);
        }
        PlanOp::ProjectPath(p) => {
            add_var_binding(layout, p.head, out);
            for seg in &p.segments {
                add_var_binding(layout, seg.to_node, out);
                match &seg.rel {
                    PathRel::Recursive { value_col } => {
                        out.insert(*value_col);
                    }
                    PathRel::Single { rel } => add_var_binding(layout, *rel, out),
                }
            }
            collect_plan_reads(&p.input, layout, out);
        }
        PlanOp::Unwind { input, list, .. } => {
            collect_expr_cols(layout, list, out);
            collect_plan_reads(input, layout, out);
        }
        PlanOp::CrossProduct { left, right, .. } => {
            collect_plan_reads(left, layout, out);
            collect_plan_reads(right, layout, out);
        }
        PlanOp::HashJoin {
            probe, build, keys, ..
        } => {
            for (pe, be) in keys {
                collect_expr_cols(layout, pe, out);
                collect_expr_cols(layout, be, out);
            }
            collect_plan_reads(probe, layout, out);
            collect_plan_reads(build, layout, out);
        }
        PlanOp::Optional { input, pattern, .. } | PlanOp::Subquery { input, pattern, .. } => {
            // The sub-pattern is correlated: its reads of outer columns count.
            collect_plan_reads(input, layout, out);
            collect_plan_reads(pattern, layout, out);
        }
        PlanOp::SequenceCall { input, .. } => collect_plan_reads(input, layout, out),
        PlanOp::MaterializeValues { input, items } => {
            for it in items {
                out.insert(it.id_col);
            }
            collect_plan_reads(input, layout, out);
        }
        PlanOp::IndexScan(s) => {
            collect_expr_cols(layout, &s.pk_value, out);
            if let Some(input) = &s.input {
                collect_plan_reads(input, layout, out);
            }
        }
        PlanOp::SingleRow
        | PlanOp::InputScan
        | PlanOp::ScanNode(_)
        | PlanOp::ScanTableFunc { .. }
        | PlanOp::ScanGraphAlgorithm(_)
        | PlanOp::LoadScan { .. } => {}
    }
}

/// Add a variable's whole binding (its id, properties, and materialized value)
/// to `out`.
fn add_var_binding(layout: &RowLayout, var: VarId, out: &mut HashSet<usize>) {
    if let Some(columns) = layout.try_var(var) {
        out.insert(columns.id_col);
        for property in &columns.props {
            out.insert(property.col_index);
        }
        if let Some(value_col) = columns.value_col {
            out.insert(value_col);
        }
    }
}

/// Top-down collapse marker; see [`mark_factorization`].
fn mark_collapsible(op: &mut PlanOp, chain_ok: bool, read: &HashSet<usize>) {
    match op {
        // A filter is transparent to multiplicity (it only narrows the selection).
        PlanOp::Filter { input, .. } => mark_collapsible(input, chain_ok, read),
        PlanOp::Extend(e) => {
            e.factorize = chain_ok && extend_introduced_unread(e, read);
            mark_collapsible(&mut e.input, e.factorize, read);
        }
        PlanOp::VarLengthExtend(e) => {
            e.factorize = chain_ok && var_extend_introduced_unread(e, read);
            mark_collapsible(&mut e.input, e.factorize, read);
        }
        // Operators that do not propagate multiplicity: nothing below them collapses.
        PlanOp::CrossProduct { left, right, .. } => {
            mark_collapsible(left, false, read);
            mark_collapsible(right, false, read);
        }
        PlanOp::HashJoin {
            probe, build, kind, ..
        } => {
            // A Mark join emits exactly one row per probe row (annotating it with an
            // existence/count column) and preserves multiplicity — so, like a Filter,
            // a collapsed fan-out may sit in its probe (this is what lets q9's tag
            // fan-out factorize *below* the decorrelated NOT EXISTS, dropping the
            // probe from 51M rows to 2.39M triples). Left/Inner fan out, so cannot.
            let probe_ok = chain_ok && matches!(kind, JoinKind::Mark { .. });
            mark_collapsible(probe, probe_ok, read);
            mark_collapsible(build, false, read);
        }
        PlanOp::Unwind { input, .. }
        | PlanOp::Optional { input, .. }
        | PlanOp::Subquery { input, .. }
        | PlanOp::SequenceCall { input, .. }
        | PlanOp::MaterializeValues { input, .. } => mark_collapsible(input, false, read),
        PlanOp::IndexScan(s) => {
            if let Some(input) = s.input.as_mut() {
                mark_collapsible(input, chain_ok, read);
            }
        }
        PlanOp::ProjectPath(p) => mark_collapsible(&mut p.input, false, read),
        PlanOp::SingleRow
        | PlanOp::InputScan
        | PlanOp::ScanNode(_)
        | PlanOp::ScanTableFunc { .. }
        | PlanOp::ScanGraphAlgorithm(_)
        | PlanOp::LoadScan { .. } => {}
    }
}

/// Whether none of an [`Extend`]'s introduced columns (the rel, plus the new node
/// for a `New` target) appear in `read` — i.e. the extend contributes only
/// cardinality and can be collapsed into a multiplicity.
fn extend_introduced_unread(e: &Extend, read: &HashSet<usize>) -> bool {
    if read.contains(&e.rel_id_col) {
        return false;
    }
    for b in &e.branches {
        for pc in &b.rel_prop_cols {
            if read.contains(&pc.col_index) {
                return false;
            }
        }
    }
    if let ExtendTarget::New {
        to_id_col,
        to_tables,
    } = &e.target
    {
        if read.contains(to_id_col) {
            return false;
        }
        for t in to_tables {
            for pc in &t.prop_cols {
                if read.contains(&pc.col_index) {
                    return false;
                }
            }
        }
    }
    true
}

/// Whether none of a [`VarLengthExtend`]'s introduced columns (the path value, plus
/// the new end node for a `New` target) appear in `read`.
fn var_extend_introduced_unread(e: &VarLengthExtend, read: &HashSet<usize>) -> bool {
    if read.contains(&e.rel_value_col) {
        return false;
    }
    if let ExtendTarget::New {
        to_id_col,
        to_tables,
    } = &e.target
    {
        if read.contains(to_id_col) {
            return false;
        }
        for t in to_tables {
            for pc in &t.prop_cols {
                if read.contains(&pc.col_index) {
                    return false;
                }
            }
        }
    }
    true
}

/// A single filter-push-down traversal. Stateless apart from the borrowed catalog
/// and layout — the in-flight conjuncts are threaded explicitly through `rewrite`,
/// so each subtree gets exactly the predicates that belong to it.
struct FilterPushDown<'a> {
    catalog: &'a Catalog,
    layout: &'a RowLayout,
    /// Columns produced by `PlanOp::InputScan` (the part's carried scope).
    input_cols: &'a HashSet<usize>,
    /// Per-table statistics for the cost-based hash-join build-side choice.
    stats: &'a StatsMap,
}

impl FilterPushDown<'_> {
    /// Rewrite `op`, given the conjuncts (`carried`) still looking for a home at or
    /// below it. Returns the rewritten subtree with every carried conjunct either
    /// absorbed (PK scan), pushed further down, or re-materialized as a `Filter`.
    fn rewrite(&self, op: PlanOp, mut carried: Vec<BoundExpr>) -> PlanOp {
        match op {
            // Collect this filter's conjuncts and drop the node — they travel down.
            PlanOp::Filter { input, predicate } => {
                carried.extend(split_and(predicate));
                self.rewrite(*input, carried)
            }

            // Partition the carried conjuncts by which side can evaluate them.
            PlanOp::CrossProduct {
                left,
                left_width,
                right,
                right_width,
            } => {
                let lcols = self.produced_cols(&left);
                let rcols = self.produced_cols(&right);
                let mut to_left = Vec::new();
                let mut to_right = Vec::new();
                let mut stuck = Vec::new();
                for p in carried {
                    let refs = self.referenced_cols(&p);
                    let in_left = refs.is_subset(&lcols);
                    let in_right = refs.is_subset(&rcols);
                    // A conjunct touching both sides (a join predicate) cannot sink
                    // into either — it stays above the product (a hash join would
                    // consume it; that is a later step).
                    if in_left && !in_right {
                        to_left.push(p);
                    } else if in_right && !in_left {
                        to_right.push(p);
                    } else {
                        stuck.push(p);
                    }
                }
                let new_left = self.rewrite(*left, to_left);
                let new_right = self.rewrite(*right, to_right);
                // If the right side is a single-table node scan constrained by a PK
                // equality against the left/input columns (`p.id = i` or
                // `p.id = i + 1`), prefer an index nested loop over a hash join. This
                // is the create-batch shape: the UNWIND rows drive O(1) PK probes
                // instead of materializing or hashing the whole MATCH side.
                if let PlanOp::ScanNode(scan) = &new_right {
                    if let Some((idx, pk_value)) = self.correlated_pk_eq(scan, &stuck, &lcols) {
                        let mut residual = stuck;
                        residual.remove(idx);
                        let index = IndexScan {
                            input: Some(Box::new(new_left)),
                            var: scan.var,
                            id_col: scan.id_col,
                            table: scan.tables[0].table,
                            prop_cols: scan.tables[0].prop_cols.clone(),
                            pk_value,
                        };
                        return self.land(PlanOp::IndexScan(index), residual);
                    }
                }
                // P3 step 7: pull equi-join conjuncts out of the both-side `stuck` set
                // into a hash join (probe = left, build = right); whatever is left
                // re-materializes as a Filter above it. With no equi-join conjunct this
                // stays the plain cross product, exactly as before.
                let mut keys = Vec::new();
                let mut residual = Vec::new();
                for p in stuck {
                    match self.as_join_key(&p, &lcols, &rcols) {
                        Some(k) => keys.push(k),
                        None => residual.push(p),
                    }
                }
                let joined = if keys.is_empty() {
                    PlanOp::CrossProduct {
                        left: Box::new(new_left),
                        left_width,
                        right: Box::new(new_right),
                        right_width,
                    }
                } else {
                    // Cost-based build side (P3 step 8): hash the smaller input. The
                    // left input fills layout cols `[0..left_width)`, the right
                    // `[left_width..+right_width)`. Build the right by default (stable
                    // on ties / no stats = the step-7 behavior); build the left only
                    // when it is *strictly* smaller, swapping each key's orientation
                    // since the probe side is then the right.
                    let left_cols = (0, left_width);
                    let right_cols = (left_width, right_width);
                    if plan_card(&new_left, self.stats) < plan_card(&new_right, self.stats) {
                        PlanOp::HashJoin {
                            probe: Box::new(new_right),
                            build: Box::new(new_left),
                            probe_cols: right_cols,
                            build_cols: left_cols,
                            keys: keys.into_iter().map(|(l, r)| (r, l)).collect(),
                            kind: JoinKind::Inner,
                        }
                    } else {
                        PlanOp::HashJoin {
                            probe: Box::new(new_left),
                            build: Box::new(new_right),
                            probe_cols: left_cols,
                            build_cols: right_cols,
                            keys,
                            kind: JoinKind::Inner,
                        }
                    }
                };
                self.land(joined, residual)
            }

            // Try the primary-key point-lookup rewrite, then land the rest.
            PlanOp::ScanNode(scan) => self.rewrite_scan(scan, carried),

            // Row-preserving single-input operators: push the conjuncts the input
            // can evaluate into it (so a `var.pk` filter reaches the scan below an
            // extend/unwind), keep the rest above.
            PlanOp::Extend(mut e) => {
                self.push_into(e.input.as_mut(), &mut carried);
                self.land(PlanOp::Extend(e), carried)
            }
            PlanOp::VarLengthExtend(mut e) => {
                self.push_into(e.input.as_mut(), &mut carried);
                self.land(PlanOp::VarLengthExtend(e), carried)
            }
            PlanOp::ProjectPath(mut p) => {
                self.push_into(p.input.as_mut(), &mut carried);
                self.land(PlanOp::ProjectPath(p), carried)
            }
            PlanOp::Unwind {
                mut input,
                list,
                target,
            } => {
                self.push_into(input.as_mut(), &mut carried);
                self.land(
                    PlanOp::Unwind {
                        input,
                        list,
                        target,
                    },
                    carried,
                )
            }
            PlanOp::Optional {
                mut input,
                pattern,
                new_cols,
            } => {
                self.push_into(input.as_mut(), &mut carried);
                self.land(
                    PlanOp::Optional {
                        input,
                        pattern,
                        new_cols,
                    },
                    carried,
                )
            }
            PlanOp::Subquery {
                mut input,
                pattern,
                result_col,
                kind,
            } => {
                self.push_into(input.as_mut(), &mut carried);
                self.land(
                    PlanOp::Subquery {
                        input,
                        pattern,
                        result_col,
                        kind,
                    },
                    carried,
                )
            }

            // `SequenceCall` is a barrier: a conjunct pushed below it would change
            // how many input rows reach it and thus how often the sequence advances.
            // Land everything above, but still optimize the input subtree.
            PlanOp::SequenceCall {
                input,
                func,
                name,
                result_col,
            } => {
                let new_input = self.rewrite(*input, Vec::new());
                self.land(
                    PlanOp::SequenceCall {
                        input: Box::new(new_input),
                        func,
                        name,
                        result_col,
                    },
                    carried,
                )
            }

            // Pass-through: filters may cross (materialization has no
            // cardinality effect), but conjuncts reading a value column must
            // land above — keep it simple and land carried conjuncts here.
            PlanOp::MaterializeValues { input, items } => {
                let new_input = self.rewrite(*input, Vec::new());
                self.land(
                    PlanOp::MaterializeValues {
                        input: Box::new(new_input),
                        items,
                    },
                    carried,
                )
            }

            PlanOp::IndexScan(mut s) => {
                if let Some(input) = s.input.as_mut() {
                    self.push_into(input.as_mut(), &mut carried);
                }
                self.land(PlanOp::IndexScan(s), carried)
            }

            // Leaves and anything else: nothing more to push into — land the
            // carried conjuncts right here. (`HashJoin` is never an *input* to a
            // rewrite — the planner emits only `CrossProduct`, which this pass turns
            // into one — but is matched here for exhaustiveness.)
            op @ (PlanOp::SingleRow
            | PlanOp::InputScan
            | PlanOp::ScanTableFunc { .. }
            | PlanOp::ScanGraphAlgorithm(_)
            | PlanOp::LoadScan { .. }
            | PlanOp::HashJoin { .. }) => self.land(op, carried),
        }
    }

    /// Push the subset of `carried` that `input` can evaluate down into it (in
    /// place), leaving the rest in `carried`. The pushed conjuncts are removed from
    /// `carried` and re-homed by a recursive `rewrite` of `input`.
    fn push_into(&self, input: &mut PlanOp, carried: &mut Vec<BoundExpr>) {
        let icols = self.produced_cols(input);
        let mut push = Vec::new();
        carried.retain(|p| {
            if self.referenced_cols(p).is_subset(&icols) {
                push.push(p.clone());
                false
            } else {
                true
            }
        });
        let child = std::mem::replace(input, PlanOp::SingleRow);
        *input = self.rewrite(child, push);
    }

    /// Rewrite a node-table scan: pop a constant `var.<pk> = <const>` equality into
    /// a leaf [`IndexScan`] when the scan is over a single table, then land leftovers.
    fn rewrite_scan(&self, scan: ScanNode, mut carried: Vec<BoundExpr>) -> PlanOp {
        if scan.tables.len() == 1 && !self.catalog.is_icebug_table(scan.tables[0].table) {
            let table = scan.tables[0].table;
            if let Some(nt) = self.catalog.node_table(table) {
                let pk = nt.primary_key_column();
                let pk_name = pk.name().to_string();
                let pk_ty = pk.logical_type().clone();
                let found = carried.iter().enumerate().find_map(|(i, p)| {
                    self.match_pk_eq(p, scan.var, &pk_name, &pk_ty)
                        .map(|v| (i, v))
                });
                if let Some((i, pk_value)) = found {
                    carried.remove(i);
                    let index = IndexScan {
                        input: None,
                        var: scan.var,
                        id_col: scan.id_col,
                        table,
                        prop_cols: scan.tables[0].prop_cols.clone(),
                        pk_value,
                    };
                    return self.land(PlanOp::IndexScan(index), carried);
                }
            }
        }
        self.land(PlanOp::ScanNode(scan), carried)
    }

    /// Wrap `op` in a single `Filter` carrying `preds` (combined with `AND`), or
    /// return `op` unchanged when there are none.
    fn land(&self, op: PlanOp, preds: Vec<BoundExpr>) -> PlanOp {
        match combine_and(preds) {
            None => op,
            Some(predicate) => PlanOp::Filter {
                input: Box::new(op),
                predicate,
            },
        }
    }

    /// If `pred` is `var.<pk> = <const>` (in either order), return the constant
    /// side — the value the [`IndexScan`] will look up. The constant's type must be
    /// PK-lookup-compatible (see [`pk_lookup_compatible`]) so the probe matches what
    /// a row-by-row `=` filter would.
    fn match_pk_eq(
        &self,
        pred: &BoundExpr,
        var: VarId,
        pk_name: &str,
        pk_ty: &LogicalType,
    ) -> Option<BoundExpr> {
        let args = match pred {
            BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args,
                ..
            } if args.len() == 2 => args,
            _ => return None,
        };
        let is_pk = |e: &BoundExpr| {
            matches!(e, BoundExpr::Property { var: v, prop, .. }
                if *v == var && prop.eq_ignore_ascii_case(pk_name))
        };
        let try_side = |pk_side: &BoundExpr, const_side: &BoundExpr| {
            (is_pk(pk_side)
                && is_constant(const_side)
                && pk_lookup_compatible(&const_side.ty(), pk_ty))
            .then(|| const_side.clone())
        };
        try_side(&args[0], &args[1]).or_else(|| try_side(&args[1], &args[0]))
    }

    /// Find a `scan.var.<pk> = <input expr>` conjunct suitable for an index nested
    /// loop. The expression must read at least one input column and no scan columns;
    /// constants are intentionally left to the leaf scan rewrite so we do not repeat
    /// the same constant probe once per input row.
    fn correlated_pk_eq(
        &self,
        scan: &ScanNode,
        preds: &[BoundExpr],
        input_cols: &HashSet<usize>,
    ) -> Option<(usize, BoundExpr)> {
        if scan.tables.len() != 1 {
            return None;
        }
        let table = scan.tables[0].table;
        let nt = self.catalog.node_table(table)?;
        let pk = nt.primary_key_column();
        preds.iter().enumerate().find_map(|(i, p)| {
            self.match_pk_eq_in_cols(p, scan.var, pk.name(), pk.logical_type(), input_cols)
                .map(|v| (i, v))
        })
    }

    fn match_pk_eq_in_cols(
        &self,
        pred: &BoundExpr,
        var: VarId,
        pk_name: &str,
        pk_ty: &LogicalType,
        input_cols: &HashSet<usize>,
    ) -> Option<BoundExpr> {
        let args = match pred {
            BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args,
                ..
            } if args.len() == 2 => args,
            _ => return None,
        };
        let is_pk = |e: &BoundExpr| {
            matches!(e, BoundExpr::Property { var: v, prop, .. }
                if *v == var && prop.eq_ignore_ascii_case(pk_name))
        };
        let try_side = |pk_side: &BoundExpr, value_side: &BoundExpr| {
            let refs = self.referenced_cols(value_side);
            (is_pk(pk_side)
                && !refs.is_empty()
                && refs.is_subset(input_cols)
                && pk_lookup_compatible(&value_side.ty(), pk_ty))
            .then(|| value_side.clone())
        };
        try_side(&args[0], &args[1]).or_else(|| try_side(&args[1], &args[0]))
    }

    /// If `pred` is an equality `e1 = e2` that cleanly splits across a cross product —
    /// one side evaluable from the probe (left) columns, the other from the build
    /// (right) columns — return it oriented as `(probe_expr, build_expr)` for a hash
    /// join. No type guard is needed: the table keys by `ValueKey`, which folds
    /// cross-numeric equality and normalizes `-0.0` exactly as Cypher `=` does, so the
    /// join yields the same rows the `=` filter would. A non-equality, or a conjunct
    /// touching both sides (e.g. `a.x + b.y = 5`), returns `None` and stays a filter.
    fn as_join_key(
        &self,
        pred: &BoundExpr,
        lcols: &HashSet<usize>,
        rcols: &HashSet<usize>,
    ) -> Option<(BoundExpr, BoundExpr)> {
        let args = match pred {
            BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args,
                ..
            } if args.len() == 2 => args,
            _ => return None,
        };
        let ar = self.referenced_cols(&args[0]);
        let br = self.referenced_cols(&args[1]);
        if ar.is_subset(lcols) && br.is_subset(rcols) {
            Some((args[0].clone(), args[1].clone()))
        } else if ar.is_subset(rcols) && br.is_subset(lcols) {
            Some((args[1].clone(), args[0].clone()))
        } else {
            None
        }
    }

    /// The layout columns a subtree produces (fills with a bound value).
    fn produced_cols(&self, op: &PlanOp) -> HashSet<usize> {
        let mut out = HashSet::new();
        self.collect_produced(op, &mut out);
        out
    }

    fn collect_produced(&self, op: &PlanOp, out: &mut HashSet<usize>) {
        match op {
            PlanOp::SingleRow => {}
            PlanOp::InputScan => out.extend(self.input_cols.iter().copied()),
            PlanOp::ScanNode(s) => {
                out.insert(s.id_col);
                for t in &s.tables {
                    for pc in &t.prop_cols {
                        out.insert(pc.col_index);
                    }
                }
            }
            PlanOp::IndexScan(s) => {
                if let Some(input) = &s.input {
                    self.collect_produced(input, out);
                }
                out.insert(s.id_col);
                for pc in &s.prop_cols {
                    out.insert(pc.col_index);
                }
            }
            PlanOp::ScanTableFunc { cols, .. } | PlanOp::LoadScan { cols, .. } => {
                out.extend(cols.iter().copied())
            }
            PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::KCoreDecomposition(scan)) => {
                out.insert(scan.node.id_col);
                for table in &scan.node.tables {
                    for property in &table.prop_cols {
                        out.insert(property.col_index);
                    }
                }
                out.insert(scan.core_col);
            }
            PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::TopologicalLevels(scan)) => {
                out.insert(scan.node.id_col);
                for table in &scan.node.tables {
                    for property in &table.prop_cols {
                        out.insert(property.col_index);
                    }
                }
                out.insert(scan.level_col);
            }
            PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::WeaklyConnectedComponents(scan)) => {
                out.insert(scan.node.id_col);
                for table in &scan.node.tables {
                    for property in &table.prop_cols {
                        out.insert(property.col_index);
                    }
                }
                out.insert(scan.component_id_col);
            }
            PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::StronglyConnectedComponents(scan)) => {
                out.insert(scan.node.id_col);
                for table in &scan.node.tables {
                    for property in &table.prop_cols {
                        out.insert(property.col_index);
                    }
                }
                out.insert(scan.component_col);
            }
            PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::PageRank(scan)) => {
                out.insert(scan.node.id_col);
                for table in &scan.node.tables {
                    for property in &table.prop_cols {
                        out.insert(property.col_index);
                    }
                }
                out.insert(scan.score_col);
            }
            PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::Louvain(scan)) => {
                out.insert(scan.node.id_col);
                for table in &scan.node.tables {
                    for property in &table.prop_cols {
                        out.insert(property.col_index);
                    }
                }
                out.insert(scan.community_col);
            }
            PlanOp::Extend(e) => {
                self.collect_produced(&e.input, out);
                out.insert(e.rel_id_col);
                for b in &e.branches {
                    for pc in &b.rel_prop_cols {
                        out.insert(pc.col_index);
                    }
                }
                if let ExtendTarget::New {
                    to_id_col,
                    to_tables,
                } = &e.target
                {
                    out.insert(*to_id_col);
                    for t in to_tables {
                        for pc in &t.prop_cols {
                            out.insert(pc.col_index);
                        }
                    }
                }
            }
            PlanOp::VarLengthExtend(e) => {
                self.collect_produced(&e.input, out);
                out.insert(e.rel_value_col);
                if let ExtendTarget::New {
                    to_id_col,
                    to_tables,
                } = &e.target
                {
                    out.insert(*to_id_col);
                    for t in to_tables {
                        for pc in &t.prop_cols {
                            out.insert(pc.col_index);
                        }
                    }
                }
            }
            PlanOp::ProjectPath(p) => {
                self.collect_produced(&p.input, out);
                out.insert(p.path_col);
            }
            PlanOp::Unwind { input, target, .. } => {
                self.collect_produced(input, out);
                match target {
                    UnwindTarget::Scalar { col } => {
                        out.insert(*col);
                    }
                    UnwindTarget::Node {
                        id_col,
                        prop_tables,
                    } => {
                        out.insert(*id_col);
                        for table in prop_tables {
                            for pc in &table.prop_cols {
                                out.insert(pc.col_index);
                            }
                        }
                    }
                }
            }
            PlanOp::CrossProduct { left, right, .. } => {
                self.collect_produced(left, out);
                self.collect_produced(right, out);
            }
            PlanOp::HashJoin {
                probe, build, kind, ..
            } => {
                self.collect_produced(probe, out);
                self.collect_produced(build, out);
                if let JoinKind::Mark { mark_col, .. } = kind {
                    out.insert(*mark_col);
                }
            }
            PlanOp::Filter { input, .. } => self.collect_produced(input, out),
            PlanOp::Optional {
                input, new_cols, ..
            } => {
                self.collect_produced(input, out);
                out.extend(new_cols.iter().copied());
            }
            PlanOp::Subquery {
                input, result_col, ..
            }
            | PlanOp::SequenceCall {
                input, result_col, ..
            } => {
                self.collect_produced(input, out);
                out.insert(*result_col);
            }
            PlanOp::MaterializeValues { input, items } => {
                self.collect_produced(input, out);
                out.extend(items.iter().map(|it| it.value_col));
            }
        }
    }

    /// The layout columns a predicate reads (resolving each variable/property and
    /// each lifted subquery/sequence reference to its column).
    fn referenced_cols(&self, e: &BoundExpr) -> HashSet<usize> {
        let mut out = HashSet::new();
        self.collect_refs(e, &mut out);
        out
    }

    fn collect_refs(&self, e: &BoundExpr, out: &mut HashSet<usize>) {
        collect_expr_cols(self.layout, e, out)
    }
}

/// The layout columns a bound expression reads (resolving each variable/property and
/// each lifted subquery/sequence reference to its column). Shared by the filter-
/// pushdown "evaluable here" test and the factorization read-set.
fn collect_expr_cols(layout: &RowLayout, e: &BoundExpr, out: &mut HashSet<usize>) {
    match e {
        BoundExpr::Literal(_) | BoundExpr::Parameter { .. } | BoundExpr::LambdaVar { .. } => {}
        BoundExpr::Column { col, .. } => {
            out.insert(*col);
        }
        BoundExpr::Property { var, prop, .. } => {
            if let Some(c) = layout.column(*var, Some(prop)) {
                out.insert(c);
            }
        }
        BoundExpr::ScalarVar { var, .. } => {
            if let Some(c) = layout.column(*var, None) {
                out.insert(c);
            }
        }
        BoundExpr::NodeRef { var, .. } => {
            // A whole node/rel value reads the variable's whole binding.
            add_var_binding(layout, *var, out);
        }
        BoundExpr::Subquery { id, .. } => {
            if let Some(c) = layout.subquery_column(*id) {
                out.insert(c);
            }
        }
        BoundExpr::SequenceCall { id, .. } => {
            if let Some(c) = layout.sequence_column(*id) {
                out.insert(c);
            }
        }
        BoundExpr::ValueProperty { value, .. } => collect_expr_cols(layout, value, out),
        BoundExpr::Cast { expr, .. } => collect_expr_cols(layout, expr, out),
        BoundExpr::Call { function, args, .. }
            if matches!(
                function,
                BuiltinScalar::Id
                    | BuiltinScalar::Offset
                    | BuiltinScalar::Label
                    | BuiltinScalar::Labels
            ) && args.len() == 1 =>
        {
            match &args[0] {
                BoundExpr::NodeRef { var, .. } => {
                    if let Some(column) = layout.column(*var, None) {
                        out.insert(column);
                    }
                }
                arg => collect_expr_cols(layout, arg, out),
            }
        }
        BoundExpr::Scalar { args, .. }
        | BoundExpr::Call { args, .. }
        | BoundExpr::Udf { args, .. }
        | BoundExpr::List { elems: args, .. } => {
            for a in args {
                collect_expr_cols(layout, a, out);
            }
        }
        BoundExpr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                collect_expr_cols(layout, a, out);
            }
        }
        BoundExpr::Struct { fields, .. } => {
            for (_, v) in fields {
                collect_expr_cols(layout, v, out);
            }
        }
        BoundExpr::ListLambda { list, body, .. } => {
            collect_expr_cols(layout, list, out);
            collect_expr_cols(layout, body, out);
        }
        BoundExpr::Case {
            operand,
            branches,
            else_,
            ..
        } => {
            if let Some(o) = operand {
                collect_expr_cols(layout, o, out);
            }
            for (c, r) in branches {
                collect_expr_cols(layout, c, out);
                collect_expr_cols(layout, r, out);
            }
            if let Some(el) = else_ {
                collect_expr_cols(layout, el, out);
            }
        }
    }
}

/// Whether a constant of type `const_ty` can be looked up against a primary key of
/// type `pk_ty` with the *same* result a row-by-row `=` filter would give. Integer
/// widths are interchangeable (the PK index normalizes every integer to `i128`, and
/// `=` compares numerically); other types must match exactly. Floating/decimal
/// constants are deliberately excluded — `5.0 = (int) 5` is true under `=` but the
/// integer index has no `5.0` key, so such a predicate stays a filter.
fn pk_lookup_compatible(const_ty: &LogicalType, pk_ty: &LogicalType) -> bool {
    (is_integer(const_ty) && is_integer(pk_ty)) || const_ty == pk_ty
}

fn is_integer(t: &LogicalType) -> bool {
    matches!(t, LogicalType::Int(_) | LogicalType::UInt128)
}

/// Whether `e` is a compile-time constant (no row dependency): a literal, or a
/// `CAST` of one. Mirrors the C++ `isConstantExpression` (we have no parameter
/// node — parameters are folded to literals during binding).
fn is_constant(e: &BoundExpr) -> bool {
    match e {
        BoundExpr::Literal(_) => true,
        BoundExpr::Cast { expr, .. } => is_constant(expr),
        _ => false,
    }
}

/// Flatten a predicate into its top-level `AND` conjuncts (a local copy of the
/// binder's helper — the binder's is private).
fn split_and(e: BoundExpr) -> Vec<BoundExpr> {
    match e {
        BoundExpr::Scalar {
            op: ScalarOp::And,
            args,
            ..
        } => args.into_iter().flat_map(split_and).collect(),
        other => vec![other],
    }
}

/// Recombine conjuncts into a single `AND` predicate (inverse of [`split_and`]).
fn combine_and(mut preds: Vec<BoundExpr>) -> Option<BoundExpr> {
    match preds.len() {
        0 => None,
        1 => Some(preds.pop().unwrap()),
        _ => Some(BoundExpr::Scalar {
            op: ScalarOp::And,
            args: preds,
            ty: LogicalType::Bool,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan;
    use koko_catalog::{ColumnDefinition, NodeTableDefinition, RelTableDefinition};
    use koko_common::RelStorageDirection;
    use koko_common::{TableId, Value};
    use koko_ir::bound::{
        BoundMatch, BoundPart, BoundQuery, BoundReadingClause, BoundSet, BoundUnwind, BoundUpdate,
        PropInfo, SequenceFn, SubqueryKind, VarInfo, VarKind,
    };

    fn empty_rel(name: &str, endpoints: Vec<(TableId, TableId)>) -> RelTableDefinition {
        RelTableDefinition {
            name: name.to_string(),
            endpoint_pairs: endpoints,
            columns: Vec::new(),
            storage_direction: RelStorageDirection::default(),
        }
    }

    /// A catalog with one node table `N(id INT64 [pk], val STRING)`.
    fn catalog_with_n() -> (Catalog, TableId) {
        let mut cat = Catalog::new();
        let t = cat
            .create_node_table(NodeTableDefinition {
                name: "N".to_string(),
                columns: vec![
                    ColumnDefinition::plain("id", LogicalType::Int64),
                    ColumnDefinition::plain("val", LogicalType::String),
                ],
                primary_key: "id".to_string(),
            })
            .unwrap();
        (cat, t)
    }

    /// A node variable `name:N` over table `t` (with both N properties).
    fn node_var(name: &str, t: TableId) -> VarInfo {
        VarInfo {
            name: name.to_string(),
            anonymous: false,
            kind: VarKind::Node {
                tables: vec![t],
                label: "N".to_string(),
            },
            properties: vec![
                PropInfo {
                    name: "id".to_string(),
                    column_id: 0,
                    ty: LogicalType::Int64,
                },
                PropInfo {
                    name: "val".to_string(),
                    column_id: 1,
                    ty: LogicalType::String,
                },
            ],
            value_backed: false,
        }
    }

    fn scalar_var(name: &str, ty: LogicalType) -> VarInfo {
        VarInfo {
            name: name.to_string(),
            anonymous: false,
            kind: VarKind::Scalar { ty },
            properties: vec![],
            value_backed: false,
        }
    }

    /// `<var>.id <op> <lit>`.
    fn cmp(var: VarId, op: ScalarOp, lit: Value) -> BoundExpr {
        BoundExpr::Scalar {
            op,
            args: vec![
                BoundExpr::Property {
                    var,
                    prop: "id".to_string(),
                    ty: LogicalType::Int64,
                },
                BoundExpr::Literal(lit),
            ],
            ty: LogicalType::Bool,
        }
    }

    fn required_match(
        match_: BoundMatch,
        where_predicate: Option<BoundExpr>,
    ) -> Vec<BoundReadingClause> {
        vec![BoundReadingClause::Match {
            match_,
            where_predicate,
        }]
    }

    /// Plan + optimize a single-part query: match `node_vars`, filtered by `pred`.
    fn optimized_root(
        cat: &Catalog,
        vars: Vec<VarInfo>,
        node_vars: Vec<VarId>,
        pred: BoundExpr,
    ) -> PlanOp {
        let query = BoundQuery {
            vars,
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars,
                        ..Default::default()
                    },
                    Some(pred),
                ),
                ..Default::default()
            }],
        };
        let stats = StatsMap::new();
        let mut plan = plan(&query, cat, &stats).unwrap();
        optimize(&query, &mut plan, cat, &stats);
        plan.parts.pop().unwrap().root
    }

    #[test]
    fn pk_equality_becomes_index_scan() {
        let (cat, t) = catalog_with_n();
        let a = VarId(0);
        let root = optimized_root(
            &cat,
            vec![node_var("a", t)],
            vec![a],
            cmp(a, ScalarOp::Eq, Value::Int64(1)),
        );
        assert!(
            matches!(root, PlanOp::IndexScan(_)),
            "pk = const should rewrite to IndexScan, got {root:?}"
        );
    }

    #[test]
    fn unwind_before_match_uses_correlated_pk_lookup() {
        let (cat, t) = catalog_with_n();
        let (i, p) = (VarId(0), VarId(1));
        let one = BoundExpr::Literal(Value::Int64(1));
        let i_plus_one = BoundExpr::Scalar {
            op: ScalarOp::Add,
            args: vec![
                BoundExpr::ScalarVar {
                    var: i,
                    ty: LogicalType::Int64,
                },
                one,
            ],
            ty: LogicalType::Int64,
        };
        let pred = BoundExpr::Scalar {
            op: ScalarOp::Eq,
            args: vec![
                BoundExpr::Property {
                    var: p,
                    prop: "id".to_string(),
                    ty: LogicalType::Int64,
                },
                i_plus_one,
            ],
            ty: LogicalType::Bool,
        };
        let query = BoundQuery {
            vars: vec![scalar_var("i", LogicalType::Int64), node_var("p", t)],
            parts: vec![BoundPart {
                reading: vec![
                    BoundReadingClause::Unwind(BoundUnwind {
                        var: i,
                        list: BoundExpr::List {
                            elems: vec![BoundExpr::Literal(Value::Int64(0))],
                            ty: LogicalType::List(Box::new(LogicalType::Int64)),
                        },
                    }),
                    BoundReadingClause::Match {
                        match_: BoundMatch {
                            node_vars: vec![p],
                            ..Default::default()
                        },
                        where_predicate: Some(pred),
                    },
                ],
                ..Default::default()
            }],
        };
        let stats = StatsMap::new();
        let mut plan = plan(&query, &cat, &stats).unwrap();
        optimize(&query, &mut plan, &cat, &stats);
        match plan.parts.pop().unwrap().root {
            PlanOp::IndexScan(s) => {
                assert!(
                    s.input.is_some(),
                    "correlated lookup must keep its UNWIND input"
                );
                assert_eq!(s.var, p);
            }
            other => panic!("expected correlated IndexScan above UNWIND, got {other:?}"),
        }
    }

    #[test]
    fn pk_range_stays_a_filtered_scan() {
        let (cat, t) = catalog_with_n();
        let a = VarId(0);
        // `>=` is not a point lookup; it must remain a Filter over a plain ScanNode.
        let root = optimized_root(
            &cat,
            vec![node_var("a", t)],
            vec![a],
            cmp(a, ScalarOp::Ge, Value::Int64(1)),
        );
        match root {
            PlanOp::Filter { input, .. } => {
                assert!(
                    matches!(*input, PlanOp::ScanNode(_)),
                    "expected Filter(ScanNode)"
                );
            }
            other => panic!("expected a Filter over a ScanNode, got {other:?}"),
        }
    }

    #[test]
    fn float_constant_against_int_pk_is_not_rewritten() {
        let (cat, t) = catalog_with_n();
        let a = VarId(0);
        // A DOUBLE constant is not index-compatible with an INT pk; stays a filter.
        let root = optimized_root(
            &cat,
            vec![node_var("a", t)],
            vec![a],
            cmp(a, ScalarOp::Eq, Value::Double(1.0)),
        );
        assert!(
            matches!(root, PlanOp::Filter { .. }),
            "double = int-pk must stay a Filter, got {root:?}"
        );
    }

    #[test]
    fn disconnected_pk_filters_sink_below_the_cross_product() {
        let (cat, t) = catalog_with_n();
        let (a, b) = (VarId(0), VarId(1));
        // MATCH (a:N), (b:N) WHERE a.id = 1 AND b.id = 2  ⇒  the O(n²) killer.
        let pred = BoundExpr::Scalar {
            op: ScalarOp::And,
            args: vec![
                cmp(a, ScalarOp::Eq, Value::Int64(1)),
                cmp(b, ScalarOp::Eq, Value::Int64(2)),
            ],
            ty: LogicalType::Bool,
        };
        let root = optimized_root(
            &cat,
            vec![node_var("a", t), node_var("b", t)],
            vec![a, b],
            pred,
        );
        match root {
            PlanOp::CrossProduct { left, right, .. } => {
                assert!(
                    matches!(*left, PlanOp::IndexScan(_)),
                    "left side should be an IndexScan, got {left:?}"
                );
                assert!(
                    matches!(*right, PlanOp::IndexScan(_)),
                    "right side should be an IndexScan, got {right:?}"
                );
            }
            other => panic!("expected CrossProduct(IndexScan, IndexScan), got {other:?}"),
        }
    }

    // --- Factorization (P3 step 6) marking tests ---

    use koko_function::AggOp;
    use koko_ir::bound::{BoundProjection, ProjItem};

    /// A catalog with node table `N(id INT64 pk, val STRING)` and rel table
    /// `R(FROM N TO N)` (no rel properties).
    fn catalog_with_n_and_r() -> (Catalog, TableId, TableId) {
        let (mut cat, n) = catalog_with_n();
        let r = cat.create_rel_table(empty_rel("R", vec![(n, n)])).unwrap();
        (cat, n, r)
    }

    /// A directed, non-recursive rel variable `name:R` from `src` to `dst`.
    fn rel_var(name: &str, rt: TableId, src: VarId, dst: VarId) -> VarInfo {
        VarInfo {
            name: name.to_string(),
            anonymous: false,
            kind: VarKind::Rel {
                tables: vec![rt],
                label: "R".to_string(),
                src,
                dst,
                directed: true,
                recursive: None,
            },
            properties: vec![],
            value_backed: false,
        }
    }

    fn count_star() -> BoundExpr {
        BoundExpr::Aggregate {
            op: AggOp::CountStar,
            distinct: false,
            arg: None,
            ty: LogicalType::Int64,
        }
    }

    fn prop(var: VarId, name: &str, ty: LogicalType) -> BoundExpr {
        BoundExpr::Property {
            var,
            prop: name.to_string(),
            ty,
        }
    }

    /// Plan + optimize `MATCH (a)-[e]->(b)` over `R` with the given projection items,
    /// then return the optimized root.
    fn optimized_extend_root(items: Vec<ProjItem>) -> PlanOp {
        let (cat, n, r) = catalog_with_n_and_r();
        let (a, b, e) = (VarId(0), VarId(1), VarId(2));
        let query = BoundQuery {
            vars: vec![node_var("a", n), node_var("b", n), rel_var("e", r, a, b)],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b],
                        rel_vars: vec![e],
                        ..Default::default()
                    },
                    None,
                ),
                projection: Some(BoundProjection {
                    distinct: false,
                    items,
                    order_by: vec![],
                    skip: None,
                    limit: None,
                }),
                ..Default::default()
            }],
        };
        let stats = StatsMap::new();
        let mut plan = plan(&query, &cat, &stats).unwrap();
        optimize(&query, &mut plan, &cat, &stats);
        plan.parts.pop().unwrap().root
    }

    #[test]
    fn count_star_over_extend_collapses() {
        // MATCH (a)-[e]->(b) RETURN count(*) — neither `e` nor `b` is read, so the
        // extend collapses its fan-out into a multiplicity.
        let root = optimized_extend_root(vec![ProjItem::Scalar {
            name: "count(*)".to_string(),
            expr: count_star(),
        }]);
        match root {
            PlanOp::Extend(e) => assert!(
                e.factorize,
                "the fan-out should be factorized for count(*), got {e:?}"
            ),
            other => panic!("expected an Extend at the root, got {other:?}"),
        }
    }

    #[test]
    fn projected_endpoint_blocks_collapse() {
        // RETURN b.val, count(*) — `b` is read, so the extend must fan out (its
        // multiplicity would otherwise lose the distinct `b` values).
        let (a, b, e) = (VarId(0), VarId(1), VarId(2));
        let _ = (a, e);
        let root = optimized_extend_root(vec![
            ProjItem::Scalar {
                name: "b.val".to_string(),
                expr: prop(b, "val", LogicalType::String),
            },
            ProjItem::Scalar {
                name: "count(*)".to_string(),
                expr: count_star(),
            },
        ]);
        match root {
            PlanOp::Extend(e) => assert!(
                !e.factorize,
                "a projected endpoint must not be collapsed, got {e:?}"
            ),
            other => panic!("expected an Extend at the root, got {other:?}"),
        }
    }

    #[test]
    fn non_aggregate_query_does_not_collapse() {
        // RETURN a.val — no aggregate consumes a multiplicity, so factorization is
        // not applied at all (the gate keeps the blast radius minimal).
        let a = VarId(0);
        let root = optimized_extend_root(vec![ProjItem::Scalar {
            name: "a.val".to_string(),
            expr: prop(a, "val", LogicalType::String),
        }]);
        match root {
            PlanOp::Extend(e) => assert!(
                !e.factorize,
                "a non-aggregate query must not factorize, got {e:?}"
            ),
            other => panic!("expected an Extend at the root, got {other:?}"),
        }
    }

    // --- Hash join (P3 step 7) rewrite tests ---

    /// `<v1>.<prop> <op> <v2>.<prop>` between two variables' same-named property.
    fn prop_cmp(v1: VarId, v2: VarId, prop: &str, op: ScalarOp, ty: LogicalType) -> BoundExpr {
        let p = |v: VarId| BoundExpr::Property {
            var: v,
            prop: prop.to_string(),
            ty: ty.clone(),
        };
        BoundExpr::Scalar {
            op,
            args: vec![p(v1), p(v2)],
            ty: LogicalType::Bool,
        }
    }

    #[test]
    fn value_equi_join_becomes_hash_join() {
        let (cat, t) = catalog_with_n();
        let (a, b) = (VarId(0), VarId(1));
        // MATCH (a:N), (b:N) WHERE a.val = b.val — a non-PK both-side equi-join.
        let root = optimized_root(
            &cat,
            vec![node_var("a", t), node_var("b", t)],
            vec![a, b],
            prop_cmp(a, b, "val", ScalarOp::Eq, LogicalType::String),
        );
        match root {
            PlanOp::HashJoin {
                probe, build, keys, ..
            } => {
                assert_eq!(keys.len(), 1, "one equi-join key");
                assert!(matches!(*probe, PlanOp::ScanNode(_)), "probe scans a");
                assert!(matches!(*build, PlanOp::ScanNode(_)), "build scans b");
            }
            other => panic!("expected a HashJoin, got {other:?}"),
        }
    }

    #[test]
    fn non_equi_both_sides_stays_a_cross_product() {
        let (cat, t) = catalog_with_n();
        let (a, b) = (VarId(0), VarId(1));
        // a.val < b.val is not an equality → no hash join; stays Filter(CrossProduct).
        let root = optimized_root(
            &cat,
            vec![node_var("a", t), node_var("b", t)],
            vec![a, b],
            prop_cmp(a, b, "val", ScalarOp::Lt, LogicalType::String),
        );
        match root {
            PlanOp::Filter { input, .. } => {
                assert!(matches!(*input, PlanOp::CrossProduct { .. }))
            }
            other => panic!("expected Filter(CrossProduct), got {other:?}"),
        }
    }

    #[test]
    fn equi_plus_residual_is_a_filtered_hash_join() {
        let (cat, t) = catalog_with_n();
        let (a, b) = (VarId(0), VarId(1));
        // a.val = b.val AND a.id < b.id → HashJoin on val with the `<` re-materialized
        // as a Filter above it.
        let pred = BoundExpr::Scalar {
            op: ScalarOp::And,
            args: vec![
                prop_cmp(a, b, "val", ScalarOp::Eq, LogicalType::String),
                prop_cmp(a, b, "id", ScalarOp::Lt, LogicalType::Int64),
            ],
            ty: LogicalType::Bool,
        };
        let root = optimized_root(
            &cat,
            vec![node_var("a", t), node_var("b", t)],
            vec![a, b],
            pred,
        );
        match root {
            PlanOp::Filter { input, .. } => assert!(
                matches!(*input, PlanOp::HashJoin { .. }),
                "expected Filter(HashJoin), got {input:?}"
            ),
            other => panic!("expected Filter(HashJoin), got {other:?}"),
        }
    }

    // --- Cost-based optimizer (P3 step 8) tests ---

    use koko_common::stats::TableStats;

    /// Statistics for table `N`: `rows` rows with all-distinct ids (so a PK equality
    /// estimates ≈ 1 row) and one `val` per id.
    fn stats_for_n(t: TableId, rows: i64) -> StatsMap {
        let mut ts = TableStats::with_columns(2);
        let ids: Vec<Value> = (0..rows).map(Value::Int64).collect();
        let vals: Vec<Value> = (0..rows).map(|i| Value::String(format!("v{i}"))).collect();
        let cols = vec![ids, vals];
        for off in 0..rows as usize {
            ts.record_row(&cols, off);
        }
        let mut stats = StatsMap::new();
        stats.insert(t, ts);
        stats
    }

    #[test]
    fn node_card_reflects_pk_selectivity() {
        let (cat, t) = catalog_with_n();
        let _ = &cat;
        let a = VarId(0);
        let query = BoundQuery {
            vars: vec![node_var("a", t)],
            parts: vec![],
        };
        let stats = stats_for_n(t, 100);
        let unfiltered = crate::cost::node_card(&query, a, None, &stats);
        let pk_eq = cmp(a, ScalarOp::Eq, Value::Int64(7));
        let filtered = crate::cost::node_card(&query, a, Some(&pk_eq), &stats);
        assert!(
            unfiltered >= 90.0,
            "unfiltered ≈ 100 rows, got {unfiltered}"
        );
        assert!(
            filtered <= 2.0,
            "a PK equality estimates ≈ 1 row, got {filtered}"
        );
    }

    #[test]
    fn cost_based_anchor_picks_the_selective_node() {
        let (cat, n, r) = catalog_with_n_and_r();
        let (a, b, e) = (VarId(0), VarId(1), VarId(2));
        // MATCH (a:N)-[e:R]->(b:N) WHERE b.id = 7 — `b` is selective (≈1), so the scan
        // anchors on `b` and extends backward to `a` (not the declaration-order `a`).
        let query = BoundQuery {
            vars: vec![node_var("a", n), node_var("b", n), rel_var("e", r, a, b)],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b],
                        rel_vars: vec![e],
                        ..Default::default()
                    },
                    Some(cmp(b, ScalarOp::Eq, Value::Int64(7))),
                ),
                ..Default::default()
            }],
        };
        let stats = stats_for_n(n, 100);
        let mut plan = plan(&query, &cat, &stats).unwrap();
        // Optimizing pushes `b.id = 7` into the anchor scan — so anchoring on the
        // selective `b` makes it an IndexScan (the step-5 + step-8 composition), and
        // the extend runs backward to `a`.
        optimize(&query, &mut plan, &cat, &stats);
        match plan.parts.pop().unwrap().root {
            PlanOp::Extend(ext) => match *ext.input {
                PlanOp::IndexScan(s) => assert_eq!(s.var, b, "anchor should be the selective `b`"),
                other => panic!("expected an IndexScan on `b` under the extend, got {other:?}"),
            },
            other => panic!("expected an Extend rooted at the `b` anchor, got {other:?}"),
        }
    }

    #[test]
    fn cost_based_build_side_hashes_the_smaller_input() {
        let (cat, t) = catalog_with_n();
        let (a, b) = (VarId(0), VarId(1));
        // MATCH (a:N), (b:N) WHERE a.id = 5 AND a.val = b.val — `a` is pinned to one
        // row (IndexScan), `b` is a full scan, so the hash join builds the smaller `a`.
        let pred = BoundExpr::Scalar {
            op: ScalarOp::And,
            args: vec![
                cmp(a, ScalarOp::Eq, Value::Int64(5)),
                prop_cmp(a, b, "val", ScalarOp::Eq, LogicalType::String),
            ],
            ty: LogicalType::Bool,
        };
        let query = BoundQuery {
            vars: vec![node_var("a", t), node_var("b", t)],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b],
                        ..Default::default()
                    },
                    Some(pred),
                ),
                ..Default::default()
            }],
        };
        let stats = stats_for_n(t, 100);
        let mut plan = plan(&query, &cat, &stats).unwrap();
        optimize(&query, &mut plan, &cat, &stats);
        match plan.parts.pop().unwrap().root {
            PlanOp::HashJoin { build, probe, .. } => {
                assert!(
                    matches!(*build, PlanOp::IndexScan(_)),
                    "should build the smaller (pinned) `a`, got {build:?}"
                );
                assert!(
                    matches!(*probe, PlanOp::ScanNode(_)),
                    "should probe the larger `b`, got {probe:?}"
                );
            }
            other => panic!("expected a HashJoin, got {other:?}"),
        }
    }

    fn rel_stats(rows: i64) -> TableStats {
        let mut ts = TableStats::with_columns(0);
        for off in 0..rows as usize {
            ts.record_row(&[], off);
        }
        ts
    }

    #[test]
    fn extend_fanout_ranks_sparse_rel_below_dense_rel() {
        // 100 nodes; a sparse rel (50 edges ⇒ fan-out 0.5) vs a dense one (1000 ⇒ 10).
        // Cost-based extend order (P3 step 10b L2) must rank the sparse edge cheaper, so
        // a cyclic/multi-path pattern follows it instead of exploding the product.
        let (mut cat, n) = catalog_with_n();
        let sparse = cat.create_rel_table(empty_rel("SR", vec![(n, n)])).unwrap();
        let dense = cat.create_rel_table(empty_rel("DR", vec![(n, n)])).unwrap();
        let _ = &cat;
        let (a, b, c, e1, e2) = (VarId(0), VarId(1), VarId(2), VarId(3), VarId(4));
        let query = BoundQuery {
            vars: vec![
                node_var("a", n),
                node_var("b", n),
                node_var("c", n),
                rel_var("e1", sparse, a, b),
                rel_var("e2", dense, a, c),
            ],
            parts: vec![],
        };
        let mut stats = stats_for_n(n, 100);
        stats.insert(sparse, rel_stats(50));
        stats.insert(dense, rel_stats(1000));
        let f_sparse = crate::cost::extend_fanout(&query, e1, a, &stats);
        let f_dense = crate::cost::extend_fanout(&query, e2, a, &stats);
        assert!(
            f_sparse < f_dense,
            "sparse fan-out {f_sparse} should rank below dense {f_dense}"
        );
    }

    fn catalog_with_property_rel() -> (Catalog, TableId, TableId) {
        let (mut catalog, node) = catalog_with_n();
        let rel = catalog
            .create_rel_table(RelTableDefinition {
                name: "R".to_string(),
                endpoint_pairs: vec![(node, node)],
                columns: vec![
                    ColumnDefinition::plain("weight", LogicalType::Int64),
                    ColumnDefinition::plain("spare", LogicalType::String),
                ],
                storage_direction: RelStorageDirection::default(),
            })
            .unwrap();
        (catalog, node, rel)
    }

    fn property_rel_var(name: &str, table: TableId, src: VarId, dst: VarId) -> VarInfo {
        let mut info = rel_var(name, table, src, dst);
        info.properties = vec![
            PropInfo {
                name: "weight".to_string(),
                column_id: 0,
                ty: LogicalType::Int64,
            },
            PropInfo {
                name: "spare".to_string(),
                column_id: 1,
                ty: LogicalType::String,
            },
        ];
        info
    }

    fn optimized_extend_part(items: Vec<ProjItem>, updating: bool) -> PartPlan {
        let (cat, n, r) = catalog_with_property_rel();
        let (a, b, e) = (VarId(0), VarId(1), VarId(2));
        let query = BoundQuery {
            vars: vec![
                node_var("a", n),
                node_var("b", n),
                property_rel_var("e", r, a, b),
            ],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b],
                        rel_vars: vec![e],
                        ..Default::default()
                    },
                    None,
                ),
                updates: updating
                    .then(|| BoundUpdate::Set(BoundSet { items: Vec::new() }))
                    .into_iter()
                    .collect(),
                projection: Some(BoundProjection {
                    distinct: false,
                    items,
                    order_by: vec![],
                    skip: None,
                    limit: None,
                }),
                ..Default::default()
            }],
        };
        let stats = StatsMap::new();
        let mut plan = plan(&query, &cat, &stats).unwrap();
        optimize(&query, &mut plan, &cat, &stats);
        plan.parts.pop().unwrap()
    }

    fn scalar_item(name: &str, expr: BoundExpr) -> ProjItem {
        ProjItem::Scalar {
            name: name.to_string(),
            expr,
        }
    }

    #[test]
    fn projection_pruning_removes_every_unread_scan_and_extend_property() {
        let part = optimized_extend_part(vec![scalar_item("count(*)", count_star())], false);
        let PlanOp::Extend(extend) = part.root else {
            panic!("expected Extend");
        };
        let PlanOp::ScanNode(scan) = extend.input.as_ref() else {
            panic!("expected ScanNode below Extend");
        };
        assert!(
            scan.tables.iter().all(|table| table.prop_cols.is_empty()),
            "the head scan should gather no unread properties: {scan:?}"
        );
        let ExtendTarget::New { to_tables, .. } = &extend.target else {
            panic!("expected an extend to a new endpoint");
        };
        assert!(
            to_tables.iter().all(|table| table.prop_cols.is_empty()),
            "the unread endpoint should gather no properties: {extend:?}"
        );
        assert!(
            extend
                .branches
                .iter()
                .all(|branch| branch.rel_prop_cols.is_empty()),
            "the unread relationship should gather no properties: {extend:?}"
        );
        assert!(
            extend.carry_cols.is_empty(),
            "no head payload is live above the count-only extend: {extend:?}"
        );
    }

    #[test]
    fn projection_pruning_retains_only_live_endpoint_properties() {
        let (a, b) = (VarId(0), VarId(1));
        let part = optimized_extend_part(
            vec![
                scalar_item("a.val", prop(a, "val", LogicalType::String)),
                scalar_item("b.val", prop(b, "val", LogicalType::String)),
                scalar_item("count(*)", count_star()),
            ],
            false,
        );
        let PlanOp::Extend(extend) = part.root else {
            panic!("expected Extend");
        };
        let PlanOp::ScanNode(scan) = extend.input.as_ref() else {
            panic!("expected ScanNode below Extend");
        };
        let head = &scan.tables[0].prop_cols;
        assert_eq!(
            head.iter()
                .map(|column| column.column_id)
                .collect::<Vec<_>>(),
            vec![1],
            "only a.val should be gathered"
        );
        assert_eq!(
            extend.carry_cols,
            vec![head[0].col_index],
            "only a.val should be copied through the extend"
        );
        let ExtendTarget::New { to_tables, .. } = &extend.target else {
            panic!("expected an extend to a new endpoint");
        };
        assert_eq!(
            to_tables[0]
                .prop_cols
                .iter()
                .map(|column| column.column_id)
                .collect::<Vec<_>>(),
            vec![1],
            "only b.val should be gathered"
        );
        assert!(
            extend
                .branches
                .iter()
                .all(|branch| branch.rel_prop_cols.is_empty()),
            "unread relationship properties should still be pruned"
        );
        assert!(
            !extend.factorize,
            "reading the introduced endpoint must block factorization"
        );
    }

    #[test]
    fn projection_pruning_retains_only_the_live_relationship_property() {
        let rel = VarId(2);
        let part = optimized_extend_part(
            vec![
                scalar_item("e.weight", prop(rel, "weight", LogicalType::Int64)),
                scalar_item("count(*)", count_star()),
            ],
            false,
        );
        let PlanOp::Extend(extend) = part.root else {
            panic!("expected Extend");
        };
        assert_eq!(
            extend.branches[0]
                .rel_prop_cols
                .iter()
                .map(|column| column.column_id)
                .collect::<Vec<_>>(),
            vec![0],
            "weight is live while spare is pruned"
        );
        let ExtendTarget::New { to_tables, .. } = &extend.target else {
            panic!("expected an extend to a new endpoint");
        };
        assert!(
            to_tables[0].prop_cols.is_empty(),
            "the unread endpoint properties remain pruned"
        );
        assert!(
            !extend.factorize,
            "reading a relationship property requires real fan-out rows"
        );
    }

    #[test]
    fn updating_parts_disable_pruning_and_factorization() {
        let part = optimized_extend_part(vec![scalar_item("count(*)", count_star())], true);
        let PlanOp::Extend(extend) = part.root else {
            panic!("expected Extend");
        };
        let PlanOp::ScanNode(scan) = extend.input.as_ref() else {
            panic!("expected ScanNode below Extend");
        };
        assert_eq!(
            scan.tables[0].prop_cols.len(),
            2,
            "updating parts retain the complete head row"
        );
        let ExtendTarget::New { to_tables, .. } = &extend.target else {
            panic!("expected an extend to a new endpoint");
        };
        assert_eq!(
            extend.branches[0].rel_prop_cols.len(),
            2,
            "updating parts retain the complete relationship row"
        );
        assert_eq!(
            to_tables[0].prop_cols.len(),
            2,
            "updating parts retain the complete endpoint row"
        );
        assert!(
            !extend.factorize,
            "an updating part must receive one physical row per match"
        );
    }

    #[test]
    fn missing_projection_disables_pruning_and_factorization() {
        let (cat, n, r) = catalog_with_property_rel();
        let (a, b, e) = (VarId(0), VarId(1), VarId(2));
        let query = BoundQuery {
            vars: vec![
                node_var("a", n),
                node_var("b", n),
                property_rel_var("e", r, a, b),
            ],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b],
                        rel_vars: vec![e],
                        ..Default::default()
                    },
                    None,
                ),
                ..Default::default()
            }],
        };
        let stats = StatsMap::new();
        let mut query_plan = plan(&query, &cat, &stats).unwrap();
        optimize(&query, &mut query_plan, &cat, &stats);
        let PlanOp::Extend(extend) = query_plan.parts.pop().unwrap().root else {
            panic!("expected Extend");
        };
        let PlanOp::ScanNode(scan) = extend.input.as_ref() else {
            panic!("expected ScanNode below Extend");
        };
        assert_eq!(scan.tables[0].prop_cols.len(), 2);
        assert_eq!(extend.branches[0].rel_prop_cols.len(), 2);
        let ExtendTarget::New { to_tables, .. } = &extend.target else {
            panic!("expected an extend to a new endpoint");
        };
        assert_eq!(to_tables[0].prop_cols.len(), 2);
        assert!(!extend.factorize);
    }

    #[test]
    fn multi_table_scan_is_not_rewritten_as_a_single_table_index_lookup() {
        let (mut cat, first) = catalog_with_n();
        let second = cat
            .create_node_table(NodeTableDefinition {
                name: "M".to_string(),
                columns: vec![
                    ColumnDefinition::plain("id", LogicalType::Int64),
                    ColumnDefinition::plain("val", LogicalType::String),
                ],
                primary_key: "id".to_string(),
            })
            .unwrap();
        let a = VarId(0);
        let mut polymorphic = node_var("a", first);
        if let VarKind::Node { tables, label } = &mut polymorphic.kind {
            *tables = vec![first, second];
            label.clear();
        }
        let root = optimized_root(
            &cat,
            vec![polymorphic],
            vec![a],
            cmp(a, ScalarOp::Eq, Value::Int64(1)),
        );
        let PlanOp::Filter { input, .. } = root else {
            panic!("a multi-table scan must keep its filter");
        };
        let PlanOp::ScanNode(scan) = *input else {
            panic!("expected Filter(ScanNode)");
        };
        assert_eq!(scan.tables.len(), 2);
    }

    fn two_scan_plan() -> (Catalog, BoundQuery, QueryPlan, VarId, VarId) {
        let (cat, table) = catalog_with_n();
        let (a, b) = (VarId(0), VarId(1));
        let query = BoundQuery {
            vars: vec![node_var("a", table), node_var("b", table)],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b],
                        ..Default::default()
                    },
                    None,
                ),
                ..Default::default()
            }],
        };
        let plan = plan(&query, &cat, &StatsMap::new()).unwrap();
        (cat, query, plan, a, b)
    }

    #[test]
    fn optional_and_subquery_boundaries_push_only_outer_predicates() {
        for optional in [true, false] {
            let (cat, query, mut plan, a, b) = two_scan_plan();
            let part = &mut plan.parts[0];
            let PlanOp::CrossProduct { left, right, .. } =
                std::mem::replace(&mut part.root, PlanOp::SingleRow)
            else {
                panic!("expected two independent scans");
            };
            let boundary = if optional {
                PlanOp::Optional {
                    input: left,
                    pattern: right,
                    new_cols: Vec::new(),
                }
            } else {
                PlanOp::Subquery {
                    input: left,
                    pattern: right,
                    result_col: part.layout.allocate(LogicalType::Bool),
                    kind: SubqueryKind::Exists,
                }
            };
            part.root = PlanOp::Filter {
                input: Box::new(boundary),
                predicate: BoundExpr::Scalar {
                    op: ScalarOp::And,
                    args: vec![
                        cmp(a, ScalarOp::Eq, Value::Int64(1)),
                        cmp(b, ScalarOp::Eq, Value::Int64(2)),
                    ],
                    ty: LogicalType::Bool,
                },
            };
            optimize(&query, &mut plan, &cat, &StatsMap::new());
            let PlanOp::Filter { input, .. } = plan.parts.pop().unwrap().root else {
                panic!("the predicate on the introduced node must remain above the boundary");
            };
            let outer = match *input {
                PlanOp::Optional { input, pattern, .. }
                | PlanOp::Subquery { input, pattern, .. } => {
                    assert!(
                        matches!(*pattern, PlanOp::ScanNode(_)),
                        "the correlated pattern must remain independent"
                    );
                    input
                }
                other => panic!("expected the correlation boundary, got {other:?}"),
            };
            assert!(
                matches!(*outer, PlanOp::IndexScan(_)),
                "the outer-only predicate should sink to its scan"
            );
        }
    }

    #[test]
    fn sequence_and_value_materialization_are_filter_pushdown_barriers() {
        let (cat, table) = catalog_with_n();
        let a = VarId(0);
        let query = BoundQuery {
            vars: vec![node_var("a", table)],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a],
                        ..Default::default()
                    },
                    None,
                ),
                ..Default::default()
            }],
        };
        for sequence in [true, false] {
            let mut plan = plan(&query, &cat, &StatsMap::new()).unwrap();
            let part = &mut plan.parts[0];
            let scan = std::mem::replace(&mut part.root, PlanOp::SingleRow);
            let boundary = if sequence {
                PlanOp::SequenceCall {
                    input: Box::new(scan),
                    func: SequenceFn::NextVal,
                    name: "s".to_string(),
                    result_col: part.layout.add_sequence_column(0, LogicalType::Int64),
                }
            } else {
                let node_type = LogicalType::Node(table);
                let (id_col, value_col) = part
                    .layout
                    .add_value_column(a, node_type)
                    .expect("the node has no value column yet");
                PlanOp::MaterializeValues {
                    input: Box::new(scan),
                    items: vec![koko_ir::plan::MaterializeItem {
                        id_col,
                        value_col,
                        is_node: true,
                    }],
                }
            };
            part.root = PlanOp::Filter {
                input: Box::new(boundary),
                predicate: cmp(a, ScalarOp::Eq, Value::Int64(1)),
            };
            optimize(&query, &mut plan, &cat, &StatsMap::new());
            let PlanOp::Filter { input, .. } = plan.parts.pop().unwrap().root else {
                panic!("the filter must remain above the opaque boundary");
            };
            match *input {
                PlanOp::SequenceCall { input, .. } | PlanOp::MaterializeValues { input, .. } => {
                    assert!(
                        matches!(*input, PlanOp::ScanNode(_)),
                        "the PK predicate must not cross the boundary"
                    )
                }
                other => panic!("expected an opaque boundary, got {other:?}"),
            }
        }
    }

    fn reset_factorization(op: &mut PlanOp) {
        match op {
            PlanOp::Extend(extend) => extend.factorize = false,
            other => panic!("expected an Extend, got {other:?}"),
        }
    }

    fn factorization_flags(op: &PlanOp, output: &mut Vec<bool>) {
        match op {
            PlanOp::Extend(extend) => {
                output.push(extend.factorize);
                factorization_flags(&extend.input, output);
            }
            PlanOp::VarLengthExtend(extend) => {
                output.push(extend.factorize);
                factorization_flags(&extend.input, output);
            }
            PlanOp::ProjectPath(path) => factorization_flags(&path.input, output),
            PlanOp::Filter { input, .. }
            | PlanOp::Unwind { input, .. }
            | PlanOp::SequenceCall { input, .. }
            | PlanOp::MaterializeValues { input, .. } => factorization_flags(input, output),
            PlanOp::CrossProduct { left, right, .. }
            | PlanOp::HashJoin {
                probe: left,
                build: right,
                ..
            }
            | PlanOp::Optional {
                input: left,
                pattern: right,
                ..
            }
            | PlanOp::Subquery {
                input: left,
                pattern: right,
                ..
            } => {
                factorization_flags(left, output);
                factorization_flags(right, output);
            }
            PlanOp::IndexScan(scan) => {
                if let Some(input) = &scan.input {
                    factorization_flags(input, output);
                }
            }
            PlanOp::SingleRow
            | PlanOp::InputScan
            | PlanOp::ScanNode(_)
            | PlanOp::ScanTableFunc { .. }
            | PlanOp::ScanGraphAlgorithm(_)
            | PlanOp::LoadScan { .. } => {}
        }
    }

    fn marked_factorization(root: PlanOp) -> Vec<bool> {
        let bound = BoundPart {
            projection: Some(BoundProjection {
                distinct: false,
                items: vec![scalar_item("count(*)", count_star())],
                order_by: Vec::new(),
                skip: None,
                limit: None,
            }),
            ..Default::default()
        };
        let mut part = PartPlan {
            root,
            layout: RowLayout::default(),
            inputs: Vec::new(),
            update_ops: Vec::new(),
        };
        mark_factorization(&bound, &mut part);
        let mut flags = Vec::new();
        factorization_flags(&part.root, &mut flags);
        flags
    }

    fn fresh_unfactorized_extend() -> PlanOp {
        let mut root = optimized_extend_root(vec![scalar_item("count(*)", count_star())]);
        reset_factorization(&mut root);
        root
    }

    #[test]
    fn factorization_crosses_only_multiplicity_preserving_boundaries() {
        let true_predicate = BoundExpr::Literal(Value::Bool(true));
        assert_eq!(
            marked_factorization(PlanOp::Filter {
                input: Box::new(fresh_unfactorized_extend()),
                predicate: true_predicate,
            }),
            vec![true],
            "Filter preserves multiplicity"
        );
        assert_eq!(
            marked_factorization(PlanOp::HashJoin {
                probe: Box::new(fresh_unfactorized_extend()),
                build: Box::new(PlanOp::SingleRow),
                probe_cols: (0, 0),
                build_cols: (0, 0),
                keys: Vec::new(),
                kind: JoinKind::Mark {
                    mark_col: 99,
                    kind: SubqueryKind::Exists,
                },
            }),
            vec![true],
            "a Mark join preserves one row per probe row"
        );

        let blockers = vec![
            PlanOp::Unwind {
                input: Box::new(fresh_unfactorized_extend()),
                list: BoundExpr::List {
                    elems: vec![BoundExpr::Literal(Value::Int64(1))],
                    ty: LogicalType::List(Box::new(LogicalType::Int64)),
                },
                target: UnwindTarget::Scalar { col: 99 },
            },
            PlanOp::SequenceCall {
                input: Box::new(fresh_unfactorized_extend()),
                func: SequenceFn::NextVal,
                name: "s".to_string(),
                result_col: 99,
            },
            PlanOp::MaterializeValues {
                input: Box::new(fresh_unfactorized_extend()),
                items: Vec::new(),
            },
            PlanOp::ProjectPath(Box::new(koko_ir::plan::ProjectPath {
                input: Box::new(fresh_unfactorized_extend()),
                path_col: 99,
                head: VarId(0),
                segments: Vec::new(),
            })),
            PlanOp::Optional {
                input: Box::new(fresh_unfactorized_extend()),
                pattern: Box::new(PlanOp::SingleRow),
                new_cols: Vec::new(),
            },
            PlanOp::Subquery {
                input: Box::new(fresh_unfactorized_extend()),
                pattern: Box::new(PlanOp::SingleRow),
                result_col: 99,
                kind: SubqueryKind::Exists,
            },
            PlanOp::CrossProduct {
                left: Box::new(fresh_unfactorized_extend()),
                left_width: 0,
                right: Box::new(PlanOp::SingleRow),
                right_width: 0,
            },
            PlanOp::HashJoin {
                probe: Box::new(fresh_unfactorized_extend()),
                build: Box::new(PlanOp::SingleRow),
                probe_cols: (0, 0),
                build_cols: (0, 0),
                keys: Vec::new(),
                kind: JoinKind::Inner,
            },
            PlanOp::HashJoin {
                probe: Box::new(fresh_unfactorized_extend()),
                build: Box::new(PlanOp::SingleRow),
                probe_cols: (0, 0),
                build_cols: (0, 0),
                keys: Vec::new(),
                kind: JoinKind::Left,
            },
        ];
        for blocker in blockers {
            assert_eq!(
                marked_factorization(blocker),
                vec![false],
                "fan-out or opaque boundaries must block multiplicity"
            );
        }
    }

    #[test]
    fn planner_uses_the_lowest_fanout_extend_first() {
        let (mut cat, n) = catalog_with_n();
        let sparse = cat.create_rel_table(empty_rel("SR", vec![(n, n)])).unwrap();
        let dense = cat.create_rel_table(empty_rel("DR", vec![(n, n)])).unwrap();
        let (a, b, c, sparse_rel, dense_rel) = (VarId(0), VarId(1), VarId(2), VarId(3), VarId(4));
        let query = BoundQuery {
            vars: vec![
                node_var("a", n),
                node_var("b", n),
                node_var("c", n),
                rel_var("sparse", sparse, a, b),
                rel_var("dense", dense, a, c),
            ],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b, c],
                        rel_vars: vec![dense_rel, sparse_rel],
                        ..Default::default()
                    },
                    None,
                ),
                ..Default::default()
            }],
        };
        let mut stats = stats_for_n(n, 100);
        stats.insert(sparse, rel_stats(50));
        stats.insert(dense, rel_stats(1000));
        let mut query_plan = plan(&query, &cat, &stats).unwrap();
        let PlanOp::Extend(outer) = query_plan.parts.pop().unwrap().root else {
            panic!("expected the second extend");
        };
        let PlanOp::Extend(inner) = outer.input.as_ref() else {
            panic!("expected the first extend below it");
        };
        assert_eq!(
            inner.branches[0].rel_table, sparse,
            "the sparse relationship must be extended before the declaration-first dense one"
        );
        assert_eq!(outer.branches[0].rel_table, dense);
    }

    #[test]
    fn recursive_relationship_keeps_declaration_order_anchor() {
        let (cat, n, r) = catalog_with_n_and_r();
        let (a, b, e) = (VarId(0), VarId(1), VarId(2));
        let mut recursive = rel_var("e", r, a, b);
        let VarKind::Rel {
            recursive: recursive_spec,
            ..
        } = &mut recursive.kind
        else {
            unreachable!()
        };
        *recursive_spec = Some(Box::new(koko_ir::bound::RecursiveSpec {
            lower: 1,
            upper: 2,
            mode: koko_ir::bound::RecursiveMode::All,
            semantic: koko_ir::bound::PathSemantic::Walk,
            filter: None,
            weight: None,
        }));
        let query = BoundQuery {
            vars: vec![node_var("a", n), node_var("b", n), recursive],
            parts: vec![BoundPart {
                reading: required_match(
                    BoundMatch {
                        node_vars: vec![a, b],
                        rel_vars: vec![e],
                        ..Default::default()
                    },
                    Some(cmp(b, ScalarOp::Eq, Value::Int64(7))),
                ),
                ..Default::default()
            }],
        };
        let stats = stats_for_n(n, 100);
        let mut query_plan = plan(&query, &cat, &stats).unwrap();
        let PlanOp::Filter { input, .. } = query_plan.parts.pop().unwrap().root else {
            panic!("the introduced endpoint predicate remains above the recursive extend");
        };
        let PlanOp::VarLengthExtend(extend) = *input else {
            panic!("expected VariableLengthExtend");
        };
        let PlanOp::ScanNode(scan) = extend.input.as_ref() else {
            panic!("expected the declaration-first anchor scan");
        };
        assert_eq!(
            scan.var, a,
            "recursive path direction is order-sensitive, so selective b must not become the anchor"
        );
    }
}
