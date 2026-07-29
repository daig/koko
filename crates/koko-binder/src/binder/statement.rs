use super::*;

pub(super) fn bind_storage_direction(value: Option<&str>) -> Result<RelStorageDirection> {
    match value {
        None => Ok(RelStorageDirection::Both),
        Some(value) if value.eq_ignore_ascii_case("FWD") => Ok(RelStorageDirection::Fwd),
        Some(value) if value.eq_ignore_ascii_case("BWD") => Ok(RelStorageDirection::Bwd),
        Some(value) if value.eq_ignore_ascii_case("BOTH") => Ok(RelStorageDirection::Both),
        Some(value) => Err(Error::runtime(format!(
            "Cannot parse {value} as ExtendDirection."
        ))),
    }
}

pub(super) fn column_type_text(type_name: &str, logical_type: &LogicalType) -> String {
    if type_name.eq_ignore_ascii_case("SERIAL") {
        "SERIAL".to_string()
    } else {
        logical_type.to_string()
    }
}

impl Binder<'_, '_> {
    /// Dispatch a non-query statement to its DDL, COPY, or interchange binder.
    pub(super) fn bind_nonquery(&mut self, statement: &ast::Statement) -> Result<BoundStatement> {
        match statement {
            ast::Statement::Explain { inner, .. } => self.bind_nonquery(inner),
            ast::Statement::CreateNodeTable(table) => self.bind_create_node_table(table),
            ast::Statement::CreateRelTable(table) => self.bind_create_rel_table(table),
            ast::Statement::DropTable(drop) => self.bind_drop_table(drop),
            ast::Statement::Alter(alter) => self.bind_alter(alter),
            ast::Statement::CreateSequence(sequence) => self.bind_create_sequence(sequence),
            ast::Statement::DropSequence(drop) => self.bind_drop_sequence(drop),
            ast::Statement::Comment(comment) => self.bind_comment(comment),
            ast::Statement::CreateType(type_) => self.bind_create_type(type_),
            ast::Statement::Copy(copy) => self.bind_copy(copy),
            ast::Statement::CopyTo(copy) => self.bind_copy_to(copy),
            ast::Statement::ExportDatabase(export) => self.bind_export_database(export),
            ast::Statement::ImportDatabase(import) => self.bind_import_database(import),
            ast::Statement::Query(_) | ast::Statement::CreateTableAs(_) => {
                unreachable!("queries/CTAS route through bind_statement")
            }
            ast::Statement::Transaction(_)
            | ast::Statement::Call(_)
            | ast::Statement::CreateGraph(_)
            | ast::Statement::UseGraph { .. }
            | ast::Statement::CreateIndex(_)
            | ast::Statement::DropIndex(_)
            | ast::Statement::DropGraph { .. } => {
                unreachable!("transaction/CALL statements are handled pre-bind")
            }
            ast::Statement::CreateMacro(_) | ast::Statement::DropMacro { .. } => {
                unreachable!("macro statements are handled pre-bind")
            }
        }
    }
}

impl<'catalog, 'bind> Binder<'catalog, 'bind> {
    pub(super) fn resolve_copy_target(&self, name: &str) -> Option<(TableId, bool)> {
        if let Some(table) = self.catalog.table_id(name) {
            return Some((table, false));
        }
        for rel_id in self.catalog.rel_table_ids() {
            let rel = self.catalog.rel_table(rel_id)?;
            for pair in rel.pairs() {
                let from_name = self.catalog.node_table(pair.from)?.name();
                let to_name = self.catalog.node_table(pair.to)?.name();
                let legacy = format!("{}_{}_{}", rel.name(), from_name, to_name);
                if legacy.eq_ignore_ascii_case(name) {
                    return Some((pair.member, true));
                }
            }
        }
        None
    }

