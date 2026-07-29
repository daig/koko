//! Shared property-graph value contracts.

/// Relationship-table storage direction metadata (`WITH (storage_direction=...)`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RelStorageDirection {
    Fwd,
    Bwd,
    #[default]
    Both,
}

impl RelStorageDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fwd => "fwd",
            Self::Bwd => "bwd",
            Self::Both => "both",
        }
    }
}
