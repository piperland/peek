//! SQLite-backed persistence for the Peek index.
//!
//! This module exists because Cortex's storage layer had no incremental substrate at all: one
//! 4.5 MB JSON value under a single sled key, no transactions, no secondary indexes, a swap
//! scheme whose recovery path was unreachable *and* destructive, and a single `.ok()` on the
//! one write call site that made a full disk report success (audit A1, A4, A5, A6, A7, A14,
//! A15, A16, A17). D-0002 records the decision to use SQLite instead.
//!
//! # The four properties that are load-bearing
//!
//! 1. **Writes are atomic and loud.** [`Store::apply_update`] is one transaction. Either the
//!    whole batch lands or none of it does, and a failure is always `Err` — never `Ok` with a
//!    plausible number. A store that lies about having indexed something is worse than a store
//!    that is down, because the caller stops checking.
//! 2. **Generation never resets.** [`Store::generation`] increments on every successful commit
//!    and is carried across a migration, so a reader can tell "nothing changed" from "I am
//!    looking at a different index than last time". Cortex's `revision` was reset to 1 on every
//!    full build, so it could not be compared against anything.
//! 3. **Adjacency is an index seek.** [`Store::outgoing`] and [`Store::incoming`] are the fix
//!    for Cortex's O(V×E) traversal, which came from scanning the whole edge set inside a BFS
//!    loop. Both are covered by query-plan tests that assert the index is used, so a future
//!    index change that reintroduces a scan fails a test rather than a benchmark nobody runs.
//! 4. **The store knows which repository it belongs to.** A store opened against the wrong
//!    repository is an error, not a merge of two document sets.
//!
//! # Bounded growth
//!
//! Unbounded growth is the default outcome of a local index, and Cortex achieved 3.94×
//! amplification while its README claimed otherwise. Each growth source is named here with the
//! choice that bounds it:
//!
//! * **A second copy of the state.** Cortex kept sled blobs *and* a byte-identical `state.json`,
//!   doubling the payload before any redundancy (audit A7, H5). There is exactly one artefact:
//!   this database file. There is no snapshot, no exported JSON, and no "previous index".
//! * **Deleted rows accumulating.** Rows only grow the file if the rows survive. Removal deletes
//!   them, foreign keys cascade so a deleted entity takes its relations and candidates with it,
//!   and SQLite reuses freed pages for later writes rather than appending forever.
//! * **The write-ahead log.** WAL lets a reader proceed during a write, at the cost of a second
//!   file that grows until it is checkpointed. It is checkpointed by
//!   [`Store::checkpoint`] and by SQLite's own automatic checkpointing, and
//!   [`Store::stats`] reports its size so "bounded" is a measurement rather than a promise.
//! * **Index churn.** Every index is a second copy of the columns it covers, so a wide index is a
//!   real cost. The three adjacency indexes are wide on purpose — their trailing columns match
//!   the `ORDER BY` so a query never builds a temporary b-tree — and no index is created that
//!   no query uses. A separate `entity(path)` index is deliberately absent because the primary
//!   key already leads with `path`.
//! * **Vacuum.** SQLite never shrinks the file on its own; freed pages are reused but not
//!   returned. Growth therefore flattens rather than returning to the original size. That is a
//!   deliberate trade-off: an automatic `VACUUM` rewrites the whole database and is exactly the
//!   unbounded-latency spike this design is avoiding. `stats()` exposes the size so the plateau
//!   is visible, and reclaiming it is an explicit operator decision.
//!
//! # Concurrency
//!
//! SQLite admits one writer at a time. Peek's model is one indexer process owning writes and any
//! number of readers using WAL snapshots, which is a fit rather than a limitation. A busy
//! timeout absorbs a concurrent writer's brief overlap; a *sustained* conflict surfaces as
//! [`StoreError::Transaction`] rather than as a partial write. Readers never block writers and
//! writers never block readers, which is the opposite of Cortex, where every read took sled's
//! exclusive file lock (audit A15).

mod error;
mod repo;
mod row;
mod schema;
mod stats;
mod update;

mod query;
mod write;

pub use error::StoreError;
pub use repo::RepoId;
pub use schema::SCHEMA_VERSION;
pub use stats::StoreStats;
pub use update::{IndexUpdate, Removal, UpdateStats};

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// How long a statement waits for a competing writer before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// A SQLite-backed index for one repository.
///
/// The file lives outside the repository, under the OS cache root, per D-0006. Opening is the
/// only place that validates; once a `Store` exists, every operation returns a typed error
/// rather than guessing.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    path: PathBuf,
    repo: RepoId,
    generation: u64,
    schema_version: u32,
}

