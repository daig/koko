use super::*;

/// Bound read-side clauses for one query part, in textual order.
pub(super) struct BoundReading {
    pub(super) clauses: Vec<BoundReadingClause>,
}

#[derive(Clone)]
pub(super) struct ProjectionOutputRef {
    pub(super) index: usize,
    pub(super) ty: LogicalType,
}

impl ProjectionOutputRef {
    pub(super) fn as_column(&self) -> BoundExpr {
        BoundExpr::Column {
            col: self.index,
            ty: self.ty.clone(),
        }
    }
}

fn call_output_bindings(
    call: &ast::CallClause,
    schema_names: &[String],
) -> Result<Vec<(usize, String)>> {
    if call.yield_items.is_empty() {
        return Ok(schema_names
            .iter()
            .enumerate()
            .map(|(index, name)| (index, name.clone()))
            .collect());
    }

    let mut selected_sources = HashSet::new();
    let mut exposed_names = HashSet::new();
    let mut bindings = Vec::with_capacity(call.yield_items.len());
    for (column, alias) in &call.yield_items {
        let source_index = schema_names
            .iter()
            .position(|name| name.eq_ignore_ascii_case(column))
            .ok_or_else(|| {
                Error::binder(format!(
                    "Unknown table function output variable name: {column}."
                ))
            })?;
        if !selected_sources.insert(source_index) {
            return Err(Error::binder(format!(
                "Table function output variable {column} appears more than once in the yield clause."
            )));
        }
        let exposed_name = alias.clone().unwrap_or_else(|| column.clone());
        if !exposed_names.insert(exposed_name.to_ascii_lowercase()) {
            return Err(Error::binder(format!(
                "Variable {exposed_name} already exists."
            )));
        }
        bindings.push((source_index, exposed_name));
    }
    Ok(bindings)
}

impl Binder<'_, '_> {
    /// Bind one query operand into its ordered sequence of WITH-delimited parts.
    pub(super) fn bind_query(&mut self, query: &ast::SingleQuery) -> Result<BoundQuery> {
        let mut parts = Vec::new();
        let mut input_vars = Vec::new();
        let mut input_filter = None;

        for part in &query.parts {
            let reading = self.bind_reading(&part.reading)?;
            let updates = self.bind_updating(&part.updating)?;
            let projection = self.bind_with_projection(&part.with.projection)?;
            let subqueries = self.drain_subqueries()?;
            let sequence_calls = self.drain_sequence_calls();
            let carried = self.carry_forward(&projection)?;
            let next_filter = match &part.with.where_clause {
                Some(predicate) if !self.has_unbound_param(predicate) => {
                    Some(self.bind_expr(predicate)?)
                }
                _ => None,
            };
            if !self.pending_sequence_calls.is_empty() {
                return Err(Error::not_implemented(
                    "a sequence call in a WITH ... WHERE is not supported in this phase"
                        .to_string(),
                ));
            }
            parts.push(BoundPart {
                input_vars,
                input_filter,
                reading: reading.clauses,
                subqueries,
                sequence_calls,
                updates,
                projection: Some(projection),
                carried: carried.clone(),
            });
            input_vars = carried;
            input_filter = next_filter;
        }

        let reading = self.bind_reading(&query.reading)?;
        let updates = self.bind_updating(&query.updating)?;
        let projection = match &query.ret {
            Some(return_) => Some(self.bind_projection(return_)?),
            None => None,
        };
        let subqueries = self.drain_subqueries()?;
        let sequence_calls = self.drain_sequence_calls();
        if updates.is_empty() && projection.is_none() {
            return Err(Error::binder(
                "a query must either RETURN results or write data".to_string(),
            ));
        }
        parts.push(BoundPart {
            input_vars,
            input_filter,
            reading: reading.clauses,
            subqueries,
            sequence_calls,
            updates,
            projection,
            carried: Vec::new(),
        });

        Ok(BoundQuery {
            vars: std::mem::take(&mut self.vars),
            parts,
        })
    }
}

impl<'catalog, 'bind> Binder<'catalog, 'bind> {
    /// Validate a `HINT <join-tree>` against its MATCH clause (C++ hint binder;
    /// the single-order planner otherwise ignores the tree). Checks, in oracle
    /// order: correlation with previous patterns, anonymous pattern parts,
    /// unknown hint names, pattern-variable coverage, pairwise join
    /// resolvability, and rel storage-direction compatibility.
    pub(super) fn validate_join_hint(
        &self,
        hint: &ast::JoinHint,
        m: &ast::MatchClause,
        pre_scope: &std::collections::HashSet<String>,
    ) -> Result<()> {
        use std::collections::HashSet;
        // Pattern inventory: node vars, rel vars with endpoint names + table.
        let mut nodes: Vec<String> = Vec::new();
        let mut rels: Vec<(String, String, String, Vec<TableId>)> = Vec::new();
        let mut anonymous = false;
        for pe in &m.patterns {
            match &pe.head.var {
                Some(v) => nodes.push(v.clone()),
                None => anonymous = true,
            }
            let mut prev = pe.head.var.clone().unwrap_or_default();
            for (rel, node) in &pe.chains {
                let node_name = match &node.var {
                    Some(v) => {
                        nodes.push(v.clone());
                        v.clone()
                    }
                    None => {
                        anonymous = true;
                        String::new()
                    }
                };
                match &rel.var {
                    Some(rv) => {
                        let (src, dst) = match rel.direction {
                            ast::Direction::Left => (node_name.clone(), prev.clone()),
                            _ => (prev.clone(), node_name.clone()),
                        };
                        let tables = self.resolve_rel_tables(&rel.labels).unwrap_or_default();
                        rels.push((rv.clone(), src, dst, tables));
                    }
                    None => anonymous = true,
                }
                prev = node_name;
            }
        }
        // 1. Correlation: a pattern variable that already existed in scope.
        if nodes
            .iter()
            .chain(rels.iter().map(|(r, _, _, _)| r))
            .any(|v| pre_scope.contains(v))
        {
            return Err(Error::Raw(
                "Hint join pattern has correlation with previous patterns. This is not \
                 supported yet."
                    .to_string(),
            ));
        }
        // 2. Anonymous parts cannot be hinted ('patter' typo is verbatim C++).
        if anonymous {
            return Err(Error::binder(
                "Cannot hint join order in a match patter with anonymous node or relationship."
                    .to_string(),
            ));
        }
        // 3/4. Hint names must be pattern vars; every pattern var must appear.
        let mut hint_vars: Vec<String> = Vec::new();
        collect_hint_vars(hint, &mut hint_vars);
        let pattern_vars: HashSet<&str> = nodes
            .iter()
            .map(|s| s.as_str())
            .chain(rels.iter().map(|(r, _, _, _)| r.as_str()))
            .collect();
        for v in &hint_vars {
            if !pattern_vars.contains(v.as_str()) {
                return Err(Error::binder(format!(
                    "Cannot bind {v} to a node or relationship pattern"
                )));
            }
        }
        let hinted: HashSet<&str> = hint_vars.iter().map(|s| s.as_str()).collect();
        for v in pattern_vars {
            if !hinted.contains(v) {
                return Err(Error::binder(format!("Cannot find {v} in join hint.")));
            }
        }
        // 5/6. Pairwise resolvability + storage direction, bottom-up.
        self.resolve_hint_tree(hint, &rels)?;
        Ok(())
    }

    /// Resolve one hint subtree to its variable set, checking that each JOIN's
    /// sides connect through a rel endpoint and that the rel's anchor side is
    /// storable (fwd/bwd) for its table.
    pub(super) fn resolve_hint_tree(
        &self,
        t: &ast::JoinHint,
        rels: &[(String, String, String, Vec<TableId>)],
    ) -> Result<std::collections::HashSet<String>> {
        use std::collections::HashSet;
        match t {
            ast::JoinHint::Var(v) => Ok(HashSet::from([v.clone()])),
            ast::JoinHint::MultiJoin(l, names) => {
                let mut acc = self.resolve_hint_tree(l, rels)?;
                for n in names {
                    if let Some((_, src, dst, _)) = rels.iter().find(|(r, _, _, _)| r == n) {
                        if !acc.contains(src) && !acc.contains(dst) {
                            return Err(Error::binder(format!(
                                "Cannot resolve join condition between {} and Scan({n}).",
                                render_hint(l, rels)
                            )));
                        }
                    }
                    acc.insert(n.clone());
                }
                Ok(acc)
            }
            ast::JoinHint::Join(l, r) => {
                let left = self.resolve_hint_tree(l, rels)?;
                let right = self.resolve_hint_tree(r, rels)?;
                let mut connected = false;
                for (rel, src, dst, tables) in rels {
                    let (rel_in, other) = if left.contains(rel) {
                        (&left, &right)
                    } else if right.contains(rel) {
                        (&right, &left)
                    } else {
                        continue;
                    };
                    let src_in_rel_side = rel_in.contains(src);
                    let dst_in_rel_side = rel_in.contains(dst);
                    let src_in_other = other.contains(src);
                    let dst_in_other = other.contains(dst);
                    // A rel connects the two sides only through an endpoint in
                    // the *other* side; one fully internal to its own side
                    // contributes nothing (JOIN(Scan(a,e),Scan(b)) with c).
                    if src_in_other || dst_in_other {
                        // The rel's *anchor* is whichever endpoint it meets
                        // first: dst-anchored extends backward, which a
                        // fwd-only table cannot serve.
                        let anchored_dst =
                            (dst_in_rel_side || dst_in_other) && !(src_in_rel_side || src_in_other);
                        let anchored_src =
                            (src_in_rel_side || src_in_other) && !(dst_in_rel_side || dst_in_other);
                        for &tid in tables {
                            if let Some(rt) = self.catalog.rel_table(tid) {
                                if anchored_dst
                                    && rt.storage_direction() == RelStorageDirection::Fwd
                                {
                                    return Err(Error::runtime(format!(
                                        "Failed to get bwd data for rel table \"{}\", please \
                                         set the storage direction to BOTH",
                                        rt.name()
                                    )));
                                }
                                if anchored_src
                                    && rt.storage_direction() == RelStorageDirection::Bwd
                                {
                                    return Err(Error::runtime(format!(
                                        "Failed to get fwd data for rel table \"{}\", please \
                                         set the storage direction to BOTH",
                                        rt.name()
                                    )));
                                }
                            }
                        }
                        connected = true;
                    }
                }
                if !connected {
                    return Err(Error::binder(format!(
                        "Cannot resolve join condition between {} and {}.",
                        render_hint(l, rels),
                        render_hint(r, rels)
                    )));
                }
                Ok(left.union(&right).cloned().collect())
            }
        }
    }

