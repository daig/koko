//! Turn a [`BoundQuery`]'s query graph into a logical operator tree and a
//! [`RowLayout`] assigning variables and properties to runtime columns.
//!
//! Planning connects pattern components with scans and extensions, uses catalog
//! statistics to choose anchors and join sides, and cross-products disconnected
//! components. Later optimizer passes may push predicates, prune columns,
//! factorize fan-out, or decorrelate eligible subqueries.

mod r#match;
mod subquery;
mod update;

use crate::cost::{self, StatsMap};
use koko_catalog::Catalog;
use koko_common::{ExtendDir, LogicalType, Result, TableId};
use koko_ir::bound::{
    BoundExpr, BoundGraphAlgorithmConfig, BoundGraphAlgorithmOutput, BoundMatch,
    BoundOptionalMatch, BoundPart, BoundQuery, BoundReadingClause, BoundRegularQuery, BoundUnwind,
    OrderKey, PathSemantic, ProjItem, RecursiveFilter, RecursiveMode, SubqueryKind, VarId, VarKind,
};
use koko_ir::plan::*;
use r#match::{
    bwd_or_both, collect_expr_vars, collect_sequence_ids, collect_value_consumed_vars, fwd_or_both,
};
use std::collections::HashSet;
use subquery::{collect_subquery_ids, plan_subquery};

/// Building a whole decorrelated relation is wasteful for a selective outer pipeline; below this
/// estimate, repeated seeded probes retain less state and usually perform less work.
const DECORRELATE_MIN_PROBE_ROWS: f64 = 1024.0;

/// Plan a `UNION` query: one [`QueryPlan`] per operand. `stats` (the P3 cost model's
/// input) drives cost-based join order; pass an empty map for the pre-step-8 greedy
/// plan.
pub fn plan_regular(
    rq: &BoundRegularQuery,
    catalog: &Catalog,
    stats: &StatsMap,
) -> Result<RegularPlan> {
    let operands = rq
        .operands
        .iter()
        .map(|q| plan(q, catalog, stats))
        .collect::<Result<Vec<_>>>()?;
    Ok(RegularPlan {
        operands,
        distinct: rq.distinct,
    })
}

/// Plan a bound query: one [`PartPlan`] per part, in order.
pub fn plan(query: &BoundQuery, catalog: &Catalog, stats: &StatsMap) -> Result<QueryPlan> {
    let parts = query
        .parts
        .iter()
        .map(|part| plan_part(query, part, catalog, stats))
        .collect::<Result<Vec<_>>>()?;
    Ok(QueryPlan { parts })
}