impl Store {
    /// Open (creating if absent) the index for `repo` at `path`.
    ///
    /// A missing file is created at [`SCHEMA_VERSION`]. An existing file must be a Peek store
    /// for this repository: a file that is not SQLite, a file that is SQLite but not ours, and a
    /// file written by a newer Peek are three distinct errors, because each needs a different
    /// response and collapsing them into one IO error is what let Cortex report a healthy
    /// install over an unreadable index (audit A6, A8).
    ///
    /// The parent directory is created if missing, since the OS cache root will not exist on a
    /// machine's first run.
    pub fn open(path: &Path, repo: &RepoId) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| {
                    StoreError::Io(format!("cannot create {}: {e}", parent.display()))
                })?;
            }
        }
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        // SQLite defers reading the header until the first statement, so a file full of garbage
        // opens successfully and only fails on first use. Establishing whether the file was
        // already a store *before* that happens is what turns a confusing mid-query IO error into
        // a `Corrupt` the caller can act on.
        let existed = fs::metadata(path).is_ok_and(|m| m.len() > 0);
        let mut version = SCHEMA_VERSION;
        if existed {
            version = validate_existing_store(&conn, path, repo)?;
        }

        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| StoreError::Io(format!("cannot set busy timeout: {e}")))?;
        configure(&conn)?;

        if !existed {
            schema::create_v1(&conn)?;
            schema::write_meta(&conn, schema::META_REPO_ID, repo.as_str())?;
            schema::write_meta(
                &conn,
                schema::META_GENERATION,
                crate::store::write::INITIAL_GENERATION,
            )?;
            schema::write_meta(
                &conn,
                schema::META_SCHEMA_VERSION,
                &SCHEMA_VERSION.to_string(),
            )?;
        } else if version < SCHEMA_VERSION {
            // A v0 store carries no `repo_id`, so there is nothing for `validate_existing_store`
            // to check and nothing for it to contradict: the caller's assertion stands. Every
            // later store does carry one, and the check does apply.
            schema::migrate(&mut conn, version)?;
            if schema::read_meta(&conn, schema::META_REPO_ID)?.is_none() {
                schema::write_meta(&conn, schema::META_REPO_ID, repo.as_str())?;
            }
        }

        let generation = read_generation(&conn, path)?;
        let schema_version = schema::read_meta(&conn, schema::META_SCHEMA_VERSION)?
            .and_then(|raw| raw.parse::<u32>().ok())
            .unwrap_or(SCHEMA_VERSION);
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            repo: repo.clone(),
            generation,
            schema_version,
        })
    }

    /// The file this store reads and writes.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The repository this store belongs to.
    pub fn repo(&self) -> &RepoId {
        &self.repo
    }

    /// The commit counter.
    ///
    /// Monotonically increasing for the life of the file, carried across migrations, and reset
    /// by nothing. A reader that caches a generation and later sees a smaller one is looking at
    /// a different index, not at an older version of this one.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The connection, for the submodules that do the actual work.
    fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access to the connection, for the one place that writes.
    fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Flush the write-ahead log back into the main database file.
    ///
    /// Callable at any time, including straight after a write. `TRUNCATE` also shrinks the log
    /// file to zero, so a long-running indexer that calls this after each batch does not leave a
    /// growing second file behind. A checkpoint can fail while a reader holds a snapshot; that is
    /// reported as an error rather than ignored, because a silently-skipped checkpoint is a
    /// silently-growing file.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        // (busy, log_frames, checkpointed_frames): a non-zero `busy` means a reader prevented
        // completion. Returning that as success would be the Cortex pattern of converting a
        // failure into a number.
        let (busy, _log, _moved) = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| StoreError::Query(format!("wal_checkpoint failed: {e}")))?;
        if busy != 0 {
            return Err(StoreError::Transaction(format!(
                "wal_checkpoint could not complete: {busy} reader(s) hold a snapshot"
            )));
        }
        Ok(())
    }

    /// Run SQLite's own integrity check over the whole database, and confirm the cached
    /// generation still matches the stored one.
    ///
    /// A deliberate diagnosis, not part of opening: `integrity_check` reads every page, so
    /// running it on open would make `peek status` O(database size). The result is a
    /// [`StoreError::Corrupt`] naming the pages at fault rather than a boolean, because "the
    /// store is broken" is not actionable without knowing where.
    ///
    /// The generation comparison is the other half. Page damage does not stop SQLite from
    /// answering queries — which is exactly why Cortex's `health()` reported a healthy install
    /// over a broken index (audit A6) — so a damaged page is noticed here or not at all.
    pub fn verify(&self) -> Result<(), StoreError> {
        let mut stmt = self
            .conn
            .prepare("PRAGMA integrity_check")
            .map_err(|e| StoreError::Query(format!("cannot run integrity_check: {e}")))?;
        let problems: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| StoreError::Query(format!("integrity_check failed: {e}")))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(|e| StoreError::Query(format!("integrity_check failed: {e}")))?
            .into_iter()
            .filter(|line| line != "ok")
            .collect();
        if !problems.is_empty() {
            return Err(StoreError::Corrupt(format!(
                "integrity_check reported: {}",
                problems.join("; ")
            )));
        }

        let on_disk = stats::stored_generation(&self.conn)
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        if on_disk != self.generation {
            return Err(StoreError::Corrupt(format!(
                "the cached generation is {} but the store records {on_disk}: \
                 the index is not the one this handle was opened against",
                self.generation
            )));
        }
        Ok(())
    }
}

