//! Bounded, typed readers for NumPy `.npy` files.
//!
//! One file is one logical output column. Rank-one arrays produce scalar values;
//! higher-rank arrays produce fixed-size `ARRAY` values by flattening every
//! row's trailing dimensions in C order. Inspection and target validation happen
//! before a [`NpyBatchReader`] is opened, so callers can preflight every source
//! before mutating storage.

use koko_common::{DataChunk, Error, IntKind, LogicalType, Result, VECTOR_CAPACITY, Value};
use npyz::{DType, Endianness, NpyHeader, Order, TypeChar};
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const NPY_MAGIC: &[u8; 6] = b"\x93NUMPY";
const NPY_V1_PREFIX_LEN: usize = 10;

/// Scalar dtypes supported by Koko's NPY reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpyDType {
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
}

impl NpyDType {
    /// The scalar Koko type represented by this dtype.
    pub fn logical_type(self) -> LogicalType {
        match self {
            Self::Int16 => LogicalType::Int(IntKind::I16),
            Self::Int32 => LogicalType::Int(IntKind::I32),
            Self::Int64 => LogicalType::Int64,
            Self::Float32 => LogicalType::Float,
            Self::Float64 => LogicalType::Double,
        }
    }

    fn item_size(self) -> usize {
        match self {
            Self::Int16 => size_of::<i16>(),
            Self::Int32 | Self::Float32 => size_of::<i32>(),
            Self::Int64 | Self::Float64 => size_of::<i64>(),
        }
    }
}

/// Preflight information for one NPY file/output column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpyColumnMetadata {
    /// Stable LOAD name (`column0`, `column1`, ...), in source order.
    pub name: String,
    /// Original resolved path, retained for execution and diagnostics.
    pub path: PathBuf,
    pub dtype: NpyDType,
    /// Original NumPy shape.
    pub shape: Vec<u64>,
    /// First-dimension row count.
    pub row_count: u64,
    /// Product of all dimensions after the first.
    pub elements_per_row: u64,
    /// Scalar for rank one; fixed ARRAY for rank two and above.
    pub logical_type: LogicalType,
    data_offset: u64,
    byte_order: ByteOrder,
}

/// Fully validated metadata for one-file LOAD or one-file-per-column BY COLUMN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpyMetadata {
    pub columns: Vec<NpyColumnMetadata>,
    pub row_count: u64,
}

impl NpyMetadata {
    pub fn logical_types(&self) -> Vec<LogicalType> {
        self.columns
            .iter()
            .map(|column| column.logical_type.clone())
            .collect()
    }

