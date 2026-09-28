//! Schema definition, versioning, and migration.
//!
//! # Why a version at all
//!
//! Cortex had none (audit B1). Adding an `enum Language` variant was a silent, unrecoverable
//! break: nothing on any type could record which shape a row was written in. Peek records the
//! shape in [`META_SCHEMA_VERSION`] and refuses to read anything it does not understand.
//!
//! # Why `STRICT`
//!
//! Every table is `STRICT`, so SQLite rejects a value of the wrong storage class instead of
//! applying affinity. This matters because the whole point of the version check is to avoid
//! *quietly misreading*; type coercion is the other way a row can be quietly misread, and it
//! happens silently, on every row, forever. With `STRICT` the misread becomes a loud error on
//! the first write attempt.
//!
//! # The identity of a row
//!
//! [`ENTITY_DDL`] stores the four components of [`crate::model::EntityId`] as four separate
//! indexed columns rather than as one opaque key string. That is the difference between
//! "look up everything in `src/payments`" being an index seek and being a scan of every row in
//! the index, and it is why the four columns are the primary key rather than a surrogate.

use rusqlite::Connection;
use rusqlite::params;

use crate::store::error::StoreError;

/// The schema version this build writes and reads without migrating.
pub const SCHEMA_VERSION: u32 = 1;

/// The version that predates schema versioning.
///
/// No v0 has shipped. It is modelled anyway so that the migration machinery is exercised by a
/// test rather than being shipped untested — an untested migration path is a migration path that
/// does not work when it is first needed.
pub(crate) const SCHEMA_V0: u32 = 0;

/// The `meta` table name. Its presence is what makes a file recognisable as a Peek index.
pub const META_TABLE: &str = "meta";

/// `meta` row naming the repository the store belongs to.
pub const META_REPO_ID: &str = "repo_id";

/// `meta` row holding the commit counter. Never reset.
pub const META_GENERATION: &str = "generation";

/// `meta` row holding the schema version.
pub const META_SCHEMA_VERSION: &str = "schema_version";

/// Every table this module owns, in the order a migration must drop them.
///
/// Dropping the child before the parent is not cosmetic. With foreign keys enabled SQLite
/// performs an implicit `DELETE` before a `DROP`, which fires `ON DELETE` actions; dropping
/// `entity` while `relation` still exists would make the target foreign key's `SET NULL` run and
/// then be rejected by the state/target CHECK. Children first makes that impossible.
const OWNED_TABLES: [&str; 4] = ["relation_candidate", "relation", "entity", "meta"];

const META_DDL: &str = "\
CREATE TABLE meta (
  key   TEXT NOT NULL PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
";

const ENTITY_DDL: &str = "\
CREATE TABLE entity (
  path                   TEXT    NOT NULL,
  kind                   TEXT    NOT NULL,
  qualified_name         TEXT    NOT NULL,
  entity_ordinal         INTEGER NOT NULL,
  name                   TEXT    NOT NULL,
  signature              TEXT,
  doc                    TEXT,
  start_byte             INTEGER,
  end_byte               INTEGER,
  start_line             INTEGER,
  start_column           INTEGER,
  end_line               INTEGER,
  end_column             INTEGER,
  language               TEXT,
  is_test                INTEGER NOT NULL DEFAULT 0 CHECK (is_test IN (0, 1)),
  structural_fingerprint TEXT,
  PRIMARY KEY (path, kind, qualified_name, entity_ordinal),
  -- A span is present or it is absent. Six independently nullable columns would otherwise let a
  -- half-written span through, and a span with a start but no end is not a span.
  CHECK (
    (start_byte IS NULL) = (end_byte IS NULL) AND
    (start_byte IS NULL) = (start_line IS NULL) AND
    (start_byte IS NULL) = (start_column IS NULL) AND
    (start_byte IS NULL) = (end_line IS NULL) AND
    (start_byte IS NULL) = (end_column IS NULL)
  )
) STRICT;
";

