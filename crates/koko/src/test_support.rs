//! First-party corpus support. Not a stable embedding API.

use crate::{Connection, Result};
use std::path::Path;

pub fn load_csv_dataset(connection: &Connection, directory: &Path) -> Result<()> {
    connection.load_csv_dataset(directory)
}