/// Apply the pragmas the design depends on, and verify each one took.
///
/// Audit A16: Cortex turned a failing `dir_size` into `0` via `unwrap_or(0)`, so its statistics
/// were invented rather than measured. A pragma that silently does not apply is the same defect
/// at a different layer, so each is read back.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|e| StoreError::Query(format!("cannot enable WAL: {e}")))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::Query(format!(
            "journal_mode is {mode:?}, not wal; the index cannot be read consistently during a write"
        )));
    }

    // `synchronous = NORMAL` under WAL: durable across a process crash, without forcing an fsync
    // on every commit. `FULL` would fsync per commit, a per-file syscall tax on a workload that
    // commits in batches.
    //
    // Enforced foreign keys, not merely declared ones. Without this the two foreign keys in the
    // schema are documentation: SQLite parses them, reports no error, and ignores them.
    //
    // Both are set through `execute_batch` because a pragma that *assigns* returns no rows, and
    // `query_row` would fail on the empty result rather than on the setting.
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")
        .map_err(|e| StoreError::Query(format!("cannot apply connection pragmas: {e}")))?;

    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .map_err(|e| StoreError::Query(format!("cannot read foreign_keys: {e}")))?;
    if foreign_keys != 1 {
        return Err(StoreError::Query(
            "foreign keys are not enforced; deleting an entity would leave dangling relations"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Read the generation, rejecting a store whose counter is not a number.
fn read_generation(conn: &Connection, path: &Path) -> Result<u64, StoreError> {
    let raw = schema::read_meta(conn, schema::META_GENERATION)?.ok_or_else(|| {
        StoreError::Corrupt(format!(
            "{} has no generation; it is not a Peek index",
            path.display()
        ))
    })?;
    raw.parse::<u64>()
        .map_err(|e| StoreError::Corrupt(format!("generation {raw:?} is not a number: {e}")))
}

/// Check that an existing file is a Peek index this build can read, and return its version.
///
/// Runs before any pragma is applied, so a garbage file produces [`StoreError::Corrupt`] rather
/// than an IO error from SQLite's own header parser. The two mean different things to the person
/// holding the machine: one is a bad install, the other is a file that is not what it was believed
/// to be.
///
/// Returns [`schema::SCHEMA_V0`] for a store with no `schema_version` row, which is the one state
/// a migration can still rescue. Everything else older than the current version is migrated by
/// the caller; anything newer is refused here, before a single row is read.
fn validate_existing_store(
    conn: &Connection,
    path: &Path,
    repo: &RepoId,
) -> Result<u32, StoreError> {
    // A file that is not SQLite at all cannot answer this query. That failure *is* the
    // corruption signal, and it is reported as such rather than swallowed.
    let has_meta = schema::has_table(conn, schema::META_TABLE).map_err(|_| {
        StoreError::Corrupt(format!(
            "{} is not a readable SQLite database",
            path.display()
        ))
    })?;
    if !has_meta {
        return Err(StoreError::Corrupt(format!(
            "{} is a SQLite database but has no `{}` table; it is not a Peek index",
            path.display(),
            schema::META_TABLE
        )));
    }

    let found = match schema::read_meta(conn, schema::META_SCHEMA_VERSION)? {
        Some(text) => text.parse::<u32>().map_err(|e| {
            StoreError::Corrupt(format!("schema_version {text:?} is not a number: {e}"))
        })?,
        // No version row is a v0 store: exactly the one state the migration framework handles.
        None => schema::SCHEMA_V0,
    };
    if found > schema::SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew {
            found,
            supported: schema::SCHEMA_VERSION,
        });
    }

    // A v0 store predates repository identity, so there is nothing to compare and the caller's
    // assertion is recorded instead of contradicted. Every later store carries an id and is
    // checked.
    if let Some(stored) = schema::read_meta(conn, schema::META_REPO_ID)? {
        if stored != repo.as_str() {
            return Err(StoreError::WrongRepository {
                stored,
                expected: repo.as_str().to_owned(),
            });
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
