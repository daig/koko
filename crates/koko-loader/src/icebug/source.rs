use crate::icebug::{validate_schema, validate_version};
use crate::parquet::{ParquetField, ParquetReader, ParquetSchema};
use koko_catalog::{Catalog, IcebugTableSource, RelTable};
use koko_common::{
    DataChunk, Error, ExtendDir, InternalId, LogicalType, MemoryReservation, MemoryTracker,
    QueryControl, Result, TableId, VECTOR_CAPACITY, Value,
};
use koko_storage::BatchNeighbor;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// One decoded node batch plus the physical offset of its first row.
pub struct IcebugNodeBatch {
    pub table: TableId,
    pub start_offset: u64,
    pub properties: DataChunk,
    _reservation: MemoryReservation,
}

/// A pinned range scan over one local read-only node table.
pub struct IcebugNodeScan {
    table: TableId,
    next_offset: u64,
    end: u64,
    property_count: usize,
    reader: ParquetReader,
}

impl IcebugNodeScan {
    pub fn next_chunk(
        &mut self,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
    ) -> Result<Option<IcebugNodeBatch>> {
        control.check()?;
        let Some(properties) = self.reader.next_chunk()? else {
            if self.next_offset != self.end {
                return Err(Error::runtime(
                    "Icebug-disk node file ended before its declared row count.",
                ));
            }
            return Ok(None);
        };
        let reservation = memory.try_reserve(properties.allocated_bytes())?;
        let rows = properties.sel.len() as u64;
        let start_offset = self.next_offset;
        self.next_offset = self.next_offset.saturating_add(rows);
        if self.next_offset > self.end {
            return Err(Error::runtime(
                "Icebug-disk node file exceeds its declared row count.",
            ));
        }
        debug_assert_eq!(properties.columns.len(), self.property_count.max(1));
        Ok(Some(IcebugNodeBatch {
            table: self.table,
            start_offset,
            properties,
            _reservation: reservation,
        }))
    }

    pub fn property_count(&self) -> usize {
        self.property_count
    }
}

enum PinnedIcebugSource {
    Node {
        path: PathBuf,
        file: File,
        num_rows: u64,
    },
    RelCsr {
        indices_path: PathBuf,
        indices_file: File,
        indptr_path: PathBuf,
        indptr_file: File,
        target_column: String,
        num_rows: u64,
        num_bound_nodes: u64,
    },
    RelFlat {
        path: PathBuf,
        file: File,
        source_column: String,
        target_column: String,
        num_rows: u64,
    },
}

/// Query-lifetime pinned handles for every selected local `icebug-disk` table.
#[derive(Default)]
pub struct IcebugQuerySources {
    tables: HashMap<TableId, PinnedIcebugSource>,
}

impl IcebugQuerySources {
    /// Open and validate all local sources at the statement boundary.
    pub fn capture(
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
    ) -> Result<Self> {
        let mut tables = HashMap::new();
        for table in catalog
            .node_table_ids()
            .into_iter()
            .chain(catalog.rel_table_ids())
        {
            let Some(source) = catalog.icebug_table(table).and_then(|entry| entry.source()) else {
                continue;
            };
            let pinned = match source {
                IcebugTableSource::Node { path, num_rows } => PinnedIcebugSource::Node {
                    path: path.clone(),
                    file: open_source(path)?,
                    num_rows: *num_rows,
                },
                IcebugTableSource::RelCsr {
                    indices_path,
                    indptr_path,
                    target_column,
                    num_rows,
                    num_bound_nodes,
                } => PinnedIcebugSource::RelCsr {
                    indices_path: indices_path.clone(),
                    indices_file: open_source(indices_path)?,
                    indptr_path: indptr_path.clone(),
                    indptr_file: open_source(indptr_path)?,
                    target_column: target_column.clone(),
                    num_rows: *num_rows,
                    num_bound_nodes: *num_bound_nodes,
                },
                IcebugTableSource::RelFlat {
                    path,
                    source_column,
                    target_column,
                    num_rows,
                } => PinnedIcebugSource::RelFlat {
                    path: path.clone(),
                    file: open_source(path)?,
                    source_column: source_column.clone(),
                    target_column: target_column.clone(),
                    num_rows: *num_rows,
                },
            };
            tables.insert(table, pinned);
        }
        let sources = Self { tables };
        sources.validate(catalog, control, memory)?;
        Ok(sources)
    }