    pub(super) fn select_copy_rel_member(
        &self,
        table: TableId,
        table_name: &str,
        options: &CsvLoadOptions,
        legacy_target: bool,
    ) -> Result<TableId> {
        if legacy_target {
            return Ok(table);
        }
        let rel = self
            .catalog
            .rel_table(table)
            .expect("COPY target kind is relationship");
        let selectors = options.from.as_deref().zip(options.to.as_deref());
        if rel.pairs().len() > 1 && selectors.is_none() {
            return Err(Error::binder(format!(
                "The table {table_name} has multiple FROM and TO pairs defined in the schema. A \
                 specific pair of FROM and TO options is expected when copying data into the \
                 {table_name} table."
            )));
        }
        if options.from.is_some() != options.to.is_some() {
            return Err(Error::binder(
                "Both FROM and TO options are required when selecting a relationship pair."
                    .to_string(),
            ));
        }
        let Some((from_name, to_name)) = selectors else {
            return Ok(rel.pairs()[0].member);
        };
        for pair in rel.pairs() {
            let from_matches = self
                .catalog
                .node_table(pair.from)
                .is_some_and(|node| node.name().eq_ignore_ascii_case(from_name));
            let to_matches = self
                .catalog
                .node_table(pair.to)
                .is_some_and(|node| node.name().eq_ignore_ascii_case(to_name));
            if from_matches && to_matches {
                return Ok(pair.member);
            }
        }
        Err(Error::binder(format!(
            "Rel table {table_name} does not contain {from_name}-{to_name} from-to pair."
        )))
    }

