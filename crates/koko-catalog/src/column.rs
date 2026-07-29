use koko_common::{ColumnId, LogicalType, Value};

/// A resolved explicit column default.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnDefault {
    /// A constant value folded before it reaches the catalog.
    Constant(Value),
    /// `nextval('sequence')`, evaluated once per inserted row.
    NextVal(String),
}

/// A schema-definition column's mutually exclusive generation mode.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnGeneration {
    None,
    Default {
        value: ColumnDefault,
        source_text: String,
    },
    Serial,
}

/// One column supplied to a table-definition operation.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDefinition {
    pub name: String,
    pub logical_type: LogicalType,
    pub type_text: String,
    pub generation: ColumnGeneration,
}

impl ColumnDefinition {
    pub fn plain(name: impl Into<String>, logical_type: LogicalType) -> Self {
        let type_text = logical_type.to_string();
        Self {
            name: name.into(),
            logical_type,
            type_text,
            generation: ColumnGeneration::None,
        }
    }

    pub fn with_default(
        name: impl Into<String>,
        logical_type: LogicalType,
        value: ColumnDefault,
        source_text: impl Into<String>,
    ) -> Self {
        let type_text = logical_type.to_string();
        Self {
            name: name.into(),
            logical_type,
            type_text,
            generation: ColumnGeneration::Default {
                value,
                source_text: source_text.into(),
            },
        }
    }

    pub fn serial(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            logical_type: LogicalType::Serial,
            type_text: "SERIAL".to_string(),
            generation: ColumnGeneration::Serial,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum StoredColumnGeneration {
    None,
    Default(ColumnDefault),
    Serial { sequence: String },
}

/// A catalog-owned table column.
#[derive(Debug, Clone)]
pub struct Column {
    pub(crate) name: String,
    pub(crate) logical_type: LogicalType,
    pub(crate) type_text: String,
    pub(crate) column_id: ColumnId,
    pub(crate) generation: StoredColumnGeneration,
    pub(crate) default_text: String,
}

impl Column {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn logical_type(&self) -> &LogicalType {
        &self.logical_type
    }

    pub fn type_text(&self) -> &str {
        &self.type_text
    }

    pub const fn column_id(&self) -> ColumnId {
        self.column_id
    }

    pub fn default(&self) -> Option<ColumnDefault> {
        match &self.generation {
            StoredColumnGeneration::None => None,
            StoredColumnGeneration::Default(value) => Some(value.clone()),
            StoredColumnGeneration::Serial { sequence } => {
                Some(ColumnDefault::NextVal(sequence.clone()))
            }
        }
    }

    pub fn default_text(&self) -> &str {
        &self.default_text
    }

    /// Whether this column owns a catalog-managed implicit `SERIAL` sequence.
    pub fn is_serial(&self) -> bool {
        matches!(self.generation, StoredColumnGeneration::Serial { .. })
    }

    pub(crate) fn serial_sequence(&self) -> Option<&str> {
        match &self.generation {
            StoredColumnGeneration::Serial { sequence } => Some(sequence),
            StoredColumnGeneration::None | StoredColumnGeneration::Default(_) => None,
        }
    }
}