    pub fn column_names(&self) -> Vec<String> {
        self.columns
            .iter()
            .map(|column| column.name.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn from_endianness(endianness: Endianness) -> Self {
        match endianness {
            Endianness::Little => Self::Little,
            Endianness::Big => Self::Big,
            Endianness::Irrelevant => {
                if cfg!(target_endian = "little") {
                    Self::Little
                } else {
                    Self::Big
                }
            }
        }
    }
}

/// Inspect one file as LOAD column `column0`.
///
/// This validates the magic, v1.0 header, C-order layout, native endian marker,
/// supported scalar dtype, non-empty first dimension, shape arithmetic, and
/// physical data extent.
pub fn inspect_npy(path: &Path) -> Result<NpyColumnMetadata> {
    inspect_column(path, 0)
}

/// Preflight one-file LOAD (`expected_types == None`) or one-file-per-column BY
/// COLUMN (`Some(target_types)`). File count is checked before any file is
/// opened. Every file is then structurally inspected, row counts are compared,
/// and only then are target dtype/shape checks performed.
pub fn preflight_npy(
    paths: &[PathBuf],
    expected_types: Option<&[LogicalType]>,
) -> Result<NpyMetadata> {
    match expected_types {
        Some(expected) if paths.len() != expected.len() => {
            return Err(Error::binder(format!(
                "Number of columns mismatch. Expected {} but got {}.",
                expected.len(),
                paths.len()
            )));
        }
        None if paths.len() != 1 => {
            return Err(Error::binder(format!(
                "Number of columns mismatch. Expected 1 but got {}.",
                paths.len()
            )));
        }
        _ => {}
    }
    if paths.is_empty() {
        return Err(Error::copy("At least one NPY file is required."));
    }

    let columns = paths
        .iter()
        .enumerate()
        .map(|(index, path)| inspect_column(path, index))
        .collect::<Result<Vec<_>>>()?;

    let row_count = columns.first().map_or(0, |column| column.row_count);
    for column in columns.iter().skip(1) {
        if column.row_count != row_count {
            return Err(Error::copy(
                "Number of rows in npy files is not equal to each other.",
            ));
        }
    }

    if let Some(expected) = expected_types {
        for (column, target) in columns.iter().zip(expected) {
            validate_target(column, target)?;
        }
    }

    Ok(NpyMetadata { columns, row_count })
}

fn inspect_column(path: &Path, index: usize) -> Result<NpyColumnMetadata> {
    let mut file = File::open(path).map_err(|error| {
        Error::copy(format!(
            "Failed to open NPY file {}: {error}.",
            path.display()
        ))
    })?;
    let file_len = file
        .metadata()
        .map_err(|error| invalid_file(path, error))?
        .len();

    let mut prefix = [0u8; NPY_V1_PREFIX_LEN];
    file.read_exact(&mut prefix)
        .map_err(|error| invalid_file(path, error))?;
    if &prefix[..NPY_MAGIC.len()] != NPY_MAGIC {
        return Err(Error::copy("Invalid NPY file"));
    }
    if prefix[6] != 1 || prefix[7] != 0 {
        return Err(Error::copy("Unsupported NPY file version."));
    }

    let header_len = usize::from(u16::from_le_bytes([prefix[8], prefix[9]]));
    let data_offset = NPY_V1_PREFIX_LEN
        .checked_add(header_len)
        .ok_or_else(|| invalid_file(path, "header length overflow"))?;
    let mut encoded_header = vec![0u8; data_offset];
    encoded_header[..NPY_V1_PREFIX_LEN].copy_from_slice(&prefix);
    file.read_exact(&mut encoded_header[NPY_V1_PREFIX_LEN..])
        .map_err(|error| invalid_file(path, error))?;
    normalize_native_endian_markers(&mut encoded_header[NPY_V1_PREFIX_LEN..]);

    let header =
        std::panic::catch_unwind(move || NpyHeader::from_reader(Cursor::new(encoded_header)))
            .map_err(|_| invalid_file(path, "header shape arithmetic overflowed"))?
            .map_err(|error| invalid_file(path, error))?;

    if header.order() == Order::Fortran {
        return Err(Error::copy(
            "Fortran-order NPY files are not currently supported.",
        ));
    }

    let (dtype, byte_order) = parse_dtype(path, header.dtype())?;
    let shape = header.shape().to_vec();
    let Some(&row_count) = shape.first() else {
        return Err(invalid_file(path, "array must have at least one dimension"));
    };
    if row_count == 0 {
        return Err(Error::copy(format!(
            "Number of rows in npy file {} is 0.",
            path.display()
        )));
    }

    let elements_per_row = shape[1..]
        .iter()
        .try_fold(1u64, |product, dimension| product.checked_mul(*dimension));
    let elements_per_row =
        elements_per_row.ok_or_else(|| invalid_file(path, "shape is too large"))?;
    usize::try_from(elements_per_row).map_err(|_| invalid_file(path, "row width is too large"))?;
    let total_elements = row_count
        .checked_mul(elements_per_row)
        .ok_or_else(|| invalid_file(path, "shape is too large"))?;
    let item_size =
        u64::try_from(dtype.item_size()).map_err(|_| invalid_file(path, "dtype is too large"))?;
    let data_bytes = total_elements
        .checked_mul(item_size)
        .ok_or_else(|| invalid_file(path, "data size is too large"))?;
    let data_offset =
        u64::try_from(data_offset).map_err(|_| invalid_file(path, "data offset is too large"))?;
    let required_len = data_offset
        .checked_add(data_bytes)
        .ok_or_else(|| invalid_file(path, "data size is too large"))?;
    if file_len < required_len {
        return Err(invalid_file(
            path,
            format!("file is truncated (expected at least {required_len} bytes, got {file_len})"),
        ));
    }

    let scalar_type = dtype.logical_type();
    let logical_type = if shape.len() == 1 {
        scalar_type
    } else {
        LogicalType::Array(Box::new(scalar_type), elements_per_row)
    };

    Ok(NpyColumnMetadata {
        name: format!("column{index}"),
        path: path.to_path_buf(),
        dtype,
        shape,
        row_count,
        elements_per_row,
        logical_type,
        data_offset,
        byte_order,
    })
}

fn normalize_native_endian_markers(header: &mut [u8]) {
    let native = if cfg!(target_endian = "little") {
        b'<'
    } else {
        b'>'
    };
    for index in 1..header.len().saturating_sub(1) {
        if matches!(header[index - 1], b'\'' | b'"')
            && header[index] == b'='
            && matches!(header[index + 1], b'i' | b'u' | b'f' | b'c' | b'm' | b'M')
        {
            header[index] = native;
        }
    }
}

fn parse_dtype(path: &Path, dtype: DType) -> Result<(NpyDType, ByteOrder)> {
    let DType::Plain(type_string) = dtype else {
        return Err(Error::copy(format!(
            "Unsupported data type: {}",
            dtype.descr()
        )));
    };

    let endianness = type_string.endianness();
    if !matches!(endianness, Endianness::Irrelevant) && endianness != Endianness::of_machine() {
        return Err(Error::copy(
            "The endianness of the file does not match the machine's endianness.",
        ));
    }

    let parsed = match (type_string.type_char(), type_string.size_field()) {
        (TypeChar::Int, 2) => NpyDType::Int16,
        (TypeChar::Int, 4) => NpyDType::Int32,
        (TypeChar::Int, 8) => NpyDType::Int64,
        (TypeChar::Float, 4) => NpyDType::Float32,
        (TypeChar::Float, 8) => NpyDType::Float64,
        (kind, size) => {
            return Err(Error::copy(format!(
                "Unsupported data type: {}{size}",
                kind.to_str()
            )));
        }
    };

    let item_size = parsed.item_size();
    if type_string.num_bytes() != Some(item_size) {
        return Err(invalid_file(path, "dtype has an invalid item size"));
    }
    Ok((parsed, ByteOrder::from_endianness(endianness)))
}

fn validate_target(column: &NpyColumnMetadata, target: &LogicalType) -> Result<()> {
    if column.shape.len() == 1 {
        if target == &column.dtype.logical_type() {
            return Ok(());
        }
        return Err(type_mismatch(column));
    }

    let LogicalType::Array(child, width) = target else {
        return Err(Error::copy(format!(
            "Cannot copy a vector property in npy file {} to a scalar property.",
            column.path.display()
        )));
    };
    if child.as_ref() != &column.dtype.logical_type() {
        return Err(type_mismatch(column));
    }
    if *width != column.elements_per_row {
        return Err(Error::copy(format!(
            "The shape of {} does not match {}.",
            column.path.display(),
            target
        )));
    }
    Ok(())
}

fn type_mismatch(column: &NpyColumnMetadata) -> Error {
    Error::copy(format!(
        "The type of npy file {} does not match the expected type.",
        column.path.display()
    ))
}

fn invalid_file(path: &Path, detail: impl std::fmt::Display) -> Error {
    Error::copy(format!("Invalid NPY file {}: {detail}.", path.display()))
}

struct ColumnReader {
    metadata: NpyColumnMetadata,
    input: BufReader<File>,
}

impl ColumnReader {
    fn open(metadata: NpyColumnMetadata) -> Result<Self> {
        let file = File::open(&metadata.path).map_err(|error| {
            Error::copy(format!(
                "Failed to open NPY file {}: {error}.",
                metadata.path.display()
            ))
        })?;
        let mut input = BufReader::new(file);
        input
            .seek(SeekFrom::Start(metadata.data_offset))
            .map_err(|error| read_error(&metadata.path, error))?;
        Ok(Self { metadata, input })
    }

    fn read_value(&mut self) -> Result<Value> {
        match self.metadata.dtype {
            NpyDType::Int16 => {
                let bytes = self.read_array::<2>()?;
                let value = match self.metadata.byte_order {
                    ByteOrder::Little => i16::from_le_bytes(bytes),
                    ByteOrder::Big => i16::from_be_bytes(bytes),
                };
                Ok(Value::IntX {
                    value: i128::from(value),
                    kind: IntKind::I16,
                })
            }
            NpyDType::Int32 => {
                let bytes = self.read_array::<4>()?;
                let value = match self.metadata.byte_order {
                    ByteOrder::Little => i32::from_le_bytes(bytes),
                    ByteOrder::Big => i32::from_be_bytes(bytes),
                };
                Ok(Value::IntX {
                    value: i128::from(value),
                    kind: IntKind::I32,
                })
            }
            NpyDType::Int64 => {
                let bytes = self.read_array::<8>()?;
                let value = match self.metadata.byte_order {
                    ByteOrder::Little => i64::from_le_bytes(bytes),
                    ByteOrder::Big => i64::from_be_bytes(bytes),
                };
                Ok(Value::Int64(value))
            }
            NpyDType::Float32 => {
                let bytes = self.read_array::<4>()?;
                let bits = match self.metadata.byte_order {
                    ByteOrder::Little => u32::from_le_bytes(bytes),
                    ByteOrder::Big => u32::from_be_bytes(bytes),
                };
                Ok(Value::Float(f32::from_bits(bits)))
            }
            NpyDType::Float64 => {
                let bytes = self.read_array::<8>()?;
                let bits = match self.metadata.byte_order {
                    ByteOrder::Little => u64::from_le_bytes(bytes),
                    ByteOrder::Big => u64::from_be_bytes(bytes),
                };
                Ok(Value::Double(f64::from_bits(bits)))
            }
        }
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut bytes = [0u8; N];
        self.input
            .read_exact(&mut bytes)
            .map_err(|error| read_error(&self.metadata.path, error))?;
        Ok(bytes)
    }
}

fn read_error(path: &Path, error: io::Error) -> Error {
    Error::copy(format!(
        "Failed to read NPY file {}: {error}.",
        path.display()
    ))
}

/// Streaming DataChunk iterator opened from already validated metadata.
///
/// At most [`VECTOR_CAPACITY`] rows are materialized at a time. Primitive
/// conversion is column-wise and the reader never constructs a full row matrix.
pub struct NpyBatchReader {
    metadata: NpyMetadata,
    columns: Vec<ColumnReader>,
    logical_types: Vec<LogicalType>,
    next_row: u64,
    failed: bool,
}

impl NpyBatchReader {
    /// Open all files from one completed preflight result.
    pub fn from_metadata(metadata: NpyMetadata) -> Result<Self> {
        let logical_types = metadata.logical_types();
        let columns = metadata
            .columns
            .iter()
            .cloned()
            .map(ColumnReader::open)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            metadata,
            columns,
            logical_types,
            next_row: 0,
            failed: false,
        })
    }

