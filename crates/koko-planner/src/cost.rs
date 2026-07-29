//! Cardinality cost model (P3 step 8): estimates over the P3 in-memory statistics
//! ([`koko_common::stats::TableStats`]), consumed by cost-based **join order**
//! (anchor selection, in [`crate::PlanBuilder::build_match`]) and the hash-join
//! **build-side** choice (in [`crate::optimize`]).
//!
//! It is deliberately crude — its only job is to *rank* alternatives, and every
//! decision it drives is **result-neutral** (a worse estimate yields a slower plan,
//! never a wrong answer; inner joins commute, and the test runner sorts results).
//! With no statistics (the [`StatsMap`] empty) every estimate falls back to a
//! constant, so the chosen plan is identical to the pre-step-8 greedy planner.

use koko_common::TableId;
use koko_common::stats::TableStats;
use koko_function::ScalarOp;
use koko_ir::bound::{BoundExpr, BoundQuery, VarId};
use koko_ir::plan::PlanOp;
use std::collections::HashMap;

/// A snapshot of per-table statistics, keyed by table id (node + rel tables). Built
/// once per query from the storage backend and threaded into the planner/optimizer.
pub type StatsMap = HashMap<TableId, TableStats>;

/// Cardinality assumed for a table / subtree with **no** statistics — large enough
/// that a known-small side is preferred over an unknown one, but not so large that it
/// swamps a known-large one. Ties (e.g. all-unknown) leave the greedy order intact.
const DEFAULT_CARD: f64 = 1000.0;
/// Assumed average fan-out of an extend with no rel statistics.
const DEFAULT_FANOUT: f64 = 3.0;
/// Assumed selectivity of a residual filter.
const DEFAULT_FILTER_SEL: f64 = 0.3;
/// Assumed shrink factor of closing a both-bound edge (an existence filter), used by
/// the anchor-choice peak simulation (P3 step 10b L2b).
pub const CLOSE_SEL: f64 = 0.3;

/// Summed committed row count of a set of candidate tables, or `None` if **none** of
/// them have statistics (so the caller can fall back to a default).
fn tables_rows(tables: &[TableId], stats: &StatsMap) -> Option<f64> {
    let mut total = 0.0;
    let mut any = false;
    for t in tables {
        if let Some(s) = stats.get(t) {
            total += s.num_tuples() as f64;
            any = true;
        }
    }
    any.then_some(total)
}

/// Estimated cardinality of scanning node variable `var`, reduced by any
/// `var.<col> = <const>` equality in `where_pred`. Each such equality divides the
/// estimate by that column's distinct count — so a **primary-key** equality (whose
/// distinct count ≈ the row count) lands near `1`, which is exactly the anchor a
/// cost-based join order wants (and which step-5 filter-pushdown then turns into an
/// `IndexScan`). Returns [`DEFAULT_CARD`] when the table has no statistics.
pub fn node_card(
    query: &BoundQuery,
    var: VarId,
    where_pred: Option<&BoundExpr>,
    stats: &StatsMap,
) -> f64 {
    let info = query.var(var);
    let tables = info.node_tables();
    let Some(base) = tables_rows(tables, stats) else {
        return DEFAULT_CARD;
    };
    let mut card = base;
    if let Some(pred) = where_pred {
        let mut conjuncts = Vec::new();
        collect_conjuncts(pred, &mut conjuncts);
        for conj in conjuncts {
            let Some(prop) = eq_const_prop(conj, var) else {
                continue;
            };
            // Distinct count of that property on the representative (first) table.
            let distinct = tables
                .first()
                .and_then(|t| stats.get(t))
                .and_then(|s| {
                    info.properties
                        .iter()
                        .find(|p| p.name.eq_ignore_ascii_case(prop))
                        .and_then(|p| s.column(p.column_id as usize))
                })
                .map(|cs| cs.num_distinct().max(1) as f64)
                .unwrap_or(1.0);
            card /= distinct.max(1.0);
        }
    }
    card.max(1.0)
}