    pub(super) fn bind_copy(&mut self, c: &ast::CopyStatement) -> Result<BoundStatement> {
        // The target table resolves before option validation (C++ order:
        // `COPY missing FROM ... (bad_opt=...)` blames the table).
        let (mut table, legacy_target) = self
            .resolve_copy_target(&c.table)
            .ok_or_else(|| Error::binder(format!("Table {} does not exist.", c.table)))?;
        // A table-function source takes no reader options — the C++ binder
        // rejects any before validating individual option names.
        if !c.options.is_empty()
            && c.source_query
                .as_ref()
                .is_some_and(|q| table_func_source_name(q).is_some())
        {
            return Err(Error::binder(
                "No option is supported when copying from table functions.".to_string(),
            ));
        }
        if c.source_query.is_none() {
            let format = hinted_input_format(&c.file_path, &c.options)?;
            if c.by_column && format != FileFormat::Npy {
                let format_name = match format {
                    FileFormat::Csv => "csv",
                    FileFormat::Parquet => "parquet",
                    FileFormat::Npy => unreachable!(),
                };
                return Err(Error::binder(format!(
                    "Copy by column with {format_name} file type is not supported."
                )));
            }
            validate_columnar_input_options(format, &c.options, true)?;
        }
        let options = bind_csv_options(&c.options)?;
        // File sources are fully expanded and validated once during binding.
        // Execution consumes only canonical concrete paths.
        let is_node = match self.catalog.table_kind(table) {
            Some(koko_catalog::TableKind::Node) => true,
            Some(koko_catalog::TableKind::Rel) => false,
            None => return Err(Error::binder(format!("Table {} does not exist.", c.table))),
        };
        if c.by_column && !is_node {
            return Err(Error::binder(
                "Copy by column is not supported for relationship table.".to_string(),
            ));
        }
        if !is_node {
            table = self.select_copy_rel_member(table, &c.table, &options, legacy_target)?;
        }
        // A query source binds now (against the pre-COPY catalog), and its
        // column count must match the COPY input width (C++ bind error).
        let source_query = match &c.source_query {
            Some(q) => {
                let BoundStatement::Query(bq) = bind_regular_query(
                    self.catalog,
                    q,
                    self.params,
                    &self.session_config,
                    self.prepared_parameters.as_deref_mut(),
                )?
                else {
                    unreachable!("bind_regular_query yields a Query");
                };
                let got = bq.result_columns().len();
                let expected = match &c.columns {
                    Some(cols) => {
                        if is_node {
                            cols.len()
                        } else {
                            2 + cols.len()
                        }
                    }
                    None => {
                        if is_node {
                            self.catalog
                                .node_table(table)
                                .map(|nt| {
                                    nt.columns()
                                        .iter()
                                        .filter(|column| {
                                            column.logical_type() != &LogicalType::Serial
                                                && !matches!(
                                                    column.default(),
                                                    Some(koko_catalog::ColumnDefault::NextVal(_))
                                                )
                                        })
                                        .count()
                                })
                                .unwrap_or(0)
                        } else {
                            self.catalog
                                .rel_table(table)
                                .map(|rt| 2 + rt.columns().len())
                                .unwrap_or(2)
                        }
                    }
                };
                if got != expected {
                    // A table-function-only source names the FUNCTION in the
                    // mismatch (C++ `SHOW_OFFICIAL_EXTENSIONS has 2 columns but
                    // 1 columns were expected.`); a general query says "Query".
                    return Err(Error::binder(match table_func_source_name(q) {
                        Some(fname) => format!(
                            "{fname} has {got} columns but {expected} columns were expected."
                        ),
                        None => format!(
                            "Query returns {got} columns but {expected} columns were expected."
                        ),
                    }));
                }
                Some(bq)
            }
            None => None,
        };
        let (file_path, extra_files, format) = if source_query.is_some() {
            (c.file_path.clone(), c.extra_files.clone(), None)
        } else {
            let spellings: Vec<String> = std::iter::once(&c.file_path)
                .chain(c.extra_files.iter())
                .cloned()
                .collect();
            let resolver = FileResolverConfig {
                base_dir: self.session_config.base_dir.clone(),
                home_directory: self.session_config.home_directory.clone(),
                file_search_path: self.session_config.file_search_path.clone(),
            };
            let resolved = resolve_files(&spellings, &resolver, options.file_format.as_deref())?;
            let format = resolved[0].format;
            validate_columnar_input_options(format, &c.options, !is_node)?;
            let mut paths = resolved
                .into_iter()
                .map(|file| file.path.to_string_lossy().into_owned());
            let file_path = paths.next().expect("non-empty source spellings");
            (file_path, paths.collect(), Some(format))
        };
        if let Some(format) = format {
            let paths: Vec<String> = std::iter::once(file_path.clone())
                .chain(extra_files.iter().cloned())
                .collect();
            let expected = match &c.columns {
                Some(columns) => columns.len() + usize::from(!is_node) * 2,
                None if is_node => self
                    .catalog
                    .node_table(table)
                    .map(|node| {
                        node.columns()
                            .iter()
                            .filter(|column| {
                                column.logical_type() != &LogicalType::Serial
                                    && !matches!(
                                        column.default(),
                                        Some(koko_catalog::ColumnDefault::NextVal(_))
                                    )
                            })
                            .count()
                    })
                    .unwrap_or(0),
                None => self
                    .catalog
                    .rel_table(table)
                    .map_or(2, |rel| 2 + rel.columns().len()),
            };
            if format == FileFormat::Npy && c.by_column {
                if paths.len() != expected {
                    return Err(Error::binder(format!(
                        "Number of columns mismatch. Expected {expected} but got {}.",
                        paths.len()
                    )));
                }
            } else if format != FileFormat::Csv {
                let inspect = self.session_config.file_schema_resolver.ok_or_else(|| {
                    Error::not_implemented(format!(
                        "{format:?} metadata inspection is unavailable in this binder context."
                    ))
                })?;
                let source = inspect(format, &paths)?;
                if source.len() != expected {
                    return Err(Error::binder(format!(
                        "Number of columns mismatch. Expected {expected} but got {}.",
                        source.len()
                    )));
                }
            }
        }
        Ok(BoundStatement::Copy(BoundCopy {
            table,
            is_node,
            columns: c.columns.clone(),
            file_path,
            extra_files,
            format,
            by_column: c.by_column,
            source_query,
            options,
        }))
    }

    pub(super) fn bind_copy_to(&mut self, copy: &ast::CopyToStatement) -> Result<BoundStatement> {
        // C++ dispatches the output function before binding the query, so an
        // unsupported extension wins over errors inside the query.
        let options = bind_output_options(&copy.path, &copy.options)?;
        let BoundStatement::Query(query) = bind_regular_query(
            self.catalog,
            &copy.query,
            self.params,
            &self.session_config,
            self.prepared_parameters.as_deref_mut(),
        )?
        else {
            unreachable!("bind_regular_query yields a Query");
        };
        let columns = query.result_columns().to_vec();
        Ok(BoundStatement::CopyTo(BoundCopyTo {
            query,
            columns,
            path: resolve_destination_path(&copy.path, &self.session_config),
            options,
        }))
    }

