//! Failures this crate can report.
//!
//! Note what is *not* here: sending an event cannot fail. Analytics sits beside
//! the request path, never in it, so [`crate::StatsSink::send`] returns `()`
//! and a full queue costs a row rather than a request.

use std::path::PathBuf;

/// Anything that can go wrong opening, writing or querying `stats.db`.
#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    /// SQLite refused a statement.
    #[error("stats database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database file could not be opened.
    #[error("opening stats database at {path}: {source}")]
    Open {
        /// The path that failed.
        path: PathBuf,
        /// What SQLite said about it.
        source: rusqlite::Error,
    },

    /// The directory holding the database could not be created.
    #[error("creating the directory for the stats database at {path}: {source}")]
    Directory {
        /// The directory that failed.
        path: PathBuf,
        /// What the filesystem said about it.
        source: std::io::Error,
    },

    /// An event would not serialize to JSON.
    #[error("serializing a stats event: {0}")]
    Json(#[from] serde_json::Error),

    /// The writer thread could not be started.
    #[error("starting the stats writer thread: {0}")]
    Thread(std::io::Error),

    /// A batch could not be committed. The writer has already logged it and
    /// moved on; this carries the message across the thread boundary to
    /// whoever called `flush`, since `rusqlite::Error` is not `Clone`.
    #[error("committing a stats batch failed: {0}")]
    Batch(String),

    /// The writer thread is gone, so a flush or shutdown cannot be
    /// acknowledged. Events sent to a sink in this state are dropped and
    /// counted, exactly as an overflow is.
    #[error("the stats writer thread is not running")]
    WriterGone,

    /// A request the caller can fix: what it asked for is not something
    /// this database can do as asked.
    #[error("{0}")]
    Invalid(String),

    /// The writer thread panicked. The batch it held is lost; the database is
    /// intact, because every batch is one transaction.
    #[error("the stats writer thread panicked")]
    WriterPanicked,
}