/// Average fan-out of extending relationship `rel` from a node of variable `from`:
/// `rel_rows / from_node_rows` (the mean out-degree). Drives cost-based **extend
/// order** in [`crate::PlanBuilder::build_match`] — a selective edge (e.g. `knows`,
/// fanned out from a person) is preferred over re-deriving a node through a wide
/// relation, which is what stops a cyclic/multi-path pattern (lsqb q2/q3) from
/// materializing the full node cross-product before the cycle closes. Returns
/// [`DEFAULT_FANOUT`] when either side lacks statistics.
pub fn extend_fanout(query: &BoundQuery, rel: VarId, from: VarId, stats: &StatsMap) -> f64 {
    let rel_rows = tables_rows(query.var(rel).rel_tables(), stats);
    let from_rows = tables_rows(query.var(from).node_tables(), stats);
    match (rel_rows, from_rows) {
        (Some(r), Some(f)) if f > 0.0 => (r / f).max(0.01),
        _ => DEFAULT_FANOUT,
    }
}

/// Estimated cardinality of a planned subtree, for the hash-join build-side choice
/// (build the smaller side). Crude and monotonic; see the module note.
pub fn plan_card(op: &PlanOp, stats: &StatsMap) -> f64 {
    match op {
        PlanOp::SingleRow => 1.0,
        PlanOp::InputScan => DEFAULT_CARD,
        PlanOp::ScanNode(s) => {
            let tables: Vec<TableId> = s.tables.iter().map(|t| t.table).collect();
            tables_rows(&tables, stats).unwrap_or(DEFAULT_CARD)
        }
        PlanOp::IndexScan(s) => s
            .input
            .as_ref()
            .map_or(1.0, |input| plan_card(input, stats)),
        PlanOp::ScanTableFunc { .. } | PlanOp::LoadScan { .. } => DEFAULT_CARD,
        PlanOp::Filter { input, .. } => plan_card(input, stats) * DEFAULT_FILTER_SEL,
        PlanOp::Extend(e) => plan_card(&e.input, stats) * DEFAULT_FANOUT,
        PlanOp::VarLengthExtend(e) => plan_card(&e.input, stats) * DEFAULT_FANOUT * DEFAULT_FANOUT,
        PlanOp::ProjectPath(p) => plan_card(&p.input, stats),
        PlanOp::Unwind { input, .. } => plan_card(input, stats) * DEFAULT_FANOUT,
        PlanOp::CrossProduct { left, right, .. } => {
            plan_card(left, stats) * plan_card(right, stats)
        }
        // The join output is bounded above by the larger input; good enough to rank.
        PlanOp::HashJoin { probe, build, .. } => {
            plan_card(probe, stats).max(plan_card(build, stats))
        }
        PlanOp::Optional { input, .. }
        | PlanOp::Subquery { input, .. }
        | PlanOp::SequenceCall { input, .. }
        | PlanOp::MaterializeValues { input, .. } => plan_card(input, stats),
    }
}

/// Flatten a predicate's top-level `AND` conjuncts into `out` (borrowing).
fn collect_conjuncts<'a>(e: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match e {
        BoundExpr::Scalar {
            op: ScalarOp::And,
            args,
            ..
        } => {
            for a in args {
                collect_conjuncts(a, out);
            }
        }
        other => out.push(other),
    }
}

/// If `conj` is `var.<prop> = <const>` (in either operand order), return `<prop>`.
fn eq_const_prop(conj: &BoundExpr, var: VarId) -> Option<&str> {
    let args = match conj {
        BoundExpr::Scalar {
            op: ScalarOp::Eq,
            args,
            ..
        } if args.len() == 2 => args,
        _ => return None,
    };
    match (&args[0], &args[1]) {
        (BoundExpr::Property { var: v, prop, .. }, other)
        | (other, BoundExpr::Property { var: v, prop, .. })
            if *v == var && is_const_expr(other) =>
        {
            Some(prop.as_str())
        }
        _ => None,
    }
}

/// A compile-time constant (a literal or a CAST), for the cost model's selectivity.
fn is_const_expr(e: &BoundExpr) -> bool {
    matches!(e, BoundExpr::Literal(_) | BoundExpr::Cast { .. })
}
