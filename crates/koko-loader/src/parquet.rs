//! Streaming Parquet interchange for Koko's typed [`DataChunk`] currency.
//!
//! The Parquet crate decodes one Arrow `RecordBatch` at a time.  This adapter
//! fixes that batch size at [`VECTOR_CAPACITY`], applies root-column projection
//! before decoding, and converts each projected column directly into one
//! `ValueVector`.  It never constructs a row matrix.

use arrow_array::builder::{
    ArrayBuilder, BinaryBuilder, BooleanBuilder, Date32Builder, Decimal128Builder,
    FixedSizeBinaryBuilder, FixedSizeListBuilder, Float32Builder, Float64Builder, Int8Builder,
    Int16Builder, Int32Builder, Int64Builder, ListBuilder, MapBuilder, StringBuilder,
    StructBuilder, TimestampMicrosecondBuilder, TimestampMillisecondBuilder,
    TimestampNanosecondBuilder, TimestampSecondBuilder, UInt8Builder, UInt16Builder, UInt32Builder,
    UInt64Builder, make_builder,
};
use arrow_array::{
    Array, ArrayRef, BinaryArray, BinaryViewArray, BooleanArray, Date32Array, Date64Array,
    Decimal128Array, FixedSizeBinaryArray, FixedSizeListArray, Float32Array, Float64Array,
    Int8Array, Int16Array, Int32Array, Int64Array, LargeBinaryArray, LargeListArray,
    LargeListViewArray, LargeStringArray, ListArray, ListViewArray, MapArray, NullArray,
    RecordBatch, StringArray, StringViewArray, StructArray, TimestampMicrosecondArray,
    TimestampMillisecondArray, TimestampNanosecondArray, TimestampSecondArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow_schema::{DataType, Field, FieldRef, Fields, Schema, SchemaRef, TimeUnit};
use bytes::Bytes;
use koko_common::{
    DataChunk, Error, IntKind, Interval, LogicalType, Result, VECTOR_CAPACITY, Value,
};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::arrow::arrow_writer::ArrowWriter;
use parquet::basic::{Compression, GzipLevel, ZstdLevel};
use parquet::errors::Result as ParquetResult;
use parquet::file::properties::WriterProperties;
use parquet::file::reader::{ChunkReader, Length};
use parquet::format::FileMetaData;
use parquet::thrift::{TCompactOutputProtocol, TSerializable};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thrift::protocol::TCompactInputProtocol;

#[derive(Clone)]
struct PinnedFile {
    file: Arc<File>,
    len: u64,
}

impl PinnedFile {
    fn new(file: File) -> std::io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            file: Arc::new(file),
            len,
        })
    }
}

impl Length for PinnedFile {
    fn len(&self) -> u64 {
        self.len
    }
}

impl ChunkReader for PinnedFile {
    type T = PinnedReader;

    fn get_read(&self, start: u64) -> ParquetResult<Self::T> {
        Ok(PinnedReader {
            file: Arc::clone(&self.file),
            offset: start,
        })
    }

    fn get_bytes(&self, start: u64, length: usize) -> ParquetResult<Bytes> {
        let mut reader = self.get_read(start)?;
        let mut bytes = vec![0u8; length];
        reader.read_exact(&mut bytes)?;
        Ok(Bytes::from(bytes))
    }
}

struct PinnedReader {
    file: Arc<File>,
    offset: u64,
}

impl Read for PinnedReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        let read = std::os::unix::fs::FileExt::read_at(self.file.as_ref(), buffer, self.offset)?;
        #[cfg(windows)]
        let read =
            std::os::windows::fs::FileExt::seek_read(self.file.as_ref(), buffer, self.offset)?;
        self.offset = self.offset.saturating_add(read as u64);
        Ok(read)
    }
}

/// Arrow field metadata key used to preserve Koko distinctions that do not
/// have a unique Arrow physical type (for example `SERIAL`, `TIMESTAMP_NS`, and
/// fixed `ARRAY` values).
pub const LOGICAL_TYPE_METADATA_KEY: &str = "koko.logical_type";

/// One top-level Parquet field, including the information needed by bare
/// `LOAD FROM` binding and by typed COPY preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParquetField {
    pub name: String,
    pub logical_type: LogicalType,
    pub nullable: bool,
    pub metadata: BTreeMap<String, String>,
}

impl ParquetField {
    pub fn new(name: impl Into<String>, logical_type: LogicalType, nullable: bool) -> Self {
        let metadata = BTreeMap::from([(
            LOGICAL_TYPE_METADATA_KEY.to_string(),
            logical_type.to_string(),
        )]);
        Self {
            name: name.into(),
            logical_type,
            nullable,
            metadata,
        }
    }
}

/// Ordered schema of a Parquet source or sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParquetSchema {
    pub fields: Vec<ParquetField>,
}

impl ParquetSchema {
    /// Construct and validate an ordered schema.  Names are matched
    /// case-insensitively throughout COPY/LOAD, so case-only duplicates are
    /// rejected here as well.
    pub fn new(fields: Vec<ParquetField>) -> Result<Self> {
        let mut names = HashSet::with_capacity(fields.len());
        for field in &fields {
            if field.name.is_empty() {
                return Err(Error::binder("Parquet column names cannot be empty."));
            }
            if !names.insert(field.name.to_ascii_lowercase()) {
                return Err(Error::binder(format!(
                    "Duplicate Parquet column name `{}`.",
                    field.name
                )));
            }
            // Validate writer support and nested ARRAY widths eagerly. This is
            // also the physical compatibility contract for metadata-bearing
            // fields read back from our own files.
            arrow_type_for_logical(&field.logical_type)?;
        }
        Ok(Self { fields })
    }

    pub fn names(&self) -> Vec<String> {
        self.fields.iter().map(|field| field.name.clone()).collect()
    }

    pub fn types(&self) -> Vec<LogicalType> {
        self.fields
            .iter()
            .map(|field| field.logical_type.clone())
            .collect()
    }

    /// Validate an exact projected schema. This is intended for multi-file
    /// preflight and exact-schema APIs; ordinary table COPY may instead cast a
    /// decoded source column into its target type.
    pub fn validate_exact(&self, expected: &ParquetSchema) -> Result<()> {
        if self.fields.len() != expected.fields.len() {
            return Err(Error::binder(format!(
                "Number of columns mismatch. Expected {} but got {}.",
                expected.fields.len(),
                self.fields.len()
            )));
        }
        for (actual, expected) in self.fields.iter().zip(&expected.fields) {
            if !actual.name.eq_ignore_ascii_case(&expected.name) {
                return Err(Error::binder(format!(
                    "Parquet column name mismatch. Expected `{}` but got `{}`.",
                    expected.name, actual.name
                )));
            }
            if actual.logical_type != expected.logical_type {
                return Err(Error::binder(format!(
                    "Column `{}` type mismatch. Expected {} but got {}.",
                    expected.name, expected.logical_type, actual.logical_type
                )));
            }
        }
        Ok(())
    }

    fn to_arrow_schema(&self) -> Result<SchemaRef> {
        let fields = self
            .fields
            .iter()
            .map(field_to_arrow)
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(Schema::new(fields)))
    }
}

/// Metadata available before the first source batch is decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParquetFileMetadata {
    pub schema: ParquetSchema,
    pub num_rows: u64,
    pub num_row_groups: usize,
    pub key_value_metadata: HashMap<String, String>,
}

/// Inspect schema and row counts without decoding a data page.
pub fn inspect(path: impl AsRef<Path>) -> Result<ParquetFileMetadata> {
    let path = path.as_ref();
    match ParquetRecordBatchReaderBuilder::try_new(File::open(path)?) {
        Ok(builder) => metadata_from_builder(path, &builder),
        Err(error) if legacy_converted_type_error(&error) => {
            let bytes = sanitize_legacy_converted_types(path)?;
            let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)
                .map_err(|error| parquet_error(path, "reading legacy metadata", error))?;
            metadata_from_builder(path, &builder)
        }
        Err(error) => Err(parquet_error(path, "reading metadata", error)),
    }
}

fn legacy_converted_type_error(error: &parquet::errors::ParquetError) -> bool {
    error
        .to_string()
        .contains("unexpected parquet converted type")
}

/// Koko 0.17.0 wrote the private converted-type value 22 for SERIAL
/// columns. Apache Arrow correctly rejects that value because the legacy
/// ConvertedType enum ends at 21. The physical INT64 representation is
/// otherwise standard, so strip only unknown converted-type annotations in a
/// bounded temporary copy while leaving every data-page byte untouched.
fn sanitize_legacy_converted_types(path: &Path) -> Result<File> {
    sanitize_legacy_converted_types_from_file(path, File::open(path)?)
}

