//! Validated descriptors for local read-only `icebug-disk` tables.
//!
//! DDL inspects Parquet metadata and streams the compact CSR offset column for
//! validation. Node/property and relationship/index batches are decoded only by
//! the processor while a query is running.

use koko_catalog::IcebugTableSource;
use koko_common::{Error, LogicalType, Result, Value};
use std::path::{Path, PathBuf};

use crate::parquet::{ParquetFileMetadata, ParquetReader, ParquetSchema, inspect};

const VERSION_METADATA_KEY: &str = "icebug_disk_version";
const CURRENT_VERSION: &str = "v1";

fn table_file(root: &str, prefix: &str, table: &str) -> PathBuf {
    Path::new(root).join(format!("{prefix}_{}.parquet", table.to_ascii_lowercase()))
}

pub fn is_remote(root: &str) -> bool {
    root.contains("://")
}

pub fn deferred_node_error(root: &str, table: &str) -> String {
    format!(
        "Cannot open {}.",
        table_file(root, "nodes", table).display()
    )
}

pub fn deferred_rel_error(root: &str, table: &str) -> String {
    let path = if root.to_ascii_lowercase().ends_with(".parquet") {
        PathBuf::from(root)
    } else {
        table_file(root, "indices", table)
    };
    format!("Cannot open {}.", path.display())
}

fn inspect_file(path: &Path) -> Result<ParquetFileMetadata> {
    inspect(path)
        .map_err(|error| Error::runtime(format!("Cannot open {}: {error}", path.display())))
}

pub fn validate_version(metadata: &ParquetFileMetadata, path: &Path) -> Result<()> {
    let Some(version) = metadata.key_value_metadata.get(VERSION_METADATA_KEY) else {
        // Matches the C++ engine: legacy files without the key remain readable.
        return Ok(());
    };
    if !version.eq_ignore_ascii_case(CURRENT_VERSION) {
        return Err(Error::runtime(format!(
            "{}: current koko version does not support icebug_disk_version: {version}",
            path.display()
        )));
    }
    Ok(())
}

pub fn validate_schema(
    actual: &ParquetSchema,
    columns: &[(String, LogicalType)],
    path: &Path,
) -> Result<()> {
    if actual.fields.len() != columns.len() {
        return Err(Error::runtime(format!(
            "Icebug-disk file {} has {} columns, expected {}.",
            path.display(),
            actual.fields.len(),
            columns.len()
        )));
    }
    for (field, (name, logical_type)) in actual.fields.iter().zip(columns) {
        if !field.name.eq_ignore_ascii_case(name) || field.logical_type != *logical_type {
            return Err(Error::runtime(format!(
                "Icebug-disk file {} column {} has type {}, expected {} {}.",
                path.display(),
                field.name,
                field.logical_type,
                name,
                logical_type
            )));
        }
    }
    Ok(())
}

fn integer(value: Value, path: &Path) -> Result<u64> {
    let value = value.as_u128().ok_or_else(|| {
        Error::runtime(format!(
            "Icebug-disk offset in {} is not an unsigned integer.",
            path.display()
        ))
    })?;
    u64::try_from(value).map_err(|_| {
        Error::runtime(format!(
            "Icebug-disk offset in {} exceeds UINT64.",
            path.display()
        ))
    })
}