    pub fn is_external(&self, table: TableId) -> bool {
        self.tables.contains_key(&table)
    }

    pub fn num_rows(&self, table: TableId) -> Option<u64> {
        self.tables.get(&table).map(|source| match source {
            PinnedIcebugSource::Node { num_rows, .. }
            | PinnedIcebugSource::RelCsr { num_rows, .. }
            | PinnedIcebugSource::RelFlat { num_rows, .. } => *num_rows,
        })
    }

    pub fn open_node_scan(
        &self,
        table: TableId,
        projected_columns: &[usize],
        start: u64,
        end: u64,
        catalog: &Catalog,
    ) -> Result<IcebugNodeScan> {
        let Some(PinnedIcebugSource::Node {
            path,
            file,
            num_rows,
        }) = self.tables.get(&table)
        else {
            return Err(Error::runtime(
                "Icebug-disk source kind does not match its node catalog entry.",
            ));
        };
        if end > *num_rows || start > end {
            return Err(Error::runtime(
                "Icebug-disk node scan range exceeds its declared row count.",
            ));
        }
        let entry = catalog
            .node_table(table)
            .ok_or_else(|| Error::runtime("Icebug-disk node table is missing from the catalog."))?;
        let expected: Vec<_> = entry
            .columns()
            .iter()
            .map(|column| (column.name().to_string(), column.logical_type().clone()))
            .collect();
        let projection = projected_columns
            .iter()
            .map(|position| {
                entry
                    .columns()
                    .get(*position)
                    .map(|column| column.name().to_string())
                    .ok_or_else(|| {
                        Error::runtime(
                            "Icebug-disk node projection references a missing catalog column.",
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let property_count = projection.len();
        let decode_projection = if projection.is_empty() {
            entry
                .columns()
                .first()
                .map(|column| vec![column.name().to_string()])
                .ok_or_else(|| Error::runtime("Icebug-disk node table has no physical columns."))?
        } else {
            projection
        };
        let reader = ParquetReader::open_projected_file_range(
            path,
            file.try_clone()?,
            &decode_projection,
            start,
            end - start,
        )?;
        validate_version(reader.file_metadata(), path)?;
        validate_schema(&reader.file_metadata().schema, &expected, path)?;
        if reader.file_metadata().num_rows != *num_rows {
            return Err(Error::runtime(format!(
                "Icebug-disk node file {} row count changed after table creation.",
                path.display()
            )));
        }
        Ok(IcebugNodeScan {
            table,
            next_offset: start,
            end,
            property_count,
            reader,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn extend_batch_into(
        &self,
        relationship: TableId,
        nodes: &[InternalId],
        direction: ExtendDir,
        output: &mut Vec<BatchNeighbor>,
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
    ) -> Result<bool> {
        let Some(source) = self.tables.get(&relationship) else {
            return Ok(false);
        };
        let entry = catalog.rel_table(relationship).ok_or_else(|| {
            Error::runtime("Icebug-disk relationship table is missing from the catalog.")
        })?;
        let pair = entry
            .pairs()
            .first()
            .ok_or_else(|| Error::runtime("Icebug-disk relationship has no endpoint pair."))?;
        for (input_pos, node) in nodes.iter().copied().enumerate() {
            control.check()?;
            if matches!(direction, ExtendDir::Forward | ExtendDir::Both)
                && node.table_id == pair.from
            {
                edges_for_node(
                    source,
                    node.offset.0,
                    true,
                    input_pos,
                    relationship,
                    pair.from,
                    pair.to,
                    output,
                    control,
                    memory,
                )?;
            }
            if matches!(direction, ExtendDir::Backward | ExtendDir::Both)
                && node.table_id == pair.to
            {
                edges_for_node(
                    source,
                    node.offset.0,
                    false,
                    input_pos,
                    relationship,
                    pair.from,
                    pair.to,
                    output,
                    control,
                    memory,
                )?;
            }
        }
        let bytes = u64::try_from(output.capacity())
            .unwrap_or(u64::MAX)
            .saturating_mul(std::mem::size_of::<BatchNeighbor>() as u64);
        let _reservation = memory.try_reserve(bytes)?;
        Ok(true)
    }

    pub fn projected_rows(
        &self,
        table: TableId,
        offsets: &[u64],
        columns: &[usize],
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
    ) -> Result<Option<Vec<DataChunk>>> {
        let Some(source) = self.tables.get(&table) else {
            return Ok(None);
        };
        let (path, file, num_rows, names, types) =
            projection_details(source, table, columns, catalog)?;
        if offsets.iter().any(|offset| *offset >= num_rows) {
            return Err(Error::runtime(
                "Icebug-disk property offset exceeds its declared row count.",
            ));
        }
        if columns.is_empty() {
            return Ok(Some(
                offsets
                    .chunks(VECTOR_CAPACITY)
                    .map(|positions| {
                        let mut chunk = DataChunk::new(&[]);
                        chunk.set_flat(positions.len());
                        chunk
                    })
                    .collect(),
            ));
        }

        let mut output = Vec::new();
        let mut current = DataChunk::new(&types);
        let mut output_position = 0usize;
        let mut copied = 0usize;
        let mut first = 0usize;
        while first < offsets.len() {
            control.check()?;
            let mut last = first + 1;
            while last < offsets.len() && offsets[last] == offsets[last - 1].saturating_add(1) {
                last += 1;
            }
            let mut reader = ParquetReader::open_projected_file_range(
                path,
                file.try_clone()?,
                &names,
                offsets[first],
                (last - first) as u64,
            )?;
            let run_start = copied;
            while let Some(chunk) = reader.next_chunk()? {
                control.check()?;
                let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
                for position in chunk.sel.iter() {
                    for (column, source) in chunk.columns.iter().enumerate() {
                        current.columns[column]
                            .set_value_owned(output_position, source.get_value(position));
                    }
                    output_position += 1;
                    copied += 1;
                    if output_position == VECTOR_CAPACITY {
                        current.set_flat(output_position);
                        output.push(current);
                        current = DataChunk::new(&types);
                        output_position = 0;
                    }
                }
            }
            if copied - run_start != last - first {
                return Err(Error::runtime(
                    "Icebug-disk property range ended before its declared row count.",
                ));
            }
            first = last;
        }
        if output_position > 0 {
            current.set_flat(output_position);
            output.push(current);
        }
        Ok(Some(output))
    }

    pub fn projected_values(
        &self,
        table: TableId,
        offset: u64,
        columns: &[usize],
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
    ) -> Result<Option<Vec<Value>>> {
        let Some(batches) =
            self.projected_rows(table, &[offset], columns, catalog, control, memory)?
        else {
            return Ok(None);
        };
        let values = columns
            .iter()
            .enumerate()
            .map(|(column, _)| {
                batches
                    .first()
                    .map(|batch| batch.columns[column].get_value(0))
                    .unwrap_or(Value::Null)
            })
            .collect();
        Ok(Some(values))
    }

    pub fn relationship_endpoints(
        &self,
        table: TableId,
        offset: u64,
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
    ) -> Result<Option<(InternalId, InternalId)>> {
        let Some(source) = self.tables.get(&table) else {
            return Ok(None);
        };
        let entry = catalog.rel_table(table).ok_or_else(|| {
            Error::runtime("Icebug-disk relationship table is missing from the catalog.")
        })?;
        let pair = entry
            .pairs()
            .first()
            .ok_or_else(|| Error::runtime("Icebug-disk relationship has no endpoint pair."))?;
        let endpoints = match source {
            PinnedIcebugSource::Node { .. } => {
                return Err(Error::runtime(
                    "Icebug-disk source kind does not match its relationship catalog entry.",
                ));
            }
            PinnedIcebugSource::RelCsr {
                indices_path,
                indices_file,
                indptr_path,
                indptr_file,
                target_column,
                num_rows,
                num_bound_nodes,
            } => {
                if offset >= *num_rows {
                    return Err(Error::runtime(
                        "Icebug-disk relationship offset exceeds its row count.",
                    ));
                }
                let mut source_offset = None;
                for candidate in 0..*num_bound_nodes {
                    let (start, end) =
                        csr_bounds(indptr_path, indptr_file, candidate, control, memory)?;
                    if start <= offset && offset < end {
                        source_offset = Some(candidate);
                        break;
                    }
                }
                let source_offset = source_offset.ok_or_else(|| {
                    Error::runtime("Icebug-disk CSR does not own a relationship row.")
                })?;
                let mut target = None;
                scan_csr_edges(
                    indices_path,
                    indices_file,
                    target_column,
                    offset,
                    offset + 1,
                    |_, value| target = Some(value),
                    control,
                    memory,
                )?;
                (
                    InternalId::new(pair.from, source_offset),
                    InternalId::new(
                        pair.to,
                        target.expect("validated one-row CSR relationship range"),
                    ),
                )
            }
            PinnedIcebugSource::RelFlat {
                path,
                file,
                source_column,
                target_column,
                num_rows,
            } => {
                if offset >= *num_rows {
                    return Err(Error::runtime(
                        "Icebug-disk relationship offset exceeds its row count.",
                    ));
                }
                let projection = [source_column.clone(), target_column.clone()];
                let mut reader = ParquetReader::open_projected_file_range(
                    path,
                    file.try_clone()?,
                    &projection,
                    offset,
                    1,
                )?;
                let chunk = reader
                    .next_chunk()?
                    .ok_or_else(|| Error::runtime("Icebug-disk relationship row is missing."))?;
                let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
                (
                    InternalId::new(
                        pair.from,
                        parquet_offset(chunk.columns[0].get_value(0), path)?,
                    ),
                    InternalId::new(
                        pair.to,
                        parquet_offset(chunk.columns[1].get_value(0), path)?,
                    ),
                )
            }
        };
        Ok(Some(endpoints))
    }

    fn validate(
        &self,
        catalog: &Catalog,
        control: QueryControl<'_>,
        memory: &MemoryTracker,
    ) -> Result<()> {
        for (&table, source) in &self.tables {
            control.check()?;
            match source {
                PinnedIcebugSource::Node {
                    path,
                    file,
                    num_rows,
                } => {
                    let entry = catalog.node_table(table).ok_or_else(|| {
                        Error::runtime("Icebug-disk node table is missing from the catalog.")
                    })?;
                    let expected: Vec<_> = entry
                        .columns()
                        .iter()
                        .map(|column| (column.name().to_string(), column.logical_type().clone()))
                        .collect();
                    let projection = expected
                        .first()
                        .map(|(name, _)| vec![name.clone()])
                        .ok_or_else(|| {
                            Error::runtime("Icebug-disk node table has no physical columns.")
                        })?;
                    let reader =
                        ParquetReader::open_projected_file(path, file.try_clone()?, &projection)?;
                    validate_version(reader.file_metadata(), path)?;
                    validate_schema(&reader.file_metadata().schema, &expected, path)?;
                    validate_row_count(reader.file_metadata().num_rows, *num_rows, path)?;
                }
                PinnedIcebugSource::RelCsr {
                    indices_path,
                    indices_file,
                    indptr_path,
                    indptr_file,
                    target_column,
                    num_rows,
                    num_bound_nodes,
                } => {
                    let entry = catalog.rel_table(table).ok_or_else(|| {
                        Error::runtime(
                            "Icebug-disk relationship table is missing from the catalog.",
                        )
                    })?;
                    let pair = entry.pairs().first().ok_or_else(|| {
                        Error::runtime("Icebug-disk relationship has no endpoint pair.")
                    })?;
                    validate_csr_source(
                        entry,
                        indices_path,
                        indices_file,
                        indptr_path,
                        indptr_file,
                        target_column,
                        *num_rows,
                        *num_bound_nodes,
                        external_node_count(catalog, pair.to)?,
                        control,
                        memory,
                    )?;
                }
                PinnedIcebugSource::RelFlat {
                    path,
                    file,
                    source_column,
                    target_column,
                    num_rows,
                } => {
                    let entry = catalog.rel_table(table).ok_or_else(|| {
                        Error::runtime(
                            "Icebug-disk relationship table is missing from the catalog.",
                        )
                    })?;
                    let pair = entry.pairs().first().ok_or_else(|| {
                        Error::runtime("Icebug-disk relationship has no endpoint pair.")
                    })?;
                    validate_flat_source(
                        entry,
                        path,
                        file,
                        source_column,
                        target_column,
                        *num_rows,
                        external_node_count(catalog, pair.from)?,
                        external_node_count(catalog, pair.to)?,
                        control,
                        memory,
                    )?;
                }
            }
        }
        Ok(())
    }
}

type ProjectionDetails<'a> = (&'a Path, &'a File, u64, Vec<String>, Vec<LogicalType>);

fn projection_details<'a>(
    source: &'a PinnedIcebugSource,
    table: TableId,
    columns: &[usize],
    catalog: &'a Catalog,
) -> Result<ProjectionDetails<'a>> {
    let (path, file, rows, catalog_columns, missing) = match source {
        PinnedIcebugSource::Node {
            path,
            file,
            num_rows,
        } => (
            path.as_path(),
            file,
            *num_rows,
            catalog
                .node_table(table)
                .map(|entry| entry.columns())
                .ok_or_else(|| {
                    Error::runtime("Icebug-disk node table is missing from the catalog.")
                })?,
            "Icebug-disk node property references a missing column.",
        ),
        PinnedIcebugSource::RelCsr {
            indices_path,
            indices_file,
            num_rows,
            ..
        } => (
            indices_path.as_path(),
            indices_file,
            *num_rows,
            catalog
                .rel_table(table)
                .map(|entry| entry.columns())
                .ok_or_else(|| {
                    Error::runtime("Icebug-disk relationship table is missing from the catalog.")
                })?,
            "Icebug-disk relationship property references a missing column.",
        ),
        PinnedIcebugSource::RelFlat {
            path,
            file,
            num_rows,
            ..
        } => (
            path.as_path(),
            file,
            *num_rows,
            catalog
                .rel_table(table)
                .map(|entry| entry.columns())
                .ok_or_else(|| {
                    Error::runtime("Icebug-disk relationship table is missing from the catalog.")
                })?,
            "Icebug-disk relationship property references a missing column.",
        ),
    };
    let selected = columns
        .iter()
        .map(|position| {
            catalog_columns
                .get(*position)
                .ok_or_else(|| Error::runtime(missing))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((
        path,
        file,
        rows,
        selected
            .iter()
            .map(|column| column.name().to_string())
            .collect(),
        selected
            .iter()
            .map(|column| column.logical_type().clone())
            .collect(),
    ))
}

fn open_source(path: &Path) -> Result<File> {
    File::open(path)
        .map_err(|error| Error::runtime(format!("Cannot open {}: {error}", path.display())))
}

fn external_node_count(catalog: &Catalog, table: TableId) -> Result<u64> {
    catalog
        .icebug_table(table)
        .and_then(|entry| entry.source())
        .map(IcebugTableSource::num_rows)
        .ok_or_else(|| {
            Error::runtime("Icebug-disk relationship endpoint has no pinned local node descriptor.")
        })
}

fn validate_row_count(actual: u64, expected: u64, path: &Path) -> Result<()> {
    if actual != expected {
        return Err(Error::runtime(format!(
            "Icebug-disk file {} row count changed after table creation.",
            path.display()
        )));
    }
    Ok(())
}

fn parquet_offset(value: Value, path: &Path) -> Result<u64> {
    let value = value.as_u128().ok_or_else(|| {
        Error::runtime(format!(
            "Icebug-disk endpoint offset in {} is not an unsigned integer.",
            path.display()
        ))
    })?;
    u64::try_from(value).map_err(|_| {
        Error::runtime(format!(
            "Icebug-disk endpoint offset in {} exceeds UINT64.",
            path.display()
        ))
    })
}

fn validate_rel_properties(table: &RelTable, fields: &[ParquetField], path: &Path) -> Result<()> {
    let expected = table
        .columns()
        .iter()
        .map(|column| (column.name().to_string(), column.logical_type().clone()))
        .collect::<Vec<_>>();
    validate_schema(&ParquetSchema::new(fields.to_vec())?, &expected, path)
}

#[allow(clippy::too_many_arguments)]
fn validate_csr_source(
    table: &RelTable,
    indices_path: &Path,
    indices_file: &File,
    indptr_path: &Path,
    indptr_file: &File,
    target_column: &str,
    num_rows: u64,
    num_bound_nodes: u64,
    target_count: u64,
    control: QueryControl<'_>,
    memory: &MemoryTracker,
) -> Result<()> {
    let projection = [target_column.to_string()];
    let mut indices =
        ParquetReader::open_projected_file(indices_path, indices_file.try_clone()?, &projection)?;
    validate_version(indices.file_metadata(), indices_path)?;
    validate_row_count(indices.file_metadata().num_rows, num_rows, indices_path)?;
    let fields = &indices.file_metadata().schema.fields;
    if fields.len() != table.columns().len() + 1
        || !fields[0].name.eq_ignore_ascii_case(target_column)
        || !matches!(fields[0].logical_type, LogicalType::Int(_))
    {
        return Err(Error::runtime(format!(
            "Icebug-disk indices file {} has an invalid endpoint column.",
            indices_path.display()
        )));
    }
    validate_rel_properties(table, &fields[1..], indices_path)?;
    let mut seen = 0u64;
    while let Some(chunk) = indices.next_chunk()? {
        control.check()?;
        let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            if parquet_offset(chunk.columns[0].get_value(position), indices_path)? >= target_count {
                return Err(Error::runtime(format!(
                    "Icebug-disk relationship endpoint in {} is out of range.",
                    indices_path.display()
                )));
            }
            seen += 1;
        }
    }
    if seen != num_rows {
        return Err(Error::runtime(format!(
            "Icebug-disk indices file {} ended before its declared row count.",
            indices_path.display()
        )));
    }

    let mut indptr =
        ParquetReader::open_projected_file(indptr_path, indptr_file.try_clone()?, &[])?;
    validate_version(indptr.file_metadata(), indptr_path)?;
    validate_row_count(
        indptr.file_metadata().num_rows,
        num_bound_nodes.saturating_add(1),
        indptr_path,
    )?;
    if indptr.file_metadata().schema.fields.len() != 1
        || !matches!(
            indptr.file_metadata().schema.fields[0].logical_type,
            LogicalType::Int(_)
        )
    {
        return Err(Error::runtime(format!(
            "Icebug-disk indptr file {} must contain one integer column.",
            indptr_path.display()
        )));
    }
    let mut seen = 0u64;
    let mut previous = 0u64;
    while let Some(chunk) = indptr.next_chunk()? {
        control.check()?;
        let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            let offset = parquet_offset(chunk.columns[0].get_value(position), indptr_path)?;
            if (seen == 0 && offset != 0) || (seen > 0 && offset < previous) {
                return Err(Error::runtime(format!(
                    "Icebug-disk indptr file {} is not monotone from zero.",
                    indptr_path.display()
                )));
            }
            previous = offset;
            seen += 1;
        }
    }
    if seen != num_bound_nodes.saturating_add(1) || previous != num_rows {
        return Err(Error::runtime(format!(
            "Icebug-disk CSR files for {} disagree on relationship count.",
            table.name()
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_flat_source(
    table: &RelTable,
    path: &Path,
    file: &File,
    source_column: &str,
    target_column: &str,
    num_rows: u64,
    source_count: u64,
    target_count: u64,
    control: QueryControl<'_>,
    memory: &MemoryTracker,
) -> Result<()> {
    let projection = [source_column.to_string(), target_column.to_string()];
    let mut reader = ParquetReader::open_projected_file(path, file.try_clone()?, &projection)?;
    validate_version(reader.file_metadata(), path)?;
    validate_row_count(reader.file_metadata().num_rows, num_rows, path)?;
    let fields = &reader.file_metadata().schema.fields;
    if fields.len() != table.columns().len() + 2
        || !fields[0].name.eq_ignore_ascii_case(source_column)
        || !fields[1].name.eq_ignore_ascii_case(target_column)
        || !matches!(fields[0].logical_type, LogicalType::Int(_))
        || !matches!(fields[1].logical_type, LogicalType::Int(_))
    {
        return Err(Error::runtime(format!(
            "Icebug-disk flat relationship file {} has invalid endpoint columns.",
            path.display()
        )));
    }
    validate_rel_properties(table, &fields[2..], path)?;
    let mut seen = 0u64;
    while let Some(chunk) = reader.next_chunk()? {
        control.check()?;
        let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            if parquet_offset(chunk.columns[0].get_value(position), path)? >= source_count
                || parquet_offset(chunk.columns[1].get_value(position), path)? >= target_count
            {
                return Err(Error::runtime(format!(
                    "Icebug-disk relationship endpoint in {} is out of range.",
                    path.display()
                )));
            }
            seen += 1;
        }
    }
    if seen != num_rows {
        return Err(Error::runtime(format!(
            "Icebug-disk relationship file {} ended before its declared row count.",
            path.display()
        )));
    }
    Ok(())
}

fn csr_bounds(
    path: &Path,
    file: &File,
    source_offset: u64,
    control: QueryControl<'_>,
    memory: &MemoryTracker,
) -> Result<(u64, u64)> {
    let mut reader =
        ParquetReader::open_projected_file_range(path, file.try_clone()?, &[], source_offset, 2)?;
    let mut bounds = [0u64; 2];
    let mut count = 0usize;
    while let Some(chunk) = reader.next_chunk()? {
        control.check()?;
        let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            if count >= bounds.len() {
                return Err(Error::runtime(
                    "Icebug-disk CSR indptr range returned too many offsets.",
                ));
            }
            bounds[count] = parquet_offset(chunk.columns[0].get_value(position), path)?;
            count += 1;
        }
    }
    if count != 2 {
        return Err(Error::runtime(
            "Icebug-disk CSR indptr range ended before two offsets.",
        ));
    }
    Ok((bounds[0], bounds[1]))
}

#[allow(clippy::too_many_arguments)]
fn scan_csr_edges(
    path: &Path,
    file: &File,
    target_column: &str,
    start: u64,
    end: u64,
    mut visit: impl FnMut(u64, u64),
    control: QueryControl<'_>,
    memory: &MemoryTracker,
) -> Result<()> {
    let projection = [target_column.to_string()];
    let mut reader = ParquetReader::open_projected_file_range(
        path,
        file.try_clone()?,
        &projection,
        start,
        end.saturating_sub(start),
    )?;
    let mut physical = start;
    while let Some(chunk) = reader.next_chunk()? {
        control.check()?;
        let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
        for position in chunk.sel.iter() {
            visit(
                physical,
                parquet_offset(chunk.columns[0].get_value(position), path)?,
            );
            physical += 1;
        }
    }
    if physical != end {
        return Err(Error::runtime(
            "Icebug-disk relationship range ended before its declared row count.",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn edges_for_node(
    source: &PinnedIcebugSource,
    bound_offset: u64,
    forward: bool,
    input_pos: usize,
    relationship: TableId,
    from_table: TableId,
    to_table: TableId,
    output: &mut Vec<BatchNeighbor>,
    control: QueryControl<'_>,
    memory: &MemoryTracker,
) -> Result<()> {
    match source {
        PinnedIcebugSource::Node { .. } => Err(Error::runtime(
            "Icebug-disk source kind does not match its relationship catalog entry.",
        )),
        PinnedIcebugSource::RelCsr {
            indices_path,
            indices_file,
            indptr_path,
            indptr_file,
            target_column,
            num_bound_nodes,
            ..
        } => {
            if forward {
                let (start, end) =
                    csr_bounds(indptr_path, indptr_file, bound_offset, control, memory)?;
                scan_csr_edges(
                    indices_path,
                    indices_file,
                    target_column,
                    start,
                    end,
                    |physical, target| {
                        output.push(BatchNeighbor {
                            input_pos,
                            nbr: InternalId::new(to_table, target),
                            rel: InternalId::new(relationship, physical),
                        });
                    },
                    control,
                    memory,
                )?;
            } else {
                for source_offset in 0..*num_bound_nodes {
                    control.check()?;
                    let (start, end) =
                        csr_bounds(indptr_path, indptr_file, source_offset, control, memory)?;
                    scan_csr_edges(
                        indices_path,
                        indices_file,
                        target_column,
                        start,
                        end,
                        |physical, target| {
                            if target == bound_offset {
                                output.push(BatchNeighbor {
                                    input_pos,
                                    nbr: InternalId::new(from_table, source_offset),
                                    rel: InternalId::new(relationship, physical),
                                });
                            }
                        },
                        control,
                        memory,
                    )?;
                }
            }
            Ok(())
        }
        PinnedIcebugSource::RelFlat {
            path,
            file,
            source_column,
            target_column,
            num_rows,
        } => {
            let projection = [source_column.clone(), target_column.clone()];
            let mut reader =
                ParquetReader::open_projected_file(path, file.try_clone()?, &projection)?;
            let mut physical = 0u64;
            while let Some(chunk) = reader.next_chunk()? {
                control.check()?;
                let _reservation = memory.try_reserve(chunk.allocated_bytes())?;
                for position in chunk.sel.iter() {
                    let source = parquet_offset(chunk.columns[0].get_value(position), path)?;
                    let target = parquet_offset(chunk.columns[1].get_value(position), path)?;
                    if if forward {
                        source == bound_offset
                    } else {
                        target == bound_offset
                    } {
                        output.push(BatchNeighbor {
                            input_pos,
                            nbr: if forward {
                                InternalId::new(to_table, target)
                            } else {
                                InternalId::new(from_table, source)
                            },
                            rel: InternalId::new(relationship, physical),
                        });
                    }
                    physical += 1;
                }
            }
            if physical != *num_rows {
                return Err(Error::runtime(
                    "Icebug-disk relationship file ended before its declared row count.",
                ));
            }
            Ok(())
        }
    }
}