fn sanitize_legacy_converted_types_from_file(path: &Path, mut source: File) -> Result<File> {
    const TRAILER_LEN: u64 = 8;
    const MAGIC: &[u8; 4] = b"PAR1";

    let file_len = source.metadata()?.len();
    if file_len < TRAILER_LEN {
        return Err(Error::copy(format!(
            "Invalid Parquet footer in {}.",
            path.display()
        )));
    }
    source.seek(SeekFrom::End(-(TRAILER_LEN as i64)))?;
    let mut trailer = [0u8; TRAILER_LEN as usize];
    source.read_exact(&mut trailer)?;
    if &trailer[4..] != MAGIC {
        return Err(Error::copy(format!(
            "Invalid Parquet footer in {}.",
            path.display()
        )));
    }
    let footer_len = u32::from_le_bytes(
        trailer[..4]
            .try_into()
            .expect("Parquet footer length is four bytes"),
    ) as u64;
    let footer_start = file_len
        .checked_sub(TRAILER_LEN)
        .and_then(|offset| offset.checked_sub(footer_len))
        .ok_or_else(|| {
            Error::copy(format!(
                "Invalid Parquet footer length in {}.",
                path.display()
            ))
        })?;

    source.seek(SeekFrom::Start(footer_start))?;
    let mut original_footer = vec![
        0u8;
        usize::try_from(footer_len).map_err(|_| {
            Error::copy(format!(
                "Parquet footer is too large in {}.",
                path.display()
            ))
        })?
    ];
    source.read_exact(&mut original_footer)?;
    let mut input = TCompactInputProtocol::new(Cursor::new(&original_footer));
    let mut metadata = FileMetaData::read_from_in_protocol(&mut input).map_err(|error| {
        Error::copy(format!(
            "Invalid Parquet metadata in {}: {error}",
            path.display()
        ))
    })?;
    let mut changed = false;
    for field in &mut metadata.schema {
        if field.converted_type.is_some_and(|converted| {
            !parquet::format::ConvertedType::ENUM_VALUES.contains(&converted)
        }) {
            field.converted_type = None;
            changed = true;
        }
    }
    if !changed {
        return Err(Error::copy(format!(
            "Unsupported Parquet converted type in {}.",
            path.display()
        )));
    }

    let mut footer = Vec::with_capacity(original_footer.len());
    metadata
        .write_to_out_protocol(&mut TCompactOutputProtocol::new(&mut footer))
        .map_err(|error| {
            Error::copy(format!(
                "Cannot normalize Parquet metadata in {}: {error}",
                path.display()
            ))
        })?;
    let normalized_len = u32::try_from(footer.len()).map_err(|_| {
        Error::copy(format!(
            "Parquet footer is too large in {}.",
            path.display()
        ))
    })?;

    // Keep the legacy fallback bounded: stream unchanged pages into an
    // anonymous temporary file and replace only its footer.
    let mut normalized = tempfile::tempfile()?;
    source.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut (&mut source).take(footer_start), &mut normalized)?;
    normalized.write_all(&footer)?;
    normalized.write_all(&normalized_len.to_le_bytes())?;
    normalized.write_all(MAGIC)?;
    normalized.seek(SeekFrom::Start(0))?;
    Ok(normalized)
}

/// Streaming projected Parquet reader. Every successful batch contains at
/// most [`VECTOR_CAPACITY`] rows. An empty file yields no batches while keeping
/// its schema available through [`Self::schema`].
pub struct ParquetReader {
    path: PathBuf,
    file_metadata: ParquetFileMetadata,
    projected_schema: ParquetSchema,
    batch_column_indices: Vec<usize>,
    reader: ParquetRecordBatchReader,
}

impl ParquetReader {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_projected(path, &[])
    }

    /// Open a reader with root-column projection pushdown. An empty projection
    /// means all columns. Requested columns are returned in request order;
    /// lookup is case-insensitive and duplicate requests are rejected.
    pub fn open_projected(path: impl AsRef<Path>, projection: &[String]) -> Result<Self> {
        let path = path.as_ref();
        Self::open_file(path, File::open(path)?, projection, None)
    }

    /// Open a projected reader over an already-open file. The owned file pins
    /// this query's source even when its directory entry is concurrently removed.
    pub fn open_projected_file(
        path: impl AsRef<Path>,
        file: File,
        projection: &[String],
    ) -> Result<Self> {
        Self::open_file(path.as_ref(), file, projection, None)
    }

    /// Open a projected, bounded row range over an already-open file.
    pub fn open_projected_file_range(
        path: impl AsRef<Path>,
        file: File,
        projection: &[String],
        offset: u64,
        limit: u64,
    ) -> Result<Self> {
        let offset = usize::try_from(offset)
            .map_err(|_| Error::runtime("Parquet row offset exceeds platform limits."))?;
        let limit = usize::try_from(limit)
            .map_err(|_| Error::runtime("Parquet row limit exceeds platform limits."))?;
        Self::open_file(path.as_ref(), file, projection, Some((offset, limit)))
    }

    fn open_file(
        path: &Path,
        file: File,
        projection: &[String],
        range: Option<(usize, usize)>,
    ) -> Result<Self> {
        let legacy_file = file.try_clone()?;
        let pinned = PinnedFile::new(file)?;
        match ParquetRecordBatchReaderBuilder::try_new(pinned) {
            Ok(builder) => Self::from_builder(path, projection, range, builder),
            Err(error) if legacy_converted_type_error(&error) => {
                let bytes = sanitize_legacy_converted_types_from_file(path, legacy_file)?;
                let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)
                    .map_err(|error| parquet_error(path, "reading legacy metadata", error))?;
                Self::from_builder(path, projection, range, builder)
            }
            Err(error) => Err(parquet_error(path, "reading metadata", error)),
        }
    }

    fn from_builder<T: ChunkReader + 'static>(
        path: &Path,
        projection: &[String],
        range: Option<(usize, usize)>,
        builder: ParquetRecordBatchReaderBuilder<T>,
    ) -> Result<Self> {
        let file_metadata = metadata_from_builder(path, &builder)?;

        let selected = resolve_projection(&file_metadata.schema, projection)?;
        let mut included = selected.clone();
        included.sort_unstable();
        included.dedup();
        let batch_column_indices = selected
            .iter()
            .map(|source_index| {
                included
                    .binary_search(source_index)
                    .expect("selected Parquet root is included")
            })
            .collect();
        let projected_fields = selected
            .iter()
            .map(|index| file_metadata.schema.fields[*index].clone())
            .collect();
        let projected_schema = ParquetSchema::new(projected_fields)?;

        let mask = ProjectionMask::roots(builder.parquet_schema(), included);
        let mut builder = builder
            .with_batch_size(VECTOR_CAPACITY)
            .with_projection(mask);
        if let Some((offset, limit)) = range {
            builder = builder.with_offset(offset).with_limit(limit);
        }
        let reader = builder
            .build()
            .map_err(|error| parquet_error(path, "building reader", error))?;

        Ok(Self {
            path: path.to_path_buf(),
            file_metadata,
            projected_schema,
            batch_column_indices,
            reader,
        })
    }

    /// Full, unprojected file metadata.
    pub fn file_metadata(&self) -> &ParquetFileMetadata {
        &self.file_metadata
    }

    /// Ordered output schema after projection.
    pub fn schema(&self) -> &ParquetSchema {
        &self.projected_schema
    }

    pub fn next_chunk(&mut self) -> Result<Option<DataChunk>> {
        let Some(batch) = self.reader.next() else {
            return Ok(None);
        };
        let batch = batch.map_err(|error| parquet_error(&self.path, "decoding data", error))?;
        if batch.num_rows() > VECTOR_CAPACITY {
            return Err(Error::copy(format!(
                "Parquet reader produced {} rows, exceeding vector capacity {}.",
                batch.num_rows(),
                VECTOR_CAPACITY
            )));
        }

        let types = self.projected_schema.types();
        let mut chunk = DataChunk::new(&types);
        for (output_index, (field, batch_index)) in self
            .projected_schema
            .fields
            .iter()
            .zip(&self.batch_column_indices)
            .enumerate()
        {
            let array = batch.column(*batch_index);
            for row in 0..batch.num_rows() {
                let value = value_from_arrow(array.as_ref(), row, &field.logical_type).map_err(
                    |error| {
                        Error::copy(format!(
                            "Error decoding Parquet column `{}` at row {}: {}",
                            field.name,
                            row + 1,
                            error_without_prefix(error)
                        ))
                    },
                )?;
                chunk.columns[output_index].set_value_owned(row, value);
            }
        }
        chunk.set_flat(batch.num_rows());
        Ok(Some(chunk))
    }
}

impl Iterator for ParquetReader {
    type Item = Result<DataChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_chunk().transpose()
    }
}