    pub(super) fn bind_export_database(
        &self,
        export: &ast::ExportDatabaseStatement,
    ) -> Result<BoundStatement> {
        let mut options = dedupe_options(&export.options);
        let format = if let Some((_, value)) = options.iter().find(|(key, _)| key == "FORMAT") {
            let ast::LoadOptVal::Str(format) = value else {
                return Err(Error::binder(
                    "The type of format option must be a string.".to_string(),
                ));
            };
            if format.eq_ignore_ascii_case("csv") {
                FileFormat::Csv
            } else if format.eq_ignore_ascii_case("parquet") {
                FileFormat::Parquet
            } else {
                return Err(Error::binder(
                    "Export database currently only supports csv and parquet files.".to_string(),
                ));
            }
        } else {
            FileFormat::Parquet
        };
        options.retain(|(key, _)| key != "FORMAT");

        let schema_only =
            if let Some((_, value)) = options.iter().find(|(key, _)| key == "SCHEMA_ONLY") {
                let ast::LoadOptVal::Bool(schema_only) = value else {
                    return Err(Error::binder(
                        "The 'SCHEMA_ONLY' option must have a BOOL value.".to_string(),
                    ));
                };
                *schema_only
            } else {
                false
            };
        if schema_only && export.options.len() != 1 {
            return Err(Error::binder(
                "When 'SCHEMA_ONLY' option is set to true in export database, no other options are allowed."
                    .to_string(),
            ));
        }
        options.retain(|(key, _)| key != "SCHEMA_ONLY");

        let output_options = if schema_only {
            BoundOutputOptions::Parquet {
                compression: BoundParquetCompression::default(),
            }
        } else {
            match format {
                FileFormat::Csv => bind_csv_options(&options).map(BoundOutputOptions::Csv)?,
                FileFormat::Parquet if !options.is_empty() => {
                    return Err(Error::binder(
                        "Only export to csv can have options.".to_string(),
                    ));
                }
                FileFormat::Parquet => BoundOutputOptions::Parquet {
                    compression: BoundParquetCompression::default(),
                },
                FileFormat::Npy => unreachable!("validated above"),
            }
        };
        Ok(BoundStatement::ExportDatabase(BoundExportDatabase {
            path: resolve_destination_path(&export.path, &self.session_config),
            options: output_options,
            schema_only,
        }))
    }

    pub(super) fn bind_import_database(
        &self,
        import: &ast::ImportDatabaseStatement,
    ) -> Result<BoundStatement> {
        Ok(BoundStatement::ImportDatabase(BoundImportDatabase {
            path: resolve_destination_path(&import.path, &self.session_config),
        }))
    }

