use super::*;

// ---------------------------------------------------------------------------
// Node/rel value assembly
// ---------------------------------------------------------------------------

pub(crate) fn read_var_id(
    var: VarId,
    layout: &RowLayout,
    chunk: &DataChunk,
    pos: usize,
) -> InternalId {
    let id_col = layout.var(var).id_col;
    match chunk.columns[id_col].get_value(pos) {
        Value::InternalId(id) => id,
        _ => InternalId::new(TableId(u64::MAX), u64::MAX),
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EntityReader<'a> {
    pub(crate) catalog: &'a Catalog,
    pub(crate) storage: &'a InMemStorage,
    pub(crate) read: StorageReadHandle,
    pub(crate) sources: &'a IcebugQuerySources,
    pub(crate) memory: &'a QueryMemory,
    pub(crate) control: QueryControl<'a>,
}

impl<'a> EntityReader<'a> {
    pub(crate) fn from_ctx(ctx: &OperatorContext<'a>) -> Self {
        Self {
            catalog: ctx.catalog,
            storage: ctx.storage,
            read: ctx.read(),
            sources: ctx.sources,
            memory: ctx.memory,
            control: ctx.control,
        }
    }
}

pub(crate) fn assemble_var(
    var: VarId,
    layout: &RowLayout,
    chunk: &DataChunk,
    pos: usize,
    entity: EntityReader<'_>,
) -> Result<Value> {
    let id = read_var_id(var, layout, chunk, pos);
    assemble_id(var, id, layout, entity)
}

/// Assemble a whole node/rel value from its internal id, reading properties
/// straight from storage (so it works in both the projection and aggregate
/// paths without depending on which columns the chunk carries).
pub(crate) fn assemble_id(
    var: VarId,
    id: InternalId,
    layout: &RowLayout,
    entity: EntityReader<'_>,
) -> Result<Value> {
    if id.table_id.0 == u64::MAX {
        return Ok(Value::Null);
    }
    match &layout.var(var).kind {
        // For a polymorphic node/rel the actual table comes from the runtime id,
        // so the label and property set are those of the matched table.
        VarColKind::Node { .. } => Ok(Value::Node(Box::new(assemble_node_value(id, entity)?))),
        VarColKind::Rel { .. } => Ok(Value::Rel(Box::new(assemble_rel_value(id, entity)?))),
        // Scalar variables are projected as plain column reads, never here.
        VarColKind::Scalar => unreachable!("scalar var is not a node/rel"),
    }
}

/// Restrict a property list to the projected names (case-insensitive). `None`
/// keeps all properties; `Some(names)` keeps only those (the recursive-lambda
/// projection over intermediate node/rel values).
pub(crate) fn project_props(
    props: Vec<(String, Value)>,
    proj: Option<&Vec<String>>,
) -> Vec<(String, Value)> {
    match proj {
        None => props,
        Some(keep) => props
            .into_iter()
            .filter(|(name, _)| keep.iter().any(|k| k.eq_ignore_ascii_case(name)))
            .collect(),
    }
}

/// Build a whole [`NodeValue`] from its id (label + all properties of its actual
/// table), reading cells straight from storage.
pub(crate) fn assemble_node_value(id: InternalId, entity: EntityReader<'_>) -> Result<NodeValue> {
    let table = id.table_id;
    let node_table = entity
        .catalog
        .node_table(table)
        .ok_or_else(|| Error::runtime("Node id references a table missing from the catalog."))?;
    let columns: Vec<usize> = node_table
        .columns()
        .iter()
        .map(|column| column.column_id().0 as usize)
        .collect();
    let values = if let Some(values) = entity.sources.projected_values(
        table,
        id.offset.0,
        &columns,
        entity.catalog,
        entity.control,
        entity.memory.tracker(),
    )? {
        values
    } else {
        entity
            .storage
            .node_projected_values(entity.read, table, id.offset.0, &columns)
    };
    let props = node_table
        .columns()
        .iter()
        .zip(values)
        .map(|(column, value)| (column.name().to_string(), value))
        .collect();
    Ok(NodeValue {
        id,
        label: node_table.name().to_string(),
        props,
    })
}

/// Build a whole [`RelValue`] from its id (endpoints + label + all properties).
pub(crate) fn assemble_rel_value(id: InternalId, entity: EntityReader<'_>) -> Result<RelValue> {
    let table = id.table_id;
    let rel_table = entity.catalog.rel_table(table).ok_or_else(|| {
        Error::runtime("Relationship id references a table missing from the catalog.")
    })?;
    let (src, dst) = if let Some(endpoints) = entity.sources.relationship_endpoints(
        table,
        id.offset.0,
        entity.catalog,
        entity.control,
        entity.memory.tracker(),
    )? {
        endpoints
    } else {
        entity
            .storage
            .rel_endpoints(entity.read, table, id.offset.0)
    };
    let columns: Vec<usize> = rel_table
        .columns()
        .iter()
        .map(|column| column.column_id().0 as usize)
        .collect();
    let values = if let Some(values) = entity.sources.projected_values(
        table,
        id.offset.0,
        &columns,
        entity.catalog,
        entity.control,
        entity.memory.tracker(),
    )? {
        values
    } else {
        entity
            .storage
            .rel_projected_values(entity.read, table, id.offset.0, &columns)
    };
    let props = rel_table
        .columns()
        .iter()
        .zip(values)
        .map(|(column, value)| (column.name().to_string(), value))
        .collect();
    Ok(RelValue {
        src,
        dst,
        id,
        label: rel_table.name().to_string(),
        props,
        src_node: assemble_node_opt(src, entity)?.map(Box::new),
        dst_node: assemble_node_opt(dst, entity)?.map(Box::new),
    })
}

/// Assemble a node value from its id, or `None` for the sentinel "absent" id (an
/// unmatched optional endpoint).
pub(crate) fn assemble_node_opt(
    id: InternalId,
    entity: EntityReader<'_>,
) -> Result<Option<NodeValue>> {
    if id.table_id.0 == u64::MAX {
        Ok(None)
    } else {
        assemble_node_value(id, entity).map(Some)
    }
}

/// True if `ty` carries a NODE or REL anywhere — i.e. a value of this type may
/// hold a bare `InternalId` that must be inflated to a full node/rel value when
/// it escapes inside a container. `InternalId` (an `id()` result) and
/// `RecursiveRel` (paths, already materialized eagerly) deliberately do not count.
pub(crate) fn type_contains_graph(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Node(_) | LogicalType::Rel(_) => true,
        LogicalType::List(inner) | LogicalType::Array(inner, _) => type_contains_graph(inner),
        LogicalType::Map(k, v) => type_contains_graph(k) || type_contains_graph(v),
        LogicalType::Struct(fields) | LogicalType::Union(fields) => {
            fields.iter().any(|(_, t)| type_contains_graph(t))
        }
        _ => false,
    }
}