/// COPY TO compression modes accepted by Koko 0.17.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParquetCompression {
    Uncompressed,
    #[default]
    Snappy,
    Zstd,
    Gzip,
    Lz4Raw,
}

/// Options for a streaming Parquet sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ParquetWriterOptions {
    pub compression: ParquetCompression,
}

/// Streaming `DataChunk` to Parquet writer. Input chunks are converted one
/// column at a time. Factorization multiplicities are expanded into bounded
/// record batches so COPY TO observes logical rows rather than physical slots.
pub struct ParquetFileWriter {
    path: PathBuf,
    schema: ParquetSchema,
    arrow_schema: SchemaRef,
    writer: ArrowWriter<File>,
    rows_written: u64,
}

impl ParquetFileWriter {
    pub fn create(
        path: impl AsRef<Path>,
        schema: ParquetSchema,
        options: ParquetWriterOptions,
    ) -> Result<Self> {
        // Revalidate public-field mutation before touching the destination.
        let schema = ParquetSchema::new(schema.fields)?;
        let arrow_schema = schema.to_arrow_schema()?;
        let properties = WriterProperties::builder()
            .set_compression(compression(options.compression))
            .set_max_row_group_size(VECTOR_CAPACITY)
            .build();
        let path = path.as_ref();
        let file = File::create(path)?;
        let writer = ArrowWriter::try_new(file, arrow_schema.clone(), Some(properties))
            .map_err(|error| parquet_error(path, "creating writer", error))?;
        Ok(Self {
            path: path.to_path_buf(),
            schema,
            arrow_schema,
            writer,
            rows_written: 0,
        })
    }

    pub fn schema(&self) -> &ParquetSchema {
        &self.schema
    }

    pub fn rows_written(&self) -> u64 {
        self.rows_written
    }

    pub fn write_chunk(&mut self, chunk: &DataChunk) -> Result<()> {
        validate_chunk_schema(chunk, &self.schema)?;

        let mut positions = Vec::with_capacity(VECTOR_CAPACITY);
        for position in chunk.sel.iter() {
            let multiplicity = chunk.multiplicity(position);
            for _ in 0..multiplicity {
                positions.push(position);
                if positions.len() == VECTOR_CAPACITY {
                    self.write_positions(chunk, &positions)?;
                    positions.clear();
                }
            }
        }
        if !positions.is_empty() {
            self.write_positions(chunk, &positions)?;
        }
        Ok(())
    }

    fn write_positions(&mut self, chunk: &DataChunk, positions: &[usize]) -> Result<()> {
        let arrays = chunk
            .columns
            .iter()
            .zip(&self.schema.fields)
            .map(|(column, field)| {
                let mut builder = make_builder(
                    &arrow_type_for_logical(&field.logical_type)?,
                    positions.len(),
                );
                for position in positions {
                    let value = column.get_value(*position);
                    if value.is_null() && !field.nullable {
                        return Err(Error::copy(format!(
                            "NULL value in required Parquet column `{}`.",
                            field.name
                        )));
                    }
                    append_value(builder.as_mut(), &field.logical_type, &value)?;
                }
                Ok(builder.finish())
            })
            .collect::<Result<Vec<ArrayRef>>>()?;
        let batch = RecordBatch::try_new(self.arrow_schema.clone(), arrays)
            .map_err(|error| Error::copy(format!("Cannot build Parquet output batch: {error}")))?;
        self.writer
            .write(&batch)
            .map_err(|error| parquet_error(&self.path, "writing data", error))?;
        self.rows_written = self
            .rows_written
            .checked_add(positions.len() as u64)
            .ok_or_else(|| Error::overflow("Parquet output row count overflow."))?;
        Ok(())
    }

    /// Finalize the footer. Calling this without writing a chunk creates a
    /// valid empty Parquet file carrying the requested schema.
    pub fn finish(self) -> Result<u64> {
        let rows = self.rows_written;
        self.writer
            .close()
            .map_err(|error| parquet_error(&self.path, "finalizing writer", error))?;
        Ok(rows)
    }
}

fn metadata_from_builder<T: ChunkReader + 'static>(
    path: &Path,
    builder: &ParquetRecordBatchReaderBuilder<T>,
) -> Result<ParquetFileMetadata> {
    let fields = builder
        .schema()
        .fields()
        .iter()
        .map(|field| field_from_arrow(path, field))
        .collect::<Result<Vec<_>>>()?;
    let schema = ParquetSchema::new(fields)?;
    let num_rows = u64::try_from(builder.metadata().file_metadata().num_rows()).map_err(|_| {
        Error::copy(format!(
            "Parquet file {} reports a negative row count.",
            path.display()
        ))
    })?;
    let key_value_metadata = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .value
                .as_ref()
                .map(|value| (entry.key.clone(), value.clone()))
        })
        .collect();
    Ok(ParquetFileMetadata {
        schema,
        num_rows,
        num_row_groups: builder.metadata().num_row_groups(),
        key_value_metadata,
    })
}

fn resolve_projection(schema: &ParquetSchema, projection: &[String]) -> Result<Vec<usize>> {
    if projection.is_empty() {
        return Ok((0..schema.fields.len()).collect());
    }
    let mut seen = HashSet::with_capacity(projection.len());
    projection
        .iter()
        .map(|name| {
            let folded = name.to_ascii_lowercase();
            if !seen.insert(folded) {
                return Err(Error::binder(format!(
                    "Duplicate Parquet projection column `{name}`."
                )));
            }
            schema
                .fields
                .iter()
                .position(|field| field.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| Error::binder(format!("Parquet column `{name}` does not exist.")))
        })
        .collect()
}

fn field_from_arrow(path: &Path, field: &FieldRef) -> Result<ParquetField> {
    let metadata = field
        .metadata()
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let logical_type = match field.metadata().get(LOGICAL_TYPE_METADATA_KEY) {
        Some(display) => {
            let logical_type = LogicalType::from_ddl_str(display).map_err(|_| {
                Error::copy(format!(
                    "Invalid {LOGICAL_TYPE_METADATA_KEY} metadata `{display}` on Parquet column `{}` in {}.",
                    field.name(),
                    path.display()
                ))
            })?;
            ensure_arrow_compatible(field.data_type(), &logical_type).map_err(|reason| {
                Error::copy(format!(
                    "Parquet column `{}` metadata declares {} but its Arrow type is {}: {reason}",
                    field.name(),
                    logical_type,
                    field.data_type()
                ))
            })?;
            logical_type
        }
        None => logical_from_arrow(field.data_type())?,
    };
    Ok(ParquetField {
        name: field.name().clone(),
        logical_type,
        nullable: field.is_nullable(),
        metadata,
    })
}

fn field_to_arrow(field: &ParquetField) -> Result<Field> {
    let mut metadata = field
        .metadata
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    metadata.insert(
        LOGICAL_TYPE_METADATA_KEY.to_string(),
        field.logical_type.to_string(),
    );
    Ok(Field::new(
        &field.name,
        arrow_type_for_logical(&field.logical_type)?,
        field.nullable,
    )
    .with_metadata(metadata))
}

fn logical_from_arrow(data_type: &DataType) -> Result<LogicalType> {
    Ok(match data_type {
        DataType::Null => LogicalType::Any,
        DataType::Boolean => LogicalType::Bool,
        DataType::Int8 => LogicalType::Int(IntKind::I8),
        DataType::Int16 => LogicalType::Int(IntKind::I16),
        DataType::Int32 => LogicalType::Int(IntKind::I32),
        DataType::Int64 => LogicalType::Int64,
        DataType::UInt8 => LogicalType::Int(IntKind::U8),
        DataType::UInt16 => LogicalType::Int(IntKind::U16),
        DataType::UInt32 => LogicalType::Int(IntKind::U32),
        DataType::UInt64 => LogicalType::Int(IntKind::U64),
        DataType::Float32 => LogicalType::Float,
        DataType::Float64 => LogicalType::Double,
        DataType::Decimal128(precision, scale) if *scale >= 0 => {
            LogicalType::Decimal(*precision, *scale as u8)
        }
        DataType::Decimal128(_, scale) => {
            return Err(Error::copy(format!(
                "Parquet DECIMAL with negative scale {scale} is not supported."
            )));
        }
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => LogicalType::String,
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => LogicalType::Blob,
        DataType::Date32 | DataType::Date64 => LogicalType::Date,
        DataType::Timestamp(_, timezone) if timezone.is_some() => LogicalType::TimestampTz,
        DataType::Timestamp(TimeUnit::Second, None) => LogicalType::TimestampSec,
        DataType::Timestamp(TimeUnit::Millisecond, None) => LogicalType::TimestampMs,
        DataType::Timestamp(TimeUnit::Microsecond, None) => LogicalType::Timestamp,
        DataType::Timestamp(TimeUnit::Nanosecond, None) => LogicalType::TimestampNs,
        DataType::List(child) | DataType::LargeList(child) => {
            LogicalType::List(Box::new(logical_from_arrow(child.data_type())?))
        }
        DataType::ListView(child) | DataType::LargeListView(child) => {
            LogicalType::List(Box::new(logical_from_arrow(child.data_type())?))
        }
        DataType::FixedSizeList(child, length) => LogicalType::Array(
            Box::new(logical_from_arrow(child.data_type())?),
            u64::try_from(*length).map_err(|_| {
                Error::copy(format!("Invalid negative Parquet ARRAY length {length}."))
            })?,
        ),
        DataType::Struct(fields) => LogicalType::Struct(
            fields
                .iter()
                .map(|field| Ok((field.name().clone(), logical_from_arrow(field.data_type())?)))
                .collect::<Result<Vec<_>>>()?,
        ),
        DataType::Map(entries, _) => {
            let DataType::Struct(fields) = entries.data_type() else {
                return Err(Error::copy("Parquet MAP entries must be a STRUCT."));
            };
            if fields.len() != 2 {
                return Err(Error::copy(format!(
                    "Parquet MAP entries require two fields, found {}.",
                    fields.len()
                )));
            }
            LogicalType::Map(
                Box::new(logical_from_arrow(fields[0].data_type())?),
                Box::new(logical_from_arrow(fields[1].data_type())?),
            )
        }
        DataType::Dictionary(_, value) => logical_from_arrow(value)?,
        other => {
            return Err(Error::copy(format!(
                "Unsupported Parquet/Arrow type {other}."
            )));
        }
    })
}

