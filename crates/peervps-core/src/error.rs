use thiserror::Error;

/// Crate-wide error type.
#[derive(Debug, Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("insufficient capacity: {0}")]
    Capacity(String),

    #[error("insufficient funds: need {needed} µcredits, have {available}")]
    InsufficientFunds { needed: i64, available: i64 },

    #[error("invalid argument: {0}")]
    Invalid(String),

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("cryptographic failure: {0}")]
    Crypto(String),

    #[error("hypervisor error: {0}")]
    Hypervisor(String),

    #[error("feature unavailable on this host: {0}")]
    Unsupported(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("background task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
