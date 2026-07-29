use koko_common::TableId;

/// The observable implementation kind of a named primary-key index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexType {
    Hash,
    Art,
}

impl IndexType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hash => "HASH",
            Self::Art => "ART",
        }
    }
}

/// Catalog metadata for one explicitly named in-memory primary-key index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub(crate) name: String,
    pub(crate) table_id: TableId,
    pub(crate) index_type: IndexType,
    pub(crate) property_names: Vec<String>,
}

impl IndexEntry {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn table_id(&self) -> TableId {
        self.table_id
    }

    pub const fn index_type(&self) -> IndexType {
        self.index_type
    }

    pub fn property_names(&self) -> &[String] {
        &self.property_names
    }
}

/// Outcome of idempotent index creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateIndexOutcome {
    Created,
    Existing(String),
}