    /// Bind a part's reading clauses in textual order, so each clause sees only
    /// variables introduced by its predecessors and keeps its own predicate.
    pub(super) fn bind_reading(&mut self, reading: &[ast::ReadingClause]) -> Result<BoundReading> {
        let mut clauses = Vec::with_capacity(reading.len());
        for clause in reading {
            match clause {
                ast::ReadingClause::LoadFrom(l) => {
                    let hinted_format = hinted_input_format(&l.path, &l.options)?;
                    validate_columnar_input_options(hinted_format, &l.options, false)?;
                    let mut options = bind_csv_options(&l.options)?;
                    let spellings: Vec<String> = std::iter::once(&l.path)
                        .chain(l.extra_paths.iter())
                        .cloned()
                        .collect();
                    let resolver = FileResolverConfig {
                        base_dir: self.session_config.base_dir.clone(),
                        home_directory: self.session_config.home_directory.clone(),
                        file_search_path: self.session_config.file_search_path.clone(),
                    };
                    let resolved =
                        resolve_files(&spellings, &resolver, options.file_format.as_deref())?;
                    let format = resolved[0].format;
                    validate_columnar_input_options(format, &l.options, false)?;
                    let paths: Vec<String> = resolved
                        .iter()
                        .map(|file| file.path.to_string_lossy().into_owned())
                        .collect();
                    let source_schema = if format == FileFormat::Csv {
                        None
                    } else {
                        let inspect = self.session_config.file_schema_resolver.ok_or_else(|| {
                            Error::not_implemented(format!(
                                "{format:?} metadata inspection is unavailable in this binder context."
                            ))
                        })?;
                        Some(inspect(format, &paths)?)
                    };
                    // Every file must share the sniffed arity (C++ "Number of
                    // columns mismatch." across a multi-file / glob source).
                    if format == FileFormat::Csv && paths.len() > 1 {
                        let arity0 =
                            koko_common::csv_dialect::sniffed_arity(&paths[0], &options, None);
                        for p in &paths[1..] {
                            let a = koko_common::csv_dialect::sniffed_arity(p, &options, None);
                            if let (Some(n), Some(m)) = (arity0, a) {
                                if n != m {
                                    return Err(Error::binder(format!(
                                        "Number of columns mismatch. Expected {n} but got {m}."
                                    )));
                                }
                            }
                        }
                    }
                    // The sniffing/column binding below use the first file.
                    let l = &ast::LoadFromClause {
                        headers: l.headers.clone(),
                        path: paths[0].clone(),
                        extra_paths: Vec::new(),
                        options: l.options.clone(),
                        where_clause: l.where_clause.clone(),
                    };
                    // Register each output column as a scalar variable, so a following
                    // WHERE/RETURN/CREATE/MATCH binds against it (the WHERE and MATCH
                    // that follow are bound afterwards, like a table-func scan).
                    let (columns, col_names, bare) = match (
                        l.headers.as_ref(),
                        source_schema.as_ref(),
                    ) {
                        // Typed `LOAD WITH HEADERS (col TYPE, …)`: declared columns.
                        // The declared count must match the source schema.
                        (Some(headers), source_schema) => {
                            let actual = source_schema.map(Vec::len).or_else(|| {
                                koko_common::csv_dialect::sniffed_arity(
                                    &l.path,
                                    &options,
                                    Some(headers.len()),
                                )
                            });
                            if actual.is_some_and(|actual| actual != headers.len()) {
                                return Err(Error::binder(format!(
                                    "Number of columns mismatch. Expected {} but got {}.",
                                    headers.len(),
                                    actual.unwrap()
                                )));
                            }
                            let mut columns = Vec::with_capacity(headers.len());
                            let mut col_names = Vec::with_capacity(headers.len());
                            for (index, (name, type_name)) in headers.iter().enumerate() {
                                let ty = self.resolve_ddl_type(type_name)?;
                                if let Some(source_schema) = source_schema {
                                    let (_, actual_ty) = &source_schema[index];
                                    if !actual_ty.to_string().eq_ignore_ascii_case(&ty.to_string())
                                    {
                                        return Err(Error::binder(format!(
                                            "{name} has data type {actual_ty} but {ty} was expected."
                                        )));
                                    }
                                }
                                let var = self.add_scalar_var(name.clone(), ty.clone());
                                columns.push((var, ty));
                                col_names.push(name.clone());
                            }
                            (columns, col_names, false)
                        }
                        (None, Some(source_schema)) => {
                            let mut columns = Vec::with_capacity(source_schema.len());
                            let mut col_names = Vec::with_capacity(source_schema.len());
                            for (index, (name, ty)) in source_schema.iter().enumerate() {
                                let var = self.add_scalar_var(name.clone(), ty.clone());
                                let positional = format!("column{index}");
                                if !self
                                    .scope
                                    .keys()
                                    .any(|bound| bound.eq_ignore_ascii_case(&positional))
                                {
                                    self.scope.insert(positional, var);
                                }
                                columns.push((var, ty.clone()));
                                col_names.push(name.clone());
                            }
                            (columns, col_names, false)
                        }
                        (None, None) => {
                            // Bare LOAD: the header decision is C++'s type-conflict
                            // sniff (audit W3 — a header-ish looking first data row
                            // must NOT be dropped). The decision is recorded into
                            // options.header so the scan honors it deterministically.
                            let col_names = match options.header {
                                Some(h) => koko_common::csv_dialect::sniff_columns(
                                    std::path::Path::new(&l.path),
                                    options.delimiter,
                                    options.quote,
                                    options.escape,
                                    options.auto_detect,
                                    options.parallel,
                                    h,
                                )?,
                                None => {
                                    let (h, names) = koko_common::csv_dialect::sniff_bare_columns(
                                        std::path::Path::new(&l.path),
                                        &options,
                                    )?;
                                    options.header = Some(h);
                                    names
                                }
                            };
                            // A header cell may carry a TYPED name
                            // (`age:UINT64`, `height:HEIGHT` — UDT aliases
                            // resolve): the declared type wins; other columns
                            // type from the sampled data (C++ sniffer:
                            // INT64/DECIMAL/DOUBLE/BOOL/temporal/UUID, else
                            // STRING).
                            let mut declared: Vec<Option<LogicalType>> =
                                Vec::with_capacity(col_names.len());
                            let mut col_names: Vec<String> = col_names
                                .into_iter()
                                .map(|n| match n.rsplit_once(':') {
                                    Some((base, ty_str)) if options.header == Some(true) => {
                                        match self.resolve_ddl_type(ty_str.trim()) {
                                            Ok(ty) => {
                                                declared.push(Some(ty));
                                                base.trim().to_string()
                                            }
                                            Err(_) => {
                                                declared.push(None);
                                                n
                                            }
                                        }
                                    }
                                    _ => {
                                        declared.push(None);
                                        n
                                    }
                                })
                                .collect();
                            let mut used_names = HashSet::new();
                            for name in &mut col_names {
                                let base = name.clone();
                                let mut suffix = 1usize;
                                while !used_names.insert(name.clone()) {
                                    *name = format!("{base}_{suffix}");
                                    suffix += 1;
                                }
                            }
                            let col_types = koko_common::csv_dialect::sniff_column_types(
                                std::path::Path::new(&l.path),
                                &options,
                                options.header == Some(true),
                                col_names.len(),
                            )?;
                            let mut columns = Vec::with_capacity(col_names.len());
                            for ((name, sniffed), decl) in
                                col_names.iter().zip(col_types).zip(declared)
                            {
                                let ty = decl.unwrap_or(sniffed);
                                let var = self.add_scalar_var(name.clone(), ty.clone());
                                columns.push((var, ty));
                            }
                            (columns, col_names, true)
                        }
                    };
                    let scan = BoundLoadScan {
                        columns,
                        col_names,
                        path: l.path.clone(),
                        paths,
                        format,
                        options,
                        bare,
                    };
                    // A `WHERE` on the LOAD filters at this clause's position.
                    let where_predicate = match &l.where_clause {
                        Some(predicate) if !self.has_unbound_param(predicate) => {
                            Some(self.bind_expr(predicate)?)
                        }
                        _ => None,
                    };
                    clauses.push(BoundReadingClause::Load {
                        scan,
                        where_predicate,
                    });
                }
                ast::ReadingClause::Call(call) => {
                    let (bound_call, kind) = self.bind_function_call(call)?;
                    match bound_call {
                        BoundCall::Table(bound_call) => {
                            if kind == FunctionCatalogKind::StandaloneTable {
                                return Err(Error::binder(format!(
                                    "{} is a standalone table function and cannot be used in a query pipeline.",
                                    crate::table_function::display_name(bound_call.function)
                                )));
                            }
                            let schema = table_func_schema(
                                self.catalog,
                                bound_call.function,
                                &bound_call.arguments,
                            )?;
                            let schema_names: Vec<String> =
                                schema.iter().map(|(name, _)| name.clone()).collect();
                            let output_bindings = call_output_bindings(call, &schema_names)?;
                            for (_, name) in &output_bindings {
                                if self.scope.keys().any(|key| key.eq_ignore_ascii_case(name)) {
                                    return Err(Error::binder(format!(
                                        "Variable {name} already exists."
                                    )));
                                }
                            }

                            let mut output_vars = vec![None; schema.len()];
                            for (source_index, name) in output_bindings {
                                let logical_type = schema[source_index].1.clone();
                                output_vars[source_index] =
                                    Some(self.add_scalar_var(name, logical_type));
                            }
                            let columns = schema
                                .iter()
                                .enumerate()
                                .map(|(source_index, (_, logical_type))| {
                                    let var = output_vars[source_index].unwrap_or_else(|| {
                                        self.add_var(
                                            None,
                                            VarKind::Scalar {
                                                ty: logical_type.clone(),
                                            },
                                            Vec::new(),
                                        )
                                    });
                                    (var, logical_type.clone())
                                })
                                .collect();
                            let where_predicate = match &call.where_clause {
                                Some(predicate) if !self.has_unbound_param(predicate) => {
                                    Some(self.bind_expr(predicate)?)
                                }
                                _ => None,
                            };
                            clauses.push(BoundReadingClause::TableFunction {
                                scan: BoundTableFunctionScan {
                                    call: bound_call,
                                    columns,
                                },
                                where_predicate,
                            });
                        }
                        BoundCall::GraphAlgorithm(bound_call) => {
                            debug_assert_eq!(kind, FunctionCatalogKind::Algorithm);
                            let function = bound_call.function;
                            let scalar_name = match function {
                                BuiltinGraphAlgorithm::KCoreDecomposition => "core",
                                BuiltinGraphAlgorithm::Louvain => "community_id",
                                BuiltinGraphAlgorithm::PageRank => "score",
                                BuiltinGraphAlgorithm::TopologicalLevels => "level",
                                BuiltinGraphAlgorithm::WeaklyConnectedComponents
                                | BuiltinGraphAlgorithm::StronglyConnectedComponents => {
                                    "component_id"
                                }
                            };
                            let output_bindings = call_output_bindings(
                                call,
                                &["node".to_string(), scalar_name.to_string()],
                            )?;
                            for (_, name) in &output_bindings {
                                if self.scope.keys().any(|key| key.eq_ignore_ascii_case(name)) {
                                    return Err(Error::binder(format!(
                                        "Variable {name} already exists."
                                    )));
                                }
                            }

                            let node_tables = &bound_call.graph.node_tables;
                            let label = node_tables
                                .first()
                                .and_then(|&table| self.catalog.node_table(table))
                                .map(|table| table.name().to_string())
                                .unwrap_or_default();
                            let properties = self.node_props_union(node_tables);
                            let scalar_type = if function == BuiltinGraphAlgorithm::PageRank {
                                LogicalType::Double
                            } else {
                                LogicalType::Int(koko_common::IntKind::I64)
                            };
                            let mut output_vars = [None, None];
                            for (source_index, name) in output_bindings {
                                let var = match source_index {
                                    0 => self.add_var(
                                        Some(name),
                                        VarKind::Node {
                                            tables: node_tables.clone(),
                                            label: label.clone(),
                                        },
                                        properties.clone(),
                                    ),
                                    1 => self.add_scalar_var(name, scalar_type.clone()),
                                    _ => unreachable!("graph algorithms have two outputs"),
                                };
                                output_vars[source_index] = Some(var);
                            }
                            let node = output_vars[0].unwrap_or_else(|| {
                                self.add_var(
                                    None,
                                    VarKind::Node {
                                        tables: node_tables.clone(),
                                        label,
                                    },
                                    properties,
                                )
                            });
                            let scalar = output_vars[1].unwrap_or_else(|| {
                                self.add_var(None, VarKind::Scalar { ty: scalar_type }, Vec::new())
                            });
                            let id = GraphAlgorithmScanId(self.next_graph_algorithm_scan_id);
                            self.next_graph_algorithm_scan_id = self
                                .next_graph_algorithm_scan_id
                                .checked_add(1)
                                .ok_or_else(|| {
                                    Error::binder(
                                        "Query contains too many graph algorithm scans."
                                            .to_string(),
                                    )
                                })?;
                            let where_predicate = match &call.where_clause {
                                Some(predicate) if !self.has_unbound_param(predicate) => {
                                    Some(self.bind_expr(predicate)?)
                                }
                                _ => None,
                            };
                            let output = match function {
                                BuiltinGraphAlgorithm::KCoreDecomposition => {
                                    BoundGraphAlgorithmOutput::KCoreDecomposition {
                                        node,
                                        core: scalar,
                                    }
                                }
                                BuiltinGraphAlgorithm::Louvain => {
                                    BoundGraphAlgorithmOutput::Louvain {
                                        node,
                                        community_id: scalar,
                                    }
                                }
                                BuiltinGraphAlgorithm::PageRank => {
                                    BoundGraphAlgorithmOutput::PageRank {
                                        node,
                                        score: scalar,
                                    }
                                }
                                BuiltinGraphAlgorithm::TopologicalLevels => {
                                    BoundGraphAlgorithmOutput::TopologicalLevels {
                                        node,
                                        level: scalar,
                                    }
                                }
                                BuiltinGraphAlgorithm::WeaklyConnectedComponents => {
                                    BoundGraphAlgorithmOutput::WeaklyConnectedComponents {
                                        node,
                                        component_id: scalar,
                                    }
                                }
                                BuiltinGraphAlgorithm::StronglyConnectedComponents => {
                                    BoundGraphAlgorithmOutput::StronglyConnectedComponents {
                                        node,
                                        component_id: scalar,
                                    }
                                }
                            };
                            clauses.push(BoundReadingClause::GraphAlgorithm {
                                scan: BoundGraphAlgorithmScan {
                                    id,
                                    call: bound_call,
                                    output,
                                },
                                where_predicate,
                            });
                        }
                    }
                }
                ast::ReadingClause::Match(m) if m.optional => {
                    let mut opt_match = BoundMatch::default();
                    let mut opt_preds = Vec::new();
                    let pre_scope: std::collections::HashSet<String> =
                        self.scope.keys().cloned().collect();
                    // Variables bound before this OPTIONAL clause (id < boundary) are
                    // the left rows it joins against; narrowing their endpoints would
                    // reduce the outer driving cardinality (fixes match7.S25). Fresh
                    // endpoints introduced within the pattern still narrow normally.
                    let boundary = self.vars.len();
                    for pe in &m.patterns {
                        self.bind_match_pattern(
                            pe,
                            &mut opt_match,
                            &mut opt_preds,
                            Some(boundary),
                        )?;
                    }
                    if let Some(hint) = &m.hint {
                        self.validate_join_hint(hint, m, &pre_scope)?;
                    }
                    if let Some(w) = &m.where_clause {
                        if !self.has_unbound_param(w) {
                            opt_preds.push(self.bind_expr(w)?);
                        }
                    }
                    clauses.push(BoundReadingClause::OptionalMatch(BoundOptionalMatch {
                        match_: opt_match,
                        where_predicate: combine_and(opt_preds),
                    }));
                }
                ast::ReadingClause::Match(m) => {
                    let mut match_ = BoundMatch::default();
                    let mut predicates = Vec::new();
                    let pre_scope: std::collections::HashSet<String> =
                        self.scope.keys().cloned().collect();
                    for pe in &m.patterns {
                        self.bind_match_pattern(pe, &mut match_, &mut predicates, None)?;
                    }
                    if let Some(hint) = &m.hint {
                        self.validate_join_hint(hint, m, &pre_scope)?;
                    }
                    if let Some(w) = &m.where_clause {
                        if !self.has_unbound_param(w) {
                            predicates.push(self.bind_expr(w)?);
                        }
                    }
                    clauses.push(BoundReadingClause::Match {
                        match_,
                        where_predicate: combine_and(predicates),
                    });
                }
                ast::ReadingClause::Unwind(u) => {
                    // Redefining an in-scope variable is the C++ binder error
                    // (names compare case-insensitively).
                    if self.scope.keys().any(|k| k.eq_ignore_ascii_case(&u.var)) {
                        return Err(Error::binder(format!("Variable {} already exists.", u.var)));
                    }
                    let list = self.bind_expr(&u.expr)?;
                    let elem_ty = match list.ty() {
                        LogicalType::List(inner) | LogicalType::Array(inner, _) => *inner,
                        // NULL or an untyped expression unwinds to zero rows.
                        // STRING remains permissive because a bare CSV cell may
                        // contain normalized list text while retaining static
                        // STRING type. Every other scalar is rejected below.
                        LogicalType::Any | LogicalType::String => LogicalType::Any,
                        other => {
                            return Err(Error::binder(format!(
                                "{} has data type {other} but LIST was expected.",
                                expr_name(&u.expr)
                            )));
                        }
                    };
                    let var = self.add_unwind_var(u.var.clone(), elem_ty);
                    clauses.push(BoundReadingClause::Unwind(BoundUnwind { var, list }));
                }
            }
        }
        Ok(BoundReading { clauses })
    }

