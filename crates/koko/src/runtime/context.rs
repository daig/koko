//! Per-statement inputs, cancellation controls, and coherent execution snapshots.

use super::graph::GraphState;
use crate::macros::MacroRegistry;
use crate::{DatabaseConfig, Error, LogicalType, Result, Value};
use koko_binder::config::SessionConfig;
use koko_catalog::Catalog;
use koko_common::{MemoryTracker, ReadView, TableId, Ts};
use koko_storage::{SharedStorage, StorageReadHandle, StorageWriteHandle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

/// The error when a statement that mutates the database runs inside a `BEGIN …
/// READ ONLY` transaction (matches the C++ engine — note the `Can not` spelling).
pub(crate) const READ_ONLY_WRITE_MSG: &str =
    "Can not execute a write query inside a read-only transaction.";

#[derive(Clone)]
pub(super) struct StatementControl {
    pub(super) interrupt_epoch: Arc<AtomicU64>,
    pub(super) captured_epoch: u64,
    pub(super) started: Instant,
}

/// Immutable statement inputs plus statement-scoped sinks. A standalone
/// `CALL k=v` records one pending update, committed to the owning connection
/// only after successful execution.
pub(super) struct QueryContext {
    pub(super) parameters: HashMap<String, Value>,
    pub(super) view: ReadView,
    pub(super) settings: koko_common::settings::SessionSettings,
    pub(super) base_dir: PathBuf,
    pub(super) warnings: koko_common::warnings::WarningSink,
    pub(super) random: koko_function::oracle_hash::RandomState,
    pub(super) scalar_udfs: Arc<HashMap<String, Arc<koko_common::RegisteredScalarFunction>>>,
    pub(super) setting_update: Option<(String, Value)>,
    pub(super) show_table_rows: Option<Vec<Vec<Value>>>,
    pub(super) compilation_time: Duration,
    pub(super) interrupt_epoch: Arc<AtomicU64>,
    pub(super) captured_epoch: u64,
    pub(super) deadline: Option<Instant>,
}

impl QueryContext {
    pub(super) fn binder_config(&self) -> SessionConfig {
        let mut config = binder_config_from_settings(&self.settings);
        config.base_dir = self.base_dir.clone();
        config.file_schema_resolver = Some(inspect_file_schema);
        config.scalar_udfs = Arc::clone(&self.scalar_udfs);
        config
    }

    pub(super) fn optimizer_enabled(&self) -> bool {
        self.settings
            .current("enable_plan_optimizer")
            .as_bool()
            .unwrap_or(true)
    }

    pub(super) fn storage_read(&self) -> StorageReadHandle {
        StorageReadHandle::new(self.view)
    }

    pub(super) fn storage_write(&self) -> Result<StorageWriteHandle> {
        let writer_id = self
            .view
            .writer_id
            .ok_or_else(|| Error::transaction(READ_ONLY_WRITE_MSG))?;
        Ok(StorageWriteHandle::new(self.view, writer_id))
    }

    pub(super) fn worker_count(&self) -> usize {
        self.settings
            .current("threads")
            .as_int128()
            .unwrap_or(1)
            .clamp(1, usize::MAX as i128) as usize
    }

    pub(super) fn query_control(&self) -> koko_common::QueryControl<'_> {
        koko_common::QueryControl::new(&self.interrupt_epoch, self.captured_epoch, self.deadline)
    }
}

/// The MVCC read view for a statement (P3): read committed state at `read_ts`, plus
/// the writer's own still-uncommitted versions when `writer_id` is set.
pub(super) fn mvcc_view(read_ts: Ts, writer_id: Option<Ts>) -> ReadView {
    match writer_id {
        Some(id) => ReadView::writer(read_ts, id),
        None => ReadView::reader(read_ts),
    }
}

fn inspect_file_schema(
    format: koko_common::file_resolver::FileFormat,
    paths: &[String],
) -> Result<Vec<(String, koko_common::LogicalType)>> {
    match format {
        koko_common::file_resolver::FileFormat::Csv => unreachable!("CSV is sniffed by the binder"),
        koko_common::file_resolver::FileFormat::Parquet => {
            let first = koko_loader::parquet::inspect(&paths[0])?;
            for path in &paths[1..] {
                let metadata = koko_loader::parquet::inspect(path)?;
                metadata.schema.validate_exact(&first.schema)?;
            }
            Ok(first
                .schema
                .fields
                .into_iter()
                .map(|field| (field.name, field.logical_type))
                .collect())
        }
        koko_common::file_resolver::FileFormat::Npy => {
            let npy_paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
            let preflight = koko_loader::npy::preflight_npy(&npy_paths, None)?;
            Ok(preflight
                .columns
                .into_iter()
                .enumerate()
                .map(|(index, column)| (format!("column{index}"), column.logical_type))
                .collect())
        }
    }
}