fn validate_indptr(path: &Path, expected_nodes: u64, expected_rels: u64) -> Result<()> {
    let metadata = inspect_file(path)?;
    validate_version(&metadata, path)?;
    if metadata.schema.fields.len() != 1
        || !matches!(metadata.schema.fields[0].logical_type, LogicalType::Int(_))
    {
        return Err(Error::runtime(format!(
            "Icebug-disk indptr file {} must contain one integer column.",
            path.display()
        )));
    }
    if metadata.num_rows != expected_nodes.saturating_add(1) {
        return Err(Error::runtime(format!(
            "Icebug-disk indptr file {} has {} rows, expected {} for its source node table.",
            path.display(),
            metadata.num_rows,
            expected_nodes.saturating_add(1)
        )));
    }
    let mut reader = ParquetReader::open(path)
        .map_err(|error| Error::runtime(format!("Cannot open {}: {error}", path.display())))?;
    let mut seen = 0u64;
    let mut previous = 0u64;
    while let Some(chunk) = reader.next_chunk()? {
        for position in chunk.sel.iter() {
            let offset = integer(chunk.columns[0].get_value(position), path)?;
            if (seen == 0 && offset != 0) || (seen > 0 && offset < previous) {
                return Err(Error::runtime(format!(
                    "Icebug-disk indptr file {} is not a monotone CSR offset vector starting at zero.",
                    path.display()
                )));
            }
            previous = offset;
            seen += 1;
        }
    }
    if seen != metadata.num_rows || previous != expected_rels {
        return Err(Error::runtime(format!(
            "Icebug-disk CSR files {} and its indices file disagree on relationship count.",
            path.display()
        )));
    }
    Ok(())
}

pub fn inspect_node_table(
    table_name: &str,
    columns: &[(String, LogicalType)],
    root: &str,
) -> Result<IcebugTableSource> {
    let path = table_file(root, "nodes", table_name);
    let metadata = inspect_file(&path)?;
    validate_version(&metadata, &path)?;
    validate_schema(&metadata.schema, columns, &path)?;
    Ok(IcebugTableSource::Node {
        path,
        num_rows: metadata.num_rows,
    })
}

pub fn inspect_rel_table(
    table_name: &str,
    columns: &[(String, LogicalType)],
    root: &str,
    from_node_count: u64,
) -> Result<IcebugTableSource> {
    if root.to_ascii_lowercase().ends_with(".parquet") {
        let path = PathBuf::from(root);
        let metadata = inspect_file(&path)?;
        validate_version(&metadata, &path)?;
        if metadata.schema.fields.len() != columns.len() + 2
            || !matches!(metadata.schema.fields[0].logical_type, LogicalType::Int(_))
            || !matches!(metadata.schema.fields[1].logical_type, LogicalType::Int(_))
        {
            return Err(Error::runtime(format!(
                "Icebug-disk flat relationship file {} must contain source and target offsets followed by {} relationship properties.",
                path.display(),
                columns.len()
            )));
        }
        validate_schema(
            &ParquetSchema::new(metadata.schema.fields[2..].to_vec())?,
            columns,
            &path,
        )?;
        return Ok(IcebugTableSource::RelFlat {
            source_column: metadata.schema.fields[0].name.clone(),
            target_column: metadata.schema.fields[1].name.clone(),
            path,
            num_rows: metadata.num_rows,
        });
    }

    let indptr_path = table_file(root, "indptr", table_name);
    let indices_path = table_file(root, "indices", table_name);
    let metadata = inspect_file(&indices_path)?;
    validate_version(&metadata, &indices_path)?;
    if metadata.schema.fields.len() != columns.len() + 1
        || !matches!(metadata.schema.fields[0].logical_type, LogicalType::Int(_))
    {
        return Err(Error::runtime(format!(
            "Icebug-disk indices file {} must contain a target offset followed by {} relationship properties.",
            indices_path.display(),
            columns.len()
        )));
    }
    validate_schema(
        &ParquetSchema::new(metadata.schema.fields[1..].to_vec())?,
        columns,
        &indices_path,
    )?;
    validate_indptr(&indptr_path, from_node_count, metadata.num_rows)?;
    Ok(IcebugTableSource::RelCsr {
        target_column: metadata.schema.fields[0].name.clone(),
        indices_path,
        indptr_path,
        num_rows: metadata.num_rows,
        num_bound_nodes: from_node_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn rejects_unsupported_icebug_disk_versions() {
        let metadata = ParquetFileMetadata {
            schema: ParquetSchema::new(Vec::new()).unwrap(),
            num_rows: 0,
            num_row_groups: 0,
            key_value_metadata: HashMap::from([(
                VERSION_METADATA_KEY.to_string(),
                "v999".to_string(),
            )]),
        };
        let error = validate_version(&metadata, Path::new("nodes_person.parquet"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("does not support icebug_disk_version: v999"),
            "{error}"
        );
    }
}