    /// Bind a part's updating clauses (`CREATE`). Returns `None` if there are none.
    /// Bind a part's updating clauses (`CREATE`/`SET`/`DELETE`) in order.
    pub(super) fn bind_updating(
        &mut self,
        updating: &[ast::UpdatingClause],
    ) -> Result<Vec<BoundUpdate>> {
        let mut out = Vec::with_capacity(updating.len());
        for clause in updating {
            match clause {
                ast::UpdatingClause::Create(c) => {
                    let mut create = BoundCreate::default();
                    for pe in &c.patterns {
                        self.bind_create_pattern(pe, &mut create)?;
                    }
                    // Every pattern part resolved to an already-bound variable —
                    // nothing to create is a bind error (C++ bindCreateClause).
                    if create.nodes.is_empty() && create.rels.is_empty() {
                        return Err(Error::binder(
                            "Cannot resolve any node or relationship to create.".to_string(),
                        ));
                    }
                    out.push(BoundUpdate::Create(create));
                }
                ast::UpdatingClause::Set(s) => out.push(BoundUpdate::Set(self.bind_set(s)?)),
                ast::UpdatingClause::Delete(d) => {
                    out.push(BoundUpdate::Delete(self.bind_delete(d)?))
                }
                ast::UpdatingClause::Merge(m) => {
                    out.push(BoundUpdate::Merge(Box::new(self.bind_merge(m)?)))
                }
            }
        }
        Ok(out)
    }

    /// Bind a `SET` clause.
    pub(super) fn bind_set(&mut self, set: &ast::SetClause) -> Result<BoundSet> {
        self.bind_set_items(&set.items)
    }

    /// Bind a list of `SET` assignments (shared by `SET` and `MERGE`'s
    /// `ON CREATE`/`ON MATCH SET`): resolve each target and reject PK writes.
    pub(super) fn bind_set_items(&mut self, set_items: &[ast::SetItem]) -> Result<BoundSet> {
        let mut items = Vec::with_capacity(set_items.len());
        for item in set_items {
            let mut value = self.bind_expr(&item.value)?;
            let target = match &item.target {
                ast::SetTarget::Property { var, name } => {
                    let id = self.set_target_var(var)?;
                    let info = &self.vars[id.0 as usize];
                    let dynamic = info.property(name).is_none() && self.is_any_var(id);
                    if dynamic {
                        BoundSetTarget::DynamicProperty {
                            var: id,
                            prop: name.clone(),
                        }
                    } else {
                        let prop = info.property(name).ok_or_else(|| {
                            Error::binder(format!("Cannot find property {name} for {var}."))
                        })?;
                        let prop_name = prop.name.clone();
                        let prop_ty = prop.ty.clone();
                        let node_tables = info.node_tables().to_vec();
                        adopt_struct_field_names(&mut value, &prop_ty);
                        assignable_or_err(&value, &prop_ty, &expr_name(&item.value))?;
                        value = self.coerce_to(value, &prop_ty);
                        // A primary-key column cannot be updated. For polymorphic nodes,
                        // this only rejects candidate tables whose own PK has this name;
                        // tables lacking the property are skipped by the runtime setter.
                        for t in node_tables {
                            let nt = self.catalog.node_table(t).unwrap();
                            if nt.primary_key_column().name().eq_ignore_ascii_case(name) {
                                return Err(Error::binder(format!(
                                    "Cannot set property {name} in table {} because it is used as \
                                     primary key. Try delete and then insert.",
                                    nt.name()
                                )));
                            }
                        }
                        BoundSetTarget::Property {
                            var: id,
                            prop: prop_name,
                        }
                    }
                }
                ast::SetTarget::Var(var) => {
                    let id = self.set_target_var(var)?;
                    // A whole-map `SET a = {…}` is C++'s `SET a = properties`
                    // surface: each listed key is a property write, and unlisted
                    // properties are preserved. Validate literal keys at bind time
                    // against the target's current (possibly endpoint-pruned) property
                    // set, and reject primary-key writes just like `SET a.pk = …`.
                    if let ast::Expr::Struct(fields) = &item.value {
                        if self.is_any_var(id) {
                            items.push(BoundSetItem {
                                target: BoundSetTarget::Var { var: id },
                                value,
                            });
                            continue;
                        }
                        let info = &self.vars[id.0 as usize];
                        for (k, _) in fields {
                            if info.property(k).is_none() {
                                return Err(Error::binder(format!(
                                    "Cannot find property {k} for {var}."
                                )));
                            }
                        }
                        for &t in info.node_tables() {
                            let nt = self.catalog.node_table(t).unwrap();
                            let pk = nt.primary_key_column().name();
                            if fields.iter().any(|(k, _)| k.eq_ignore_ascii_case(pk)) {
                                return Err(Error::binder(format!(
                                    "Cannot set property {pk} in table {} because it is used as \
                                     primary key. Try delete and then insert.",
                                    nt.name()
                                )));
                            }
                        }
                        if let BoundExpr::Struct {
                            fields: bound_fields,
                            ..
                        } = &value
                        {
                            for (k, v) in bound_fields {
                                let prop = info.property(k).unwrap();
                                // Render the field's *value* expression (C++ reports the
                                // assigned expression, not the property name).
                                let ename = fields
                                    .iter()
                                    .find(|(fk, _)| fk.eq_ignore_ascii_case(k))
                                    .map(|(_, ve)| expr_name(ve))
                                    .unwrap_or_else(|| k.clone());
                                assignable_or_err(v, &prop.ty, &ename)?;
                            }
                        }
                    }
                    BoundSetTarget::Var { var: id }
                }
            };
            items.push(BoundSetItem { target, value });
        }
        Ok(BoundSet { items })
    }

    /// Resolve a `SET` target variable, requiring it be a (non-recursive) node or
    /// relationship.
    pub(super) fn set_target_var(&self, name: &str) -> Result<VarId> {
        let id = self.lookup_var(name)?;
        let info = &self.vars[id.0 as usize];
        if info.is_recursive() || !matches!(info.kind, VarKind::Node { .. } | VarKind::Rel { .. }) {
            return Err(Error::binder(format!(
                "Cannot set expression {name} with type VARIABLE. Expect node or rel pattern."
            )));
        }
        Ok(id)
    }

    /// Bind a `[DETACH] DELETE`: each target must be a node/relationship variable.
    pub(super) fn bind_delete(&self, del: &ast::DeleteClause) -> Result<BoundDelete> {
        let mut vars = Vec::with_capacity(del.exprs.len());
        for e in &del.exprs {
            let ast::Expr::Variable(name) = e else {
                // C++ types the rejection by expression kind (PROPERTY etc.).
                return Err(Error::binder(format!(
                    "Cannot delete expression {} with type {}. Expect node or rel pattern.",
                    expr_name(e),
                    ast_expr_kind(e)
                )));
            };
            let id = self.lookup_var(name)?;
            let info = &self.vars[id.0 as usize];
            match &info.kind {
                VarKind::Node { .. } => {}
                VarKind::Rel {
                    recursive: Some(_), ..
                } => {
                    return Err(Error::binder(format!(
                        "Cannot delete expression {name} with type VARIABLE. Expect node or rel pattern."
                    )));
                }
                VarKind::Rel { directed, .. } => {
                    if del.detach {
                        return Err(Error::binder(
                            "Detach delete on rel tables is not supported.".to_string(),
                        ));
                    }
                    if !*directed {
                        return Err(Error::binder(
                            "Delete undirected rel is not supported.".to_string(),
                        ));
                    }
                }
                VarKind::Path { .. } | VarKind::Scalar { .. } => {
                    return Err(Error::binder(format!(
                        "Cannot delete expression {name} with type VARIABLE. Expect node or rel pattern."
                    )));
                }
            }
            vars.push(id);
        }
        Ok(BoundDelete {
            vars,
            detach: del.detach,
        })
    }

    pub(super) fn bind_match_pattern(
        &mut self,
        pe: &ast::PatternElement,
        match_: &mut BoundMatch,
        predicates: &mut Vec<BoundExpr>,
        opt_boundary: Option<usize>,
    ) -> Result<()> {
        let head = self.bind_match_node(&pe.head)?;
        match_.node_vars.push(head);
        self.inline_predicates(head, &pe.head.properties, predicates)?;
        self.any_label_predicates(head, &pe.head.labels, predicates);

        let mut prev = head;
        let mut segments = Vec::with_capacity(pe.chains.len());
        for (rel, node) in &pe.chains {
            let node_var = self.bind_match_node(node)?;
            let rel_var = self.bind_match_rel(rel, prev, node_var, opt_boundary)?;
            self.any_label_predicates(rel_var, &rel.labels, predicates);
            self.any_label_predicates(node_var, &node.labels, predicates);
            self.inline_predicates(node_var, &node.properties, predicates)?;
            // Inline properties on a recursive rel (`[e:knows* {date: …}]`) are a
            // per-step filter over each rel in the path (handled with the lambda
            // predicate), not a single-rel equality — deferred.
            if rel.recursive.is_none() {
                self.inline_predicates(rel_var, &rel.properties, predicates)?;
            }
            match_.node_vars.push(node_var);
            match_.rel_vars.push(rel_var);
            segments.push((rel_var, node_var));
            prev = node_var;
        }

        // A named path `p = (…)` binds `p` to the whole pattern as a
        // `RECURSIVE_REL` value (assembled by the planner/processor).
        if let Some(name) = &pe.name {
            let path = self.add_var(
                Some(name.clone()),
                VarKind::Path { head, segments },
                Vec::new(),
            );
            match_.path_vars.push(path);
        }
        Ok(())
    }

    /// Bind a node pattern in MATCH context (reusing an in-scope variable).
    pub(super) fn bind_match_node(&mut self, np: &ast::NodePattern) -> Result<VarId> {
        if let Some(name) = &np.var {
            if let Some(&id) = self.scope.get(name) {
                if !self.vars[id.0 as usize].is_node() {
                    return Err(Error::binder(format!(
                        "Cannot bind {name} as node pattern."
                    )));
                }
                // Re-stating labels on an in-scope node *unions* them into the
                // candidate set (Kùzu semantics): `MATCH (a:A:B) MATCH (a:C)`
                // treats `a` as `A ∪ B ∪ C`.
                if !np.labels.is_empty() {
                    let add = self.resolve_node_tables(&np.labels)?;
                    self.union_node_tables(id, &add);
                }
                return Ok(id);
            }
        }
        let tables = self.resolve_node_tables(&np.labels)?;
        // An unlabeled pattern over an empty database has no representative table;
        // its label is empty and it scans nothing.
        let label = tables
            .first()
            .and_then(|&t| self.catalog.node_table(t))
            .map(|table| table.name().to_string())
            .unwrap_or_default();
        let props = self.node_props_union(&tables);
        Ok(self.add_var(np.var.clone(), VarKind::Node { tables, label }, props))
    }

