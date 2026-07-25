//! Connection-owned runtime settings (`CALL k=v` / `current_setting(k)`).
//!
//! [`SessionSettings`] is a plain value. The connection owns the authoritative
//! instance and copies it into each statement's query context, so reads never
//! consult process-global state.

use crate::value::Value;
use std::collections::HashMap;

/// The settings visible to one connection.
#[derive(Debug, Clone, Default)]
pub struct SessionSettings {
    values: HashMap<String, Value>,
}

impl SessionSettings {
    /// Record `key` (lowercased by the caller) = `value`.
    pub fn set(&mut self, key: &str, value: Value) {
        self.values.insert(key.to_string(), value);
    }

    /// The explicitly recorded value for `key`, if any.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// The value `current_setting(key)` reports: the connection's value, the
    /// C++ default, or the empty string.
    pub fn current(&self, key: &str) -> Value {
        self.get(key)
            .cloned()
            .or_else(|| default_setting(key))
            .unwrap_or(Value::String(String::new()))
    }

    /// Initialize process-environment compatibility controls once, at
    /// connection construction rather than on a planning/execution hot path.
    pub fn with_environment_defaults() -> Self {
        let mut settings = Self::default();
        if let Ok(threads) = std::env::var("KOKO_THREADS").and_then(|raw| {
            raw.parse::<i64>()
                .map_err(|error| std::env::VarError::NotUnicode(error.to_string().into()))
        }) {
            settings.set("threads", Value::Int64(threads.max(1)));
        }
        if std::env::var_os("KOKO_NO_OPTIMIZE").is_some() {
            settings.set("enable_plan_optimizer", Value::Bool(false));
        }
        settings
    }
}

/// The C++ default for an unset knob (audit V15) — reported by
/// `current_setting` instead of an empty string.
pub fn default_setting(key: &str) -> Option<Value> {
    Some(match key {
        "threads" => Value::Int64(
            std::thread::available_parallelism()
                .map(|n| n.get() as i64)
                .unwrap_or(1),
        ),
        "var_length_extend_max_depth" => Value::Int64(30),
        "timeout" => Value::Int64(0),
        "progress_bar" => Value::Bool(false),
        "checkpoint_threshold" => Value::Int64(16_777_216),
        // Optimizer/planner knobs enabled by default (oracle-verified).
        "enable_plan_optimizer" | "enable_semi_mask" | "enable_zone_map" => Value::Bool(true),
        "sparse_frontier_threshold" => Value::Int64(1000),
        "recursive_pattern_factor" => Value::Int64(100),
        _ => return None,
    })
}
