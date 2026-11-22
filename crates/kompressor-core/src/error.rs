//! Errors that reach a caller.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config {0}: {1}")]
    Read(String, #[source] std::io::Error),
    #[error("could not parse config: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("ref store: {0}")]
    Store(#[from] kompressor_store::types::StoreError),
    #[error("statistics: {0}")]
    Stats(#[from] kompressor_stats::error::StatsError),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The S3-compatible store refused, or could not be reached. Never holds
    /// a credential.
    #[error("object storage: {0}")]
    ObjectStore(String),
}

/// Why a request was refused before any work happened.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthError {
    #[error("no credential presented")]
    Missing,
    #[error("unknown credential")]
    Unknown,
    #[error("this credential is disabled")]
    Disabled,
}
