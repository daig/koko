//! In-memory schema catalog and read-only catalog entry API.

mod catalog;
mod column;
mod external;
mod index;
mod node;
mod relation;
mod sequence;

pub use catalog::{Catalog, TableKind};
pub use column::{Column, ColumnDefault, ColumnDefinition, ColumnGeneration};
pub use external::{AnyTables, IcebugTable, IcebugTableSource};
pub use index::{CreateIndexOutcome, IndexEntry, IndexType};
pub use node::{NodeTable, NodeTableDefinition};
pub use relation::{RelTable, RelTableDefinition, RelTablePair};
pub use sequence::{Sequence, serial_sequence_name};