pub(super) fn mvcc_write(read_ts: Ts, writer_id: Ts) -> StorageWriteHandle {
    StorageWriteHandle::new(mvcc_view(read_ts, Some(writer_id)), writer_id)
}

/// Whether to run the logical-plan optimizer (P3 step 5+). On by default; the
/// `KOKO_NO_OPTIMIZE` env var bypasses it (a debug/bisection escape hatch, the
/// analog of the C++ `enablePlanOptimizer` client config). Read once.
/// C++-parity defaults for `current_setting` knobs that were never `CALL`-set
/// (audit V15). `threads` mirrors the machine's parallelism like C++.
/// The standalone-`CALL` option registry: name (lowercased) → declared input type.
/// Mirrors the C++ `DBConfig` options table (src/include/main/settings.h); a name
/// outside it is "Invalid option name". Extension options require an IM5 scope decision.
pub(super) const RUNTIME_SETTING_SPECS: &[(&str, LogicalType)] = &[
    ("threads", LogicalType::Int(koko_common::IntKind::U64)),
    ("warning_limit", LogicalType::Int(koko_common::IntKind::U64)),
    ("timeout", LogicalType::Int(koko_common::IntKind::U64)),
    (
        "var_length_extend_max_depth",
        LogicalType::Int(koko_common::IntKind::I64),
    ),
    (
        "sparse_frontier_threshold",
        LogicalType::Int(koko_common::IntKind::I64),
    ),
    (
        "recursive_pattern_factor",
        LogicalType::Int(koko_common::IntKind::I64),
    ),
    (
        "checkpoint_threshold",
        LogicalType::Int(koko_common::IntKind::I64),
    ),
    ("progress_bar", LogicalType::Bool),
    ("enable_semi_mask", LogicalType::Bool),
    ("disable_map_key_check", LogicalType::Bool),
    ("enable_zone_map", LogicalType::Bool),
    ("debug_enable_multi_writes", LogicalType::Bool),
    ("auto_checkpoint", LogicalType::Bool),
    ("force_checkpoint_on_close", LogicalType::Bool),
    ("enable_default_hash_index", LogicalType::Bool),
    ("spill_to_disk", LogicalType::Bool),
    ("enable_plan_optimizer", LogicalType::Bool),
    ("enable_internal_catalog", LogicalType::Bool),
    ("home_directory", LogicalType::String),
    ("file_search_path", LogicalType::String),
    ("recursive_pattern_semantic", LogicalType::String),
];

/// Immutable execution inputs captured while briefly holding graph/database coordinators.
pub(super) struct ConcurrentQuerySnapshot {
    pub(super) graph: Arc<GraphState>,
    pub(super) catalog: Arc<Catalog>,
    pub(super) macros: Arc<MacroRegistry>,
    pub(super) catalog_version: u64,
    pub(super) storage: Arc<SharedStorage>,
    pub(super) config: DatabaseConfig,
    pub(super) memory: MemoryTracker,
    pub(super) read_ts: Ts,
    pub(super) writer_id: Option<Ts>,
    pub(super) mark: usize,
    pub(super) rel_base: HashMap<TableId, u64>,
    pub(super) sequence_before: Option<Vec<(String, i64, u64)>>,
    pub(super) show_table_rows: Vec<Vec<Value>>,
}

pub(super) fn binder_config_from_settings(
    settings: &koko_common::settings::SessionSettings,
) -> SessionConfig {
    let mut config = SessionConfig::default();
    if let Some(depth) = settings
        .get("var_length_extend_max_depth")
        .and_then(Value::as_i64)
    {
        config.var_length_extend_max_depth = depth.max(0) as u32;
    }
    if let Some(disable) = settings
        .get("disable_map_key_check")
        .and_then(Value::as_bool)
    {
        config.disable_map_key_check = disable;
    }
    if let Some(home) = settings.get("home_directory").and_then(Value::as_str) {
        config.home_directory = if home.is_empty() {
            None
        } else {
            Some(home.into())
        };
    }
    if let Some(search) = settings.get("file_search_path").and_then(Value::as_str) {
        config.file_search_path = search.to_string();
    }
    config
}
