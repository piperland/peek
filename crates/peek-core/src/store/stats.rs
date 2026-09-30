//! What the store knows about itself.
//!
//! # Why these numbers must be measured
//!
//! Audit A16: Cortex's `dir_size` failures became `0` through `unwrap_or(0)`, so
//! `IndexStats.bytes_reclaimed` was an invented number — and it cost a full recursive walk of the
//! store to invent it. Every value here is either a real `COUNT` or a real `stat`, and a `stat`
//! that fails for a reason other than "the file is not there" is an error rather than a zero.
//!
//! The sizes and counts are what make "bounded growth" (contract H2) checkable. An engine that
//! claims its index does not grow without bound should be able to produce the evidence, and it
//! should be possible to tell a plateau from a leak by reading one struct.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use super::Store;
use super::error::StoreError;
use super::schema;

/// A snapshot of the store's size and contents.
///
/// Every field is counted at the moment [`Store::stats`] was called. Nothing is cached across
/// calls and nothing is estimated.
///
/// `Serialize` so a status surface sends the store's own numbers rather than a re-typed copy. The
/// one thing a consumer must remember is [`Self::generation`]: it is the generation **this handle
/// was opened at**, not the generation on disk right now, so a process holding a long-lived reader
/// alongside a writer sees a number that goes stale. The field is named honestly rather than being
/// read from the database on the way out, because the other numbers are per-call measurements and
/// mixing the two would be worse than saying which is which. A surface that has to report that
/// staleness reads [`Store::stored_generation`] beside it and compares the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoreStats {
    /// The schema these rows were written with.
    pub schema_version: u32,
    /// The commit counter at the time of the call.
    pub generation: u64,
    /// Size of the main database file on disk, in bytes: the pages a checkpoint has copied into
    /// it. The write-ahead log is reported separately because it is a different file with a
    /// different lifetime, and the two **overlap**: a frame copied into the file stays in the
    /// log until a checkpoint empties it, so neither number is the other's complement.
    pub file_size_bytes: u64,
    /// Size of the `-wal` file: every frame still in the log, including frames that are already
    /// in the main file. Non-zero means a checkpoint has not emptied it;
    /// [`Store::checkpoint`] returns it to zero. A checkpoint a reader refuses still copies the
    /// frames it may, so this counts frames rather than work outstanding.
    pub wal_size_bytes: u64,
    /// Rows in `entity`.
    pub entity_count: u64,
    /// Rows in `relation`, of every resolution state.
    pub relation_count: u64,
    /// Rows in `relation_candidate`: the total number of ambiguity across the index.
    pub candidate_count: u64,
    /// Relations proven to have exactly one target.
    pub resolved_relations: u64,
    /// Relations with more than one candidate and no decisive evidence.
    pub ambiguous_relations: u64,
    /// Relations explicitly known not to have a target, with a stated reason.
    pub unresolved_relations: u64,
    /// Relations derived from another fact rather than observed.
    pub inferred_relations: u64,
    /// Relations the extractor has written but the resolver has not yet decided.
    ///
    /// Counted because it is the **work remaining**, not a defect. A build that reports zero here
    /// has either resolved everything or extracted nothing, and those are very different
    /// situations that would otherwise look identical. Audit B11: state that exists only inside a
    /// payload blob cannot be counted, so it cannot be reported honestly — hence a
    /// `resolution_state` column with an index, and a counter over it.
    pub pending_relations: u64,
    /// Relations naming a target entity that is not in the index.
    ///
    /// Structurally zero while foreign keys are enforced, and that is the point: it is the check
    /// that the constraint is doing its job, not a value assumed to be zero. If a future migration
    /// drops the constraint, this counter is what notices.
    pub orphan_relations: u64,
}

impl Store {
    /// Measure the store.
    ///
    /// Costs a scan of the entity and relation tables, so it is a diagnostic and a benchmark
    /// fixture rather than something to call on a hot path. Everything it reports is a
    /// measurement; a failure to take one is an error, not a zero.
    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let conn = self.conn();
        let by_state = count_by_state(conn)?;
        let wal_size_bytes = file_size(&wal_path(self.path()))?;
        Ok(StoreStats {
            schema_version: self.schema_version(),
            generation: self.generation(),
            file_size_bytes: file_size(self.path())?,
            wal_size_bytes,
            entity_count: count(conn, "SELECT COUNT(*) FROM entity", "entity count")?,
            relation_count: count(conn, "SELECT COUNT(*) FROM relation", "relation count")?,
            candidate_count: count(
                conn,
                "SELECT COUNT(*) FROM relation_candidate",
                "candidate count",
            )?,
            resolved_relations: state_count(&by_state, "resolved"),
            ambiguous_relations: state_count(&by_state, "ambiguous"),
            unresolved_relations: state_count(&by_state, "unresolved"),
            inferred_relations: state_count(&by_state, "inferred"),
            pending_relations: state_count(&by_state, "pending"),
            orphan_relations: count(
                conn,
                "SELECT COUNT(*) FROM relation r \
                 WHERE r.target_path IS NOT NULL AND NOT EXISTS (\
                   SELECT 1 FROM entity e \
                    WHERE e.path = r.target_path AND e.kind = r.target_kind \
                      AND e.qualified_name = r.target_qualified_name \
                      AND e.entity_ordinal = r.target_ordinal)",
                "orphan count",
            )?,
        })
    }

    /// The schema version these rows were written with.
    ///
    /// Cached at open because it cannot change while the connection lives: the only code path
    /// that changes it is a migration, and that runs inside [`Store::open`].
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

