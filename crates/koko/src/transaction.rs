//! Statically scoped explicit transactions.

use crate::{Connection, Result};

/// An RAII handle that exclusively borrows a connection for one transaction.
///
/// Committing publishes writes:
///
/// ```
/// # use koko::Database;
/// # fn main() -> koko::Result<()> {
/// let database = Database::new();
/// let mut connection = database.connect();
/// connection.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")?;
/// let transaction = connection.transaction()?;
/// transaction.execute("CREATE (:P {id: 1})")?;
/// transaction.commit()?;
/// # Ok(())
/// # }
/// ```
///
/// Dropping without committing rolls back:
///
/// ```
/// # use koko::Database;
/// # fn main() -> koko::Result<()> {
/// let database = Database::new();
/// let mut connection = database.connect();
/// connection.execute("CREATE NODE TABLE P(id INT64, PRIMARY KEY(id))")?;
/// {
///     let transaction = connection.transaction()?;
///     transaction.execute("CREATE (:P {id: 1})")?;
/// }
/// assert!(connection.execute("MATCH (p:P) RETURN p")?.is_empty());
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping a transaction without committing rolls it back"]
pub struct Transaction<'conn> {
    conn: &'conn mut Connection,
    finished: bool,
}

impl<'conn> Transaction<'conn> {
    pub(crate) const fn new(conn: &'conn mut Connection) -> Self {
        Self {
            conn,
            finished: false,
        }
    }

    /// Commit the transaction, publishing its writes to all connections.
    pub fn commit(mut self) -> Result<()> {
        self.conn
            .handle_transaction_op(koko_parser::ast::TxnOp::Commit)?;
        self.finished = true;
        Ok(())
    }

    /// Roll the transaction back, discarding its writes.
    pub fn rollback(mut self) -> Result<()> {
        self.conn
            .handle_transaction_op(koko_parser::ast::TxnOp::Rollback)?;
        self.finished = true;
        Ok(())
    }
}

impl std::ops::Deref for Transaction<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn
    }
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Transaction")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self
                .conn
                .handle_transaction_op(koko_parser::ast::TxnOp::Rollback);
        }
    }
}