const RELATION_DDL: &str = "\
CREATE TABLE relation (
  id                    INTEGER NOT NULL PRIMARY KEY,
  kind                  TEXT    NOT NULL,
  source_path           TEXT    NOT NULL,
  source_kind           TEXT    NOT NULL,
  source_qualified_name TEXT    NOT NULL,
  source_ordinal        INTEGER NOT NULL,
  target_name           TEXT    NOT NULL,
  target_path           TEXT,
  target_kind           TEXT,
  target_qualified_name TEXT,
  target_ordinal        INTEGER,
  start_byte            INTEGER NOT NULL,
  end_byte              INTEGER NOT NULL,
  start_line            INTEGER NOT NULL,
  start_column          INTEGER NOT NULL,
  end_line              INTEGER NOT NULL,
  end_column            INTEGER NOT NULL,
  resolution_state      TEXT    NOT NULL
    CHECK (resolution_state IN ('resolved', 'ambiguous', 'unresolved', 'inferred')),
  resolution_json       TEXT    NOT NULL,
  -- The natural key of a relation: who said it, what they said, to what, and where. Two records
  -- agreeing on all twelve describe the same fact and must collapse to one row, or a re-index
  -- doubles the edge set. The surrogate `id` exists only so `relation_candidate` can point at a
  -- relation with a single-column foreign key.
  UNIQUE (
    source_path, source_kind, source_qualified_name, source_ordinal,
    kind, target_name,
    start_byte, end_byte, start_line, start_column, end_line, end_column
  ),
  -- The target is either wholly present or wholly absent.
  CHECK (
    (target_path IS NULL) = (target_kind IS NULL) AND
    (target_path IS NULL) = (target_qualified_name IS NULL) AND
    (target_path IS NULL) = (target_ordinal IS NULL)
  ),
  -- The check that makes the store honest. A relation cannot claim to be resolved to an entity
  -- it does not name, and an ambiguous or unresolved relation cannot name one at all. This is
  -- D-0003 and D-0009 enforced by the database instead of by reviewer discipline.
  CHECK ((resolution_state IN ('resolved', 'inferred')) = (target_path IS NOT NULL)),
  -- A relation cannot outlive the entity it was observed in. Deleting a file's entities deletes
  -- its relations and, through them, their candidates: contract G3 (no orphan nodes or edges
  -- after a delete) holds by construction rather than by remembering to clean up.
  FOREIGN KEY (source_path, source_kind, source_qualified_name, source_ordinal)
    REFERENCES entity (path, kind, qualified_name, entity_ordinal) ON DELETE CASCADE,
  -- `ON DELETE SET NULL` is declared, but the CHECK above turns it into a practical RESTRICT:
  -- nulling the target of a resolved relation would leave a row claiming `resolved` with no
  -- target, which the CHECK rejects. Deleting a referenced entity therefore *fails* unless the
  -- caller first demotes the relations that point at it — which is exactly what
  -- `Store::apply_update` does, and which is the only way an entity can lose a referent without
  -- an edge silently changing its meaning.
  FOREIGN KEY (target_path, target_kind, target_qualified_name, target_ordinal)
    REFERENCES entity (path, kind, qualified_name, entity_ordinal) ON DELETE SET NULL
) STRICT;
";

const CANDIDATE_DDL: &str = "\
CREATE TABLE relation_candidate (
  relation_id    INTEGER NOT NULL,
  ordinal        INTEGER NOT NULL CHECK (ordinal >= 0),
  path           TEXT    NOT NULL,
  kind           TEXT    NOT NULL,
  qualified_name TEXT    NOT NULL,
  entity_ordinal INTEGER NOT NULL,
  PRIMARY KEY (relation_id, ordinal),
  FOREIGN KEY (relation_id) REFERENCES relation (id) ON DELETE CASCADE
) STRICT;
";

const INDEX_DDL: &str = "\
-- Trailing columns of each adjacency index mirror its ORDER BY exactly, so an adjacency query
-- never builds a temporary b-tree and `LIMIT` stops the scan early. That is the whole point:
-- Cortex scanned every edge inside a BFS loop, making `dependencies` O(V x E).
CREATE INDEX entity_by_name
  ON entity (name, path, kind, qualified_name, entity_ordinal);
CREATE INDEX entity_by_qualified_name
  ON entity (qualified_name, path, kind, entity_ordinal);
-- Deliberately absent: an index on entity(path) alone. The primary key already begins with
-- `path`, so a second index would duplicate the path of every entity row in the file and serve
-- no query the primary key does not serve as well. The query plan tests pin that.
CREATE INDEX relation_by_source
  ON relation (source_path, source_kind, source_qualified_name, source_ordinal,
               kind, target_name, start_byte);
CREATE INDEX relation_by_target
  ON relation (target_path, target_kind, target_qualified_name, target_ordinal,
               kind, source_path, start_byte);
CREATE INDEX relation_by_state
  ON relation (resolution_state, source_path, source_kind, source_qualified_name,
               source_ordinal, start_byte);
";

/// Create an empty v1 database. Fails loudly if any table already exists, so a half-created
/// store cannot be mistaken for a good one.
pub fn create_v1(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(META_DDL)?;
    conn.execute_batch(ENTITY_DDL)?;
    conn.execute_batch(RELATION_DDL)?;
    conn.execute_batch(CANDIDATE_DDL)?;
    conn.execute_batch(INDEX_DDL)?;
    Ok(())
}

/// Move a store at `from` up to [`SCHEMA_VERSION`], returning the version now present.
///
/// Adding a version is one match arm and one function. The order of the arms is the migration
/// order, so a version can never be skipped by accident.
pub fn migrate(conn: &mut Connection, from: u32) -> Result<u32, StoreError> {
    match from {
        // v0 predates `schema_version` entirely. There is nothing to translate: the index is a
        // derived artefact and a rebuild is always correct, so the honest migration is to throw
        // the rows away and re-declare the shape. The generation counter and the repository
        // identity are carried across so a reader that remembers either still recognises the
        // same store.
        SCHEMA_V0 => rebuild_from_v0(conn),
        // Reachable from the next version onwards. It exists so that a gap in the chain is an
        // error rather than a silent no-op.
        older if older < SCHEMA_VERSION => Err(StoreError::Corrupt(format!(
            "no migration path from schema version {older}"
        ))),
        current => Ok(current),
    }
}

