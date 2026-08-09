//! Name and type resolution for parsed Cypher statements.
//!
//! The crate exposes statement binding, preparation metadata, session binding
//! policy, and table-function schema validation. Bound representation types live
//! in `koko-ir`; row production lives in `koko-processor`.

mod binder;
pub mod config;
mod entry;
mod load_options;
mod table_function;

pub use binder::bind_standalone_table_call;
pub use entry::{PreparedBinding, bind_config_value, bind_statement, bind_statement_for_prepare};
pub use table_function::schema as table_func_schema;