fn arrow_type_for_logical(logical_type: &LogicalType) -> Result<DataType> {
    Ok(match logical_type {
        LogicalType::Bool => DataType::Boolean,
        LogicalType::Int(IntKind::I8) => DataType::Int8,
        LogicalType::Int(IntKind::I16) => DataType::Int16,
        LogicalType::Int(IntKind::I32) => DataType::Int32,
        LogicalType::Int(IntKind::I64) | LogicalType::Serial => DataType::Int64,
        LogicalType::Int(IntKind::I128) | LogicalType::UInt128 | LogicalType::Uuid => {
            DataType::FixedSizeBinary(16)
        }
        LogicalType::Int(IntKind::U8) => DataType::UInt8,
        LogicalType::Int(IntKind::U16) => DataType::UInt16,
        LogicalType::Int(IntKind::U32) => DataType::UInt32,
        LogicalType::Int(IntKind::U64) => DataType::UInt64,
        LogicalType::Decimal(precision, scale) => {
            if *precision == 0 || *precision > 38 || *scale > *precision {
                return Err(Error::copy(format!(
                    "Cannot write invalid Parquet DECIMAL({precision}, {scale})."
                )));
            }
            DataType::Decimal128(*precision, *scale as i8)
        }
        LogicalType::Double => DataType::Float64,
        LogicalType::Float => DataType::Float32,
        LogicalType::String => DataType::Utf8,
        LogicalType::Json => DataType::Utf8,
        LogicalType::Blob => DataType::Binary,
        LogicalType::Date => DataType::Date32,
        LogicalType::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
        LogicalType::TimestampNs => DataType::Timestamp(TimeUnit::Nanosecond, None),
        LogicalType::TimestampMs => DataType::Timestamp(TimeUnit::Millisecond, None),
        LogicalType::TimestampSec => DataType::Timestamp(TimeUnit::Second, None),
        LogicalType::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
        // Parquet intervals have millisecond precision in the C++ interchange
        // contract. Fixed bytes + metadata preserve the signed components after
        // that required truncation without exposing Arrow's unsupported interval writer.
        LogicalType::Interval => DataType::FixedSizeBinary(16),
        LogicalType::List(child) => DataType::List(Arc::new(Field::new_list_field(
            arrow_type_for_logical(child)?,
            true,
        ))),
        LogicalType::Array(child, length) => DataType::FixedSizeList(
            Arc::new(Field::new_list_field(arrow_type_for_logical(child)?, true)),
            i32::try_from(*length).map_err(|_| {
                Error::copy(format!("Parquet ARRAY length {length} exceeds INT32."))
            })?,
        ),
        LogicalType::Struct(fields) => DataType::Struct(Fields::from(
            fields
                .iter()
                .map(|(name, logical_type)| {
                    Ok(Field::new(
                        name,
                        arrow_type_for_logical(logical_type)?,
                        true,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        )),
        LogicalType::Map(key, value) => {
            let entries = DataType::Struct(Fields::from(vec![
                Field::new("key", arrow_type_for_logical(key)?, false),
                Field::new("value", arrow_type_for_logical(value)?, true),
            ]));
            DataType::Map(Arc::new(Field::new("entries", entries, false)), false)
        }
        LogicalType::Any => DataType::Null,
        LogicalType::Union(_)
        | LogicalType::InternalId
        | LogicalType::Node(_)
        | LogicalType::Rel(_)
        | LogicalType::RecursiveRel => {
            return Err(Error::copy(format!(
                "Writing a column with type: {logical_type} to parquet is not supported."
            )));
        }
    })
}

fn ensure_arrow_compatible(
    data_type: &DataType,
    logical_type: &LogicalType,
) -> std::result::Result<(), String> {
    let compatible = match logical_type {
        LogicalType::String | LogicalType::Json => matches!(
            data_type,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
        ),
        LogicalType::Blob => matches!(
            data_type,
            DataType::Binary
                | DataType::LargeBinary
                | DataType::BinaryView
                | DataType::FixedSizeBinary(_)
        ),
        LogicalType::Date => matches!(data_type, DataType::Date32 | DataType::Date64),
        LogicalType::Timestamp
        | LogicalType::TimestampNs
        | LogicalType::TimestampMs
        | LogicalType::TimestampSec
        | LogicalType::TimestampTz => matches!(data_type, DataType::Timestamp(_, _)),
        LogicalType::List(child) => match data_type {
            DataType::List(field)
            | DataType::LargeList(field)
            | DataType::ListView(field)
            | DataType::LargeListView(field) => {
                ensure_arrow_compatible(field.data_type(), child).is_ok()
            }
            _ => false,
        },
        LogicalType::Array(child, length) => match data_type {
            DataType::FixedSizeList(field, actual) => {
                u64::try_from(*actual).ok() == Some(*length)
                    && ensure_arrow_compatible(field.data_type(), child).is_ok()
            }
            _ => false,
        },
        LogicalType::Struct(expected) => match data_type {
            DataType::Struct(actual) if actual.len() == expected.len() => {
                actual.iter().zip(expected).all(|(field, (name, ty))| {
                    field.name() == name && ensure_arrow_compatible(field.data_type(), ty).is_ok()
                })
            }
            _ => false,
        },
        LogicalType::Map(key, value) => match data_type {
            DataType::Map(entries, _) => match entries.data_type() {
                DataType::Struct(fields) if fields.len() == 2 => {
                    ensure_arrow_compatible(fields[0].data_type(), key).is_ok()
                        && ensure_arrow_compatible(fields[1].data_type(), value).is_ok()
                }
                _ => false,
            },
            _ => false,
        },
        LogicalType::Int(IntKind::I128)
        | LogicalType::UInt128
        | LogicalType::Uuid
        | LogicalType::Interval => matches!(data_type, DataType::FixedSizeBinary(16)),
        LogicalType::Any => matches!(data_type, DataType::Null),
        other => arrow_type_for_logical(other).as_ref().ok() == Some(data_type),
    };
    if compatible {
        Ok(())
    } else {
        Err("physical and logical types are incompatible".to_string())
    }
}

fn parquet_output_type(logical_type: &LogicalType) -> LogicalType {
    match logical_type {
        LogicalType::Int(IntKind::I128) | LogicalType::UInt128 => LogicalType::Double,
        LogicalType::TimestampTz => LogicalType::Timestamp,
        LogicalType::Any => LogicalType::String,
        LogicalType::Union(variants) => {
            let mut fields = Vec::with_capacity(variants.len() + 1);
            fields.push(("tag".to_string(), LogicalType::Int(IntKind::U8)));
            fields.extend(
                variants
                    .iter()
                    .map(|(name, logical_type)| (name.clone(), parquet_output_type(logical_type))),
            );
            LogicalType::Struct(fields)
        }
        LogicalType::List(child) => LogicalType::List(Box::new(parquet_output_type(child))),
        LogicalType::Array(child, length) => {
            LogicalType::Array(Box::new(parquet_output_type(child)), *length)
        }
        LogicalType::Struct(fields) => LogicalType::Struct(
            fields
                .iter()
                .map(|(name, logical_type)| (name.clone(), parquet_output_type(logical_type)))
                .collect(),
        ),
        LogicalType::Map(key, value) => LogicalType::Map(
            Box::new(parquet_output_type(key)),
            Box::new(parquet_output_type(value)),
        ),
        other => other.clone(),
    }
}

fn validate_chunk_schema(chunk: &DataChunk, schema: &ParquetSchema) -> Result<()> {
    if chunk.columns.len() != schema.fields.len() {
        return Err(Error::binder(format!(
            "Number of columns mismatch. Expected {} but got {}.",
            schema.fields.len(),
            chunk.columns.len()
        )));
    }
    for (column, field) in chunk.columns.iter().zip(&schema.fields) {
        if column.logical_type != field.logical_type
            && parquet_output_type(&column.logical_type) != field.logical_type
        {
            return Err(Error::binder(format!(
                "Column `{}` type mismatch. Expected {} but got {}.",
                field.name, field.logical_type, column.logical_type
            )));
        }
    }
    Ok(())
}

fn value_from_arrow(array: &dyn Array, row: usize, logical_type: &LogicalType) -> Result<Value> {
    if array.is_null(row) {
        return Ok(Value::Null);
    }
    macro_rules! value {
        ($array:ty, $ctor:expr) => {{
            let array = downcast_array::<$array>(array)?;
            $ctor(array.value(row))
        }};
    }
    Ok(match logical_type {
        LogicalType::Bool => value!(BooleanArray, Value::Bool),
        LogicalType::Int(IntKind::I8) => value!(Int8Array, |v| Value::IntX {
            value: v as i128,
            kind: IntKind::I8,
        }),
        LogicalType::Int(IntKind::I16) => value!(Int16Array, |v| Value::IntX {
            value: v as i128,
            kind: IntKind::I16,
        }),
        LogicalType::Int(IntKind::I32) => value!(Int32Array, |v| Value::IntX {
            value: v as i128,
            kind: IntKind::I32,
        }),
        LogicalType::Int(IntKind::I64) | LogicalType::Serial => value!(Int64Array, Value::Int64),
        LogicalType::Int(IntKind::I128) => {
            let bytes = value!(FixedSizeBinaryArray, |v: &[u8]| v.to_vec());
            Value::IntX {
                value: i128::from_be_bytes(bytes.try_into().map_err(|_| {
                    Error::copy("Parquet INT128 value must contain exactly 16 bytes.")
                })?),
                kind: IntKind::I128,
            }
        }
        LogicalType::Int(IntKind::U8) => value!(UInt8Array, |v| Value::IntX {
            value: v as i128,
            kind: IntKind::U8,
        }),
        LogicalType::Int(IntKind::U16) => value!(UInt16Array, |v| Value::IntX {
            value: v as i128,
            kind: IntKind::U16,
        }),
        LogicalType::Int(IntKind::U32) => value!(UInt32Array, |v| Value::IntX {
            value: v as i128,
            kind: IntKind::U32,
        }),
        LogicalType::Int(IntKind::U64) => value!(UInt64Array, |v| Value::IntX {
            value: v as i128,
            kind: IntKind::U64,
        }),
        LogicalType::UInt128 => {
            let bytes = value!(FixedSizeBinaryArray, |v: &[u8]| v.to_vec());
            Value::UInt128(u128::from_be_bytes(bytes.try_into().map_err(|_| {
                Error::copy("Parquet UINT128 value must contain exactly 16 bytes.")
            })?))
        }
        LogicalType::Decimal(precision, scale) => value!(Decimal128Array, |value| Value::Decimal {
            value,
            precision: *precision,
            scale: *scale,
        }),
        LogicalType::Double => value!(Float64Array, Value::Double),
        LogicalType::Float => value!(Float32Array, Value::Float),
        LogicalType::String => match array.data_type() {
            DataType::Utf8 => value!(StringArray, |v: &str| Value::String(v.to_string())),
            DataType::LargeUtf8 => {
                value!(LargeStringArray, |v: &str| Value::String(v.to_string()))
            }
            DataType::Utf8View => {
                value!(StringViewArray, |v: &str| Value::String(v.to_string()))
            }
            _ => return Err(type_mismatch(array, logical_type)),
        },
        LogicalType::Json => {
            let text = match array.data_type() {
                DataType::Utf8 => downcast_array::<StringArray>(array)?.value(row),
                DataType::LargeUtf8 => downcast_array::<LargeStringArray>(array)?.value(row),
                DataType::Utf8View => downcast_array::<StringViewArray>(array)?.value(row),
                _ => return Err(type_mismatch(array, logical_type)),
            };
            Value::Json(koko_common::JsonValue::parse(text)?)
        }
        LogicalType::Blob => match array.data_type() {
            DataType::Binary => value!(BinaryArray, |v: &[u8]| Value::Blob(v.to_vec())),
            DataType::LargeBinary => {
                value!(LargeBinaryArray, |v: &[u8]| Value::Blob(v.to_vec()))
            }
            DataType::BinaryView => {
                value!(BinaryViewArray, |v: &[u8]| Value::Blob(v.to_vec()))
            }
            DataType::FixedSizeBinary(_) => {
                value!(FixedSizeBinaryArray, |v: &[u8]| Value::Blob(v.to_vec()))
            }
            _ => return Err(type_mismatch(array, logical_type)),
        },
        LogicalType::Date => match array.data_type() {
            DataType::Date32 => value!(Date32Array, Value::Date),
            DataType::Date64 => {
                let millis = downcast_array::<Date64Array>(array)?.value(row);
                Value::Date(i32::try_from(millis.div_euclid(86_400_000)).map_err(|_| {
                    Error::overflow("Parquet DATE value is outside Koko's supported range.")
                })?)
            }
            _ => return Err(type_mismatch(array, logical_type)),
        },
        LogicalType::Timestamp
        | LogicalType::TimestampNs
        | LogicalType::TimestampMs
        | LogicalType::TimestampSec
        | LogicalType::TimestampTz => {
            let micros = timestamp_micros(array, row)?;
            if *logical_type == LogicalType::TimestampTz {
                Value::TimestampTz(micros)
            } else {
                Value::Timestamp(micros)
            }
        }
        LogicalType::Interval => {
            let bytes = value!(FixedSizeBinaryArray, |v: &[u8]| v.to_vec());
            if bytes.len() != 16 {
                return Err(Error::copy(
                    "Parquet INTERVAL value must contain exactly 16 bytes.",
                ));
            }
            Value::Interval(Interval {
                months: i32::from_le_bytes(bytes[0..4].try_into().expect("four-byte slice")),
                days: i32::from_le_bytes(bytes[4..8].try_into().expect("four-byte slice")),
                micros: i64::from_le_bytes(bytes[8..16].try_into().expect("eight-byte slice")),
            })
        }
        LogicalType::Uuid => {
            let bytes = value!(FixedSizeBinaryArray, |v: &[u8]| v.to_vec());
            Value::Uuid(u128::from_be_bytes(bytes.try_into().map_err(|_| {
                Error::copy("Parquet UUID value must contain exactly 16 bytes.")
            })?))
        }
        LogicalType::List(child) => Value::List(list_values(array, row, child)?),
        LogicalType::Array(child, expected) => {
            let values = fixed_list_values(array, row, child)?;
            if values.len() as u64 != *expected {
                return Err(Error::copy(format!(
                    "Parquet ARRAY expected {expected} elements but got {}.",
                    values.len()
                )));
            }
            Value::List(values)
        }
        LogicalType::Struct(fields) => {
            let array = downcast_array::<StructArray>(array)?;
            let values = fields
                .iter()
                .enumerate()
                .map(|(index, (name, logical_type))| {
                    Ok((
                        name.clone(),
                        value_from_arrow(array.column(index).as_ref(), row, logical_type)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Value::Struct(values)
        }
        LogicalType::Map(key, value) => {
            let array = downcast_array::<MapArray>(array)?;
            let offsets = array.value_offsets();
            let start = offsets[row] as usize;
            let end = offsets[row + 1] as usize;
            let mut entries = Vec::with_capacity(end - start);
            for index in start..end {
                entries.push((
                    value_from_arrow(array.keys().as_ref(), index, key)?,
                    value_from_arrow(array.values().as_ref(), index, value)?,
                ));
            }
            Value::Map(entries)
        }
        LogicalType::Any if matches!(array.data_type(), DataType::Null) => {
            let _ = downcast_array::<NullArray>(array)?;
            Value::Null
        }
        LogicalType::Any
        | LogicalType::Union(_)
        | LogicalType::InternalId
        | LogicalType::Node(_)
        | LogicalType::Rel(_)
        | LogicalType::RecursiveRel => return Err(type_mismatch(array, logical_type)),
    })
}

fn list_values(array: &dyn Array, row: usize, child: &LogicalType) -> Result<Vec<Value>> {
    let values = match array.data_type() {
        DataType::List(_) => downcast_array::<ListArray>(array)?.value(row),
        DataType::LargeList(_) => downcast_array::<LargeListArray>(array)?.value(row),
        DataType::ListView(_) => downcast_array::<ListViewArray>(array)?.value(row),
        DataType::LargeListView(_) => downcast_array::<LargeListViewArray>(array)?.value(row),
        _ => {
            return Err(type_mismatch(
                array,
                &LogicalType::List(Box::new(child.clone())),
            ));
        }
    };
    (0..values.len())
        .map(|index| value_from_arrow(values.as_ref(), index, child))
        .collect()
}

fn fixed_list_values(array: &dyn Array, row: usize, child: &LogicalType) -> Result<Vec<Value>> {
    let values = downcast_array::<FixedSizeListArray>(array)?.value(row);
    (0..values.len())
        .map(|index| value_from_arrow(values.as_ref(), index, child))
        .collect()
}

fn timestamp_micros(array: &dyn Array, row: usize) -> Result<i64> {
    match array.data_type() {
        DataType::Timestamp(TimeUnit::Second, _) => {
            let value = downcast_array::<TimestampSecondArray>(array)?.value(row);
            value
                .checked_mul(1_000_000)
                .ok_or_else(|| Error::overflow("Parquet second timestamp overflows microseconds."))
        }
        DataType::Timestamp(TimeUnit::Millisecond, _) => {
            let value = downcast_array::<TimestampMillisecondArray>(array)?.value(row);
            value.checked_mul(1_000).ok_or_else(|| {
                Error::overflow("Parquet millisecond timestamp overflows microseconds.")
            })
        }
        DataType::Timestamp(TimeUnit::Microsecond, _) => {
            Ok(downcast_array::<TimestampMicrosecondArray>(array)?.value(row))
        }
        DataType::Timestamp(TimeUnit::Nanosecond, _) => {
            Ok(downcast_array::<TimestampNanosecondArray>(array)?
                .value(row)
                .div_euclid(1_000))
        }
        _ => Err(type_mismatch(array, &LogicalType::Timestamp)),
    }
}

fn append_value(
    builder: &mut dyn ArrayBuilder,
    logical_type: &LogicalType,
    value: &Value,
) -> Result<()> {
    macro_rules! append {
        ($builder:ty, $value:expr) => {{
            let builder = downcast_builder::<$builder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                _ => builder.append_value($value),
            }
        }};
    }
    match logical_type {
        LogicalType::Bool => append!(BooleanBuilder, expect_bool(value)?),
        LogicalType::Int(IntKind::I8) => append!(
            Int8Builder,
            i8::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::Int(IntKind::I16) => append!(
            Int16Builder,
            i16::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::Int(IntKind::I32) => append!(
            Int32Builder,
            i32::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::Int(IntKind::I64) | LogicalType::Serial => append!(
            Int64Builder,
            i64::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::Int(IntKind::I128) => {
            let builder = downcast_builder::<FixedSizeBinaryBuilder>(builder, logical_type)?;
            if value.is_null() {
                builder.append_null();
            } else {
                builder
                    .append_value(expect_int(value)?.to_be_bytes())
                    .map_err(|error| {
                        Error::copy(format!("Cannot append Parquet INT128: {error}"))
                    })?;
            }
        }
        LogicalType::Int(IntKind::U8) => append!(
            UInt8Builder,
            u8::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::Int(IntKind::U16) => append!(
            UInt16Builder,
            u16::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::Int(IntKind::U32) => append!(
            UInt32Builder,
            u32::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::Int(IntKind::U64) => append!(
            UInt64Builder,
            u64::try_from(expect_int(value)?).map_err(|_| integer_range(logical_type))?
        ),
        LogicalType::UInt128 => {
            let builder = downcast_builder::<FixedSizeBinaryBuilder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                Value::UInt128(value) => {
                    builder.append_value(value.to_be_bytes()).map_err(|error| {
                        Error::copy(format!("Cannot append Parquet UINT128: {error}"))
                    })?
                }
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Decimal(precision, scale) => {
            let builder = downcast_builder::<Decimal128Builder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                Value::Decimal {
                    value,
                    precision: actual_precision,
                    scale: actual_scale,
                } if actual_precision == precision && actual_scale == scale => {
                    builder.append_value(*value)
                }
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Double => append!(Float64Builder, expect_double(value)?),
        LogicalType::Float => append!(Float32Builder, expect_float(value)?),
        LogicalType::String => {
            let builder = downcast_builder::<StringBuilder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                Value::String(value) => builder.append_value(value),
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Json => {
            let builder = downcast_builder::<StringBuilder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                Value::Json(value) => builder.append_value(value.render()),
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Blob => {
            let builder = downcast_builder::<BinaryBuilder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                Value::Blob(value) => builder.append_value(value),
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Date => append!(Date32Builder, expect_date(value)?),
        LogicalType::Timestamp => append!(TimestampMicrosecondBuilder, expect_timestamp(value)?),
        LogicalType::TimestampNs => append!(
            TimestampNanosecondBuilder,
            expect_timestamp(value)?
                .checked_mul(1_000)
                .ok_or_else(|| Error::overflow("TIMESTAMP_NS overflows Parquet nanoseconds."))?
        ),
        LogicalType::TimestampMs => append!(
            TimestampMillisecondBuilder,
            expect_timestamp(value)?.div_euclid(1_000)
        ),
        LogicalType::TimestampSec => append!(
            TimestampSecondBuilder,
            expect_timestamp(value)?.div_euclid(1_000_000)
        ),
        LogicalType::TimestampTz => append!(TimestampMicrosecondBuilder, expect_timestamp(value)?),
        LogicalType::Interval => {
            let builder = downcast_builder::<FixedSizeBinaryBuilder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                Value::Interval(value) => {
                    let mut bytes = [0u8; 16];
                    bytes[0..4].copy_from_slice(&value.months.to_le_bytes());
                    bytes[4..8].copy_from_slice(&value.days.to_le_bytes());
                    let parquet_micros = value.micros / 1_000 * 1_000;
                    bytes[8..16].copy_from_slice(&parquet_micros.to_le_bytes());
                    builder.append_value(bytes).map_err(|error| {
                        Error::copy(format!("Cannot append Parquet INTERVAL: {error}"))
                    })?;
                }
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Uuid => {
            let builder = downcast_builder::<FixedSizeBinaryBuilder>(builder, logical_type)?;
            match value {
                Value::Null => builder.append_null(),
                Value::Uuid(value) => builder
                    .append_value(value.to_be_bytes())
                    .map_err(|error| Error::copy(format!("Cannot append Parquet UUID: {error}")))?,
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::List(child) => {
            let builder =
                downcast_builder::<ListBuilder<Box<dyn ArrayBuilder>>>(builder, logical_type)?;
            match value {
                Value::Null => builder.append(false),
                Value::List(values) => {
                    for value in values {
                        append_value(builder.values().as_mut(), child, value)?;
                    }
                    builder.append(true);
                }
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Array(child, expected) => {
            let builder = downcast_builder::<FixedSizeListBuilder<Box<dyn ArrayBuilder>>>(
                builder,
                logical_type,
            )?;
            match value {
                Value::Null => {
                    for _ in 0..*expected {
                        append_value(builder.values().as_mut(), child, &Value::Null)?;
                    }
                    builder.append(false);
                }
                Value::List(values) if values.len() as u64 == *expected => {
                    for value in values {
                        append_value(builder.values().as_mut(), child, value)?;
                    }
                    builder.append(true);
                }
                Value::List(values) => {
                    return Err(Error::copy(format!(
                        "Parquet ARRAY expected {expected} elements but got {}.",
                        values.len()
                    )));
                }
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Struct(fields) => {
            let builder = downcast_builder::<StructBuilder>(builder, logical_type)?;
            match value {
                Value::Null => {
                    for (child_builder, (_, child_type)) in
                        builder.field_builders_mut().iter_mut().zip(fields)
                    {
                        append_value(child_builder.as_mut(), child_type, &Value::Null)?;
                    }
                    builder.append(false);
                }
                Value::Struct(values) if values.len() == fields.len() => {
                    for (index, ((expected_name, child_type), (actual_name, value))) in
                        fields.iter().zip(values).enumerate()
                    {
                        if expected_name != actual_name {
                            return Err(Error::copy(format!(
                                "Parquet STRUCT field mismatch. Expected `{expected_name}` but got `{actual_name}`."
                            )));
                        }
                        append_value(
                            builder.field_builders_mut()[index].as_mut(),
                            child_type,
                            value,
                        )?;
                    }
                    builder.append(true);
                }
                Value::Union {
                    variants,
                    tag,
                    value,
                } if fields.len() == variants.len() + 1 && *tag < variants.len() => {
                    append_value(
                        builder.field_builders_mut()[0].as_mut(),
                        &fields[0].1,
                        &Value::Int64(*tag as i64),
                    )?;
                    for (variant, ((expected_name, child_type), (actual_name, _))) in
                        fields.iter().skip(1).zip(variants).enumerate()
                    {
                        if expected_name != actual_name {
                            return Err(Error::copy(format!(
                                "Parquet UNION field mismatch. Expected `{expected_name}` but got `{actual_name}`."
                            )));
                        }
                        let child_value = if variant == *tag {
                            value.as_ref()
                        } else {
                            &Value::Null
                        };
                        append_value(
                            builder.field_builders_mut()[variant + 1].as_mut(),
                            child_type,
                            child_value,
                        )?;
                    }
                    builder.append(true);
                }
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Map(key_type, value_type) => {
            let builder = downcast_builder::<
                MapBuilder<Box<dyn ArrayBuilder>, Box<dyn ArrayBuilder>>,
            >(builder, logical_type)?;
            match value {
                Value::Null => builder.append(false).map_err(|error| {
                    Error::copy(format!("Cannot append NULL Parquet MAP: {error}"))
                })?,
                Value::Map(entries) => {
                    for (key, value) in entries {
                        if key.is_null() {
                            return Err(Error::copy("Parquet MAP keys cannot be NULL."));
                        }
                        let (keys, values) = builder.entries();
                        append_value(keys.as_mut(), key_type, key)?;
                        append_value(values.as_mut(), value_type, value)?;
                    }
                    builder.append(true).map_err(|error| {
                        Error::copy(format!("Cannot append Parquet MAP: {error}"))
                    })?;
                }
                _ => return Err(value_type_mismatch(logical_type, value)),
            }
        }
        LogicalType::Any => {
            let builder =
                downcast_builder::<arrow_array::builder::NullBuilder>(builder, logical_type)?;
            if !value.is_null() {
                return Err(value_type_mismatch(logical_type, value));
            }
            builder.append_null();
        }
        LogicalType::Union(_)
        | LogicalType::InternalId
        | LogicalType::Node(_)
        | LogicalType::Rel(_)
        | LogicalType::RecursiveRel => return Err(value_type_mismatch(logical_type, value)),
    }
    Ok(())
}

fn downcast_array<T: Array + 'static>(array: &dyn Array) -> Result<&T> {
    array.as_any().downcast_ref::<T>().ok_or_else(|| {
        Error::copy(format!(
            "Parquet array downcast failed for Arrow type {}.",
            array.data_type()
        ))
    })
}

fn downcast_builder<'a, T: ArrayBuilder + 'static>(
    builder: &'a mut dyn ArrayBuilder,
    logical_type: &LogicalType,
) -> Result<&'a mut T> {
    builder.as_any_mut().downcast_mut::<T>().ok_or_else(|| {
        Error::copy(format!(
            "Parquet builder does not support Koko type {logical_type}."
        ))
    })
}

fn type_mismatch(array: &dyn Array, logical_type: &LogicalType) -> Error {
    Error::copy(format!(
        "Parquet Arrow type {} cannot be decoded as {logical_type}.",
        array.data_type()
    ))
}

fn value_type_mismatch(logical_type: &LogicalType, value: &Value) -> Error {
    Error::copy(format!(
        "Cannot write value of type {} to Parquet column of type {logical_type}.",
        value.logical_type()
    ))
}

fn integer_range(logical_type: &LogicalType) -> Error {
    Error::overflow(format!(
        "Integer value is outside the range of {logical_type}."
    ))
}

fn expect_int(value: &Value) -> Result<i128> {
    match value {
        Value::Null => Ok(0),
        Value::Int64(value) => Ok(*value as i128),
        Value::IntX { value, .. } => Ok(*value),
        _ => Err(value_type_mismatch(&LogicalType::Int64, value)),
    }
}

fn expect_bool(value: &Value) -> Result<bool> {
    match value {
        Value::Null => Ok(false),
        Value::Bool(value) => Ok(*value),
        _ => Err(value_type_mismatch(&LogicalType::Bool, value)),
    }
}

fn expect_double(value: &Value) -> Result<f64> {
    match value {
        Value::Null => Ok(0.0),
        Value::Double(value) => Ok(*value),
        Value::Int64(value) => Ok(*value as f64),
        Value::IntX { value, .. } => Ok(*value as f64),
        Value::UInt128(value) => Ok(*value as f64),
        _ => Err(value_type_mismatch(&LogicalType::Double, value)),
    }
}

fn expect_float(value: &Value) -> Result<f32> {
    match value {
        Value::Null => Ok(0.0),
        Value::Float(value) => Ok(*value),
        _ => Err(value_type_mismatch(&LogicalType::Float, value)),
    }
}

fn expect_date(value: &Value) -> Result<i32> {
    match value {
        Value::Null => Ok(0),
        Value::Date(value) => Ok(*value),
        _ => Err(value_type_mismatch(&LogicalType::Date, value)),
    }
}

fn expect_timestamp(value: &Value) -> Result<i64> {
    match value {
        Value::Null => Ok(0),
        Value::Timestamp(value) | Value::TimestampTz(value) => Ok(*value),
        _ => Err(value_type_mismatch(&LogicalType::Timestamp, value)),
    }
}

fn compression(value: ParquetCompression) -> Compression {
    match value {
        ParquetCompression::Uncompressed => Compression::UNCOMPRESSED,
        ParquetCompression::Snappy => Compression::SNAPPY,
        ParquetCompression::Zstd => Compression::ZSTD(ZstdLevel::default()),
        ParquetCompression::Gzip => Compression::GZIP(GzipLevel::default()),
        ParquetCompression::Lz4Raw => Compression::LZ4_RAW,
    }
}

fn parquet_error(path: &Path, action: &str, error: impl std::fmt::Display) -> Error {
    Error::copy(format!(
        "Error {action} for Parquet file {}: {error}",
        path.display()
    ))
}

fn error_without_prefix(error: Error) -> String {
    match error {
        Error::Parser(message)
        | Error::Binder(message)
        | Error::Catalog(message)
        | Error::Runtime(message)
        | Error::Conversion(message)
        | Error::Overflow(message)
        | Error::Io(message)
        | Error::Copy(message)
        | Error::Configuration(message)
        | Error::NotImplemented(message)
        | Error::Transaction(message)
        | Error::Raw(message) => message,
        Error::Interrupt => "Interrupted.".to_string(),
        Error::BufferManager => {
            "Unable to allocate memory! The buffer pool is full and no memory could be freed!"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    fn temp_parquet(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "koko-{label}-{}-{}.parquet",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn corpus_file(relative: &str) -> Option<PathBuf> {
        let root = std::env::var_os("KOKO_ROOT_DIRECTORY").map(PathBuf::from)?;
        let path = root.join(relative);
        path.is_file().then_some(path)
    }

    #[test]
    fn reads_projected_codec_and_timestamp_corpus_files() {
        for (relative, expected_rows) in [
            ("dataset/reader/parquet/compression/zstd.parquet", 5),
            ("dataset/reader/parquet/compression/gzip.parquet", 5),
            ("dataset/reader/parquet/compression/brotli.parquet", 5),
            (
                "dataset/reader/parquet/timestamp/impala_timestamp.parquet",
                3,
            ),
            (
                "dataset/reader/parquet/timestamp/timestamp_ms_ns.parquet",
                3,
            ),
        ] {
            let Some(path) = corpus_file(relative) else {
                return;
            };
            let metadata = inspect(&path).unwrap();
            assert_eq!(metadata.num_rows, expected_rows);
            assert!(!metadata.schema.fields.is_empty());
            let projection = vec![metadata.schema.fields[0].name.to_ascii_uppercase()];
            let mut reader = ParquetReader::open_projected(&path, &projection).unwrap();
            assert_eq!(reader.schema().fields.len(), 1);
            let mut rows = 0;
            while let Some(chunk) = reader.next_chunk().unwrap() {
                assert!(chunk.size() <= VECTOR_CAPACITY);
                rows += chunk.size() as u64;
            }
            assert_eq!(rows, expected_rows);
        }

        let zstd = corpus_file("dataset/reader/parquet/compression/zstd.parquet").unwrap();
        let mut reader = ParquetReader::open(&zstd).unwrap();
        assert_eq!(
            reader.schema().types(),
            vec![
                LogicalType::Int64,
                LogicalType::String,
                LogicalType::List(Box::new(LogicalType::Int64)),
            ]
        );
        let chunk = reader.next_chunk().unwrap().unwrap();
        assert_eq!(chunk.columns[0].get_value(0), Value::Int64(3));
        assert_eq!(chunk.columns[1].get_value(0), Value::Null);
        assert_eq!(
            chunk.columns[2].get_value(0),
            Value::List(vec![Value::Int64(2), Value::Int64(4), Value::Int64(3)])
        );

        let timestamp =
            corpus_file("dataset/reader/parquet/timestamp/timestamp_ms_ns.parquet").unwrap();
        let projection = vec![
            "timestamp_ns_column".to_string(),
            "timestamp_ms_column".to_string(),
        ];
        let mut reader = ParquetReader::open_projected(&timestamp, &projection).unwrap();
        assert_eq!(
            reader.schema().types(),
            vec![LogicalType::Timestamp, LogicalType::TimestampMs]
        );
        let chunk = reader.next_chunk().unwrap().unwrap();
        assert_eq!(
            chunk.columns[0].get_value(0),
            Value::Timestamp(1_335_885_204_000_000)
        );
        assert_eq!(
            chunk.columns[1].get_value(0),
            Value::Timestamp(1_635_885_200_000_000)
        );

        let impala =
            corpus_file("dataset/reader/parquet/timestamp/impala_timestamp.parquet").unwrap();
        let mut reader = ParquetReader::open(&impala).unwrap();
        let chunk = reader.next_chunk().unwrap().unwrap();
        assert_eq!(
            chunk.columns[0].get_value(0),
            Value::Timestamp(1_698_926_400_000_000)
        );

        let large = corpus_file("dataset/copy-test/node/parquet/types_50k_1.parquet").unwrap();
        let metadata = inspect(&large).unwrap();
        assert_eq!(
            metadata.schema.types(),
            vec![
                LogicalType::Int64,
                LogicalType::Int64,
                LogicalType::Double,
                LogicalType::Bool,
                LogicalType::Date,
                LogicalType::String,
                LogicalType::List(Box::new(LogicalType::Int64)),
                LogicalType::List(Box::new(LogicalType::String)),
                LogicalType::List(Box::new(LogicalType::List(Box::new(LogicalType::Int64,)))),
                LogicalType::Struct(vec![
                    ("id".to_string(), LogicalType::Int64),
                    ("name".to_string(), LogicalType::String),
                ]),
            ]
        );
        let projection = vec![metadata.schema.fields[0].name.clone()];
        let mut reader = ParquetReader::open_projected(&large, &projection).unwrap();
        let mut rows = 0u64;
        let mut batches = 0usize;
        while let Some(chunk) = reader.next_chunk().unwrap() {
            assert!(chunk.size() <= VECTOR_CAPACITY);
            rows += chunk.size() as u64;
            batches += 1;
        }
        assert_eq!(rows, metadata.num_rows);
        assert!(batches > 1);
    }

    #[test]
    fn round_trips_exact_scalar_null_and_nested_types() {
        let fields = vec![
            ParquetField::new("id", LogicalType::Int(IntKind::I32), false),
            ParquetField::new("tiny", LogicalType::Int(IntKind::I8), true),
            ParquetField::new("unsigned", LogicalType::Int(IntKind::U64), true),
            ParquetField::new("wide", LogicalType::Int(IntKind::I128), true),
            ParquetField::new("uwide", LogicalType::UInt128, true),
            ParquetField::new("ratio", LogicalType::Float, true),
            ParquetField::new("score", LogicalType::Double, true),
            ParquetField::new("at", LogicalType::Timestamp, true),
            ParquetField::new("at_tz", LogicalType::TimestampTz, true),
            ParquetField::new("flag", LogicalType::Bool, true),
            ParquetField::new("name", LogicalType::String, true),
            ParquetField::new("payload", LogicalType::Blob, true),
            ParquetField::new("amount", LogicalType::Decimal(12, 3), true),
            ParquetField::new("day", LogicalType::Date, true),
            ParquetField::new("at_ms", LogicalType::TimestampMs, true),
            ParquetField::new("at_ns", LogicalType::TimestampNs, true),
            ParquetField::new(
                "tags",
                LogicalType::List(Box::new(LogicalType::String)),
                true,
            ),
            ParquetField::new(
                "coords",
                LogicalType::Array(Box::new(LogicalType::Double), 2),
                true,
            ),
            ParquetField::new(
                "attrs",
                LogicalType::Map(Box::new(LogicalType::String), Box::new(LogicalType::Int64)),
                true,
            ),
            ParquetField::new(
                "profile",
                LogicalType::Struct(vec![
                    ("age".to_string(), LogicalType::Int64),
                    ("active".to_string(), LogicalType::Bool),
                ]),
                true,
            ),
        ];
        let schema = ParquetSchema::new(fields).unwrap();
        let types = schema.types();
        let mut input = DataChunk::new(&types);
        let row0 = vec![
            Value::IntX {
                value: 7,
                kind: IntKind::I32,
            },
            Value::IntX {
                value: -8,
                kind: IntKind::I8,
            },
            Value::IntX {
                value: u64::MAX as i128,
                kind: IntKind::U64,
            },
            Value::IntX {
                value: i128::MIN + 17,
                kind: IntKind::I128,
            },
            Value::UInt128(u128::MAX - 23),
            Value::Float(1.25),
            Value::Double(-9.5),
            Value::Timestamp(1_637_091_234_567_890),
            Value::TimestampTz(1_637_091_234_567_890),
            Value::Bool(true),
            Value::String("Ada".to_string()),
            Value::Blob(vec![0, 1, 255]),
            Value::Decimal {
                value: 123_456,
                precision: 12,
                scale: 3,
            },
            Value::Date(19_000),
            Value::Timestamp(1_637_091_234_000_000),
            Value::Timestamp(1_637_091_234_567_000),
            Value::List(vec![Value::String("graph".to_string()), Value::Null]),
            Value::List(vec![Value::Double(1.25), Value::Double(-2.5)]),
            Value::Map(vec![
                (Value::String("x".to_string()), Value::Int64(1)),
                (Value::String("y".to_string()), Value::Null),
            ]),
            Value::Struct(vec![
                ("age".to_string(), Value::Int64(37)),
                ("active".to_string(), Value::Bool(true)),
            ]),
        ];
        for (index, (column, value)) in input.columns.iter_mut().zip(row0).enumerate() {
            column.set_value_owned(0, value);
            column.set_value_owned(
                1,
                if index == 0 {
                    Value::IntX {
                        value: 8,
                        kind: IntKind::I32,
                    }
                } else {
                    Value::Null
                },
            );
        }
        input.set_flat(2);

        let path = temp_parquet("roundtrip");
        let mut writer =
            ParquetFileWriter::create(&path, schema.clone(), ParquetWriterOptions::default())
                .unwrap();
        writer.write_chunk(&input).unwrap();
        assert_eq!(writer.finish().unwrap(), 2);

        let metadata = inspect(&path).unwrap();
        assert_eq!(metadata.schema, schema);
        assert_eq!(metadata.num_rows, 2);
        let chunks = ParquetReader::open(&path)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].size(), 2);
        for (input_column, output_column) in input.columns.iter().zip(&chunks[0].columns) {
            assert_eq!(input_column.get_value(0), output_column.get_value(0));
            assert_eq!(input_column.get_value(1), output_column.get_value(1));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn exact_schema_diagnostics_are_stable() {
        let actual =
            ParquetSchema::new(vec![ParquetField::new("id", LogicalType::Int64, false)]).unwrap();
        let expected =
            ParquetSchema::new(vec![ParquetField::new("id", LogicalType::String, false)]).unwrap();
        assert_eq!(
            actual.validate_exact(&expected).unwrap_err().to_string(),
            "Binder exception: Column `id` type mismatch. Expected STRING but got INT64."
        );
        assert_eq!(
            resolve_projection(&actual, &[String::from("missing")])
                .unwrap_err()
                .to_string(),
            "Binder exception: Parquet column `missing` does not exist."
        );
    }

    #[test]
    fn empty_file_keeps_schema_and_yields_no_batches() {
        let path = temp_parquet("empty");
        let schema =
            ParquetSchema::new(vec![ParquetField::new("id", LogicalType::Int64, false)]).unwrap();
        let mut writer = ParquetFileWriter::create(
            &path,
            schema.clone(),
            ParquetWriterOptions {
                compression: ParquetCompression::Gzip,
            },
        )
        .unwrap();
        let empty_chunk = DataChunk::new(&schema.types());
        writer.write_chunk(&empty_chunk).unwrap();
        assert_eq!(writer.rows_written(), 0);
        assert_eq!(writer.finish().unwrap(), 0);
        let metadata = inspect(&path).unwrap();
        assert_eq!(metadata.schema, schema);
        assert_eq!(metadata.num_rows, 0);
        assert!(ParquetReader::open(&path).unwrap().next().is_none());
        std::fs::remove_file(path).unwrap();
    }
}