    /// Add tables to an in-scope node variable's candidate set (re-match union).
    pub(super) fn union_node_tables(&mut self, var: VarId, add: &[TableId]) {
        let info = &self.vars[var.0 as usize];
        if !matches!(info.kind, VarKind::Node { .. }) {
            return;
        }
        let mut tables = info.node_tables().to_vec();
        let mut changed = false;
        for &t in add {
            if !tables.contains(&t) {
                tables.push(t);
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let label = self
            .catalog
            .node_table(tables[0])
            .unwrap()
            .name()
            .to_string();
        let props = self.node_props_union(&tables);
        let info = &mut self.vars[var.0 as usize];
        info.kind = VarKind::Node { tables, label };
        info.properties = props;
    }

    pub(super) fn bind_match_rel(
        &mut self,
        rp: &ast::RelPattern,
        left: VarId,
        right: VarId,
        opt_boundary: Option<usize>,
    ) -> Result<VarId> {
        let (src, dst, directed) = match rp.direction {
            ast::Direction::Right => (left, right, true),
            ast::Direction::Left => (right, left, true),
            ast::Direction::Both => (left, right, false),
        };
        if let Some(name) = &rp.var {
            if let Some(&id) = self.scope.get(name) {
                let expected = if rp.recursive.is_some() {
                    "RECURSIVE_REL"
                } else {
                    "REL"
                };
                let actual = self.var_data_type_name(id);
                if actual != expected {
                    return Err(Error::binder(format!(
                        "{name} has data type {actual} but {expected} was expected."
                    )));
                }
                return Err(Error::binder(format!(
                    "Bind relationship {name} to relationship with same name is not supported."
                )));
            }
        }
        // A variable-length / recursive relationship binds differently: its value
        // is a `RECURSIVE_REL` (no per-property columns), the endpoints are *not*
        // narrowed (intermediate nodes may use any table along the path), and it
        // carries length bounds + search semantics.
        if let Some(rec) = &rp.recursive {
            return self.bind_recursive_rel(rp, rec, src, dst, directed);
        }
        // Candidate relationship tables (named types, or all when unlabeled),
        // then keep only those that connect the endpoints given their current
        // node-table sets and the arrow direction. Accumulate the node tables
        // each endpoint is thereby pinned to (the union over kept rels).
        let candidates = self.resolve_rel_tables(&rp.labels)?;
        let declared = candidates.clone();
        let src_tables = self.vars[src.0 as usize].node_tables().to_vec();
        let dst_tables = self.vars[dst.0 as usize].node_tables().to_vec();
        let mut kept = Vec::new();
        let mut src_narrow = Vec::new();
        let mut dst_narrow = Vec::new();
        for &r in &candidates {
            let rt = self.catalog.rel_table(r).unwrap();
            // A multi-pair rel table connects if ANY of its FROM-TO pairs does;
            // each connecting pair contributes to the endpoint narrowing.
            let mut connects = false;
            for pair in rt.pairs() {
                let fwd = src_tables.contains(&pair.from) && dst_tables.contains(&pair.to);
                // An undirected pattern also matches the reverse orientation.
                let bwd =
                    !directed && src_tables.contains(&pair.to) && dst_tables.contains(&pair.from);
                if fwd {
                    push_unique(&mut src_narrow, pair.from);
                    push_unique(&mut dst_narrow, pair.to);
                }
                if bwd {
                    push_unique(&mut src_narrow, pair.to);
                    push_unique(&mut dst_narrow, pair.from);
                }
                connects |= fwd || bwd;
            }
            if connects {
                kept.push(r);
            }
        }
        // When some candidate connects, narrow the endpoints to the tables those
        // rels pin them to and bind only the connecting rels. When NONE connects
        // (an over-constrained / wrong-label pattern, e.g. a node forced to be two
        // disjoint labels), the pattern is simply unsatisfiable: bind the declared
        // candidates without narrowing and let execution yield zero rows — Kùzu
        // returns an empty result here, not an error.
        // Storage-direction constraints (C++ binder): a pattern matching both
        // fwd-only and bwd-only tables has no common scan direction; a
        // bwd-only table cannot be queried at all.
        {
            let name = rp.var.as_deref().unwrap_or("");
            let dirs: Vec<RelStorageDirection> = kept
                .iter()
                .filter_map(|&r| self.catalog.rel_table(r).map(|rt| rt.storage_direction()))
                .collect();
            let has_fwd_only = dirs.contains(&RelStorageDirection::Fwd);
            let has_bwd_only = dirs.contains(&RelStorageDirection::Bwd);
            if has_fwd_only && has_bwd_only {
                return Err(Error::binder(format!(
                    "There are no common storage directions among the rel tables matched by \
                     pattern '{name}' (some tables have storage direction 'fwd' while others \
                     have storage direction 'bwd'). Scanning different tables matching the \
                     same pattern in different directions is currently unsupported."
                )));
            }
            if has_bwd_only {
                return Err(Error::binder(format!(
                    "Querying table matched in rel pattern '{name}' with bwd-only storage \
                     direction isn't supported."
                )));
            }
        }
        // C++ rejects an UNDIRECTED pattern when any matched rel table is not
        // stored BOTH ways (binder.cpp validateRelDirection) — the bwd scan a
        // reversed orientation needs would be missing.
        if !directed {
            let any_partial = kept.iter().any(|&r| {
                self.catalog
                    .rel_table(r)
                    .is_some_and(|rt| rt.storage_direction() != RelStorageDirection::Both)
            });
            if any_partial {
                let name = rp.var.as_deref().unwrap_or("");
                return Err(Error::binder(format!(
                    "Undirected rel pattern '{name}' has at least one matched rel table with \
                     storage type 'fwd' or 'bwd'. Undirected rel patterns are only supported \
                     if every matched rel table has storage type 'both'."
                )));
            }
        }
        let tables = if kept.is_empty() {
            candidates
        } else {
            // Don't narrow an endpoint bound by a scope enclosing an OPTIONAL MATCH:
            // narrowing only prunes the scan (the runtime extend still filters by real
            // adjacency, so dropping the prune is result-safe), but for the optional's
            // left rows it would wrongly reduce the driving cardinality (match7.S25).
            let outer_bound = |v: VarId| matches!(opt_boundary, Some(b) if (v.0 as usize) < b);
            if !outer_bound(src) {
                self.narrow_node_to_set(src, &src_narrow);
            }
            if !outer_bound(dst) {
                self.narrow_node_to_set(dst, &dst_narrow);
            }
            kept
        };
        // An unlabeled pattern over a database with no rel tables has no
        // representative table; its label is empty and it extends nothing.
        let label = tables
            .first()
            .and_then(|&t| self.catalog.rel_table(t))
            .map(|table| table.name().to_string())
            .unwrap_or_default();
        // Properties resolve against the *declared* candidate set (labels as
        // written), not the connectivity-kept subset: C++ binds `e.year` on
        // `-[e:knows|:studyAt]->` even when the endpoints keep only `knows`,
        // pruning the write / reading NULL where the column is absent.
        let props = self.rel_props_union(&declared);
        Ok(self.add_var(
            rp.var.clone(),
            VarKind::Rel {
                tables,
                label,
                src,
                dst,
                directed,
                recursive: None,
            },
            props,
        ))
    }

    /// Bind a variable-length / recursive relationship: resolve its candidate rel
    /// tables (named, or all when unlabeled), normalize + validate the length
    /// bounds, and record the search mode/semantic. Endpoints keep their declared
    /// labels (only the path's two ends are constrained; intermediates are free).
    pub(super) fn bind_recursive_rel(
        &mut self,
        rp: &ast::RelPattern,
        rec: &ast::RecursiveInfo,
        src: VarId,
        dst: VarId,
        directed: bool,
    ) -> Result<VarId> {
        let name = rp.var.clone().unwrap_or_default();
        let max_depth = self.max_recursive_depth;
        let lower = rec.bounds.0.unwrap_or(1);
        let upper = rec.bounds.1.unwrap_or(max_depth);
        if lower > upper {
            return Err(Error::binder(format!(
                "Lower bound of rel {name} is greater than upperBound."
            )));
        }
        // SHORTEST / ALL SHORTEST require a lower bound of exactly 1 (C++).
        if !matches!(rec.mode, ast::RecursiveMode::All) && lower != 1 {
            return Err(Error::binder(
                "Lower bound of shortest/all_shortest path must be 1.".to_string(),
            ));
        }
        if upper > max_depth {
            return Err(Error::binder(format!(
                "Upper bound of rel {name} exceeds maximum: {max_depth}."
            )));
        }
        let tables = self.resolve_rel_tables(&rp.labels)?;
        let label = tables
            .first()
            .and_then(|&t| self.catalog.rel_table(t))
            .map(|table| table.name().to_string())
            .unwrap_or_default();
        let filter = self.bind_recursive_filter(rp, rec)?;
        // A (ALL) WSHORTEST weight column resolves against the rel table(s): it
        // must exist and NOT be INT128 (C++ rejects that width).
        let weight = match &rec.weight_col {
            Some(col) => {
                let ty = tables
                    .iter()
                    .find_map(|&t| {
                        self.catalog.rel_table(t).and_then(|rt| {
                            rt.columns()
                                .iter()
                                .find(|column| column.name().eq_ignore_ascii_case(col))
                                .map(|column| column.logical_type().clone())
                        })
                    })
                    .ok_or_else(|| {
                        Error::binder(format!("Cannot find property {col} for {name}."))
                    })?;
                // Supported weights are the ≤64-bit integers plus FLOAT/DOUBLE;
                // INT128/UINT128 and DECIMAL are rejected (with the type name).
                let supported = matches!(
                    ty,
                    LogicalType::Int(k) if k.byte_width() <= 8
                ) || matches!(ty, LogicalType::Double | LogicalType::Float);
                if !supported {
                    return Err(Error::binder(format!(
                        "{ty} weight type is not supported for weighted shortest path."
                    )));
                }
                Some(col.clone())
            }
            None => None,
        };
        let spec = RecursiveSpec {
            lower,
            upper,
            mode: bind_recursive_mode(rec.mode),
            semantic: bind_path_semantic(rec.semantic),
            filter,
            weight,
        };
        // A recursive rel exposes no per-property columns (its value is assembled).
        Ok(self.add_var(
            rp.var.clone(),
            VarKind::Rel {
                tables,
                label,
                src,
                dst,
                directed,
                recursive: Some(Box::new(spec)),
            },
            Vec::new(),
        ))
    }

    /// Bind a recursive pattern's per-step filter: the `(r, n | WHERE …)` lambda
    /// predicate plus any inline `{prop: …}` (folded into the relationship part as
    /// `r.prop = …`). The bound predicate's top-level `AND` conjuncts are split by
    /// which lambda parameter they reference (a node-referencing conjunct gates
    /// intermediates; the rest gate every relationship).
    pub(super) fn bind_recursive_filter(
        &mut self,
        rp: &ast::RelPattern,
        rec: &ast::RecursiveInfo,
    ) -> Result<Option<RecursiveFilter>> {
        let lambda_pred = rec.lambda.as_ref().and_then(|l| l.predicate.as_ref());
        // C++ validates the node projection list before the rel list (with both
        // invalid, the node item is blamed — oracle-verified).
        let node_proj = rec
            .lambda
            .as_ref()
            .and_then(|l| l.node_projection.as_ref())
            .map(|es| projection_names(es))
            .transpose()?;
        let rel_proj = rec
            .lambda
            .as_ref()
            .and_then(|l| l.rel_projection.as_ref())
            .map(|es| projection_names(es))
            .transpose()?;
        let any_dynamic_labels = self.catalog.any_tables().is_some() && !rp.labels.is_empty();
        if lambda_pred.is_none()
            && rp.properties.is_empty()
            && rel_proj.is_none()
            && node_proj.is_none()
            && !any_dynamic_labels
        {
            return Ok(None);
        }
        let (rel_param_name, node_param_name) = match &rec.lambda {
            Some(lambda) => (lambda.rel_var.clone(), lambda.node_var.clone()),
            None => ("__rel".to_string(), "__node".to_string()),
        };
        let rel_param = self.allocate_lambda_id(&rel_param_name);
        let node_param = self.allocate_lambda_id(&node_param_name);

        let mut rel_parts = Vec::new();
        let mut node_parts = Vec::new();

        if let Some(pred) = lambda_pred {
            // Bind the predicate with both typed lambda variables in scope.
            let depth = self.lambda_params.len();
            self.lambda_params.extend([
                LambdaBinding {
                    name: rel_param_name.clone(),
                    id: rel_param,
                    ty: LogicalType::Any,
                },
                LambdaBinding {
                    name: node_param_name.clone(),
                    id: node_param,
                    ty: LogicalType::Any,
                },
            ]);
            let bound = self.bind_expr(pred);
            self.lambda_params.truncate(depth);
            let bound = bound?;
            // A lifted subquery/sequence column cannot be evaluated inside the
            // per-step lambda, so reject it at bind. Supporting this shape requires
            // the correlated frontier sub-pipeline described by ROADMAP.md.
            if bound.contains_lifted() {
                return Err(Error::binder(
                    "EXISTS/COUNT subqueries and sequence functions are not supported in \
                     a recursive relationship's per-step filter."
                        .to_string(),
                ));
            }
            for conj in split_and(bound) {
                let depend_on_node = mentions_lambda(&conj, node_param);
                let depend_on_rel = mentions_lambda(&conj, rel_param);
                if depend_on_node && depend_on_rel {
                    return Err(Error::binder(format!(
                        "Cannot evaluate {} because it depends on both {} and {}.",
                        self.bound_expr_name(&conj),
                        node_param_name,
                        rel_param_name
                    )));
                } else if depend_on_node {
                    node_parts.push(conj);
                } else {
                    rel_parts.push(conj);
                }
            }
        }

        if any_dynamic_labels {
            let label = BoundExpr::ValueProperty {
                value: Box::new(BoundExpr::LambdaVar {
                    id: rel_param,
                    ty: LogicalType::Any,
                }),
                prop: "label".to_string(),
                ty: LogicalType::String,
            };
            let mut labels = rp.labels.iter().map(|name| BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args: vec![
                    label.clone(),
                    BoundExpr::Literal(Value::String(name.clone())),
                ],
                ty: LogicalType::Bool,
            });
            if let Some(first) = labels.next() {
                rel_parts.push(labels.fold(first, |left, right| BoundExpr::Scalar {
                    op: ScalarOp::Or,
                    args: vec![left, right],
                    ty: LogicalType::Bool,
                }));
            }
        }

        // Inline `{prop: val}` ⇒ `rel_param.prop = val` on every relationship.
        for (k, v) in &rp.properties {
            let val = self.bind_expr(v)?;
            let entity = BoundExpr::LambdaVar {
                id: rel_param,
                ty: LogicalType::Any,
            };
            let lhs = if self.catalog.any_tables().is_some() {
                BoundExpr::ValueProperty {
                    value: Box::new(BoundExpr::ValueProperty {
                        value: Box::new(entity),
                        prop: "data".to_string(),
                        ty: LogicalType::Json,
                    }),
                    prop: k.clone(),
                    ty: LogicalType::Any,
                }
            } else {
                BoundExpr::ValueProperty {
                    value: Box::new(entity),
                    prop: k.clone(),
                    ty: LogicalType::Any,
                }
            };
            rel_parts.push(BoundExpr::Scalar {
                op: ScalarOp::Eq,
                args: vec![lhs, val],
                ty: LogicalType::Bool,
            });
        }

        Ok(Some(RecursiveFilter {
            rel_param,
            node_param,
            rel_pred: combine_and(rel_parts),
            node_pred: combine_and(node_parts),
            rel_proj,
            node_proj,
        }))
    }

    /// Render a bound expression for recursive-lambda binder diagnostics. This is
    /// intentionally close to the C++ expression `toString()` form used in the
    /// oracle error for mixed node/relationship-dependent conjuncts.
    pub(super) fn bound_expr_name(&self, e: &BoundExpr) -> String {
        match e {
            BoundExpr::Literal(v) => v.to_result_string(),
            BoundExpr::Parameter { name, .. } => format!("${name}"),
            BoundExpr::Column { col, .. } => format!("#{col}"),
            BoundExpr::Property { var, prop, .. } => {
                format!("{}.{}", self.vars[var.0 as usize].name, prop)
            }
            BoundExpr::ValueProperty { value, prop, .. } => {
                format!("{}.{}", self.bound_expr_name(value), prop)
            }
            BoundExpr::NodeRef { var, .. } | BoundExpr::ScalarVar { var, .. } => {
                self.vars[var.0 as usize].name.clone()
            }
            BoundExpr::LambdaVar { id, .. } => self.lambda_name(*id),
            BoundExpr::Scalar { op, args, .. } => {
                let args = args
                    .iter()
                    .map(|a| self.bound_expr_name(a))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}({args})", scalar_op_name(*op))
            }
            BoundExpr::Aggregate {
                op, distinct, arg, ..
            } => {
                let distinct = if *distinct { "DISTINCT " } else { "" };
                let arg = arg
                    .as_ref()
                    .map(|a| self.bound_expr_name(a))
                    .unwrap_or_else(|| "*".to_string());
                format!(
                    "{}({distinct}{arg})",
                    format!("{op:?}").to_ascii_uppercase()
                )
            }
            // This diagnostic mirrors the source expression; omit binder-inserted
            // coercions, which C++ does not expose in its mixed-dependency error.
            BoundExpr::Cast { expr, .. } => self.bound_expr_name(expr),
            // A property extract renders as the access it came from (`n.ID`).
            BoundExpr::Call { function, args, .. }
                if *function == BuiltinScalar::StructExtract && args.len() == 2 =>
            {
                let prop = match &args[1] {
                    BoundExpr::Literal(Value::String(s)) => s.clone(),
                    other => self.bound_expr_name(other),
                };
                format!("{}.{prop}", self.bound_expr_name(&args[0]))
            }
            BoundExpr::Call { function, args, .. }
                if *function == BuiltinScalar::Id && args.len() == 1 =>
            {
                format!("{}._ID", self.bound_expr_name(&args[0]))
            }
            BoundExpr::Call {
                called_name, args, ..
            } => {
                let args = args
                    .iter()
                    .map(|a| self.bound_expr_name(a))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}({args})", called_name.to_ascii_uppercase())
            }
            BoundExpr::Udf { function, args, .. } => {
                let args = args
                    .iter()
                    .map(|argument| self.bound_expr_name(argument))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{}({args})", function.name.to_ascii_uppercase())
            }
            BoundExpr::List { elems, .. } => {
                let elems = elems
                    .iter()
                    .map(|a| self.bound_expr_name(a))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("[{elems}]")
            }
            BoundExpr::Struct { fields, .. } => {
                let fields = fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", self.bound_expr_name(v)))
                    .collect::<Vec<_>>()
                    .join(",");
                format!("{{{fields}}}")
            }
            BoundExpr::ListLambda {
                params, list, body, ..
            } => {
                let params = params
                    .iter()
                    .map(|id| self.lambda_name(*id))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "{params} -> {} IN {}",
                    self.bound_expr_name(body),
                    self.bound_expr_name(list)
                )
            }
            BoundExpr::Subquery { id, .. } => format!("SUBQUERY#{id}"),
            BoundExpr::SequenceCall { id, .. } => format!("SEQUENCE#{id}"),
            BoundExpr::Case {
                operand,
                branches,
                else_,
                ..
            } => {
                let mut out = String::from("CASE");
                if let Some(operand) = operand {
                    out.push(' ');
                    out.push_str(&self.bound_expr_name(operand));
                }
                for (cond, result) in branches {
                    out.push_str(" WHEN ");
                    out.push_str(&self.bound_expr_name(cond));
                    out.push_str(" THEN ");
                    out.push_str(&self.bound_expr_name(result));
                }
                if let Some(else_) = else_ {
                    out.push_str(" ELSE ");
                    out.push_str(&self.bound_expr_name(else_));
                }
                out.push_str(" END");
                out
            }
        }
    }

    pub(super) fn bind_create_pattern(
        &mut self,
        pe: &ast::PatternElement,
        create: &mut BoundCreate,
    ) -> Result<()> {
        // The missing-PK check is deferred until the whole element binds: C++
        // reports a rel-property type error before a missing node PK
        // (binder_error #36, oracle order).
        let mut deferred_pk: Vec<String> = Vec::new();
        // An unlabeled CREATE endpoint adopts the adjacent rel's schema table
        // (C++: `CREATE (a)-[:T]->(b)` with T: A→Foo binds a to A, b to Foo).
        let mut hints: Vec<Option<TableId>> = vec![None; 1 + pe.chains.len()];
        for (i, (rel, _)) in pe.chains.iter().enumerate() {
            if rel.labels.is_empty() {
                continue;
            }
            let Ok(cands) = self.resolve_rel_tables(&rel.labels) else {
                continue;
            };
            let [rt] = cands.as_slice() else { continue };
            let Some(entry) = self.catalog.rel_table(*rt) else {
                continue;
            };
            let [pair] = entry.pairs() else {
                continue;
            };
            let (near, far) = match rel.direction {
                ast::Direction::Left => (pair.to, pair.from),
                _ => (pair.from, pair.to),
            };
            hints[i].get_or_insert(near);
            hints[i + 1].get_or_insert(far);
        }
        let head = self.bind_create_node(&pe.head, create, &mut deferred_pk, hints[0])?;
        let mut prev = head;
        for (i, (rel, node)) in pe.chains.iter().enumerate() {
            let node_var = self.bind_create_node(node, create, &mut deferred_pk, hints[i + 1])?;
            self.bind_create_rel(rel, prev, node_var, create)?;
            prev = node_var;
        }
        if let Some(msg) = deferred_pk.into_iter().next() {
            return Err(Error::binder(msg));
        }
        Ok(())
    }

    pub(super) fn bind_create_node(
        &mut self,
        np: &ast::NodePattern,
        create: &mut BoundCreate,
        deferred_pk: &mut Vec<String>,
        table_hint: Option<TableId>,
    ) -> Result<VarId> {
        // Reference an already-bound variable instead of creating a new node.
        if let Some(name) = &np.var {
            if let Some(&id) = self.scope.get(name) {
                if !self.vars[id.0 as usize].is_node() {
                    return Err(Error::binder(format!("Variable {name} is not a node.")));
                }
                return Ok(id);
            }
        }
        let table = match table_hint {
            Some(t) if np.labels.is_empty() => t,
            _ => self.resolve_single_node_label(np.var.as_deref().unwrap_or(""), &np.labels)?,
        };
        let entry = self.catalog.node_table(table).unwrap();
        let label = entry.name().to_string();
        let num_columns = entry.columns().len();
        let pk_col = entry.primary_key_index();
        let props = if self.catalog.is_any_node_table(table) {
            vec![
                (
                    1,
                    BoundExpr::Literal(Value::List(if np.labels.is_empty() {
                        vec![Value::String("_nodes".to_string())]
                    } else {
                        np.labels.iter().cloned().map(Value::String).collect()
                    })),
                ),
                (2, self.any_data_expr(&np.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &np.properties {
                let col = entry
                    .columns()
                    .iter()
                    .position(|column| column.name().eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = np.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // A string *literal* bypasses the implicit-cast gate: C++ casts it
                // to the column type at execution ('30' -> 30 succeeds; 'hh' is the
                // Conversion "Cast failed. Could not convert ..." — oracle-verified).
                let string_literal = matches!(&be, BoundExpr::Literal(Value::String(_)));
                if !string_literal {
                    assignable_or_err(&be, entry.columns()[col].logical_type(), &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, entry.columns()[col].logical_type())));
            }
            props
        };
        // C++ validates the primary key at *bind*: a CREATE must supply the PK
        // unless the column fills itself (SERIAL / a DEFAULT — oracle-verified:
        // `DEFAULT nextval(...)` PKs create fine). Same wording, var as typed
        // (anonymous → empty, giving the oracle's double space).
        if let Some(pk) = entry.columns().get(pk_col) {
            let provided = props.iter().any(|(c, _)| *c == pk_col);
            let self_filling = pk.logical_type() == &LogicalType::Serial || pk.default().is_some();
            if !provided && !self_filling {
                deferred_pk.push(format!(
                    "Create node {} expects primary key {} as input.",
                    np.var.as_deref().unwrap_or(""),
                    pk.name()
                ));
            }
        }
        let var_props = self.node_props(table);
        let var = self.add_var(
            np.var.clone(),
            VarKind::Node {
                tables: vec![table],
                label,
            },
            var_props,
        );
        create.nodes.push(BoundCreateNode {
            var,
            table,
            num_columns,
            pk_col,
            props,
        });
        Ok(var)
    }

    pub(super) fn bind_create_rel(
        &mut self,
        rp: &ast::RelPattern,
        left: VarId,
        right: VarId,
        create: &mut BoundCreate,
    ) -> Result<()> {
        // A recursive rel cannot be created (matches the C++ binder).
        if rp.recursive.is_some() {
            let name = rp.var.clone().unwrap_or_default();
            return Err(Error::binder(format!(
                "Cannot create recursive rel {name}."
            )));
        }
        let (src, dst) = match rp.direction {
            ast::Direction::Right => (left, right),
            ast::Direction::Left => (right, left),
            ast::Direction::Both => {
                return Err(Error::binder(
                    "Create undirected relationship is not supported. Try create 2 directed \
                     relationships instead."
                        .to_string(),
                ));
            }
        };
        let src_tables = self.vars[src.0 as usize].node_tables().to_vec();
        let dst_tables = self.vars[dst.0 as usize].node_tables().to_vec();

        // Resolve the rel type from the endpoints, mirroring C++ `bindInsertRel` /
        // `tryPruneMultiLabeled` (bind_updating_clause.cpp). An explicitly-typed rel
        // contributes its named table(s) as candidates; an *untyped* rel (`[k]`)
        // ranges over every rel table. We keep the candidates declaring a FROM-TO
        // pair that connects the created endpoints:
        //   • exactly 1 → use it (this is how an untyped rel infers its type),
        //   • 0         → "Cannot find a valid label … that connects …",
        //   • >1        → "Create rel … with multiple rel labels is not supported."
        // For polymorphic/unlabeled endpoints this stays a candidate check; the
        // processor resolves the exact per-pair member from each row's runtime
        // internal ids and errors if a particular row is incompatible.
        let candidates: Vec<TableId> = if let Some(tables) = self.catalog.any_tables() {
            vec![tables.edges()]
        } else if rp.labels.is_empty() {
            self.catalog.rel_table_ids()
        } else {
            let mut ids = Vec::with_capacity(rp.labels.len());
            for label in &rp.labels {
                let rel = self
                    .catalog
                    .rel_table_by_name(label)
                    .ok_or_else(|| Error::binder(format!("Table {label} does not exist.")))?;
                ids.push(rel.id());
            }
            ids
        };
        // C++ query-graph label analyzer (`QueryGraphLabelAnalyzer::pruneNode`,
        // query_graph_label_analyzer.cpp:76-127), which `bindInsertInfos`
        // (bind_updating_clause.cpp:131) runs with `throwOnViolate = true` over a CREATE
        // pattern *before* resolving the rel. Each endpoint's labels are pruned to those
        // valid for its position — a FROM table of some candidate for the src, a TO table
        // for the dst — and an empty result means an explicit label that cannot occupy
        // that slot. We report it here, naming the offending node and the labels its
        // position accepts, ahead of the pair-level resolution below. The src is checked
        // before the dst, matching the pattern's node order for the forward arrows this
        // guards. (MATCH uses `throwOnViolate = false`, so this lives on the create path.)
        let mut valid_from: Vec<TableId> = Vec::new();
        let mut valid_to: Vec<TableId> = Vec::new();
        for &id in &candidates {
            if let Some(r) = self.catalog.rel_table(id) {
                for pair in r.pairs() {
                    if !valid_from.contains(&pair.from) {
                        valid_from.push(pair.from);
                    }
                    if !valid_to.contains(&pair.to) {
                        valid_to.push(pair.to);
                    }
                }
            }
        }
        for (node, tables, valid) in [
            (src, &src_tables, &valid_from),
            (dst, &dst_tables, &valid_to),
        ] {
            if valid.is_empty() || tables.iter().any(|t| valid.contains(t)) {
                continue;
            }
            let name = self.vars[node.0 as usize].name.clone();
            let labels = valid
                .iter()
                .filter_map(|&t| {
                    self.catalog
                        .node_table(t)
                        .map(|node| node.name().to_string())
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::binder(format!(
                "Query node {name} violates schema. Expected labels are {labels}."
            )));
        }
        // C++ `RelExpression::isMultiLabeled()`: more than one candidate rel type, or
        // a single rel group spanning more than one FROM-TO pair. Only this case takes
        // the `tryPruneMultiLabeled` path (and its diagnostics); a single-pair rel is
        // used directly, with a wrong-side endpoint caught by the query-graph label
        // analyzer ("… violates schema …", not yet ported).
        let is_multi = candidates.len() > 1
            || candidates.iter().any(|&id| {
                self.catalog
                    .rel_table(id)
                    .is_some_and(|r| r.pairs().len() > 1)
            });
        let mut matched: Vec<TableId> = Vec::new();
        for &id in &candidates {
            let connects = self
                .catalog
                .rel_table(id)
                .unwrap()
                .pairs()
                .iter()
                .any(|pair| {
                    if src == dst && pair.from != pair.to {
                        return false;
                    }
                    src_tables.contains(&pair.from) && dst_tables.contains(&pair.to)
                });
            if connects {
                matched.push(id);
            }
        }
        if matched.len() != 1 {
            let rel_disp = rp.var.clone().unwrap_or_default();
            if matched.len() > 1 {
                return Err(Error::binder(format!(
                    "Create rel {rel_disp} with multiple rel labels is not supported."
                )));
            }
            // A single-pair rel whose endpoints fail the *pair* test but passed the
            // per-endpoint label check above — only a self-loop over a multi-labeled
            // node reaches here (e.g. `(a)-[:R]->(a)` with R: X→Y and a labeled both X
            // and Y). C++ narrows the node and proceeds; we keep the prior message
            // rather than silently mis-routing the edge. (The common wrong-side cases
            // are reported as schema violations above.) Multi-labeled rels fall through
            // to the `tryPruneMultiLabeled` diagnostic.
            if !is_multi && candidates.len() == 1 {
                let rel_name = self.catalog.rel_table(candidates[0]).unwrap().name();
                return Err(Error::binder(format!(
                    "Nodes are not connected through relationship table {rel_name}."
                )));
            }
            // Labeled rels blame the label against the endpoint tables; an
            // unlabeled rel gets the neighbour-nodes wording (both oracle-pinned:
            // issue 5716 vs create_empty).
            if !rp.labels.is_empty() {
                let name_of = |tables: &[TableId]| -> String {
                    tables
                        .first()
                        .and_then(|&t| self.catalog.node_table(t))
                        .map(|node| node.name().to_string())
                        .unwrap_or_default()
                };
                return Err(Error::binder(format!(
                    "Cannot find a valid label in {rel_disp} that connects {} and {}.",
                    name_of(&src_tables),
                    name_of(&dst_tables),
                )));
            }
            return Err(Error::binder(format!(
                "Cannot find a label for relationship {rel_disp} that connects to all of its \
                 neighbour nodes."
            )));
        }
        let rel_id = matched[0];
        let rel = self.catalog.rel_table(rel_id).unwrap();
        // A multi-pair rel (a rel group) cannot anchor a new edge on an
        // endpoint matched under several node tables — the target pair is
        // ambiguous (C++ "bound by multiple node labels"; a single-pair rel
        // pins its endpoints itself, issue 3906).
        if rel.pairs().len() > 1
            && [left, right]
                .iter()
                .any(|&e| self.vars[e.0 as usize].node_tables().len() > 1)
        {
            let rel_disp = rp.var.clone().unwrap_or_default();
            return Err(Error::binder(format!(
                "Create rel {rel_disp} bound by multiple node labels is not supported."
            )));
        }
        let (rel_name, num_columns) = (rel.name().to_string(), rel.columns().len());
        let props = if self.catalog.is_any_rel_table(rel_id) {
            vec![
                (
                    1,
                    BoundExpr::Literal(Value::String(
                        rp.labels
                            .first()
                            .cloned()
                            .unwrap_or_else(|| "_edges".to_string()),
                    )),
                ),
                (2, self.any_data_expr(&rp.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &rp.properties {
                let col = rel
                    .columns()
                    .iter()
                    .position(|column| column.name().eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = rp.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // String literals cast at execution (see the CREATE-node note).
                if !matches!(&be, BoundExpr::Literal(Value::String(_))) {
                    assignable_or_err(&be, rel.columns()[col].logical_type(), &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, rel.columns()[col].logical_type())));
            }
            props
        };
        // A *named* created rel is projectable (e.g. `CREATE (a)-[e:R]->(b) RETURN
        // e` / `id(e)`): mint a Rel-typed variable so RETURN/WITH can reference it,
        // exactly as a MERGE-created rel does. (An anonymous rel needs no variable.)
        // The processor fills this var's column with the new rel's internal id.
        let var = if rp.var.is_some() {
            let var_props = self.rel_props(rel_id);
            Some(self.add_var(
                rp.var.clone(),
                VarKind::Rel {
                    tables: vec![rel_id],
                    label: rel_name.clone(),
                    src,
                    dst,
                    directed: true,
                    recursive: None,
                },
                var_props,
            ))
        } else {
            None
        };
        create.rels.push(BoundCreateRel {
            table: rel_id,
            src,
            dst,
            num_columns,
            props,
            var,
        });
        Ok(())
    }

    /// Bind a `MERGE`: bind the (single) pattern once as match variables, collect
    /// the inline-property match filter, and build create-on-miss instructions for
    /// the parts the MERGE introduces (already-bound endpoints are reused).
    pub(super) fn bind_merge(&mut self, merge: &ast::MergeClause) -> Result<BoundMerge> {
        let mut match_ = BoundMatch::default();
        let mut filters = Vec::new();
        let mut create = BoundCreate::default();

        // Each comma-separated pattern element joins into one combined match — a
        // cross-product when the elements are disconnected (the planner builds
        // that) — governed by a single existence: if every element matched it is an
        // ON MATCH, otherwise the MERGE creates every part it introduced and applies
        // ON CREATE. A variable shared across elements (or already in scope) is
        // matched/reused, never re-created.
        for pe in &merge.patterns {
            let head_bound = pe
                .head
                .var
                .as_ref()
                .is_some_and(|n| self.scope.contains_key(n));
            let head = self.bind_match_node(&pe.head)?;
            match_.node_vars.push(head);
            self.inline_predicates(head, &pe.head.properties, &mut filters)?;
            self.any_label_predicates(head, &pe.head.labels, &mut filters);
            if !head_bound {
                self.merge_create_node(head, &pe.head, &mut create)?;
            }

            let mut prev = head;
            for (rel, node) in &pe.chains {
                let node_bound = node
                    .var
                    .as_ref()
                    .is_some_and(|n| self.scope.contains_key(n));
                let node_var = self.bind_match_node(node)?;
                let rel_var = self.bind_match_rel(rel, prev, node_var, None)?;
                self.any_label_predicates(rel_var, &rel.labels, &mut filters);
                self.any_label_predicates(node_var, &node.labels, &mut filters);
                self.inline_predicates(node_var, &node.properties, &mut filters)?;
                self.inline_predicates(rel_var, &rel.properties, &mut filters)?;
                match_.node_vars.push(node_var);
                match_.rel_vars.push(rel_var);
                if !node_bound {
                    self.merge_create_node(node_var, node, &mut create)?;
                }
                self.merge_create_rel(rel_var, rel, prev, node_var, &mut create)?;
                prev = node_var;
            }
        }

        // Every pattern part resolved to an already-bound variable — nothing to
        // merge-create is the same C++ bind error as an empty CREATE.
        if create.nodes.is_empty() && create.rels.is_empty() {
            return Err(Error::binder(
                "Cannot resolve any node or relationship to create.".to_string(),
            ));
        }
        let on_create = self.bind_set_items(&merge.on_create)?;
        let on_match = self.bind_set_items(&merge.on_match)?;
        Ok(BoundMerge {
            match_,
            filter: combine_and(filters),
            create,
            on_create,
            on_match,
        })
    }

    /// Build the create-on-miss instruction for a `MERGE`d node (an existing,
    /// freshly-bound match var); its inline properties become the insert values.
    pub(super) fn merge_create_node(
        &mut self,
        var: VarId,
        np: &ast::NodePattern,
        create: &mut BoundCreate,
    ) -> Result<()> {
        let table = self.resolve_single_node_label(np.var.as_deref().unwrap_or(""), &np.labels)?;
        let entry = self.catalog.node_table(table).unwrap();
        let num_columns = entry.columns().len();
        let pk_col = entry.primary_key_index();
        let props = if self.catalog.is_any_node_table(table) {
            vec![
                (
                    1,
                    BoundExpr::Literal(Value::List(if np.labels.is_empty() {
                        vec![Value::String("_nodes".to_string())]
                    } else {
                        np.labels.iter().cloned().map(Value::String).collect()
                    })),
                ),
                (2, self.any_data_expr(&np.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &np.properties {
                let col = entry
                    .columns()
                    .iter()
                    .position(|column| column.name().eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = np.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // String literals cast at execution (see the CREATE-node note).
                if !matches!(&be, BoundExpr::Literal(Value::String(_))) {
                    assignable_or_err(&be, entry.columns()[col].logical_type(), &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, entry.columns()[col].logical_type())));
            }
            props
        };
        create.nodes.push(BoundCreateNode {
            var,
            table,
            num_columns,
            pk_col,
            props,
        });
        Ok(())
    }

    /// Build the create-on-miss instruction for a `MERGE`d relationship.
    pub(super) fn merge_create_rel(
        &mut self,
        rel_var: VarId,
        rp: &ast::RelPattern,
        left: VarId,
        right: VarId,
        create: &mut BoundCreate,
    ) -> Result<()> {
        if rp.labels.len() != 1 {
            return Err(Error::binder(
                "a MERGE relationship must specify exactly one type".to_string(),
            ));
        }
        let (src, dst) = match rp.direction {
            ast::Direction::Right => (left, right),
            ast::Direction::Left => (right, left),
            ast::Direction::Both => {
                return Err(Error::binder(
                    "MERGE requires a directed relationship".to_string(),
                ));
            }
        };
        let rel_id = if let Some(tables) = self.catalog.any_tables() {
            tables.edges()
        } else {
            self.catalog
                .rel_table_by_name(&rp.labels[0])
                .ok_or_else(|| Error::binder(format!("Table {} does not exist.", rp.labels[0])))?
                .id()
        };
        let rel = self.catalog.rel_table(rel_id).unwrap();
        let num_columns = rel.columns().len();
        let props = if self.catalog.is_any_rel_table(rel_id) {
            vec![
                (1, BoundExpr::Literal(Value::String(rp.labels[0].clone()))),
                (2, self.any_data_expr(&rp.properties)?),
            ]
        } else {
            let mut props = Vec::new();
            for (k, e) in &rp.properties {
                let col = rel
                    .columns()
                    .iter()
                    .position(|column| column.name().eq_ignore_ascii_case(k))
                    .ok_or_else(|| {
                        // C++ names the pattern variable as typed (empty when anonymous).
                        let v = rp.var.as_deref().unwrap_or("");
                        Error::binder(format!("Cannot find property {k} for {v}."))
                    })?;
                let be = self.bind_expr(e)?;
                // String literals cast at execution (see the CREATE-node note).
                if !matches!(&be, BoundExpr::Literal(Value::String(_))) {
                    assignable_or_err(&be, rel.columns()[col].logical_type(), &expr_name(e))?;
                }
                props.push((col, self.coerce_to(be, rel.columns()[col].logical_type())));
            }
            props
        };
        create.rels.push(BoundCreateRel {
            table: rel_id,
            src,
            dst,
            num_columns,
            props,
            var: Some(rel_var),
        });
        Ok(())
    }

    pub(super) fn bind_projection(&mut self, r: &ast::ReturnClause) -> Result<BoundProjection> {
        let mut items = Vec::new();
        for item in &r.items {
            match item {
                ast::ProjectionItem::Star => {
                    // `RETURN *` expands to the variables currently IN SCOPE, in
                    // declaration order — not every variable ever declared
                    // (`self.vars`). After a `WITH`, earlier-part variables leave scope
                    // (and a carried `WITH a` re-binds `a` to a fresh id), so a global
                    // expansion would project a variable the final part's layout has no
                    // column for → a planner panic. A variable is in scope iff the scope
                    // maps its name back to this very id (which also excludes a
                    // shadowed/renamed older binding of the same name).
                    let named: Vec<(VarId, String)> = self
                        .vars
                        .iter()
                        .enumerate()
                        .filter(|(i, v)| {
                            !v.anonymous && self.scope.get(&v.name) == Some(&VarId(*i as u32))
                        })
                        .map(|(i, v)| (VarId(i as u32), v.name.clone()))
                        .collect();
                    if named.is_empty() {
                        return Err(Error::binder(
                            "RETURN or WITH * is not allowed when there are no variables in scope."
                                .to_string(),
                        ));
                    }
                    for (var, name) in named {
                        let info = &self.vars[var.0 as usize];
                        // Nodes / plain rels are assembled from their id; scalars,
                        // recursive rels and paths project as a value column.
                        if info.is_assembled_graph_var() {
                            items.push(ProjItem::Var { name, var });
                        } else {
                            items.push(ProjItem::Scalar {
                                name,
                                expr: self.var_ref_expr(var),
                            });
                        }
                    }
                }
                ast::ProjectionItem::AllProperties(name) => {
                    self.expand_all_properties(name, &mut items)?;
                }
                ast::ProjectionItem::AllStructFields(base) => {
                    self.expand_all_struct_fields(base, &mut items)?;
                }
                ast::ProjectionItem::Expr { expr, alias } => {
                    // A bare *node/plain-rel* variable projects the whole assembled
                    // value; a scalar / recursive-rel / path variable falls through
                    // to a value expression (read straight from its column).
                    if let ast::Expr::Variable(name) = expr {
                        if let Some(&var) = self.scope.get(name) {
                            if self.vars[var.0 as usize].is_assembled_graph_var() {
                                items.push(ProjItem::Var {
                                    name: alias.clone().unwrap_or_else(|| name.clone()),
                                    var,
                                });
                                continue;
                            }
                        }
                    }
                    let be = self
                        .bind_expr(expr)
                        .map_err(|e| rewrite_nested_agg(e, alias.as_deref()))?;
                    let name = alias.clone().unwrap_or_else(|| expr_name(expr));
                    items.push(ProjItem::Scalar { name, expr: be });
                }
            }
        }

        self.finish_projection(r.distinct, items, r)
    }

    /// Expand `<expr>.*` into one `struct_extract(base, field)` column per
    /// field of the struct-typed base, in declared field order (C++ names each
    /// column `STRUCT_EXTRACT(<base>, <field>)`).
    pub(super) fn expand_all_struct_fields(
        &mut self,
        base: &ast::Expr,
        items: &mut Vec<ProjItem>,
    ) -> Result<()> {
        let bound = self.bind_expr(base)?;
        let LogicalType::Struct(fields) = bound.ty() else {
            return Err(Error::binder(format!(
                "Cannot spread the fields of a non-struct expression {}.",
                expr_name(base)
            )));
        };
        let base_name = expr_name(base);
        for (fname, fty) in fields {
            items.push(ProjItem::Scalar {
                name: format!("STRUCT_EXTRACT({base_name},{fname})"),
                expr: Self::property_extract_call(bound.clone(), &fname, fty.clone()),
            });
        }
        Ok(())
    }

    /// Expand `a.*` into one scalar projection item per property of node/rel `a`,
    /// in column order, retaining the qualified `a.property` output name.
    pub(super) fn expand_all_properties(
        &self,
        name: &str,
        items: &mut Vec<ProjItem>,
    ) -> Result<()> {
        let var = self.lookup_var(name)?;
        let info = &self.vars[var.0 as usize];
        if info.is_scalar() {
            return Err(Error::binder(format!(
                "Variable {name} is not a node or relationship."
            )));
        }
        // A value-backed node expands its value struct: `_ID`/`_LABEL` lead
        // (C++ node-value fields), then the properties.
        if info.value_backed && info.is_node() {
            for (pname, pty) in [
                ("_id", LogicalType::InternalId),
                ("_label", LogicalType::String),
            ] {
                items.push(ProjItem::Scalar {
                    name: format!("{name}.{pname}"),
                    expr: Self::property_extract_call(self.var_ref_expr(var), pname, pty),
                });
            }
        }
        let props: Vec<(String, LogicalType)> = info
            .properties
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();
        for (pname, pty) in props {
            items.push(ProjItem::Scalar {
                name: format!("{name}.{pname}"),
                expr: BoundExpr::Property {
                    var,
                    prop: pname,
                    ty: pty,
                },
            });
        }
        Ok(())
    }

    /// Attach the `ORDER BY`/`SKIP`/`LIMIT` modifiers (shared by `RETURN` and
    /// `WITH`) to already-bound projection items. `ORDER BY` may reference an
    /// output column by alias/variable name, or be an arbitrary expression over
    /// Attach the `ORDER BY`/`SKIP`/`LIMIT` modifiers (shared by `RETURN` and
    /// `WITH`) to already-bound projection items.
    ///
    /// C++ binds aggregate/DISTINCT ORDER BY after clearing the input scope and
    /// exposing only the projection expressions/aliases. Non-aggregate,
    /// non-DISTINCT ORDER BY keeps the input scope, with exact output-name
    /// references using the projected value.
    pub(super) fn finish_projection(
        &mut self,
        distinct: bool,
        items: Vec<ProjItem>,
        r: &ast::ReturnClause,
    ) -> Result<BoundProjection> {
        let output_scope = self.projection_output_scope(&items);
        let output_only_order = distinct || projection_has_aggregates(&items);
        let mut order_by = Vec::new();
        for (e, asc) in &r.order_by {
            let key = if output_only_order {
                let expr = self.bind_order_expr_in_output_scope(e, &output_scope)?;
                validate_order_key_type(e, &expr.ty())?;
                match expr {
                    BoundExpr::Column { col, .. } => OrderKey::Output(col),
                    other => OrderKey::PostProjection(other),
                }
            } else if let ast::Expr::Variable(name) = e {
                if let Some(out) = output_scope.get(name) {
                    validate_order_key_type(e, &out.ty)?;
                    OrderKey::Output(out.index)
                } else {
                    let expr = self.bind_expr(e)?;
                    validate_order_key_type(e, &expr.ty())?;
                    OrderKey::Expr(expr)
                }
            } else {
                let expr = self.bind_expr(e)?;
                validate_order_key_type(e, &expr.ty())?;
                OrderKey::Expr(expr)
            };
            order_by.push((key, *asc));
        }
        Ok(BoundProjection {
            distinct,
            items,
            order_by,
            skip: r
                .skip
                .as_ref()
                .map(|e| self.bind_skip_limit(e))
                .transpose()?,
            limit: r
                .limit
                .as_ref()
                .map(|e| self.bind_skip_limit(e))
                .transpose()?,
        })
    }

    /// Bind a `SKIP`/`LIMIT` count (C++ `bindSkipLimit`): the expression must be
    /// constant — a parameter or a literal expression (foldable arithmetic like
    /// `LIMIT 1 + 1` included) — else the binder error fires here. The folded
    /// value is validated as a non-negative integer at execution (a *runtime*
    /// error, matching the C++ stage split).
    pub(super) fn bind_skip_limit(&mut self, e: &ast::Expr) -> Result<BoundExpr> {
        let bound = self.bind_expr(e)?;
        if !skip_limit_constant(&bound) {
            return Err(Error::binder(
                "The number of rows to skip/limit must be a parameter/literal expression."
                    .to_string(),
            ));
        }
        Ok(bound)
    }

    pub(super) fn projection_output_scope(
        &self,
        items: &[ProjItem],
    ) -> HashMap<String, ProjectionOutputRef> {
        let mut scope = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            let (name, ty) = match item {
                ProjItem::Scalar { name, expr } => (name.clone(), expr.ty()),
                ProjItem::Var { name, var } => (name.clone(), self.var_value_type(*var)),
            };
            scope
                .entry(name)
                .or_insert(ProjectionOutputRef { index, ty });
        }
        scope
    }

    /// Whether `e` references a `$param` with no bound value (see `bind_expr`:
    /// such a tree binds to NULL wholesale, like C++).
    /// Bind a path pattern used as an expression. The projection-less form
    /// (`WHERE (a)-[:R]->()`) is the openCypher existential pattern predicate
    /// — it binds as an EXISTS subquery over the pattern. The comprehension
    /// form (`[pattern | expr]`) only validates its labels (evaluation is not
    /// implemented; every oracle-probed use errors at bind).
    pub(super) fn bind_pattern_expression(
        &mut self,
        pattern: &ast::PatternElement,
        projection: Option<&ast::Expr>,
    ) -> Result<BoundExpr> {
        if projection.is_none() {
            let id = self.subquery_count;
            self.subquery_count += 1;
            self.pending_subqueries.push(PendingSubquery {
                id,
                kind: ast::SubqueryKind::Exists,
                patterns: vec![pattern.clone()],
                where_clause: None,
            });
            return Ok(BoundExpr::Subquery {
                id,
                ty: LogicalType::Bool,
            });
        }
        self.validate_pattern_comprehension(pattern)
    }

    /// Bind-time validation for a pattern comprehension: every node label /
    /// rel type must exist (the C++ binder errors on the first missing one —
    /// this is as far as every oracle-probed use gets); evaluation itself is
    /// not implemented.
    pub(super) fn validate_pattern_comprehension(
        &self,
        pattern: &ast::PatternElement,
    ) -> Result<BoundExpr> {
        let mut labels: Vec<&str> = pattern.head.labels.iter().map(String::as_str).collect();
        for (rel, node) in &pattern.chains {
            labels.extend(rel.labels.iter().map(String::as_str));
            labels.extend(node.labels.iter().map(String::as_str));
        }
        for l in labels {
            if self.catalog.table_id(l).is_none() {
                return Err(Error::binder(format!("Table {l} does not exist.")));
            }
        }
        Err(Error::not_implemented(
            "pattern comprehensions are not supported in this phase".to_string(),
        ))
    }

    pub(super) fn has_unbound_param(&self, e: &ast::Expr) -> bool {
        if self.prepared_parameters.is_some() {
            return false;
        }
        use ast::Expr as E;
        let any = |es: &[E]| es.iter().any(|x| self.has_unbound_param(x));
        match e {
            E::Parameter(name) => !self.params.contains_key(name),
            E::Literal(_) | E::OverflowInt(_) | E::Variable(_) | E::Star => false,
            E::Property { base, .. } => self.has_unbound_param(base),
            E::Function { args, .. } => any(args),
            E::And(es) | E::Or(es) | E::List(es) => any(es),
            E::Xor(a, b)
            | E::Comparison { lhs: a, rhs: b, .. }
            | E::Arithmetic { lhs: a, rhs: b, .. } => {
                self.has_unbound_param(a) || self.has_unbound_param(b)
            }
            E::Not(x) | E::Negate(x) | E::IsNull(x) | E::IsNotNull(x) => self.has_unbound_param(x),
            E::Struct(fields) => fields.iter().any(|(_, x)| self.has_unbound_param(x)),
            E::Lambda { body, .. } => self.has_unbound_param(body),
            E::ListComprehension {
                list,
                predicate,
                projection,
                ..
            } => {
                self.has_unbound_param(list)
                    || predicate
                        .as_deref()
                        .is_some_and(|x| self.has_unbound_param(x))
                    || projection
                        .as_deref()
                        .is_some_and(|x| self.has_unbound_param(x))
            }
            E::Case {
                operand,
                when_thens,
                else_,
            } => {
                operand
                    .as_deref()
                    .is_some_and(|x| self.has_unbound_param(x))
                    || when_thens
                        .iter()
                        .any(|(c, r)| self.has_unbound_param(c) || self.has_unbound_param(r))
                    || else_.as_deref().is_some_and(|x| self.has_unbound_param(x))
            }
            E::Subquery { where_clause, .. } => where_clause
                .as_deref()
                .is_some_and(|x| self.has_unbound_param(x)),
            E::PatternComprehension { projection, .. } => projection
                .as_deref()
                .is_some_and(|x| self.has_unbound_param(x)),
        }
    }

    pub(super) fn bind_order_expr_in_output_scope(
        &mut self,
        e: &ast::Expr,
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        if let Some(out) = scope.get(&expr_name(e)) {
            return Ok(out.as_column());
        }
        if self.has_unbound_param(e) {
            return Ok(BoundExpr::Literal(Value::Null));
        }
        match e {
            ast::Expr::PatternComprehension {
                pattern,
                projection,
            } => self.bind_pattern_expression(pattern, projection.as_deref()),
            ast::Expr::Literal(v) => Ok(BoundExpr::Literal(v.clone())),
            ast::Expr::OverflowInt(text) => Err(overflow_int_error(text)),
            ast::Expr::Variable(name) => scope
                .get(name)
                .map(ProjectionOutputRef::as_column)
                .ok_or_else(|| Error::binder(format!("Variable {name} is not in scope."))),
            ast::Expr::Property { base, name } => {
                let value = self.bind_order_expr_in_output_scope(base, scope)?;
                let display = expr_name(base);
                self.bind_value_property(value, name, &display)
            }
            ast::Expr::Parameter(name) => Ok(self.bind_parameter(name)),
            ast::Expr::Function {
                name,
                args,
                arg_names,
                ..
            } => self.bind_output_function(name, args, arg_names, scope, e),
            ast::Expr::And(terms) => self.bind_output_bool(ScalarOp::And, terms, scope),
            ast::Expr::Or(terms) => self.bind_output_bool(ScalarOp::Or, terms, scope),
            ast::Expr::Xor(a, b) => {
                self.bind_output_scalar(ScalarOp::Xor, &[a.as_ref(), b.as_ref()], scope)
            }
            ast::Expr::Not(a) => self.bind_output_scalar(ScalarOp::Not, &[a.as_ref()], scope),
            ast::Expr::Comparison { op, lhs, rhs } => {
                self.bind_output_scalar(cmp_op(*op), &[lhs.as_ref(), rhs.as_ref()], scope)
            }
            ast::Expr::Arithmetic { op, lhs, rhs } => {
                self.bind_output_scalar(arith_op(*op), &[lhs.as_ref(), rhs.as_ref()], scope)
            }
            ast::Expr::Negate(a) => self.bind_output_scalar(ScalarOp::Neg, &[a.as_ref()], scope),
            ast::Expr::IsNull(a) => self.bind_output_scalar(ScalarOp::IsNull, &[a.as_ref()], scope),
            ast::Expr::IsNotNull(a) => {
                self.bind_output_scalar(ScalarOp::IsNotNull, &[a.as_ref()], scope)
            }
            ast::Expr::List(items) => {
                let bound = items
                    .iter()
                    .map(|e| self.bind_order_expr_in_output_scope(e, scope))
                    .collect::<Result<Vec<_>>>()?;
                // Element type = the C++ `LIST_CREATION` combine — see
                // `list_literal_elem_type`.
                let elem_ty = list_literal_elem_type(&bound);
                // Each element must implicitly cast to the combined element type —
                // C++ `ListCreationFunction::bindFunc` casts every argument to the
                // combined type via `implicitCastIfNecessary` (POSITIONAL for
                // nested literals: struct/union member names adopt the combined
                // type's), so an incompatible element is a *bind-time*
                // rejection, not a deferred runtime cast failure.
                let mut bound = bound;
                for elem in &mut bound {
                    adopt_struct_field_names(elem, &elem_ty);
                    adopt_union_member_names(elem, &elem_ty);
                }
                for (item, elem) in items.iter().zip(&bound) {
                    assignable_or_err(elem, &elem_ty, &expr_name(item))?;
                }
                let elems = bound
                    .into_iter()
                    .map(|expression| self.coerce_to(expression, &elem_ty))
                    .collect();
                Ok(BoundExpr::List {
                    elems,
                    ty: LogicalType::List(Box::new(elem_ty)),
                })
            }
            ast::Expr::Struct(fields) => {
                check_struct_field_dups(fields)?;
                let bfields = fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), self.bind_order_expr_in_output_scope(v, scope)?)))
                    .collect::<Result<Vec<_>>>()?;
                // An all-NULL field types as STRING (C++ ANY-default) in the
                // struct literal's declared type.
                let ty = LogicalType::Struct(
                    bfields
                        .iter()
                        .map(|(k, v)| {
                            let t = match v.ty() {
                                LogicalType::Any => LogicalType::String,
                                t => t,
                            };
                            (k.clone(), t)
                        })
                        .collect(),
                );
                Ok(BoundExpr::Struct {
                    fields: bfields,
                    ty,
                })
            }
            ast::Expr::Case {
                operand,
                when_thens,
                else_,
            } => self.bind_output_case(operand.as_deref(), when_thens, else_.as_deref(), scope),
            ast::Expr::Star => Err(Error::binder(
                "'*' is only valid as the argument of count(*) or in RETURN *".to_string(),
            )),
            ast::Expr::Lambda { .. } => Err(Error::binder(
                "a lambda is only valid as an argument to a list function".to_string(),
            )),
            ast::Expr::ListComprehension { .. } | ast::Expr::Subquery { .. } => {
                Err(Error::not_implemented(format!(
                    "{} in ORDER BY over projected output is not supported in this phase",
                    expr_name(e)
                )))
            }
        }
    }

    pub(super) fn bind_output_function(
        &mut self,
        name: &str,
        args: &[ast::Expr],
        arg_names: &[Option<String>],
        scope: &HashMap<String, ProjectionOutputRef>,
        full_expr: &ast::Expr,
    ) -> Result<BoundExpr> {
        if let Some(bound) = self.bind_named_ctor(name, args, arg_names, Some(scope))? {
            return Ok(bound);
        }
        if let Some(b) = self.bind_keys(name, args)? {
            return Ok(b);
        }
        let registered_function = resolve_builtin(name).map(|descriptor| descriptor.function);
        if registered_function == Some(BuiltinFunction::Scalar(BuiltinScalar::CastFunction)) {
            if args.len() != 2 {
                return Err(Error::binder(
                    "cast expects (expression, type) arguments".to_string(),
                ));
            }
            let target = match &args[1] {
                ast::Expr::Literal(Value::String(s)) => self.resolve_cast_type(s)?,
                _ => {
                    return Err(Error::binder(
                        "the second argument to cast must be a type-name string".to_string(),
                    ));
                }
            };
            let expr = self.bind_order_expr_in_output_scope(&args[0], scope)?;
            return Ok(BoundExpr::Cast {
                expr: Box::new(expr),
                target,
            });
        }
        if sequence_fn(name).is_some()
            || matches!(registered_function, Some(BuiltinFunction::Aggregate(_)))
        {
            return Err(Error::binder(format!(
                "Variable {} is not in scope.",
                expr_name(full_expr)
            )));
        }
        let lk = match registered_function {
            Some(BuiltinFunction::Scalar(BuiltinScalar::ListTransform)) => {
                Some(LambdaKind::Transform)
            }
            Some(BuiltinFunction::Scalar(BuiltinScalar::ListFilter)) => Some(LambdaKind::Filter),
            Some(BuiltinFunction::Scalar(BuiltinScalar::ListReduce)) => Some(LambdaKind::Reduce),
            _ => None,
        };
        if lk.is_some() {
            return Err(Error::binder(
                "a lambda is only valid as an argument to a projected list function".to_string(),
            ));
        }
        if let Some(function) = self
            .session_config
            .scalar_udfs
            .get(&name.to_ascii_lowercase())
            .cloned()
        {
            let bound = args
                .iter()
                .map(|argument| self.bind_order_expr_in_output_scope(argument, scope))
                .collect::<Result<Vec<_>>>()?;
            return self.bind_scalar_udf(function, bound);
        }
        if let Some(function) = resolve_builtin_scalar(name) {
            let mut bound = args
                .iter()
                .map(|a| self.bind_order_expr_in_output_scope(a, scope))
                .collect::<Result<Vec<_>>>()?;
            let lname = name.to_ascii_lowercase();
            let pre_types: Vec<LogicalType> = bound.iter().map(|argument| argument.ty()).collect();
            koko_function::scalarfn::signature_gate(name, &pre_types)?;
            if matches!(
                function,
                BuiltinScalar::Coalesce
                    | BuiltinScalar::Ifnull
                    | BuiltinScalar::Greatest
                    | BuiltinScalar::Least
            ) {
                let common =
                    common_type_strict(bound.iter().map(|a| a.ty())).unwrap_or(LogicalType::String);
                bound = bound
                    .into_iter()
                    .map(|argument| self.coerce_to(argument, &common))
                    .collect();
            }
            // An empty list literal adopts the OTHER list argument's element
            // type (C++ canCastStatically): LIST_CAT(['7','3'], []) binds as
            // STRING[] × STRING[] instead of a bind-time mismatch.
            if bound.len() == 2 {
                for i in 0..2 {
                    if let Some(child) = bound[1 - i].ty().list_child().cloned() {
                        if let BoundExpr::List { elems, ty } = &mut bound[i] {
                            if elems.is_empty() {
                                *ty = LogicalType::List(Box::new(child));
                            }
                        }
                    }
                }
            }
            bound = coerce_string_params(&lname, bound);
            let arg_types: Vec<LogicalType> = bound.iter().map(|a| a.ty()).collect();
            let ty = koko_function::scalar_func_result_type(function, name, &arg_types)?;
            // With `disable_map_key_check=false`, map() validates its keys at
            // eval (NULL keys reject) — routed via the internal checked name.
            let lname = if lname == "map" && !self.disable_map_key_check {
                "map_checked".to_string()
            } else {
                lname
            };
            return Ok(BoundExpr::Call {
                function,
                called_name: lname,
                args: bound,
                ty,
            });
        }
        Err(unknown_function_error(name))
    }

    pub(super) fn bind_output_bool(
        &mut self,
        op: ScalarOp,
        terms: &[ast::Expr],
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        let args = terms
            .iter()
            .map(|t| self.bind_order_expr_in_output_scope(t, scope))
            .collect::<Result<Vec<_>>>()?;
        Ok(BoundExpr::Scalar {
            op,
            args,
            ty: LogicalType::Bool,
        })
    }

    pub(super) fn bind_output_scalar(
        &mut self,
        op: ScalarOp,
        args: &[&ast::Expr],
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        let args = args
            .iter()
            .map(|a| self.bind_order_expr_in_output_scope(a, scope))
            .collect::<Result<Vec<_>>>()?;
        if op.is_comparison() && args.len() == 2 {
            let (l, r) = (args[0].ty(), args[1].ty());
            if !koko_function::comparison_comparable(&l, &r) {
                for a in &args {
                    if let Some(Err(e)) = try_const_eval(a) {
                        return Err(e);
                    }
                }
                return Err(Error::binder(format!(
                    "Type Mismatch: Cannot compare types {l} and {r}"
                )));
            }
        }
        let arg_types: Vec<LogicalType> = args.iter().map(|a| a.ty()).collect();
        let ty = koko_function::scalar_result_type(op, &arg_types)?;
        Ok(BoundExpr::Scalar { op, args, ty })
    }

    pub(super) fn bind_output_case(
        &mut self,
        operand: Option<&ast::Expr>,
        when_thens: &[(ast::Expr, ast::Expr)],
        else_: Option<&ast::Expr>,
        scope: &HashMap<String, ProjectionOutputRef>,
    ) -> Result<BoundExpr> {
        let bound_operand = operand
            .map(|o| self.bind_order_expr_in_output_scope(o, scope))
            .transpose()?
            .map(Box::new);
        let branches = when_thens
            .iter()
            .map(|(cond, res)| {
                Ok((
                    self.bind_order_expr_in_output_scope(cond, scope)?,
                    self.bind_order_expr_in_output_scope(res, scope)?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let belse = else_
            .map(|e| self.bind_order_expr_in_output_scope(e, scope))
            .transpose()?
            .map(Box::new);
        let ty = branches
            .iter()
            .rev()
            .map(|(_, r)| r.ty())
            .find(|t| *t != LogicalType::Any)
            .or_else(|| {
                belse
                    .as_ref()
                    .map(|e| e.ty())
                    .filter(|t| *t != LogicalType::Any)
            })
            .unwrap_or(LogicalType::Any);
        let branches = branches
            .into_iter()
            .map(|(condition, result)| (condition, self.coerce_to(result, &ty)))
            .collect();
        let belse = belse.map(|expression| Box::new(self.coerce_to(*expression, &ty)));
        Ok(BoundExpr::Case {
            operand: bound_operand,
            branches,
            else_: belse,
            ty,
        })
    }

    /// Bind a `WITH` projection.
    ///
    /// Every non-variable expression requires an alias, result names are unique,
    /// and `ORDER BY` requires `SKIP` or `LIMIT`. Bare node and relationship
    /// variables retain their graph binding across the boundary; other graph-typed
    /// expressions travel as scalar value columns.
    pub(super) fn bind_with_projection(
        &mut self,
        r: &ast::ReturnClause,
    ) -> Result<BoundProjection> {
        let mut items: Vec<ProjItem> = Vec::new();
        for item in &r.items {
            match item {
                ast::ProjectionItem::Star => {
                    // `WITH *` carries every in-scope variable forward, in
                    // declaration (VarId) order for a deterministic layout.
                    if self.scope.is_empty() {
                        return Err(Error::binder(
                            "RETURN or WITH * is not allowed when there are no variables in scope."
                                .to_string(),
                        ));
                    }
                    let mut named: Vec<(VarId, String)> =
                        self.scope.iter().map(|(n, &v)| (v, n.clone())).collect();
                    named.sort_by_key(|(v, _)| v.0);
                    for (var, name) in named {
                        items.push(self.with_item(name, self.var_ref_expr(var))?);
                    }
                }
                // `WITH a.*` expands to unaliased property expressions, which C++
                // rejects like any other unaliased WITH item.
                ast::ProjectionItem::AllProperties(_) | ast::ProjectionItem::AllStructFields(_) => {
                    return Err(Error::binder(
                        "Expression in WITH must be aliased (use AS).".to_string(),
                    ));
                }
                ast::ProjectionItem::Expr { expr, alias } => {
                    let name = match (alias, expr) {
                        (Some(a), _) => a.clone(),
                        // A bare variable supplies its own name; anything else must
                        // be explicitly aliased.
                        (None, ast::Expr::Variable(n)) => n.clone(),
                        (None, _) => {
                            return Err(Error::binder(
                                "Expression in WITH must be aliased (use AS).".to_string(),
                            ));
                        }
                    };
                    let be = self
                        .bind_expr(expr)
                        .map_err(|e| rewrite_nested_agg(e, Some(name.as_str())))?;
                    items.push(self.with_item(name, be)?);
                }
            }
        }

        // Column names (the aliases) must be unique.
        let mut seen = std::collections::HashSet::new();
        for it in &items {
            let name = match it {
                ProjItem::Scalar { name, .. } | ProjItem::Var { name, .. } => name,
            };
            if !seen.insert(name.clone()) {
                return Err(Error::binder(format!(
                    "Multiple result columns with the same name {name} are not supported."
                )));
            }
        }

        let proj = self.finish_projection(r.distinct, items, r)?;
        if !proj.order_by.is_empty() && proj.skip.is_none() && proj.limit.is_none() {
            return Err(Error::binder(
                "In WITH clause, ORDER BY must be followed by SKIP or LIMIT.".to_string(),
            ));
        }
        Ok(proj)
    }

    /// Build one `WITH` projection item. A bare assembled node or relationship
    /// variable retains its binding across the part boundary. Recursive
    /// relationships, paths, and other graph-typed expressions travel as scalar
    /// value columns.
    pub(super) fn with_item(&self, name: String, expr: BoundExpr) -> Result<ProjItem> {
        // A bare node *or* relationship variable is carried as a whole value
        // (assembled at the projection). A node is re-exploded into a binding in
        // the next part; a relationship is carried as a scalar value, with property
        // access via `ValueProperty` (see `carry_forward`).
        if let BoundExpr::NodeRef { var, .. } = &expr {
            // A node / plain rel is carried as a binding (re-exploded next part);
            // a recursive rel or path is carried as a `RECURSIVE_REL` value column.
            if self.vars[var.0 as usize].is_assembled_graph_var() {
                return Ok(ProjItem::Var { name, var: *var });
            }
            return Ok(ProjItem::Scalar { name, expr });
        }
        // A node/rel-typed EXPRESSION (e.g. a CASE over two nodes) is carried
        // as a scalar VALUE column — property access via ValueProperty, and it
        // renders as the whole value (issue 2589).
        Ok(ProjItem::Scalar { name, expr })
    }

    /// After a `WITH`, replace the scope with its projected variables (the carried
    /// scope for the next part) and return their `VarId`s in column order. Scalars
    /// carry their value; a carried node becomes a fresh node binding (same
    /// table/label/properties) re-scoped under its alias, so a later
    /// `MATCH (a)-[…]` or `a.prop` resolves to it.
    pub(super) fn carry_forward(&mut self, projection: &BoundProjection) -> Result<Vec<VarId>> {
        self.scope.clear();
        let mut carried = Vec::with_capacity(projection.items.len());
        for item in &projection.items {
            let var = match item {
                ProjItem::Scalar { name, expr } => self.add_scalar_var(name.clone(), expr.ty()),
                ProjItem::Var { name, var } => {
                    if self.vars[var.0 as usize].is_node() {
                        // Node kinds are self-contained (table + label); copy the
                        // binding + property list so it re-materializes next part.
                        let info = &self.vars[var.0 as usize];
                        let kind = info.kind.clone();
                        let props = info.properties.clone();
                        self.add_var(Some(name.clone()), kind, props)
                    } else {
                        // A relationship is carried as a scalar VALUE — its endpoint
                        // VarIds don't survive the part boundary, so it can't be a
                        // re-exploded binding; property access uses `ValueProperty`.
                        let ty = self.var_value_type(*var);
                        self.add_scalar_var(name.clone(), ty)
                    }
                }
            };
            carried.push(var);
        }
        Ok(carried)
    }
}