/// Plan one query part's reading portion (match graph + unwinds + filters),
/// seeded by the part's carried input scope.
fn plan_part(
    query: &BoundQuery,
    part: &BoundPart,
    catalog: &Catalog,
    stats: &StatsMap,
) -> Result<PartPlan> {
    let mut b = PlanBuilder {
        query,
        catalog,
        stats,
        layout: RowLayout::default(),
        bound: HashSet::new(),
        recursive_value_rels: HashSet::new(),
        recursive_path_rels: HashSet::new(),
    };
    // Every table name, for evaluating `label()`/`labels()` at runtime.
    for id in catalog.node_table_ids() {
        b.layout
            .table_names
            .insert(id, catalog.node_table(id).unwrap().name().to_string());
    }
    for id in catalog.rel_table_ids() {
        let name = catalog.rel_table(id).unwrap().name().to_string();
        for (member, _, _) in catalog.rel_members(id) {
            b.layout.table_names.insert(member, name.clone());
        }
    }

    // Carried variables occupy the first layout columns; `InputScan` is the base
    // that replays the previous part's projected rows into them. A scalar takes one
    // column; a carried node takes a full binding (id + property columns) so it can
    // be re-extended from and have its properties read.
    let mut inputs = Vec::with_capacity(part.input_vars.len());
    for &v in &part.input_vars {
        let info = query.var(v);
        let slot = if info.is_scalar() {
            InputSlot::Scalar {
                col: b.layout.add_scalar(v, info.scalar_type()),
            }
        } else {
            // A carried node is unpacked through the mapping for its runtime table.
            let scan = b.make_scan(v);
            InputSlot::Node {
                id_col: scan.id_col,
                prop_tables: scan.tables,
            }
        };
        b.bound.insert(v);
        inputs.push(slot);
    }

    let has_input = !part.input_vars.is_empty();
    let mut root = if has_input {
        PlanOp::InputScan
    } else {
        PlanOp::SingleRow
    };
    let mut has_pipeline = has_input;
    let mut planned_subqueries = HashSet::new();
    let mut planned_sequences = HashSet::new();
    b.layout.ensure_subquery_slots(part.subqueries.len());

    // `WITH … WHERE` belongs to the carried input and therefore runs before
    // every reading clause in this part.
    if let Some(predicate) = &part.input_filter {
        root = prepare_expression(
            &mut b,
            root,
            predicate,
            part,
            &mut planned_subqueries,
            &mut planned_sequences,
        )?;
        root = PlanOp::Filter {
            input: Box::new(root),
            predicate: predicate.clone(),
        };
    }

    // Fold the reading pipeline from left to right. A MATCH may optimize its
    // own graph, but no clause is moved across another clause's boundary.
    for clause in &part.reading {
        match clause {
            BoundReadingClause::Match {
                match_,
                where_predicate,
            } => {
                let base = if has_pipeline { Some(root) } else { None };
                root = b.build_match(match_, where_predicate.as_ref(), base)?;
                has_pipeline = true;
                if let Some(predicate) = where_predicate {
                    root = prepare_expression(
                        &mut b,
                        root,
                        predicate,
                        part,
                        &mut planned_subqueries,
                        &mut planned_sequences,
                    )?;
                    root = PlanOp::Filter {
                        input: Box::new(root),
                        predicate: predicate.clone(),
                    };
                }
            }
            BoundReadingClause::OptionalMatch(optional) => {
                root = plan_optional_clause(
                    &mut b,
                    root,
                    optional,
                    part,
                    &mut planned_subqueries,
                    &mut planned_sequences,
                )?;
                has_pipeline = true;
            }
            BoundReadingClause::Unwind(unwind) => {
                root = prepare_expression(
                    &mut b,
                    root,
                    &unwind.list,
                    part,
                    &mut planned_subqueries,
                    &mut planned_sequences,
                )?;
                root = b.make_unwind(root, unwind);
                if query.var(unwind.var).is_node() {
                    b.bound.insert(unwind.var);
                }
                has_pipeline = true;
            }
            BoundReadingClause::TableFunction {
                scan,
                where_predicate,
            } => {
                let left_width = b.layout.width();
                let cols = scan
                    .columns
                    .iter()
                    .map(|(var, ty)| b.layout.add_scalar(*var, ty.clone()))
                    .collect();
                let source = PlanOp::ScanTableFunc {
                    call: scan.call.clone(),
                    cols,
                };
                root = if has_pipeline {
                    let right_width = b.layout.width() - left_width;
                    PlanOp::CrossProduct {
                        left: Box::new(root),
                        left_width,
                        right: Box::new(source),
                        right_width,
                    }
                } else {
                    source
                };
                has_pipeline = true;
                if let Some(predicate) = where_predicate {
                    root = prepare_expression(
                        &mut b,
                        root,
                        predicate,
                        part,
                        &mut planned_subqueries,
                        &mut planned_sequences,
                    )?;
                    root = PlanOp::Filter {
                        input: Box::new(root),
                        predicate: predicate.clone(),
                    };
                }
            }
            BoundReadingClause::GraphAlgorithm {
                scan,
                where_predicate,
            } => {
                let left_width = b.layout.width();
                let source = match (scan.output, scan.call.function, scan.call.config) {
                    (
                        BoundGraphAlgorithmOutput::TopologicalLevels { node, level },
                        koko_function::BuiltinGraphAlgorithm::TopologicalLevels,
                        BoundGraphAlgorithmConfig::TopologicalLevels,
                    ) => {
                        let node_scan = b.make_scan(node);
                        b.bound.insert(node);
                        let level_col = b.layout.add_scalar(level, query.var(level).scalar_type());
                        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::TopologicalLevels(
                            TopologicalLevelsPlan {
                                id: scan.id,
                                graph: scan.call.graph.clone(),
                                node: node_scan,
                                level_col,
                            },
                        ))
                    }
                    (
                        BoundGraphAlgorithmOutput::PageRank { node, score },
                        koko_function::BuiltinGraphAlgorithm::PageRank,
                        BoundGraphAlgorithmConfig::PageRank(config),
                    ) => {
                        let node_scan = b.make_scan(node);
                        b.bound.insert(node);
                        let score_col = b.layout.add_scalar(score, query.var(score).scalar_type());
                        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::PageRank(PageRankPlan {
                            id: scan.id,
                            graph: scan.call.graph.clone(),
                            config,
                            node: node_scan,
                            score_col,
                        }))
                    }
                    (
                        BoundGraphAlgorithmOutput::KCoreDecomposition { node, core },
                        koko_function::BuiltinGraphAlgorithm::KCoreDecomposition,
                        BoundGraphAlgorithmConfig::TopologicalLevels,
                    ) => {
                        let node_scan = b.make_scan(node);
                        b.bound.insert(node);
                        let core_col = b.layout.add_scalar(core, query.var(core).scalar_type());
                        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::KCoreDecomposition(
                            KCorePlan {
                                id: scan.id,
                                graph: scan.call.graph.clone(),
                                node: node_scan,
                                core_col,
                            },
                        ))
                    }
                    (
                        BoundGraphAlgorithmOutput::Louvain { node, community_id },
                        koko_function::BuiltinGraphAlgorithm::Louvain,
                        BoundGraphAlgorithmConfig::Louvain(config),
                    ) => {
                        let node_scan = b.make_scan(node);
                        b.bound.insert(node);
                        let community_col = b
                            .layout
                            .add_scalar(community_id, query.var(community_id).scalar_type());
                        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::Louvain(LouvainPlan {
                            id: scan.id,
                            graph: scan.call.graph.clone(),
                            node: node_scan,
                            community_col,
                            max_iterations: config.max_iterations,
                            max_phases: config.max_phases,
                        }))
                    }
                    (
                        BoundGraphAlgorithmOutput::WeaklyConnectedComponents { node, component_id },
                        koko_function::BuiltinGraphAlgorithm::WeaklyConnectedComponents,
                        BoundGraphAlgorithmConfig::TopologicalLevels,
                    ) => {
                        let node_scan = b.make_scan(node);
                        b.bound.insert(node);
                        let component_id_col = b
                            .layout
                            .add_scalar(component_id, query.var(component_id).scalar_type());
                        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::WeaklyConnectedComponents(
                            WeaklyConnectedComponentsPlan {
                                id: scan.id,
                                graph: scan.call.graph.clone(),
                                node: node_scan,
                                component_id_col,
                            },
                        ))
                    }
                    (
                        BoundGraphAlgorithmOutput::StronglyConnectedComponents {
                            node,
                            component_id,
                        },
                        koko_function::BuiltinGraphAlgorithm::StronglyConnectedComponents,
                        BoundGraphAlgorithmConfig::TopologicalLevels,
                    ) => {
                        let node_scan = b.make_scan(node);
                        b.bound.insert(node);
                        let component_col = b
                            .layout
                            .add_scalar(component_id, query.var(component_id).scalar_type());
                        PlanOp::ScanGraphAlgorithm(GraphAlgorithmPlan::StronglyConnectedComponents(
                            StronglyConnectedComponentsPlan {
                                id: scan.id,
                                graph: scan.call.graph.clone(),
                                node: node_scan,
                                component_col,
                            },
                        ))
                    }
                    _ => unreachable!("graph algorithm identity, output, and config must agree"),
                };
                root = if has_pipeline {
                    let right_width = b.layout.width() - left_width;
                    PlanOp::CrossProduct {
                        left: Box::new(root),
                        left_width,
                        right: Box::new(source),
                        right_width,
                    }
                } else {
                    source
                };
                has_pipeline = true;
                if let Some(predicate) = where_predicate {
                    root = prepare_expression(
                        &mut b,
                        root,
                        predicate,
                        part,
                        &mut planned_subqueries,
                        &mut planned_sequences,
                    )?;
                    root = PlanOp::Filter {
                        input: Box::new(root),
                        predicate: predicate.clone(),
                    };
                }
            }
            BoundReadingClause::Load {
                scan,
                where_predicate,
            } => {
                let left_width = b.layout.width();
                let cols = scan
                    .columns
                    .iter()
                    .map(|(var, ty)| b.layout.add_scalar(*var, ty.clone()))
                    .collect();
                let source = PlanOp::LoadScan {
                    cols,
                    col_names: scan.col_names.clone(),
                    path: scan.path.clone(),
                    paths: scan.paths.clone(),
                    format: scan.format,
                    options: scan.options.clone(),
                    bare: scan.bare,
                };
                root = if has_pipeline {
                    let right_width = b.layout.width() - left_width;
                    PlanOp::CrossProduct {
                        left: Box::new(root),
                        left_width,
                        right: Box::new(source),
                        right_width,
                    }
                } else {
                    source
                };
                has_pipeline = true;
                if let Some(predicate) = where_predicate {
                    root = prepare_expression(
                        &mut b,
                        root,
                        predicate,
                        part,
                        &mut planned_subqueries,
                        &mut planned_sequences,
                    )?;
                    root = PlanOp::Filter {
                        input: Box::new(root),
                        predicate: predicate.clone(),
                    };
                }
            }
        }
    }

    // Projection expressions consume the final reading rows. Prepare their
    // lifted dependencies here; the processor performs the projection itself.
    if let Some(projection) = &part.projection {
        for item in &projection.items {
            if let ProjItem::Scalar { expr, .. } = item {
                root = prepare_expression(
                    &mut b,
                    root,
                    expr,
                    part,
                    &mut planned_subqueries,
                    &mut planned_sequences,
                )?;
            }
        }
        for (key, _) in &projection.order_by {
            if let OrderKey::Expr(expression) | OrderKey::PostProjection(expression) = key {
                root = prepare_expression(
                    &mut b,
                    root,
                    expression,
                    part,
                    &mut planned_subqueries,
                    &mut planned_sequences,
                )?;
            }
        }
    }

    // A subquery referenced only by an update or another non-projection
    // expression still needs a result column. Nested subqueries are planned by
    // their owning outer subquery.
    let nested_subqueries: HashSet<usize> = part
        .subqueries
        .iter()
        .flat_map(|subquery| {
            subquery
                .where_predicate
                .as_ref()
                .map(collect_subquery_ids)
                .unwrap_or_default()
        })
        .collect();
    for (id, subquery) in part.subqueries.iter().enumerate() {
        if planned_subqueries.contains(&id) || nested_subqueries.contains(&id) {
            continue;
        }
        let result_type = match subquery.kind {
            SubqueryKind::Exists => LogicalType::Bool,
            SubqueryKind::Count => LogicalType::Int64,
        };
        let result_col = b.layout.add_subquery_column(id, result_type);
        root = plan_subquery(&mut b, root, subquery, result_col, &part.subqueries)?;
        planned_subqueries.insert(id);
    }

    // Sequence calls not consumed by a reading predicate/list belong to the
    // final projection or update and execute for every surviving row.
    for (id, sequence) in part.sequence_calls.iter().enumerate() {
        if planned_sequences.contains(&id) {
            continue;
        }
        let result_col = b.layout.add_sequence_column(id, LogicalType::Int64);
        root = PlanOp::SequenceCall {
            input: Box::new(root),
            func: sequence.func,
            name: sequence.name.clone(),
            result_col,
        };
        planned_sequences.insert(id);
    }

    // Plan the updating clauses, in order, on the shared builder (so columns for
    // created/merged variables are allocated in the layout — a created/merged node
    // can be carried through `WITH` or projected by `RETURN` — and each `MERGE`'s
    // bound variables are visible to later clauses).
    let update_ops = part
        .updates
        .iter()
        .map(|update| b.plan_update(update))
        .collect::<Result<Vec<_>>>()?;

    Ok(PartPlan {
        root,
        layout: b.layout,
        inputs,
        update_ops,
    })
}