    pub fn metadata(&self) -> &NpyMetadata {
        &self.metadata
    }

    fn read_chunk(&mut self, rows: usize) -> Result<DataChunk> {
        let mut chunk = DataChunk::new(&self.logical_types);
        for (column_index, reader) in self.columns.iter_mut().enumerate() {
            let is_array = reader.metadata.shape.len() > 1;
            let width = usize::try_from(reader.metadata.elements_per_row)
                .map_err(|_| invalid_file(&reader.metadata.path, "row width is too large"))?;
            for row in 0..rows {
                let value = if is_array {
                    let mut values = Vec::with_capacity(width);
                    for _ in 0..width {
                        values.push(reader.read_value()?);
                    }
                    Value::List(values)
                } else {
                    reader.read_value()?
                };
                chunk.columns[column_index].set_value_owned(row, value);
            }
        }
        chunk.set_flat(rows);
        Ok(chunk)
    }
}

impl Iterator for NpyBatchReader {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.next_row >= self.metadata.row_count {
            return None;
        }
        let remaining = self.metadata.row_count - self.next_row;
        let capacity = u64::try_from(VECTOR_CAPACITY).expect("vector capacity fits u64");
        let rows = usize::try_from(remaining.min(capacity)).expect("bounded batch size");
        match self.read_chunk(rows) {
            Ok(chunk) => {
                self.next_row += u64::try_from(rows).expect("batch row count fits u64");
                Some(Ok(chunk))
            }
            Err(error) => {
                self.failed = true;
                Some(Err(error))
            }
        }
    }
}