/// Inflate a bare node/rel `InternalId` nested inside `v` into a full
/// `Value::Node`/`Value::Rel`, guided by the value's static type. Used only at the
/// projection-output boundary and only for graph-typed items, so the per-row
/// pipeline keeps carrying cheap bare ids — a node/rel is materialized only when it
/// actually escapes inside a `collect`/list/map/struct. Idempotent (an already
/// assembled `Value::Node` passes through); `Null` (incl. an absent optional
/// endpoint, which reads as `Null`) passes through.
pub(crate) fn deep_materialize(
    v: Value,
    ty: &LogicalType,
    entity: EntityReader<'_>,
) -> Result<Value> {
    let value = match ty {
        LogicalType::Node(_) => match v {
            Value::InternalId(id) if id.table_id.0 != u64::MAX => {
                Value::Node(Box::new(assemble_node_value(id, entity)?))
            }
            Value::InternalId(_) => Value::Null,
            other => other,
        },
        LogicalType::Rel(_) => match v {
            Value::InternalId(id) if id.table_id.0 != u64::MAX => {
                Value::Rel(Box::new(assemble_rel_value(id, entity)?))
            }
            Value::InternalId(_) => Value::Null,
            other => other,
        },
        LogicalType::List(inner) | LogicalType::Array(inner, _) => match v {
            Value::List(items) => Value::List(
                items
                    .into_iter()
                    .map(|item| deep_materialize(item, inner, entity))
                    .collect::<Result<_>>()?,
            ),
            other => other,
        },
        LogicalType::Struct(fields) => match v {
            Value::Struct(values) => Value::Struct(
                values
                    .into_iter()
                    .enumerate()
                    .map(|(index, (name, value))| {
                        let value = match fields.get(index) {
                            Some((_, field_type)) => deep_materialize(value, field_type, entity)?,
                            None => value,
                        };
                        Ok((name, value))
                    })
                    .collect::<Result<_>>()?,
            ),
            other => other,
        },
        LogicalType::Map(key_type, value_type) => match v {
            Value::Map(pairs) => Value::Map(
                pairs
                    .into_iter()
                    .map(|(key, value)| {
                        Ok((
                            deep_materialize(key, key_type, entity)?,
                            deep_materialize(value, value_type, entity)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
            other => other,
        },
        _ => v,
    };
    Ok(value)
}

/// Static graph-containing result types that require deep materialization before
/// values leave the execution context.
pub(crate) fn output_deep_types(projection: &BoundProjection) -> Vec<Option<LogicalType>> {
    projection
        .items
        .iter()
        .map(|item| match item {
            ProjItem::Scalar { expr, .. } => {
                let ty = expr.ty();
                type_contains_graph(&ty).then_some(ty)
            }
            ProjItem::Var { .. } => None,
        })
        .collect()
}

pub(crate) fn deep_materialize_values(
    values: &mut [Value],
    item_types: &[Option<LogicalType>],
    ctx: &OperatorContext<'_>,
) -> Result<()> {
    let entity = EntityReader::from_ctx(ctx);
    for (value, ty) in values.iter_mut().zip(item_types) {
        if let Some(ty) = ty {
            let owned = std::mem::replace(value, Value::Null);
            *value = deep_materialize(owned, ty, entity)?;
        }
    }
    Ok(())
}
