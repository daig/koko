//! The unified error model shared by every layer.
//!
//! Each layer raises a variant of [`Error`]; the `Display` text mirrors the
//! C++ engine's exception prefixes (`Binder exception: …`, `Runtime exception:
//! …`, …) so that the `.test` corpus — which embeds expected error strings —
//! can serve as a differential oracle without translation.

/// The crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A categorized engine error.
///
/// Categories match the stages of the pipeline so callers can `match` on where
/// a failure originated. The wrapped `String` is the human-facing message; the
/// category prefix is supplied by the `Display` impl.
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Lexing/parsing failure (malformed Cypher).
    #[error("Parser exception: {0}")]
    Parser(String),
    /// Name/type resolution failure (unknown table, property, function; type mismatch).
    #[error("Binder exception: {0}")]
    Binder(String),
    /// Catalog invariant violation (duplicate table, etc.).
    #[error("Catalog exception: {0}")]
    Catalog(String),
    /// Failure while executing a correct plan (e.g. primary-key violation).
    #[error("Runtime exception: {0}")]
    Runtime(String),
    /// A value could not be converted to the requested type.
    #[error("Conversion exception: {0}")]
    Conversion(String),
    /// Arithmetic overflow.
    #[error("Overflow exception: {0}")]
    Overflow(String),
    /// I/O failure.
    #[error("IO exception: {0}")]
    Io(String),
    /// CSV/COPY reader failure (matches the C++ `CopyException` prefix).
    #[error("Copy exception: {0}")]
    Copy(String),
    /// Invalid database construction or resource configuration.
    #[error("Configuration exception: {0}")]
    Configuration(String),
    /// Cooperative query cancellation or deadline expiry.
    #[error("Interrupted.")]
    Interrupt,
    /// Tracked memory admission failed at the configured database limit.
    #[error(
        "Buffer manager exception: Unable to allocate memory! The buffer pool is full and no memory could be freed!"
    )]
    BufferManager,
    /// A surface that is recognized but not yet implemented in this phase.
    #[error("Not implemented exception: {0}")]
    NotImplemented(String),
    /// A transaction-control error. The engine emits these without an exception
    /// prefix (e.g. `No active transaction for COMMIT.`), so neither does `Display`.
    #[error("{0}")]
    Transaction(String),
    /// A message emitted with no exception-class prefix at all (e.g. the C++
    /// bare-`STRUCT` type parse failure `Cannot parse struct type: STRUCT`).
    #[error("{0}")]
    Raw(String),
}

impl Error {
    pub fn parser(msg: impl Into<String>) -> Self {
        Error::Parser(msg.into())
    }
    pub fn transaction(msg: impl Into<String>) -> Self {
        Error::Transaction(msg.into())
    }
    pub fn binder(msg: impl Into<String>) -> Self {
        Error::Binder(msg.into())
    }
    pub fn catalog(msg: impl Into<String>) -> Self {
        Error::Catalog(msg.into())
    }
    pub fn runtime(msg: impl Into<String>) -> Self {
        Error::Runtime(msg.into())
    }
    pub fn conversion(msg: impl Into<String>) -> Self {
        Error::Conversion(msg.into())
    }
    pub fn overflow(msg: impl Into<String>) -> Self {
        Error::Overflow(msg.into())
    }
    pub fn copy(msg: impl Into<String>) -> Self {
        Error::Copy(msg.into())
    }
    pub fn configuration(msg: impl Into<String>) -> Self {
        Error::Configuration(msg.into())
    }
    pub const fn interrupt() -> Self {
        Error::Interrupt
    }
    pub const fn buffer_manager() -> Self {
        Error::BufferManager
    }
    pub fn not_implemented(msg: impl Into<String>) -> Self {
        Error::NotImplemented(msg.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