    pub(super) fn bind_create_node_table(
        &mut self,
        t: &ast::CreateNodeTable,
    ) -> Result<BoundStatement> {
        let columns = self.bind_columns(&t.columns)?;
        if !columns
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(&t.primary_key))
        {
            return Err(Error::binder(format!(
                "Primary key {} does not match any of the predefined node properties.",
                t.primary_key
            )));
        }
        // The C++ PK type allow-list works on the *physical* type: every integer
        // width, STRING (also BLOB's physical), FLOAT/DOUBLE, and the int-backed
        // temporal/DECIMAL/UUID types are valid; BOOL, INTERVAL and nested types
        // are not (bind_ddl.cpp validatePrimaryKey, oracle-verified matrix).
        if let Some((_, pk_ty)) = columns
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(&t.primary_key))
        {
            let valid = pk_ty.is_numeric()
                || matches!(
                    pk_ty,
                    LogicalType::String
                        | LogicalType::Blob
                        | LogicalType::Uuid
                        | LogicalType::Serial
                        | LogicalType::Date
                        | LogicalType::Timestamp
                        | LogicalType::TimestampNs
                        | LogicalType::TimestampMs
                        | LogicalType::TimestampSec
                        | LogicalType::TimestampTz
                );
            if !valid {
                return Err(Error::binder(format!(
                    "Invalid primary key column type {pk_ty}. Primary keys must be either STRING \
                     or a numeric type."
                )));
            }
        }
        let columns = self.assemble_bound_columns(&t.columns, &columns)?;
        // The duplicate-name check runs last, so a malformed-and-duplicate
        // statement reports its malformed error first (matching the C++ binder).
        // `IF NOT EXISTS` defers the skip to execution.
        if !t.if_not_exists && self.catalog.contains_table(&t.name) {
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                t.name
            )));
        }
        let icebug_storage = t
            .format
            .as_deref()
            .filter(|format| format.eq_ignore_ascii_case("icebug-disk"))
            .map(|_| t.storage.clone().unwrap_or_default());
        Ok(BoundStatement::CreateNodeTable {
            name: t.name.clone(),
            columns,
            primary_key: t.primary_key.clone(),
            if_not_exists: t.if_not_exists,
            icebug_storage,
        })
    }

    pub(super) fn bind_create_rel_table(
        &mut self,
        t: &ast::CreateRelTable,
    ) -> Result<BoundStatement> {
        let resolve = |n: &str| {
            self.catalog
                .node_table_by_name(n)
                .map(|table| table.id())
                .ok_or_else(|| {
                    if self.catalog.table_id(n).is_some() {
                        Error::binder(format!("{n} is not of type NODE."))
                    } else {
                        Error::binder(format!("Table {n} does not exist."))
                    }
                })
        };
        // Column types resolve before the FROM/TO endpoints (C++ order: an
        // invalid property type reports even when an endpoint table is missing).
        let columns = self.bind_columns(&t.columns)?;
        let pairs = t
            .pairs
            .iter()
            .map(|(f, to)| Ok((resolve(f)?, resolve(to)?)))
            .collect::<Result<Vec<_>>>()?;
        let icebug_storage = t
            .format
            .as_deref()
            .filter(|format| format.eq_ignore_ascii_case("icebug-disk"))
            .map(|_| t.storage.clone().unwrap_or_default());
        let endpoints_are_icebug = pairs.iter().all(|(from, to)| {
            self.catalog.is_icebug_table(*from) && self.catalog.is_icebug_table(*to)
        });
        let endpoints_include_icebug = pairs.iter().any(|(from, to)| {
            self.catalog.is_icebug_table(*from) || self.catalog.is_icebug_table(*to)
        });
        if icebug_storage.is_some() != endpoints_are_icebug
            || endpoints_include_icebug && !endpoints_are_icebug
        {
            return Err(Error::binder(
                "Cannot mix icebug-disk tables with non-icebug-disk tables in CREATE REL TABLE.",
            ));
        }
        let columns = self.assemble_bound_columns(&t.columns, &columns)?;
        let mut seen = HashSet::new();
        for ((from_name, to_name), pair) in t.pairs.iter().zip(&pairs) {
            if !seen.insert(*pair) {
                return Err(Error::binder(format!(
                    "Found duplicate FROM-TO {from_name}-{to_name} pairs."
                )));
            }
        }
        let storage_direction = bind_storage_direction(t.storage_direction.as_deref())?;
        // The duplicate-name check runs after endpoint resolution, so a bad-endpoint
        // duplicate (e.g. `CREATE REL TABLE knows(FROM Nope TO Nope)`) reports the
        // endpoint error first. Covers REL TABLE and REL TABLE GROUP.
        if !t.if_not_exists && self.catalog.contains_table(&t.name) {
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                t.name
            )));
        }
        Ok(BoundStatement::CreateRelTable {
            name: t.name.clone(),
            pairs,
            columns,
            if_not_exists: t.if_not_exists,
            multiplicity: t.multiplicity,
            storage_direction,
            icebug_storage,
        })
    }

    pub(super) fn assemble_bound_columns(
        &mut self,
        syntax: &[ast::ColumnDef],
        bound: &[(String, LogicalType)],
    ) -> Result<Vec<BoundColumn>> {
        syntax
            .iter()
            .zip(bound)
            .map(|(column, (name, logical_type))| {
                self.bind_column(
                    name.clone(),
                    logical_type.clone(),
                    &column.type_name,
                    column.default.as_ref(),
                )
            })
            .collect()
    }

    pub(super) fn bind_column(
        &mut self,
        name: String,
        logical_type: LogicalType,
        type_name: &str,
        default: Option<&ast::Expr>,
    ) -> Result<BoundColumn> {
        let generation = if type_name.eq_ignore_ascii_case("SERIAL") {
            if default.is_some() {
                return Err(Error::binder(
                    "No DEFAULT value should be set for SERIAL columns".to_string(),
                ));
            }
            BoundColumnGeneration::Serial
        } else if let Some(expr) = default {
            BoundColumnGeneration::Default {
                value: self.bind_explicit_default(expr, &logical_type)?,
                source_text: expr_to_cypher(expr),
            }
        } else {
            BoundColumnGeneration::None
        };
        Ok(BoundColumn {
            name,
            type_text: column_type_text(type_name, &logical_type),
            logical_type,
            generation,
        })
    }

    /// Classify an explicit `DEFAULT`: `nextval('seq')` is evaluated per row;
    /// every other expression is cast now and folded by execution.
    pub(super) fn bind_explicit_default(
        &mut self,
        expr: &ast::Expr,
        ty: &LogicalType,
    ) -> Result<BoundColumnDefault> {
        if let ast::Expr::Function { name, args, .. } = expr {
            if sequence_fn(name) == Some(SequenceFn::NextVal) {
                return match args.as_slice() {
                    [ast::Expr::Literal(Value::String(sequence))] => {
                        Ok(BoundColumnDefault::NextVal(sequence.clone()))
                    }
                    _ => Err(Error::not_implemented(
                        "nextval default requires a string-literal sequence name".to_string(),
                    )),
                };
            }
        }
        let bound = self.bind_expr(expr)?;
        assignable_or_err(&bound, ty, &expr_name(expr))?;
        Ok(BoundColumnDefault::Constant(BoundExpr::Cast {
            expr: Box::new(bound),
            target: ty.clone(),
        }))
    }

    pub(super) fn bind_drop_table(&self, d: &ast::DropTable) -> Result<BoundStatement> {
        match self.catalog.table_id(&d.name) {
            Some(id) => {
                // A node table referenced by a relationship table cannot be dropped
                // (it would dangle the rel's FROM/TO endpoints).
                if self.catalog.table_kind(id) == Some(koko_catalog::TableKind::Node) {
                    if let Some(rel) = self.catalog.rel_table_referencing(id) {
                        let node = self.catalog.node_table(id).expect("node table exists");
                        return Err(Error::binder(format!(
                            "Cannot delete node table {} because it is referenced by relationship \
                             table {}.",
                            node.name(),
                            rel
                        )));
                    }
                }
                Ok(BoundStatement::DropTable {
                    name: d.name.clone(),
                    table: Some(id),
                })
            }
            // `IF EXISTS` turns a missing table into a skip (execution emits a message);
            // without it, a missing table is a bind error.
            None if d.if_exists => Ok(BoundStatement::DropTable {
                name: d.name.clone(),
                table: None,
            }),
            None => Err(Error::binder(format!("Table {} does not exist.", d.name))),
        }
    }

    pub(super) fn bind_alter(&mut self, a: &ast::AlterStatement) -> Result<BoundStatement> {
        let id = self
            .catalog
            .table_id(&a.table)
            .ok_or_else(|| Error::binder(format!("Table {} does not exist.", a.table)))?;
        if self.catalog.is_icebug_table(id) {
            return Err(Error::binder(format!(
                "Cannot alter table {}: icebug-disk tables are immutable.",
                self.table_name(id)
            )));
        }
        // Messages echo the catalog's canonical table name (matching the C++ engine,
        // which uses `entry->getName()`), independent of the user's casing.
        let table_name = self.table_name(id);
        let op = match &a.op {
            ast::AlterOp::AddProperty {
                name,
                type_name,
                default,
                if_not_exists,
            } => {
                let ty = self.resolve_ddl_type(type_name)?;
                BoundAlterOp::AddProperty {
                    column: self.bind_column(name.clone(), ty, type_name, default.as_ref())?,
                    if_not_exists: *if_not_exists,
                }
            }
            ast::AlterOp::DropProperty { name, if_exists } => {
                // Reject dropping the primary key (a node table's identity column).
                if let Some(nt) = self.catalog.node_table(id) {
                    if nt
                        .column(name)
                        .is_some_and(|c| c.column_id().0 as usize == nt.primary_key_index())
                    {
                        return Err(Error::binder(format!(
                            "Cannot drop property {name} in table {table_name} because it is used \
                             as primary key."
                        )));
                    }
                }
                BoundAlterOp::DropProperty {
                    name: name.clone(),
                    if_exists: *if_exists,
                }
            }
            ast::AlterOp::RenameProperty { old, new } => BoundAlterOp::RenameProperty {
                old: old.clone(),
                new: new.clone(),
            },
            ast::AlterOp::RenameTable { new } => BoundAlterOp::RenameTable { new: new.clone() },
            ast::AlterOp::AddFromTo {
                from,
                to,
                if_not_exists,
            } => {
                self.require_rel_table(id, &a.table)?;
                BoundAlterOp::AddFromTo {
                    from: self.resolve_node_table(from)?,
                    to: self.resolve_node_table(to)?,
                    if_not_exists: *if_not_exists,
                }
            }
            ast::AlterOp::DropFromTo {
                from,
                to,
                if_exists,
            } => {
                self.require_rel_table(id, &a.table)?;
                BoundAlterOp::DropFromTo {
                    from: self.resolve_node_table(from)?,
                    to: self.resolve_node_table(to)?,
                    if_exists: *if_exists,
                }
            }
        };
        Ok(BoundStatement::Alter {
            table: id,
            table_name,
            op,
        })
    }

    /// Resolve a node-table name to its id (the endpoint of a FROM-TO pair).
    /// A name that exists as a non-node table is typed ("R is not of type
    /// NODE."), a missing one reports absence.
    pub(super) fn resolve_node_table(&self, name: &str) -> Result<TableId> {
        self.catalog
            .node_table_by_name(name)
            .map(|table| table.id())
            .ok_or_else(|| {
                if self.catalog.table_id(name).is_some() {
                    Error::binder(format!("{name} is not of type NODE."))
                } else {
                    Error::binder(format!("Table {name} does not exist."))
                }
            })
    }

    /// Require that an `ALTER … FROM…TO` targets a relationship table.
    pub(super) fn require_rel_table(&self, id: TableId, name: &str) -> Result<()> {
        if self.catalog.rel_table(id).is_none() {
            return Err(Error::binder(format!(
                "Table {name} is not a relationship table."
            )));
        }
        Ok(())
    }

    /// The catalog's canonical name for a table id (node or rel).
    pub(super) fn table_name(&self, id: TableId) -> String {
        self.catalog
            .node_table(id)
            .map(|table| table.name().to_string())
            .or_else(|| {
                self.catalog
                    .rel_table(id)
                    .map(|table| table.name().to_string())
            })
            .unwrap_or_default()
    }

    pub(super) fn bind_create_sequence(&self, s: &ast::CreateSequence) -> Result<BoundStatement> {
        // The duplicate-name check (separate sequence namespace) precedes value
        // validation, matching the C++ binder; `IF NOT EXISTS` defers the skip to
        // execution.
        if !s.if_not_exists && self.catalog.contains_sequence(&s.name) {
            return Err(Error::binder(format!(
                "{} already exists in catalog.",
                s.name
            )));
        }
        let to_i64 = |v: i128| -> Result<i64> {
            i64::try_from(v).map_err(|_| {
                Error::binder("Out of bounds: SEQUENCE accepts integers within INT64.".to_string())
            })
        };
        let increment = match s.increment {
            Some(v) => to_i64(v)?,
            None => 1,
        };
        if increment == 0 {
            return Err(Error::binder("INCREMENT must be non-zero.".to_string()));
        }
        // Defaults flip with the increment sign: ascending counts up from 1 to
        // INT64_MAX; descending counts down from -1 to INT64_MIN.
        let min = match s.min_value {
            Some(v) => to_i64(v)?,
            None if increment > 0 => 1,
            None => i64::MIN,
        };
        let max = match s.max_value {
            Some(v) => to_i64(v)?,
            None if increment > 0 => i64::MAX,
            None => -1,
        };
        let start = match s.start {
            Some(v) => to_i64(v)?,
            None if increment > 0 => min,
            None => max,
        };
        if max < min {
            return Err(Error::binder(
                "SEQUENCE MAXVALUE should be greater than or equal to MINVALUE.".to_string(),
            ));
        }
        if start < min || start > max {
            return Err(Error::binder(
                "SEQUENCE START value should be between MINVALUE and MAXVALUE.".to_string(),
            ));
        }
        Ok(BoundStatement::CreateSequence {
            name: s.name.clone(),
            if_not_exists: s.if_not_exists,
            start,
            increment,
            min,
            max,
            cycle: s.cycle,
        })
    }

    pub(super) fn bind_create_type(&self, t: &ast::CreateType) -> Result<BoundStatement> {
        if self.catalog.contains_user_type(&t.name) {
            return Err(Error::binder(format!("Duplicated type name: {}.", t.name)));
        }
        // The underlying type is itself resolved through the registry, so a UDT may
        // alias an earlier UDT.
        let ty = self.resolve_ddl_type(&t.type_name)?;
        Ok(BoundStatement::CreateType {
            name: t.name.clone(),
            ty,
        })
    }

    /// Resolve a DDL type string, consulting the user-defined-type registry first
    /// (so an alias like `SMALLINT`/`DESCRIPTION` resolves to its underlying type).
    pub(super) fn resolve_ddl_type(&self, s: &str) -> Result<LogicalType> {
        if let Some(ty) = self.catalog.user_type(s) {
            return Ok(ty);
        }
        // Pass a resolver so a UDT alias *nested* inside a STRUCT/LIST/MAP/ARRAY
        // (which the top-level check above can't see) still resolves.
        LogicalType::from_ddl_str_with(s, &|name| self.catalog.user_type(name))
    }

    pub(super) fn bind_comment(&self, c: &ast::CommentStmt) -> Result<BoundStatement> {
        let id = self
            .catalog
            .table_id(&c.table)
            .ok_or_else(|| Error::binder(format!("Table {} does not exist.", c.table)))?;
        Ok(BoundStatement::Comment {
            table: id,
            table_name: self.table_name(id),
            comment: c.comment.clone(),
        })
    }

    pub(super) fn bind_drop_sequence(&self, d: &ast::DropSequence) -> Result<BoundStatement> {
        // Without `IF EXISTS`, a missing sequence is a bind error; with it, the
        // skip message is emitted at execution.
        if !d.if_exists && !self.catalog.contains_sequence(&d.name) {
            return Err(Error::binder(format!(
                "Sequence {} does not exist.",
                d.name
            )));
        }
        Ok(BoundStatement::DropSequence {
            name: d.name.clone(),
            if_exists: d.if_exists,
        })
    }

    pub(super) fn bind_columns(
        &self,
        cols: &[ast::ColumnDef],
    ) -> Result<Vec<(String, LogicalType)>> {
        cols.iter()
            .map(|c| {
                // The internal identity names are reserved (C++ reservedInPropertyLookup).
                if matches!(
                    c.name.to_ascii_lowercase().as_str(),
                    "_id" | "_label" | "_src" | "_dst" | "_nodes" | "_rels"
                ) {
                    return Err(Error::binder(format!(
                        "{} is a reserved property name.",
                        c.name
                    )));
                }
                Ok((c.name.clone(), self.resolve_ddl_type(&c.type_name)?))
            })
            .collect()
    }
}