/// The layout columns allocated for a node variable (the shared return of
/// `alloc_node`).
struct NodeAlloc {
    tables: Vec<TableId>,
    id_col: usize,
    /// `(property name → layout column)` for every union property, in order.
    name_to_col: Vec<(String, usize)>,
}

/// The layout columns allocated for a relationship variable (the return of
/// `alloc_rel`).
struct RelAlloc {
    id_col: usize,
    /// `(property name → layout column)` for every union property, in order.
    name_to_col: Vec<(String, usize)>,
    props: Vec<LayoutProp>,
}

/// One step the `build_match` loop takes: extend to a new node, close a both-bound
/// edge (a residual existence filter), or cross-product in a fresh node scan.
enum RelMove {
    ExtendNew(VarId),
    Close(VarId),
    Cross(VarId),
}

struct PlanBuilder<'q> {
    query: &'q BoundQuery,
    catalog: &'q Catalog,
    stats: &'q StatsMap,
    layout: RowLayout,
    bound: HashSet<VarId>,
    /// Recursive-rel variables whose `{_NODES, _RELS}` value must be assembled
    /// (the rel is named, or it is part of a named path); recomputed per match.
    recursive_value_rels: HashSet<VarId>,
    /// Rels that are segments of a named path (`MATCH p = …`).
    recursive_path_rels: HashSet<VarId>,
}

/// Add any whole-node/relationship values consumed by `expression` to the
/// current row before the expression is evaluated.
fn materialize_expression_values(
    builder: &mut PlanBuilder<'_>,
    root: PlanOp,
    expression: &BoundExpr,
) -> PlanOp {
    let mut consumed = HashSet::new();
    collect_value_consumed_vars(expression, true, &mut consumed);
    let mut consumed: Vec<_> = consumed.into_iter().collect();
    consumed.sort_unstable();

    let mut items = Vec::new();
    for var in consumed {
        let info = builder.query.var(var);
        if !info.is_assembled_graph_var() {
            continue;
        }
        let (ty, is_node) = match &info.kind {
            VarKind::Node { tables, .. } => (tables.first().copied().map(LogicalType::Node), true),
            VarKind::Rel { tables, .. } => (tables.first().copied().map(LogicalType::Rel), false),
            VarKind::Path { .. } | VarKind::Scalar { .. } => unreachable!(),
        };
        let Some(ty) = ty else {
            continue;
        };
        let Some((id_col, value_col)) = builder.layout.add_value_column(var, ty) else {
            continue;
        };
        items.push(MaterializeItem {
            id_col,
            value_col,
            is_node,
        });
    }

    if items.is_empty() {
        root
    } else {
        PlanOp::MaterializeValues {
            input: Box::new(root),
            items,
        }
    }
}

