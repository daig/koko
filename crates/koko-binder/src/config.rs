//! Binder-owned session policy and external-schema hooks.

use koko_common::{LogicalType, RegisteredScalarFunction, file_resolver::FileFormat};
use std::collections::HashMap;
use std::sync::Arc;

/// The default upper bound for an unbounded variable-length pattern and the
/// maximum a query may request.
pub const MAX_RECURSIVE_DEPTH: u32 = 30;

/// Host-provided metadata inspection for non-CSV local sources.
pub type FileSchemaResolver =
    fn(FileFormat, &[String]) -> koko_common::Result<Vec<(String, LogicalType)>>;

/// Session-level configuration consulted during binding.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub var_length_extend_max_depth: u32,
    pub disable_map_key_check: bool,
    pub base_dir: std::path::PathBuf,
    pub home_directory: Option<std::path::PathBuf>,
    pub file_search_path: String,
    pub file_schema_resolver: Option<FileSchemaResolver>,
    pub scalar_udfs: Arc<HashMap<String, Arc<RegisteredScalarFunction>>>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            var_length_extend_max_depth: MAX_RECURSIVE_DEPTH,
            disable_map_key_check: true,
            base_dir: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            home_directory: std::env::var_os("HOME").map(std::path::PathBuf::from),
            file_search_path: String::new(),
            file_schema_resolver: None,
            scalar_udfs: Arc::new(HashMap::new()),
        }
    }
}
