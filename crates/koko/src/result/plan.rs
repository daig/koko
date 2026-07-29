//! Plan and captured type-context presentation records.

use crate::tooling::{PropertyDescriptor, property_descriptor};

/// One pinned graph value type used by machine encoders after catalog changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphValueType {
    table_identity: u64,
    name: String,
    relationship: bool,
    properties: Vec<PropertyDescriptor>,
}

impl GraphValueType {
    pub const fn table_identity(&self) -> u64 {
        self.table_identity
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn is_relationship(&self) -> bool {
        self.relationship
    }

    pub fn properties(&self) -> &[PropertyDescriptor] {
        &self.properties
    }
}

/// Immutable type/catalog context captured with a materialized result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultTypeContext {
    catalog_revision: u64,
    graph_values: Vec<GraphValueType>,
}

impl ResultTypeContext {
    pub const fn catalog_revision(&self) -> u64 {
        self.catalog_revision
    }

    pub fn graph_values(&self) -> &[GraphValueType] {
        &self.graph_values
    }

    pub fn graph_value(&self, table_identity: u64) -> Option<&GraphValueType> {
        self.graph_values
            .iter()
            .find(|value| value.table_identity == table_identity)
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        (self.graph_values.capacity() * std::mem::size_of::<GraphValueType>()) as u64
            + self
                .graph_values
                .iter()
                .map(|value| {
                    value.name.capacity()
                        + value.properties.capacity() * std::mem::size_of::<PropertyDescriptor>()
                        + value
                            .properties
                            .iter()
                            .map(PropertyDescriptor::allocated_bytes)
                            .sum::<usize>()
                })
                .sum::<usize>() as u64
    }
}

/// One engine-owned plan node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanNode {
    operator: String,
    detail: Vec<(String, String)>,
    children: Vec<PlanNode>,
}

impl PlanNode {
    pub fn operator(&self) -> &str {
        &self.operator
    }

    pub fn detail(&self) -> &[(String, String)] {
        &self.detail
    }

    pub fn children(&self) -> &[PlanNode] {
        &self.children
    }

    fn allocated_bytes(&self) -> u64 {
        (self.operator.capacity()
            + self.detail.capacity() * std::mem::size_of::<(String, String)>()
            + self
                .detail
                .iter()
                .map(|(key, value)| key.capacity() + value.capacity())
                .sum::<usize>()
            + self.children.capacity() * std::mem::size_of::<PlanNode>()) as u64
            + self.children.iter().map(Self::allocated_bytes).sum::<u64>()
    }
}

/// Structural Rust planner presentation for EXPLAIN or PROFILE.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPresentation {
    profiled: bool,
    roots: Vec<PlanNode>,
    execution_time: Option<std::time::Duration>,
}

impl PlanPresentation {
    pub const fn is_profile(&self) -> bool {
        self.profiled
    }

    pub fn roots(&self) -> &[PlanNode] {
        &self.roots
    }

    pub const fn execution_time(&self) -> Option<std::time::Duration> {
        self.execution_time
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        (self.roots.capacity() * std::mem::size_of::<PlanNode>()) as u64
            + self
                .roots
                .iter()
                .map(PlanNode::allocated_bytes)
                .sum::<u64>()
    }
}

pub(crate) fn capture_result_type_context(
    catalog: &koko_catalog::Catalog,
    catalog_revision: u64,
) -> ResultTypeContext {
    let mut graph_values = Vec::new();
    for identity in catalog.node_table_ids() {
        if catalog.is_any_node_table(identity) {
            continue;
        }
        let table = catalog.node_table(identity).expect("listed node table");
        graph_values.push(GraphValueType {
            table_identity: identity.0,
            name: table.name().to_string(),
            relationship: false,
            properties: table
                .columns()
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    property_descriptor(column, index == table.primary_key_index())
                })
                .collect(),
        });
    }
    for identity in catalog.rel_table_ids() {
        if catalog.is_any_rel_table(identity) {
            continue;
        }
        let table = catalog
            .rel_table(identity)
            .expect("listed relationship table");
        graph_values.push(GraphValueType {
            table_identity: identity.0,
            name: table.name().to_string(),
            relationship: true,
            properties: table
                .columns()
                .iter()
                .map(|column| property_descriptor(column, false))
                .collect(),
        });
    }
    ResultTypeContext {
        catalog_revision,
        graph_values,
    }
}