/// Prepare lifted subqueries, sequence calls, and whole graph values exactly
/// where an expression is consumed in the reading pipeline.
fn prepare_expression(
    builder: &mut PlanBuilder<'_>,
    mut root: PlanOp,
    expression: &BoundExpr,
    part: &BoundPart,
    planned_subqueries: &mut HashSet<usize>,
    planned_sequences: &mut HashSet<usize>,
) -> Result<PlanOp> {
    root = materialize_expression_values(builder, root, expression);

    let mut subquery_ids: Vec<_> = collect_subquery_ids(expression).into_iter().collect();
    subquery_ids.sort_unstable();
    for id in subquery_ids {
        if planned_subqueries.contains(&id) {
            continue;
        }
        let subquery = &part.subqueries[id];
        let result_type = match subquery.kind {
            SubqueryKind::Exists => LogicalType::Bool,
            SubqueryKind::Count => LogicalType::Int64,
        };
        let result_col = builder.layout.add_subquery_column(id, result_type);
        root = plan_subquery(builder, root, subquery, result_col, &part.subqueries)?;
        planned_subqueries.insert(id);
    }

    let mut sequence_ids: Vec<_> = collect_sequence_ids(expression).into_iter().collect();
    sequence_ids.sort_unstable();
    for id in sequence_ids {
        if planned_sequences.contains(&id) {
            continue;
        }
        let sequence = &part.sequence_calls[id];
        let result_col = builder.layout.add_sequence_column(id, LogicalType::Int64);
        root = PlanOp::SequenceCall {
            input: Box::new(root),
            func: sequence.func,
            name: sequence.name.clone(),
            result_col,
        };
        planned_sequences.insert(id);
    }

    Ok(root)
}

/// Plan one OPTIONAL MATCH at its textual position. Its branch starts from the
/// incoming row, and every column introduced inside the branch is NULL-extended
/// when the branch produces no rows.
fn plan_optional_clause(
    builder: &mut PlanBuilder<'_>,
    mut root: PlanOp,
    optional: &BoundOptionalMatch,
    part: &BoundPart,
    planned_subqueries: &mut HashSet<usize>,
    planned_sequences: &mut HashSet<usize>,
) -> Result<PlanOp> {
    let predicate_subqueries = optional
        .where_predicate
        .as_ref()
        .map(collect_subquery_ids)
        .unwrap_or_default();
    let predicate_sequences = optional
        .where_predicate
        .as_ref()
        .map(collect_sequence_ids)
        .unwrap_or_default();

    let decorrelate = predicate_subqueries.is_empty()
        && predicate_sequences.is_empty()
        && cost::plan_card(&root, builder.stats) >= DECORRELATE_MIN_PROBE_ROWS;
    if decorrelate
        && let Some(correlation) =
            builder.correlated_nodes(&optional.match_, optional.where_predicate.as_ref())
        && correlation.len() == 1
    {
        return builder.build_decorrelated_join(
            root,
            &correlation,
            &optional.match_,
            JoinKind::Left,
        );
    }

    // Values owned by the incoming row must be materialized before entering
    // the nullable branch. Otherwise an unmatched OPTIONAL would NULL-extend
    // that outer value together with genuinely branch-local columns.
    if let Some(predicate) = &optional.where_predicate {
        root = materialize_expression_values(builder, root, predicate);
    }

    let width_before = builder.layout.width();
    let mut pattern = builder.build_match(
        &optional.match_,
        optional.where_predicate.as_ref(),
        Some(PlanOp::InputScan),
    )?;
    if let Some(predicate) = &optional.where_predicate {
        pattern = prepare_expression(
            builder,
            pattern,
            predicate,
            part,
            planned_subqueries,
            planned_sequences,
        )?;
        pattern = PlanOp::Filter {
            input: Box::new(pattern),
            predicate: predicate.clone(),
        };
    }
    let new_cols = (width_before..builder.layout.width()).collect();
    Ok(PlanOp::Optional {
        input: Box::new(root),
        pattern: Box::new(pattern),
        new_cols,
    })
}