/// Discard the contents of a v0 store and declare the v1 shape over it.
fn rebuild_from_v0(conn: &mut Connection) -> Result<u32, StoreError> {
    let tx = conn.transaction()?;
    let carried_generation = read_meta(&tx, META_GENERATION)?.unwrap_or_else(|| "0".to_owned());
    let carried_repo = read_meta(&tx, META_REPO_ID)?;
    for table in OWNED_TABLES {
        tx.execute_batch(&format!("DROP TABLE IF EXISTS {table}"))?;
    }
    create_v1(&tx)?;
    write_meta(&tx, META_SCHEMA_VERSION, &SCHEMA_VERSION.to_string())?;
    write_meta(&tx, META_GENERATION, &carried_generation)?;
    if let Some(repo) = carried_repo {
        write_meta(&tx, META_REPO_ID, &repo)?;
    }
    tx.commit()?;
    Ok(SCHEMA_VERSION)
}

/// Whether `name` exists as a table. Used to tell "not a Peek store" from "an old Peek store".
pub fn has_table(conn: &Connection, name: &str) -> Result<bool, StoreError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Read one `meta` value. `None` means the row is absent, which for `schema_version` is how a
/// v0 store is detected.
pub fn read_meta(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = stmt.query(params![key])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get::<_, String>(0)?)),
        None => Ok(None),
    }
}

/// Write one `meta` value, inserting or replacing.
pub fn write_meta(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        META_GENERATION, META_REPO_ID, META_SCHEMA_VERSION, OWNED_TABLES, SCHEMA_V0, SCHEMA_VERSION,
        create_v1, has_table, migrate, read_meta, write_meta,
    };
    use rusqlite::Connection;

    #[test]
    fn a_fresh_v1_database_declares_its_version() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        create_v1(&conn).expect("create v1");
        write_meta(&conn, META_SCHEMA_VERSION, "1").expect("write version");
        assert_eq!(read_meta(&conn, META_SCHEMA_VERSION).unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn every_owned_table_is_created() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        create_v1(&conn).expect("create v1");
        for table in OWNED_TABLES {
            assert!(has_table(&conn, table).expect("sqlite_master"), "missing {table}");
        }
    }

    #[test]
    fn a_database_without_the_meta_table_is_not_a_store() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch("CREATE TABLE notes (body TEXT)")
            .expect("create an unrelated table");
        assert!(!has_table(&conn, "meta").expect("sqlite_master"));
    }

    #[test]
    fn migrating_the_current_version_is_a_no_op() {
        let mut conn = Connection::open_in_memory().expect("in-memory database");
        create_v1(&conn).expect("create v1");
        assert_eq!(migrate(&mut conn, SCHEMA_VERSION).expect("no-op"), SCHEMA_VERSION);
    }

    #[test]
    fn rebuilding_from_v0_carries_the_generation_and_repository() {
        let mut conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('generation', '41'), ('repo_id', 'abc');",
        )
        .expect("build a v0 store");
        assert_eq!(read_meta(&conn, META_SCHEMA_VERSION).unwrap(), None);

        assert_eq!(migrate(&mut conn, SCHEMA_V0).expect("migrate"), SCHEMA_VERSION);
        assert_eq!(
            read_meta(&conn, META_GENERATION).unwrap().as_deref(),
            Some("41"),
            "the generation counter must survive a migration; it is never reset"
        );
        assert_eq!(read_meta(&conn, META_REPO_ID).unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn rebuilding_from_v0_starts_the_generation_at_zero_when_v0_had_none() {
        let mut conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("build a v0 store");
        migrate(&mut conn, SCHEMA_V0).expect("migrate");
        assert_eq!(read_meta(&conn, META_GENERATION).unwrap().as_deref(), Some("0"));
    }

    #[test]
    fn rebuilding_from_v0_discards_rows_the_new_schema_cannot_describe() {
        // A v0 store whose tables have the wrong shape is exactly the case that must not be
        // half-read. The migration throws the rows away rather than guessing at them.
        let mut conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE entity (path TEXT);
             INSERT INTO entity (path) VALUES ('src/a.rs');
             CREATE TABLE relation (kind TEXT);
             INSERT INTO relation (kind) VALUES ('calls');",
        )
        .expect("build a v0 store with a different table shape");

        migrate(&mut conn, SCHEMA_V0).expect("migrate");
        let leftover: i64 = conn
            .query_row("SELECT COUNT(*) FROM entity", [], |row| row.get(0))
            .expect("count");
        assert_eq!(leftover, 0, "v0 rows must not survive into v1");
    }
}
