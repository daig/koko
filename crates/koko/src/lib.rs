//! `koko` — the public, idiomatic Rust API for the Koko engine.
//!
//! Results are eagerly materialized in private columnar buffers. [`Row`] and
//! the views in [`result`] borrow those buffers without copying.
//!
//! ```no_run
//! use koko::{Database, Result, params};
//!
//! fn main() -> Result<()> {
//!     let database = Database::new();
//!     let mut connection = database.connect();
//!     connection.execute(
//!         "CREATE NODE TABLE Person(id INT64, name STRING, PRIMARY KEY(id))",
//!     )?;
//!     connection.execute_with(
//!         "CREATE (:Person {id: $id, name: $name})",
//!         params! { "id" => 1, "name" => "Alice" },
//!     )?;
//!
//!     let result = connection.execute_with(
//!         "MATCH (p:Person) WHERE p.id >= $min RETURN p.name",
//!         params! { "min" => 1 },
//!     )?;
//!     for row in &result {
//!         println!("{}", row.get::<String>("name")?);
//!     }
//!
//!     let transaction = connection.transaction()?;
//!     transaction.execute("CREATE (:Person {id: 2, name: 'Bob'})")?;
//!     transaction.commit()?;
//!     Ok(())
//! }
//! ```

mod arrow;
pub mod config;
mod copy;
pub mod diagnostics;
pub mod execution;
pub mod function;
mod interchange;
mod macros;
pub mod prepared;
pub mod result;
mod runtime;
#[doc(hidden)]
pub mod test_support;
pub mod tooling;
pub mod transaction;
pub mod value;

#[cfg(test)]
mod tests;

pub use config::DatabaseConfig;
pub use execution::Parameter;
pub use function::ScalarFunction;
pub use koko_common::{Error, Result};
pub use prepared::PreparedStatement;
pub use result::{QueryResult, Row};
pub use runtime::{Connection, Database, InterruptHandle};
pub use transaction::Transaction;
pub use value::{LogicalType, Value};