pub(crate) fn plan_presentation(
    plan: &koko_ir::plan::RegularPlan,
    profiled: bool,
) -> PlanPresentation {
    let roots = plan
        .operands
        .iter()
        .enumerate()
        .map(|(operand_index, operand)| PlanNode {
            operator: "UnionOperand".to_string(),
            detail: vec![("index".to_string(), (operand_index + 1).to_string())],
            children: operand
                .parts
                .iter()
                .enumerate()
                .map(|(part_index, part)| {
                    let mut children = vec![plan_op_node(&part.root)];
                    children.extend(part.update_ops.iter().map(update_node));
                    PlanNode {
                        operator: "QueryPart".to_string(),
                        detail: vec![("index".to_string(), (part_index + 1).to_string())],
                        children,
                    }
                })
                .collect(),
        })
        .collect();
    PlanPresentation {
        profiled,
        roots,
        execution_time: None,
    }
}

pub(crate) fn set_plan_execution_time(
    plan: &mut PlanPresentation,
    execution_time: std::time::Duration,
) {
    plan.execution_time = Some(execution_time);
}

fn leaf(operator: &str) -> PlanNode {
    PlanNode {
        operator: operator.to_string(),
        detail: Vec::new(),
        children: Vec::new(),
    }
}

fn unary(operator: &str, input: &koko_ir::plan::PlanOp) -> PlanNode {
    PlanNode {
        operator: operator.to_string(),
        detail: Vec::new(),
        children: vec![plan_op_node(input)],
    }
}

fn binary(operator: &str, left: &koko_ir::plan::PlanOp, right: &koko_ir::plan::PlanOp) -> PlanNode {
    PlanNode {
        operator: operator.to_string(),
        detail: Vec::new(),
        children: vec![plan_op_node(left), plan_op_node(right)],
    }
}

fn plan_op_node(operator: &koko_ir::plan::PlanOp) -> PlanNode {
    use koko_ir::plan::PlanOp;
    match operator {
        PlanOp::SingleRow => leaf("SingleRow"),
        PlanOp::InputScan => leaf("InputScan"),
        PlanOp::ScanTableFunc { .. } => leaf("TableFunctionScan"),
        PlanOp::LoadScan { format, paths, .. } => PlanNode {
            operator: "LoadScan".to_string(),
            detail: vec![
                ("format".to_string(), format!("{format:?}")),
                ("files".to_string(), paths.len().to_string()),
            ],
            children: Vec::new(),
        },
        PlanOp::ScanNode(scan) => PlanNode {
            operator: "NodeScan".to_string(),
            detail: vec![("tables".to_string(), scan.tables.len().to_string())],
            children: Vec::new(),
        },
        PlanOp::IndexScan(scan) => scan
            .input
            .as_deref()
            .map_or_else(|| leaf("IndexScan"), |input| unary("IndexScan", input)),
        PlanOp::Extend(extend) => unary("Extend", &extend.input),
        PlanOp::VarLengthExtend(extend) => unary("VariableLengthExtend", &extend.input),
        PlanOp::ProjectPath(path) => unary("ProjectPath", &path.input),
        PlanOp::CrossProduct { left, right, .. } => binary("CrossProduct", left, right),
        PlanOp::HashJoin {
            probe, build, kind, ..
        } => PlanNode {
            operator: "HashJoin".to_string(),
            detail: vec![("kind".to_string(), format!("{kind:?}"))],
            children: vec![plan_op_node(probe), plan_op_node(build)],
        },
        PlanOp::Filter { input, .. } => unary("Filter", input),
        PlanOp::Unwind { input, .. } => unary("Unwind", input),
        PlanOp::Optional { input, pattern, .. } => binary("Optional", input, pattern),
        PlanOp::Subquery {
            input,
            pattern,
            kind,
            ..
        } => PlanNode {
            operator: "Subquery".to_string(),
            detail: vec![("kind".to_string(), format!("{kind:?}"))],
            children: vec![plan_op_node(input), plan_op_node(pattern)],
        },
        PlanOp::SequenceCall { input, func, .. } => PlanNode {
            operator: "SequenceCall".to_string(),
            detail: vec![("function".to_string(), format!("{func:?}"))],
            children: vec![plan_op_node(input)],
        },
        PlanOp::MaterializeValues { input, items } => PlanNode {
            operator: "MaterializeValues".to_string(),
            detail: vec![("values".to_string(), items.len().to_string())],
            children: vec![plan_op_node(input)],
        },
    }
}

fn update_node(update: &koko_ir::plan::UpdateOp) -> PlanNode {
    let operator = match update {
        koko_ir::plan::UpdateOp::Create(_) => "Create",
        koko_ir::plan::UpdateOp::Set(_) => "Set",
        koko_ir::plan::UpdateOp::Delete(_) => "Delete",
        koko_ir::plan::UpdateOp::Merge(_) => "Merge",
    };
    leaf(operator)
}