impl PlanBuilder<'_> {
    /// Build the operator tree for one match graph (a required match, or an
    /// `OPTIONAL MATCH` sub-pattern). `base` selects the leaf: `Some(op)` uses `op`
    /// as the root and cross-products/extends the match's fresh scans onto it — used
    /// for the carried scope (`InputScan`), an optional's per-row seed, and a CSV
    /// `LOAD FROM` source; `None` scans the first node directly. Already-bound
    /// variables (in `self.bound`) are reused as correlation points; only new ones
    /// get scanned/extended.
    fn build_match(
        &mut self,
        match_: &BoundMatch,
        where_pred: Option<&BoundExpr>,
        base: Option<PlanOp>,
    ) -> Result<PlanOp> {
        // Recursive rels whose value is needed (named, or part of a named path).
        self.recursive_value_rels.clear();
        self.recursive_path_rels.clear();
        for &p in &match_.path_vars {
            if let VarKind::Path { segments, .. } = &self.query.var(p).kind {
                for &(rel, _) in segments {
                    self.recursive_value_rels.insert(rel);
                    self.recursive_path_rels.insert(rel);
                }
            }
        }
        for &r in &match_.rel_vars {
            if !self.query.var(r).anonymous {
                self.recursive_value_rels.insert(r);
            }
        }

        // Distinct, not-yet-bound node variables in declaration order.
        let mut node_vars: Vec<VarId> = Vec::new();
        for &v in &match_.node_vars {
            if !node_vars.contains(&v) {
                node_vars.push(v);
            }
        }

        // Cost-based anchor selection is result-neutral only when the match has no
        // recursive rel (see [`cheapest_unbound`]); otherwise keep declaration order.
        let cost_safe = !match_
            .rel_vars
            .iter()
            .any(|&r| self.query.var(r).is_recursive());

        // With an input scope, `InputScan` is the base and fresh node scans
        // cross-product onto it. Without one, the first node is the root (or a
        // single empty row when there are no nodes).
        let mut root = match base {
            Some(op) => op,
            None if node_vars.iter().all(|v| self.bound.contains(v)) => {
                return Ok(PlanOp::SingleRow);
            }
            None => {
                // Cost-based anchor: the node whose greedy traversal has the lowest
                // peak cardinality (P3 step 10b L2b — handles hub patterns where the
                // smallest table is the wrong anchor). A selective `var.pk = const`
                // still lands near 1 (its traversal peaks low) so it anchors here and
                // step-5 filter-pushdown turns it into an IndexScan. Stable on ties, so
                // with no stats this is the first node in declaration order.
                let anchor = self
                    .best_anchor(&node_vars, &match_.rel_vars, where_pred, cost_safe)
                    .expect("a not-all-bound match has an unbound node");
                self.bound.insert(anchor);
                PlanOp::ScanNode(self.make_scan(anchor))
            }
        };

        let mut rels_remaining: Vec<VarId> = match_.rel_vars.clone();
        // Cost-based **extend order** (P3 step 10b L2) is result-neutral only when the
        // anchor is (no recursive rel) *and* there is no named path to assemble (whose
        // segments are order-sensitive); otherwise keep the declaration-order greedy.
        let reorder_safe = cost_safe && match_.path_vars.is_empty();

        loop {
            // Pick the next relationship move. Cost-based order closes both-bound edges
            // first (they only filter), then takes the lowest-fan-out extend to a new
            // node, then cross-products — so a cyclic / multi-path pattern follows its
            // selective edges instead of materializing the full node cross-product
            // before the cycle closes (lsqb q2/q3). The declaration-order path (used
            // for recursive / named-path matches) keeps the pre-L2 behavior exactly:
            // extend-to-new first, then close, then cross-product.
            let mv = if reorder_safe {
                let both_bound = |r: VarId| {
                    let (src, dst) = self.rel_endpoints(r);
                    self.bound.contains(&src) && self.bound.contains(&dst)
                };
                if let Some(pos) = rels_remaining.iter().position(|&r| both_bound(r)) {
                    RelMove::Close(rels_remaining.remove(pos))
                } else if let Some(idx) = rels_remaining
                    .iter()
                    .enumerate()
                    .filter(|t| self.rel_extendable_to_new(*t.1))
                    .min_by(|a, b| {
                        self.extend_fanout_of(*a.1)
                            .partial_cmp(&self.extend_fanout_of(*b.1))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
                {
                    RelMove::ExtendNew(rels_remaining.remove(idx))
                } else if let Some(nv) = self.cheapest_unbound(&node_vars, where_pred, cost_safe) {
                    RelMove::Cross(nv)
                } else {
                    break;
                }
            } else if let Some(pos) = rels_remaining
                .iter()
                .position(|&r| self.rel_extendable_to_new(r))
            {
                RelMove::ExtendNew(rels_remaining.remove(pos))
            } else if let Some(pos) = rels_remaining.iter().position(|&r| {
                let (src, dst) = self.rel_endpoints(r);
                self.bound.contains(&src) && self.bound.contains(&dst)
            }) {
                RelMove::Close(rels_remaining.remove(pos))
            } else if let Some(nv) = self.cheapest_unbound(&node_vars, where_pred, cost_safe) {
                RelMove::Cross(nv)
            } else {
                break;
            };

            match mv {
                RelMove::ExtendNew(rel) => {
                    root = if self.query.var(rel).is_recursive() {
                        self.make_var_extend_new(root, rel)
                    } else {
                        self.make_extend_new(root, rel)
                    };
                }
                RelMove::Close(rel) => {
                    root = if self.query.var(rel).is_recursive() {
                        self.make_var_extend_existing(root, rel)
                    } else {
                        self.make_extend_existing(root, rel)
                    };
                }
                RelMove::Cross(nv) => {
                    let left_width = self.layout.width();
                    let scan = PlanOp::ScanNode(self.make_scan(nv));
                    let right_width = self.layout.width() - left_width;
                    self.bound.insert(nv);
                    root = PlanOp::CrossProduct {
                        left: Box::new(root),
                        left_width,
                        right: Box::new(scan),
                        right_width,
                    };
                }
            }
        }

        // Assemble each named path's value, now that all its segment columns exist.
        for &p in &match_.path_vars {
            root = self.make_project_path(root, p);
        }
        Ok(root)
    }

    /// The correlated (already-bound) node variables of a sub-pattern, if it is
    /// cleanly **decorrelatable** into a build-once hash join (P3 step 10b L1).
    ///
    /// Decorrelation is sound only when the correlation reduces to **node-id
    /// equality**: the sub-match shares ≥1 already-bound node with the outer scope,
    /// introduces no named path, and carries no WHERE predicate. The no-predicate
    /// gate is the key safety condition — a predicate could reference a correlated
    /// variable, which [`build_decorrelated_join`] restores to its *outer* column
    /// after planning the build side, so any such reference would mis-resolve.
    /// `None` ⇒ fall back to the per-row nested loop (`Optional`/`Subquery`).
    fn correlated_nodes(
        &self,
        match_: &BoundMatch,
        where_pred: Option<&BoundExpr>,
    ) -> Option<Vec<VarId>> {
        if where_pred.is_some() || !match_.path_vars.is_empty() {
            return None;
        }
        let corr: Vec<VarId> = match_
            .node_vars
            .iter()
            .copied()
            .filter(|v| self.bound.contains(v))
            .collect();
        (!corr.is_empty()).then_some(corr)
    }

    /// Unnest a correlated sub-pattern into a build-once [`PlanOp::HashJoin`] (P3
    /// step 10b L1), replacing the per-row nested-loop `Optional`/`Subquery`. The
    /// `corr` nodes (from [`correlated_nodes`]) are temporarily un-scoped so
    /// `build_match` re-scans them into fresh columns (the build side = the relation
    /// the sub-pattern enumerates, built once); their fresh id columns are the build
    /// keys and the outer columns the probe keys; then they are restored to their
    /// outer columns so the surrounding plan resolves them unchanged. `probe` (the
    /// outer scope) occupies all columns allocated so far; the build side appends.
    fn build_decorrelated_join(
        &mut self,
        probe: PlanOp,
        corr: &[VarId],
        match_: &BoundMatch,
        kind: JoinKind,
    ) -> Result<PlanOp> {
        // Capture each correlated node's outer slot + id column, then un-scope it.
        let saved: Vec<(VarId, usize, usize)> = corr
            .iter()
            .map(|&c| {
                let slot = self
                    .layout
                    .capture_var_slot(c)
                    .expect("correlated node is bound");
                (c, slot, self.layout.var(c).id_col)
            })
            .collect();
        for &c in corr {
            self.bound.remove(&c);
        }
        // Build the sub-pattern standalone (fresh scans for the now-unbound nodes).
        let build_start = self.layout.width();
        let build = self.build_match(match_, None, None)?;
        let build_len = self.layout.width() - build_start;
        // The fresh re-scanned id column of each correlated node = the build key;
        // the captured outer column = the probe key.
        let keys: Vec<(BoundExpr, BoundExpr)> = saved
            .iter()
            .map(|&(c, _, outer_col)| {
                let fresh_col = self.layout.var(c).id_col;
                (
                    BoundExpr::Column {
                        col: outer_col,
                        ty: LogicalType::InternalId,
                    },
                    BoundExpr::Column {
                        col: fresh_col,
                        ty: LogicalType::InternalId,
                    },
                )
            })
            .collect();
        // Restore the correlated nodes to their outer columns + bound state.
        for &(c, slot, _) in &saved {
            self.layout.restore_var_slot(c, slot);
            self.bound.insert(c);
        }
        Ok(PlanOp::HashJoin {
            probe: Box::new(probe),
            build: Box::new(build),
            probe_cols: (0, build_start),
            build_cols: (build_start, build_len),
            keys,
            kind,
        })
    }

    fn rel_endpoints(&self, rel: VarId) -> (VarId, VarId) {
        match &self.query.var(rel).kind {
            VarKind::Rel { src, dst, .. } => (*src, *dst),
            VarKind::Node { .. } | VarKind::Path { .. } | VarKind::Scalar { .. } => {
                unreachable!("rel var is not a relationship")
            }
        }
    }

    /// Fan-out estimate of extending `rel` from its currently-bound endpoint to its
    /// unbound one (drives cost-based extend order; P3 step 10b L2).
    fn extend_fanout_of(&self, rel: VarId) -> f64 {
        let (src, dst) = self.rel_endpoints(rel);
        let from = if self.bound.contains(&src) { src } else { dst };
        cost::extend_fanout(self.query, rel, from, self.stats)
    }

    fn rel_extendable_to_new(&self, rel: VarId) -> bool {
        let (src, dst) = self.rel_endpoints(rel);
        (self.bound.contains(&src) && !self.bound.contains(&dst))
            || (self.bound.contains(&dst) && !self.bound.contains(&src))
    }

    /// The not-yet-bound node variable to scan next. With `cost_safe`, the lowest-
    /// estimated-cardinality one (the cost-based join anchor; see [`cost::node_card`];
    /// stable, so ties / no-stats fall back to declaration order = the pre-step-8
    /// greedy planner). Without `cost_safe`, always declaration order — reordering is
    /// only result-neutral when no recursive rel is present (flipping a var-length
    /// rel's direction reverses the node/rel order in its assembled `RECURSIVE_REL`).
    fn cheapest_unbound(
        &self,
        node_vars: &[VarId],
        where_pred: Option<&BoundExpr>,
        cost_safe: bool,
    ) -> Option<VarId> {
        if !cost_safe {
            return node_vars.iter().copied().find(|v| !self.bound.contains(v));
        }
        node_vars
            .iter()
            .copied()
            .filter(|v| !self.bound.contains(v))
            .min_by(|&a, &b| {
                let ca = cost::node_card(self.query, a, where_pred, self.stats);
                let cb = cost::node_card(self.query, b, where_pred, self.stats);
                ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// The node to anchor the join on: the one whose cost-based greedy traversal has
    /// the lowest **peak** intermediate cardinality (P3 step 10b L2b). The plain
    /// "smallest table" anchor ([`cheapest_unbound`]) is wrong for a hub pattern —
    /// lsqb q2 anchors on the small `Person` (1.7K) but then explodes to ~2.3M,
    /// whereas anchoring on the larger `Comment` (215K) hub, whose edges are all
    /// fan-out-1, keeps the intermediate at ~215K. So we *simulate* the traversal
    /// from each candidate and pick the min-peak one. Stable on ties / no stats
    /// (every peak is the same constant) ⇒ declaration order = the pre-L2b anchor.
    /// Only used when `cost_safe`; otherwise the position-based anchor is kept.
    fn best_anchor(
        &self,
        node_vars: &[VarId],
        rel_vars: &[VarId],
        where_pred: Option<&BoundExpr>,
        cost_safe: bool,
    ) -> Option<VarId> {
        if !cost_safe {
            return node_vars.iter().copied().find(|v| !self.bound.contains(v));
        }
        node_vars
            .iter()
            .copied()
            .filter(|v| !self.bound.contains(v))
            .min_by(|&a, &b| {
                let pa = self.anchor_peak(a, node_vars, rel_vars, where_pred);
                let pb = self.anchor_peak(b, node_vars, rel_vars, where_pred);
                pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Estimated **peak** intermediate cardinality of the cost-based greedy traversal
    /// starting from `anchor` — a read-only what-if used by [`best_anchor`]. It mirrors
    /// the `build_match` loop's move order (close a both-bound edge, else the
    /// lowest-fan-out extend, else cross-product the cheapest node) over estimated
    /// cardinalities only (no layout mutation).
    fn anchor_peak(
        &self,
        anchor: VarId,
        node_vars: &[VarId],
        rel_vars: &[VarId],
        where_pred: Option<&BoundExpr>,
    ) -> f64 {
        let mut bound = self.bound.clone();
        bound.insert(anchor);
        let mut card = cost::node_card(self.query, anchor, where_pred, self.stats);
        let mut peak = card;
        let mut rels: Vec<VarId> = rel_vars.to_vec();
        loop {
            // 1. close a both-bound edge (a residual filter — only shrinks).
            if let Some(pos) = rels.iter().position(|&r| {
                let (s, d) = self.rel_endpoints(r);
                bound.contains(&s) && bound.contains(&d)
            }) {
                rels.remove(pos);
                card = (card * cost::CLOSE_SEL).max(1.0);
                continue;
            }
            // 2. lowest-fan-out extend to a new node.
            let best = rels
                .iter()
                .enumerate()
                .filter_map(|(i, &r)| {
                    let (s, d) = self.rel_endpoints(r);
                    let from = if bound.contains(&s) && !bound.contains(&d) {
                        Some(s)
                    } else if bound.contains(&d) && !bound.contains(&s) {
                        Some(d)
                    } else {
                        None
                    }?;
                    Some((i, r, cost::extend_fanout(self.query, r, from, self.stats)))
                })
                .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((idx, rel, fanout)) = best {
                rels.remove(idx);
                let (s, d) = self.rel_endpoints(rel);
                bound.insert(if bound.contains(&s) { d } else { s });
                card = (card * fanout).max(1.0);
                peak = peak.max(card);
                continue;
            }
            // 3. cross-product the cheapest disconnected node.
            let next = node_vars
                .iter()
                .copied()
                .filter(|v| !bound.contains(v))
                .min_by(|&a, &b| {
                    let ca = cost::node_card(self.query, a, where_pred, self.stats);
                    let cb = cost::node_card(self.query, b, where_pred, self.stats);
                    ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
                });
            if let Some(nv) = next {
                bound.insert(nv);
                card = (card * cost::node_card(self.query, nv, where_pred, self.stats)).max(1.0);
                peak = peak.max(card);
                continue;
            }
            break;
        }
        peak
    }

    fn make_scan(&mut self, var: VarId) -> ScanNode {
        let alloc = self.alloc_node(var);
        let tables = alloc
            .tables
            .iter()
            .map(|&t| ScanTable {
                table: t,
                prop_cols: self.table_prop_cols(t, &alloc.name_to_col),
            })
            .collect();
        ScanNode {
            var,
            id_col: alloc.id_col,
            tables,
        }
    }

    fn make_unwind(&mut self, input: PlanOp, unwind: &BoundUnwind) -> PlanOp {
        let target = if self.query.var(unwind.var).is_node() {
            let scan = self.make_scan(unwind.var);
            UnwindTarget::Node {
                id_col: scan.id_col,
                prop_tables: scan.tables,
            }
        } else {
            let ty = self.query.var(unwind.var).scalar_type();
            UnwindTarget::Scalar {
                col: self.layout.add_scalar(unwind.var, ty),
            }
        };
        PlanOp::Unwind {
            input: Box::new(input),
            list: unwind.list.clone(),
            target,
        }
    }

    /// Allocate a node variable's layout columns (internal id + one column per
    /// union property) and register its [`VarColumns`] binding. Returns the
    /// candidate tables, the id column, and the property-name→column map — enough
    /// for `make_scan` to build the per-table scan maps and for the carried-node
    /// input path to unpack a node value.
    fn alloc_node(&mut self, var: VarId) -> NodeAlloc {
        let info = self.query.var(var);
        let tables = info.node_tables().to_vec();
        let label = info.label().to_string();
        let id_col = self.layout.allocate(LogicalType::InternalId);
        let mut props = Vec::new();
        let mut name_to_col = Vec::with_capacity(info.properties.len());
        for p in &info.properties {
            let col_index = self.layout.allocate(p.ty.clone());
            name_to_col.push((p.name.clone(), col_index));
            props.push(LayoutProp {
                name: p.name.clone(),
                col_index,
                ty: p.ty.clone(),
            });
        }
        self.layout.add_variable(VarColumns {
            var,
            kind: VarColKind::Node {
                table: tables.first().copied(),
                label,
            },
            id_col,
            props,
            value_col: None,
        });
        NodeAlloc {
            tables,
            id_col,
            name_to_col,
        }
    }

    /// Map a node table's own columns to their layout columns (by property name)
    /// for one candidate table of a (possibly polymorphic) scan.
    fn table_prop_cols(&self, table: TableId, name_to_col: &[(String, usize)]) -> Vec<PropCol> {
        let entry = self.catalog.node_table(table).expect("scan table exists");
        entry
            .columns()
            .iter()
            .filter_map(|column| {
                name_to_col
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(column.name()))
                    .map(|&(_, col_index)| PropCol {
                        column_id: column.column_id().0,
                        col_index,
                    })
            })
            .collect()
    }

    /// Allocate a relationship variable's layout columns (internal id + one column
    /// per union property). Returns the id column, the property-name→column map
    /// (for the per-rel-table branch maps), and the [`LayoutProp`]s (for
    /// `register_rel`).
    fn alloc_rel(&mut self, rel: VarId) -> RelAlloc {
        let info = self.query.var(rel);
        let id_col = self.layout.allocate(LogicalType::InternalId);
        let mut props = Vec::new();
        let mut name_to_col = Vec::with_capacity(info.properties.len());
        for p in &info.properties {
            let col_index = self.layout.allocate(p.ty.clone());
            name_to_col.push((p.name.clone(), col_index));
            props.push(LayoutProp {
                name: p.name.clone(),
                col_index,
                ty: p.ty.clone(),
            });
        }
        RelAlloc {
            id_col,
            name_to_col,
            props,
        }
    }

    /// Map a relationship table's own columns to their layout columns (by name)
    /// for one branch of a (possibly polymorphic) extend.
    fn rel_table_prop_cols(
        &self,
        rel_table: TableId,
        name_to_col: &[(String, usize)],
    ) -> Vec<PropCol> {
        let entry = self.catalog.rel_table(rel_table).expect("rel table exists");
        entry
            .columns()
            .iter()
            .filter_map(|column| {
                name_to_col
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(column.name()))
                    .map(|&(_, col_index)| PropCol {
                        column_id: column.column_id().0,
                        col_index,
                    })
            })
            .collect()
    }

    /// Expand a rel variable's candidate group ids to their per-pair member (storage)
    /// table ids — the physical tables a rel pattern probes. A single-pair rel group is
    /// its own sole member (unchanged); a multi-pair group fans out to one member per
    /// FROM-TO pair (a rel's runtime `_ID` carries its pair's member id).
    fn rel_member_tables(&self, rel: VarId) -> Vec<TableId> {
        self.query
            .var(rel)
            .rel_tables()
            .iter()
            .flat_map(|&g| {
                self.catalog
                    .rel_members(g)
                    .into_iter()
                    .map(|(member, _, _)| member)
            })
            .collect()
    }

    /// One [`RelBranch`] per candidate per-pair member table of `rel`.
    fn rel_branches(&self, rel: VarId, name_to_col: &[(String, usize)]) -> Vec<RelBranch> {
        self.rel_member_tables(rel)
            .into_iter()
            .map(|t| RelBranch {
                rel_table: t,
                rel_prop_cols: self.rel_table_prop_cols(t, name_to_col),
            })
            .collect()
    }

    fn register_rel(&mut self, rel: VarId, rel_id_col: usize, props: Vec<LayoutProp>) {
        let (table, label, src, dst) = match &self.query.var(rel).kind {
            VarKind::Rel {
                tables,
                label,
                src,
                dst,
                ..
            } => (tables.first().copied(), label.clone(), *src, *dst),
            VarKind::Node { .. } | VarKind::Path { .. } | VarKind::Scalar { .. } => unreachable!(),
        };
        let src_id_col = self.layout.var(src).id_col;
        let dst_id_col = self.layout.var(dst).id_col;
        self.layout.add_variable(VarColumns {
            var: rel,
            kind: VarColKind::Rel {
                table,
                label,
                src_id_col,
                dst_id_col,
            },
            id_col: rel_id_col,
            props,
            value_col: None,
        });
    }

    fn make_extend_new(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let (from, to, dir) = if self.bound.contains(&src) {
            (
                src,
                dst,
                if directed {
                    ExtendDir::Forward
                } else {
                    ExtendDir::Both
                },
            )
        } else {
            (
                dst,
                src,
                if directed {
                    ExtendDir::Backward
                } else {
                    ExtendDir::Both
                },
            )
        };
        let from_id_col = self.layout.var(from).id_col;

        // Allocate relationship columns, then the new node's columns.
        let rel_alloc = self.alloc_rel(rel);
        let rel_id_col = rel_alloc.id_col;
        let scan = self.make_scan(to); // registers `to` and allocates its columns
        self.bound.insert(to);
        self.register_rel(rel, rel_id_col, rel_alloc.props);
        self.bound.insert(rel);
        let branches = self.rel_branches(rel, &rel_alloc.name_to_col);

        PlanOp::Extend(Box::new(Extend {
            input: Box::new(input),
            from_id_col,
            dir,
            rel_id_col,
            branches,
            target: ExtendTarget::New {
                to_id_col: scan.id_col,
                to_tables: scan.tables,
            },
            carry_cols: (0..rel_id_col).collect(),
            factorize: false,
        }))
    }

    fn make_extend_existing(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let from_id_col = self.layout.var(src).id_col;
        let filter_col = self.layout.var(dst).id_col;
        let dir = if directed {
            ExtendDir::Forward
        } else {
            ExtendDir::Both
        };

        let rel_alloc = self.alloc_rel(rel);
        let rel_id_col = rel_alloc.id_col;
        self.register_rel(rel, rel_id_col, rel_alloc.props);
        self.bound.insert(rel);
        let branches = self.rel_branches(rel, &rel_alloc.name_to_col);

        PlanOp::Extend(Box::new(Extend {
            input: Box::new(input),
            from_id_col,
            dir,
            rel_id_col,
            branches,
            target: ExtendTarget::Existing { filter_col },
            carry_cols: (0..rel_id_col).collect(),
            factorize: false,
        }))
    }

    /// The `(lower, upper, mode, semantic)` of a recursive rel variable.
    #[allow(clippy::type_complexity)]
    fn recursive_spec(
        &self,
        rel: VarId,
    ) -> (u32, u32, RecursiveMode, PathSemantic, Option<String>) {
        match &self.query.var(rel).kind {
            VarKind::Rel {
                recursive: Some(s), ..
            } => (s.lower, s.upper, s.mode, s.semantic, s.weight.clone()),
            _ => unreachable!("not a recursive rel"),
        }
    }

    /// The per-step filter of a recursive rel variable, if any.
    fn recursive_filter(&self, rel: VarId) -> Option<RecursiveFilter> {
        match &self.query.var(rel).kind {
            VarKind::Rel {
                recursive: Some(s), ..
            } => s.filter.clone(),
            _ => None,
        }
    }

    /// Allocate the single generic column holding a recursive rel's
    /// `{_NODES, _RELS}` value, registering the variable to resolve to it.
    fn alloc_recursive_value(&mut self, rel: VarId) -> usize {
        let col = self.layout.allocate(LogicalType::RecursiveRel);
        self.layout.add_variable(VarColumns {
            var: rel,
            kind: VarColKind::Scalar,
            id_col: col,
            props: Vec::new(),
            value_col: None,
        });
        col
    }

    fn make_var_extend_new(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let (from, to, dir) = if self.bound.contains(&src) {
            (src, dst, fwd_or_both(directed))
        } else {
            (dst, src, bwd_or_both(directed))
        };
        let from_id_col = self.layout.var(from).id_col;
        let (lower, upper, mode, semantic, weight) = self.recursive_spec(rel);
        let build_value = self.recursive_value_rels.contains(&rel);
        let rel_tables = self.rel_member_tables(rel);

        // The rel value column precedes the new node's columns (the input boundary).
        let rel_value_col = self.alloc_recursive_value(rel);
        let scan = self.make_scan(to);
        self.bound.insert(to);
        self.bound.insert(rel);

        PlanOp::VarLengthExtend(Box::new(VarLengthExtend {
            input: Box::new(input),
            from_id_col,
            dir,
            lower,
            upper,
            mode,
            semantic,
            rel_tables,
            rel_value_col,
            build_value,
            filter: self.recursive_filter(rel),
            weight,
            in_named_path: self.recursive_path_rels.contains(&rel),
            target: ExtendTarget::New {
                to_id_col: scan.id_col,
                to_tables: scan.tables,
            },
            factorize: false,
        }))
    }

    fn make_var_extend_existing(&mut self, input: PlanOp, rel: VarId) -> PlanOp {
        let (src, dst) = self.rel_endpoints(rel);
        let directed = matches!(
            self.query.var(rel).kind,
            VarKind::Rel { directed: true, .. }
        );
        let from_id_col = self.layout.var(src).id_col;
        let filter_col = self.layout.var(dst).id_col;
        let dir = fwd_or_both(directed);
        let (lower, upper, mode, semantic, weight) = self.recursive_spec(rel);
        let build_value = self.recursive_value_rels.contains(&rel);
        let rel_tables = self.rel_member_tables(rel);
        let rel_value_col = self.alloc_recursive_value(rel);
        self.bound.insert(rel);

        PlanOp::VarLengthExtend(Box::new(VarLengthExtend {
            input: Box::new(input),
            from_id_col,
            dir,
            lower,
            upper,
            mode,
            semantic,
            rel_tables,
            rel_value_col,
            build_value,
            filter: self.recursive_filter(rel),
            weight,
            in_named_path: self.recursive_path_rels.contains(&rel),
            target: ExtendTarget::Existing { filter_col },
            factorize: false,
        }))
    }

    /// Build the [`PlanOp::ProjectPath`] that assembles a named path's value from
    /// its head node and `(rel, to-node)` segments.
    fn make_project_path(&mut self, input: PlanOp, path_var: VarId) -> PlanOp {
        let (head, segs) = match &self.query.var(path_var).kind {
            VarKind::Path { head, segments } => (*head, segments.clone()),
            _ => unreachable!("not a path variable"),
        };
        let segments = segs
            .iter()
            .map(|&(rel, to_node)| {
                let rel = if self.query.var(rel).is_recursive() {
                    PathRel::Recursive {
                        value_col: self.layout.var(rel).id_col,
                    }
                } else {
                    PathRel::Single { rel }
                };
                PathSegmentPlan { rel, to_node }
            })
            .collect();
        let path_col = self.layout.allocate(LogicalType::RecursiveRel);
        self.layout.add_variable(VarColumns {
            var: path_var,
            kind: VarColKind::Scalar,
            id_col: path_col,
            props: Vec::new(),
            value_col: None,
        });
        PlanOp::ProjectPath(Box::new(ProjectPath {
            input: Box::new(input),
            path_col,
            head,
            segments,
        }))
    }
}
