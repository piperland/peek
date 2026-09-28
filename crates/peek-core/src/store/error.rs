//! The storage failure taxonomy.
//!
//! Every failure mode has its own variant on purpose. Cortex collapsed all of them into a
//! single error that the only call site then discarded with `.ok()` (audit A1), which is how a
//! full disk reported success. A `StoreError` is only useful if the caller can tell "the file
//! you handed me is not an index" from "your update did not land", because the two demand
//! different responses: one is a human problem, the other is a retry.

use rusqlite::Error as SqliteError;

use thiserror::Error;

/// Everything that can go wrong while reading or writing the index store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The file exists, is large enough to be a database, and is nevertheless unreadable as a
    /// Peek index: a bad header, a `sqlite_master` that will not parse, or a `meta` table that
    /// has been damaged.
    ///
    /// This is deliberately *not* an [`StoreError::Io`]. "I could not read the bytes" and "the
    /// bytes are not what you claim they are" are different diagnoses, and a caller that is told
    /// the wrong one will retry a fix that cannot help.
    #[error("index store is corrupt: {0}")]
    Corrupt(String),

    /// The store was written by a newer Peek than this one.
    ///
    /// Reading it anyway is the failure mode this variant exists to prevent: a v2 build that adds
    /// a column this build does not know about would read every row through the v1 field list and
    /// produce a quietly wrong index. Refusing is the only sound option.
    #[error(
        "index store uses schema version {found} but this build of Peek understands at most \
         {supported}; upgrade Peek, or delete the store and re-index"
    )]
    SchemaTooNew { found: u32, supported: u32 },

    /// The store belongs to a different repository than the one being opened against it.
    ///
    /// Audit A10: Cortex let two worktrees share one store, and each overwrote the other's
    /// repository record and document set. Keying the store to a repository makes that a loud
    /// error instead of silent data loss.
    #[error("index store belongs to repository {stored}, not {expected}; refusing to read another repository's index")]
    WrongRepository { stored: String, expected: String },

    /// A read failed, or a stored value could not be decoded back into the model.
    ///
    /// Decoding failures land here rather than being coerced: a path that no longer validates as
    /// repository-relative, a kind string this build does not recognise, or a `resolution_state`
    /// that disagrees with its own payload are all *evidence that the store is not what it
    /// claims*, and the correct response is to stop.
    #[error("index query failed: {0}")]
    Query(String),

    /// The filesystem refused us: a missing directory, a permission problem, a failed `stat`.
    #[error("index store I/O failed: {0}")]
    Io(String),

    /// An `apply_update` did not land, in whole or in part.
    ///
    /// Every failure inside a write is reported here, including disk-full, because the honest
    /// fact at this boundary is not *which* layer failed — it is that the update was discarded
    /// entirely and the generation did not advance. Classifying the cause by matching SQLite's
    /// message text would be exactly the stringly-typed error handling this module exists to
    /// replace.
    #[error("index update failed and was rolled back in full: {0}")]
    Transaction(String),
}

impl From<SqliteError> for StoreError {
    fn from(error: SqliteError) -> Self {
        StoreError::Query(error.to_string())
    }
}