/// Count the relations in each resolution state.
///
/// One `GROUP BY` rather than four `COUNT`s: indexing `resolution_state` exists so that
/// "how many edges are guessed?" is cheap, and four separate scans would throw that away.
fn count_by_state(conn: &Connection) -> Result<BTreeMap<String, u64>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT resolution_state, COUNT(*) FROM relation GROUP BY resolution_state")
        .map_err(|e| StoreError::Query(format!("cannot prepare the state count: {e}")))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|e| StoreError::Query(format!("state count failed: {e}")))?;
    let mut counts = BTreeMap::new();
    for row in rows {
        let (tag, count) =
            row.map_err(|e| StoreError::Query(format!("state count failed: {e}")))?;
        let count = u64::try_from(count)
            .map_err(|e| StoreError::Query(format!("a state count came back negative: {e}")))?;
        counts.insert(tag, count);
    }
    Ok(counts)
}

/// A state's count, or zero when the index holds no relation in it.
fn state_count(counts: &BTreeMap<String, u64>, tag: &str) -> u64 {
    counts.get(tag).copied().unwrap_or(0)
}

/// One `COUNT`, with the query named in any error so a failure says which number is missing.
fn count(conn: &Connection, sql: &str, what: &str) -> Result<u64, StoreError> {
    let value: i64 = conn
        .query_row(sql, [], |row| row.get(0))
        .map_err(|e| StoreError::Query(format!("{what} failed: {e}")))?;
    u64::try_from(value).map_err(|e| StoreError::Query(format!("{what} came back negative: {e}")))
}

/// The size of a file that may not exist.
///
/// A missing `-wal` file means "nothing is waiting to be checkpointed", which is genuinely zero
/// and not a measurement failure. Any other `stat` error is reported, because a permission or
/// I/O problem reported as `0` bytes is the Cortex defect in miniature.
fn file_size(path: &Path) -> Result<u64, StoreError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(StoreError::Io(format!(
            "cannot stat {}: {e}",
            path.display()
        ))),
    }
}

/// The write-ahead log's path. SQLite's naming convention, not a separately configured location.
fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// The generation recorded on disk, bypassing the value cached in the [`Store`].
///
/// The two answer different questions and a caller sometimes needs both: the cached one is what
/// this handle was opened against, and this one is what the index says now. They differ whenever
/// something has committed since — a watcher, or a second process — which is the normal state of a
/// store being written to. A caller that wants to report staleness compares the two; a caller that
/// wants integrity ([`Store::verify`]) expects them to agree, because a mismatch with no writer
/// running is the torn state the generation exists to detect.
pub fn stored_generation(conn: &Connection) -> Result<u64, StoreError> {
    let raw = schema::read_meta(conn, schema::META_GENERATION)?
        .ok_or_else(|| StoreError::Corrupt("the store has no generation row".to_owned()))?;
    raw.parse::<u64>()
        .map_err(|e| StoreError::Corrupt(format!("generation {raw:?} is not a number: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{file_size, state_count, wal_path};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn a_missing_file_is_zero_bytes_not_an_error() {
        let missing = std::env::temp_dir().join("peek-stats-does-not-exist-4b71");
        assert_eq!(file_size(&missing).expect("not found is not a failure"), 0);
    }

    #[test]
    fn the_wal_path_is_the_database_path_with_a_suffix() {
        let base = PathBuf::from("/var/cache/piper/peek/abc/index.db");
        assert_eq!(
            wal_path(&base),
            PathBuf::from("/var/cache/piper/peek/abc/index.db-wal")
        );
    }

    #[test]
    fn a_state_with_no_relations_counts_zero() {
        let mut counts = BTreeMap::new();
        counts.insert("resolved".to_owned(), 7_u64);
        assert_eq!(state_count(&counts, "resolved"), 7);
        assert_eq!(
            state_count(&counts, "ambiguous"),
            0,
            "an absent state is zero, not a missing value"
        );
    }

    #[test]
    fn a_real_file_reports_its_own_size() {
        let path = std::env::temp_dir().join("peek-stats-sizes-7c02.db");
        std::fs::write(&path, b"0123456789").expect("write");
        let size = file_size(&path).expect("stat");
        let _ = std::fs::remove_file(&path);
        assert!(size >= 10, "a ten-byte file measured as {size}");
    }
}