impl std::iter::FusedIterator for NpyBatchReader {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn dataset_dir() -> PathBuf {
        if let Some(path) = std::env::var_os("KOKO_DATASET_DIR") {
            return PathBuf::from(path);
        }
        let workspace_parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("workspace has a parent directory");
        let mut candidates = std::fs::read_dir(workspace_parent)
            .expect("workspace parent is readable")
            .filter_map(|entry| {
                let dataset = entry.ok()?.path().join("dataset");
                dataset
                    .join("npy-2d/id_int64.npy")
                    .is_file()
                    .then_some(dataset)
            })
            .collect::<Vec<_>>();
        candidates.sort();
        candidates.into_iter().next().expect(
            "set KOKO_DATASET_DIR to run tests that use the historical external NPY fixtures",
        )
    }

    fn files(family: &str, names: &[&str]) -> Vec<PathBuf> {
        let root = dataset_dir().join(family);
        assert!(
            root.is_dir(),
            "missing NPY dataset family {}",
            root.display()
        );
        names.iter().map(|name| root.join(name)).collect()
    }

    fn array(child: LogicalType, width: u64) -> LogicalType {
        LogicalType::Array(Box::new(child), width)
    }

    fn read_all(metadata: NpyMetadata) -> Vec<DataChunk> {
        NpyBatchReader::from_metadata(metadata)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn reads_one_dimensional_dtype_family_as_scalars() {
        let paths = files(
            "npy-1d",
            &[
                "one_dim_int64.npy",
                "one_dim_int32.npy",
                "one_dim_int16.npy",
                "one_dim_double.npy",
                "one_dim_float.npy",
            ],
        );
        let expected = [
            LogicalType::Int64,
            LogicalType::Int(IntKind::I32),
            LogicalType::Int(IntKind::I16),
            LogicalType::Double,
            LogicalType::Float,
        ];
        let metadata = preflight_npy(&paths, Some(&expected)).unwrap();
        assert_eq!(
            metadata.column_names(),
            ["column0", "column1", "column2", "column3", "column4"]
        );
        assert_eq!(metadata.row_count, 3);
        let chunks = read_all(metadata);
        assert_eq!(chunks.len(), 1);
        let chunk = &chunks[0];
        assert_eq!(chunk.size(), 3);
        assert_eq!(chunk.columns[0].get_value(2), Value::Int64(3));
        assert_eq!(
            chunk.columns[1].get_value(1),
            Value::IntX {
                value: 2,
                kind: IntKind::I32
            }
        );
        assert_eq!(chunk.columns[3].get_value(0), Value::Double(1.0));
        assert_eq!(chunk.columns[4].get_value(2), Value::Float(3.0));
    }

    #[test]
    fn reads_two_dimensional_dtype_family_as_fixed_arrays() {
        let paths = files(
            "npy-2d",
            &[
                "id_int64.npy",
                "two_dim_int64.npy",
                "two_dim_int32.npy",
                "two_dim_int16.npy",
                "two_dim_double.npy",
                "two_dim_float.npy",
            ],
        );
        let expected = [
            LogicalType::Int64,
            array(LogicalType::Int64, 3),
            array(LogicalType::Int(IntKind::I32), 3),
            array(LogicalType::Int(IntKind::I16), 3),
            array(LogicalType::Double, 3),
            array(LogicalType::Float, 3),
        ];
        let metadata = preflight_npy(&paths, Some(&expected)).unwrap();
        assert_eq!(metadata.columns[1].shape, [3, 3]);
        let chunks = read_all(metadata);
        assert_eq!(
            chunks[0].columns[1].get_value(1),
            Value::List(vec![Value::Int64(4), Value::Int64(5), Value::Int64(6)])
        );
        assert_eq!(
            chunks[0].columns[4].get_value(2),
            Value::List(vec![
                Value::Double(7.0),
                Value::Double(8.0),
                Value::Double(9.0)
            ])
        );
    }

    #[test]
    fn reads_three_dimensional_rows_flattened_in_c_order() {
        let paths = files("npy-3d", &["id_int64.npy", "three_dim_int64.npy"]);
        let expected = [LogicalType::Int64, array(LogicalType::Int64, 12)];
        let metadata = preflight_npy(&paths, Some(&expected)).unwrap();
        assert_eq!(metadata.columns[1].shape, [2, 3, 4]);
        let chunks = read_all(metadata);
        let second_row = (13..=24).map(Value::Int64).collect::<Vec<_>>();
        assert_eq!(chunks[0].columns[1].get_value(1), Value::List(second_row));
    }

    #[test]
    fn twenty_thousand_rows_are_bounded_and_deterministic() {
        let paths = files("npy-20k", &["id_int64.npy", "two_dim_float.npy"]);
        let expected = [LogicalType::Int64, array(LogicalType::Float, 10)];
        let metadata = preflight_npy(&paths, Some(&expected)).unwrap();
        let reader = NpyBatchReader::from_metadata(metadata).unwrap();
        let mut total = 0usize;
        let mut batches = 0usize;
        let mut last_id = None;
        let mut last_array = None;
        for chunk in reader {
            let chunk = chunk.unwrap();
            assert!(chunk.size() <= VECTOR_CAPACITY);
            if total == 0 {
                assert_eq!(chunk.columns[0].get_value(0), Value::Int64(0));
                assert_eq!(
                    chunk.columns[1].get_value(0),
                    Value::List((0..10).map(|value| Value::Float(value as f32)).collect())
                );
            }
            let last_row = chunk.size() - 1;
            last_id = Some(chunk.columns[0].get_value(last_row));
            last_array = Some(chunk.columns[1].get_value(last_row));
            total += chunk.size();
            batches += 1;
        }
        assert_eq!(total, 20_000);
        assert!(batches > 1);
        assert_eq!(last_id, Some(Value::Int64(19_999)));
        assert_eq!(
            last_array,
            Some(Value::List(
                (199_990..200_000)
                    .map(|value| Value::Float(value as f32))
                    .collect()
            ))
        );
    }

    #[test]
    fn load_inference_preserves_scalar_and_array_rank() {
        let scalar = files("npy-1d", &["one_dim_double.npy"]);
        let scalar = preflight_npy(&scalar, None).unwrap();
        assert_eq!(scalar.logical_types(), [LogicalType::Double]);

        let matrix = files("npy-2d", &["two_dim_int64.npy"]);
        let matrix = preflight_npy(&matrix, None).unwrap();
        assert_eq!(matrix.logical_types(), [array(LogicalType::Int64, 3)]);
    }

    #[test]
    fn validates_file_count_before_opening_or_shape() {
        let paths = vec![PathBuf::from("does-not-exist.npy")];
        let expected = [LogicalType::Int64, LogicalType::Int64];
        assert_eq!(
            preflight_npy(&paths, Some(&expected)).unwrap_err(),
            Error::binder("Number of columns mismatch. Expected 2 but got 1.")
        );

        let load_paths = vec![
            PathBuf::from("also-does-not-exist.npy"),
            PathBuf::from("still-does-not-exist.npy"),
        ];
        assert_eq!(
            preflight_npy(&load_paths, None).unwrap_err(),
            Error::binder("Number of columns mismatch. Expected 1 but got 2.")
        );
    }

    #[test]
    fn validates_rows_dtype_and_exact_array_width() {
        let mismatched_rows = vec![
            files("npy-3d", &["id_int64.npy"])[0].clone(),
            files("npy-2d", &["two_dim_int64.npy"])[0].clone(),
        ];
        let two_ints = [LogicalType::Int64, array(LogicalType::Int64, 3)];
        assert_eq!(
            preflight_npy(&mismatched_rows, Some(&two_ints)).unwrap_err(),
            Error::copy("Number of rows in npy files is not equal to each other.")
        );

        let int32 = files("npy-1d", &["one_dim_int32.npy"]);
        assert_eq!(
            preflight_npy(&int32, Some(&[LogicalType::Int64])).unwrap_err(),
            Error::copy(format!(
                "The type of npy file {} does not match the expected type.",
                int32[0].display()
            ))
        );

        let matrix = files("npy-2d", &["two_dim_int64.npy"]);
        let wrong_shape = array(LogicalType::Int64, 4);
        assert_eq!(
            preflight_npy(&matrix, Some(std::slice::from_ref(&wrong_shape))).unwrap_err(),
            Error::copy(format!(
                "The shape of {} does not match {}.",
                matrix[0].display(),
                wrong_shape
            ))
        );
    }

    #[test]
    fn rejects_actual_fortran_order_fixture() {
        let path = files("npy-1d", &["fortran_order.npy"]);
        assert_eq!(
            preflight_npy(&path, None).unwrap_err(),
            Error::copy("Fortran-order NPY files are not currently supported.")
        );
    }

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempNpy(PathBuf);

    impl TempNpy {
        fn mutated(source: &Path, label: &str, mutate: impl FnOnce(&mut Vec<u8>)) -> Self {
            let mut bytes = std::fs::read(source).unwrap();
            mutate(&mut bytes);
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("koko-npy-{label}-{}-{id}.npy", std::process::id()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TempNpy {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn validates_magic_version_header_and_endian() {
        let source = files("npy-1d", &["one_dim_int64.npy"])[0].clone();

        let bad_magic = TempNpy::mutated(&source, "magic", |bytes| bytes[0] = 0);
        assert_eq!(
            inspect_npy(&bad_magic.0).unwrap_err(),
            Error::copy("Invalid NPY file")
        );

        let bad_version = TempNpy::mutated(&source, "version", |bytes| bytes[6] = 2);
        assert_eq!(
            inspect_npy(&bad_version.0).unwrap_err(),
            Error::copy("Unsupported NPY file version.")
        );

        let bad_header = TempNpy::mutated(&source, "header", |bytes| {
            let position = bytes
                .windows(5)
                .position(|window| window == b"shape")
                .unwrap();
            bytes[position] = b'x';
        });
        let error = inspect_npy(&bad_header.0).unwrap_err().to_string();
        assert!(error.starts_with(&format!(
            "Copy exception: Invalid NPY file {}:",
            bad_header.0.display()
        )));

        let native_endian = TempNpy::mutated(&source, "native-endian", |bytes| {
            let position = bytes
                .windows(3)
                .position(|window| window[1..] == *b"i8")
                .unwrap();
            bytes[position] = b'=';
        });
        assert_eq!(
            inspect_npy(&native_endian.0).unwrap().dtype,
            NpyDType::Int64
        );

        let unsupported_dtype = TempNpy::mutated(&source, "dtype", |bytes| {
            let position = bytes
                .windows(3)
                .position(|window| window[1..] == *b"i8")
                .unwrap();
            bytes[position + 1] = b'u';
        });
        assert_eq!(
            inspect_npy(&unsupported_dtype.0).unwrap_err(),
            Error::copy("Unsupported data type: u8")
        );

        let opposite_endian = TempNpy::mutated(&source, "endian", |bytes| {
            let opposite = if cfg!(target_endian = "little") {
                b'>'
            } else {
                b'<'
            };
            let position = bytes
                .windows(3)
                .position(|window| window[1..] == *b"i8")
                .unwrap();
            bytes[position] = opposite;
        });
        assert_eq!(
            inspect_npy(&opposite_endian.0).unwrap_err(),
            Error::copy("The endianness of the file does not match the machine's endianness.")
        );
    }
}
