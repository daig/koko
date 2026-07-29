//! `koko-loader` — typed CSV, Parquet, and NPY loading plus external scan adapters.
//!
//! Node CSV rows use schema property order. Relationship CSV rows begin with
//! FROM/TO primary keys followed by properties; endpoint IDs are resolved by the
//! storage adapter. Format-specific modules own decoding, not transaction policy.

pub mod csv;
pub mod icebug;
pub mod npy;
pub mod parquet;
pub mod scan;

pub use csv::{
    CopyContext, apply_column_defaults, copy_expected_arity, copy_from_csv, parse_cell,
    validate_copy_file_arity,
};
pub use npy::{
    NpyBatchReader, NpyColumnMetadata, NpyDType, NpyMetadata, inspect_npy, preflight_npy,
};
pub use parquet::{
    ParquetCompression, ParquetField, ParquetFileMetadata, ParquetFileWriter, ParquetReader,
    ParquetSchema, ParquetWriterOptions, inspect as inspect_parquet,
};
