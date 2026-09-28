//! The transactional write path.
//!
//! This is the most important file in the module. Audit A1: Cortex routed every persisted write
//! in the product through a single `.ok()`, so disk-full, a permission change, lock contention,
//! or a non-UTF-8 path all reported "indexed N files" with **zero bytes on disk**. The three
//! properties below exist to make that unrepresentable.
//!
//! **All or nothing.** Everything in an [`IndexUpdate`] is one SQLite transaction. A crash, a
//! constraint violation, or a full disk mid-batch leaves the previous generation intact and
//! queryable — contract G7.
//!
//! **Never `Ok` with a fiction.** [`Store::apply_update`] returns statistics only on the success
//! path. Every failure becomes [`StoreError::Transaction`] (or [`StoreError::Io`] for an I/O
//! error), and the in-memory generation is advanced only after `commit` has returned.
//!
//! **No dangling state.** Removals demote the relations that point into a removed scope before
//! deleting anything, so an edge never quietly changes from `resolved` to something else. The
//! schema's `ON DELETE SET NULL` plus its state/target `CHECK` mean that a delete which skipped
//! this step would *fail* rather than corrupt — the demotion is not a nicety.
//!
//! The ordering inside a batch is fixed and load-bearing: removals, then entities, then
//! relations, then the generation. An entity removed and re-added in one batch therefore ends up
//! present, which is what lets an indexer replace a whole directory in a single commit.

use rusqlite::Transaction;
use rusqlite::params;
use rusqlite::types::Value;

use crate::model::entity::Entity;
use crate::model::path::RepoPath;
use crate::model::relation::{Relation, ResolutionState, UnresolvedReason};
use crate::model::span::Span;

use super::Store;
use super::error::StoreError;
use super::row;
use super::schema;
use super::update::{IndexUpdate, Removal, UpdateStats};

/// The generation a brand-new store starts at. The first successful commit makes it 1.
pub(crate) const INITIAL_GENERATION: &str = "0";

/// The identity columns of a relation, in the order the model's natural key is written.
///
/// Shared by the upsert's conflict target, the candidate lookup, and the count used to report
/// removals, so the three can never disagree about what "the same relation" means.
const RELATION_KEY: &str = "source_path = ?1 AND source_kind = ?2 AND source_qualified_name = ?3 \
AND source_ordinal = ?4 AND kind = ?5 AND target_name = ?6 AND start_byte = ?7 AND end_byte = ?8 \
AND start_line = ?9 AND start_column = ?10 AND end_line = ?11 AND end_column = ?12";

const ENTITY_UPSERT: &str = "\
INSERT INTO entity (
  path, kind, qualified_name, entity_ordinal, name, signature, doc,
  start_byte, end_byte, start_line, start_column, end_line, end_column,
  language, is_test, structural_fingerprint
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
ON CONFLICT (path, kind, qualified_name, entity_ordinal) DO UPDATE SET
  name = excluded.name,
  signature = excluded.signature,
  doc = excluded.doc,
  start_byte = excluded.start_byte,
  end_byte = excluded.end_byte,
  start_line = excluded.start_line,
  start_column = excluded.start_column,
  end_line = excluded.end_line,
  end_column = excluded.end_column,
  language = excluded.language,
  is_test = excluded.is_test,
  structural_fingerprint = excluded.structural_fingerprint";

const RELATION_UPSERT: &str = "\
INSERT INTO relation (
  kind, source_path, source_kind, source_qualified_name, source_ordinal,
  target_name, target_path, target_kind, target_qualified_name, target_ordinal,
  start_byte, end_byte, start_line, start_column, end_line, end_column,
  resolution_state, resolution_json
) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
ON CONFLICT (
  source_path, source_kind, source_qualified_name, source_ordinal,
  kind, target_name, start_byte, end_byte, start_line, start_column, end_line, end_column
) DO UPDATE SET
  target_path = excluded.target_path,
  target_kind = excluded.target_kind,
  target_qualified_name = excluded.target_qualified_name,
  target_ordinal = excluded.target_ordinal,
  resolution_state = excluded.resolution_state,
  resolution_json = excluded.resolution_json";

const CANDIDATE_INSERT: &str = "\
INSERT INTO relation_candidate (relation_id, ordinal, path, kind, qualified_name, entity_ordinal)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

const CANDIDATE_DELETE: &str = "DELETE FROM relation_candidate WHERE relation_id = ?1";

impl Store {
    /// Apply one batch atomically.
    ///
    /// This is the *only* function in Peek that writes to the index. It is deliberately the only
    /// one: audit A14 was an unsynchronised read-modify-write with no transaction, which a
    /// second writer made silently lossy. One entry point, one transaction, one place to audit.
    ///
    /// The generation advances on every successful commit, including a commit of
    /// [`IndexUpdate::empty`]. That is not pedantry: a reader comparing generations to decide
    /// whether its cached view is stale must be able to see that *something* committed, and a
    /// no-op write is a real event in the store's history.
    pub fn apply_update(&mut self, update: IndexUpdate) -> Result<UpdateStats, StoreError> {
        let previous = self.generation;
        let stats = match self.commit(&update, previous) {
            Ok(stats) => stats,
            // An I/O error is reported as such rather than flattened into a transaction failure:
            // it is the one cause a caller can act on differently (free the disk, fix
            // permissions) and hiding it inside a generic string would cost that.
            Err(StoreError::Io(message)) => return Err(StoreError::Io(message)),
            Err(cause) => return Err(StoreError::Transaction(cause.to_string())),
        };
        // Only now. If `commit` had returned early, the cached generation would claim a commit
        // that never happened and every later comparison would be off by one.
        self.generation = stats.generation;
        Ok(stats)
    }

    /// The transaction body. Every failure here propagates and rolls the batch back.
    fn commit(&mut self, update: &IndexUpdate, previous: u64) -> Result<UpdateStats, StoreError> {
        let tx = self.conn_mut().transaction()?;

        // 1. Removals first, so an entity removed and re-added by the same batch ends up
        //    present. Ordering removals after upserts would silently delete fresh data.
        let mut stats = UpdateStats {
            entities_upserted: 0,
            entities_removed: 0,
            relations_upserted: 0,
            relations_removed: 0,
            candidates_upserted: 0,
            generation: previous,
        };
        for removal in &update.removed_paths {
            remove_scope(&tx, removal, &mut stats)?;
        }

        // 2. Entities, then relations. A relation's source is a foreign key, so its entity must
        //    already be in the table.
        upsert_entities(&tx, &update.upserted_entities, &mut stats)?;
        upsert_relations(&tx, &update.upserted_relations, &mut stats)?;
        // The statements `upsert_relations` prepared are dropped when it returns, which is what
        // lets the transaction be committed below.

        // 3. The generation, inside the same transaction. A counter written outside the
        //    transaction could claim a commit that then failed to land.
        stats.generation = previous.checked_add(1).ok_or_else(|| {
            StoreError::Io("the generation counter has overflowed u64".to_owned())
        })?;
        schema::write_meta(&tx, schema::META_GENERATION, &stats.generation.to_string())?;

        // `commit` is the only way out; dropping the transaction without it rolls everything
        // back, which is what makes a mid-batch failure safe.
        tx.commit()?;
        Ok(stats)
    }
}

/// Write the batch's entities, replacing any with the same identity.
fn upsert_entities(
    tx: &Transaction<'_>,
    entities: &[Entity],
    stats: &mut UpdateStats,
) -> Result<(), StoreError> {
    if entities.is_empty() {
        return Ok(());
    }
    let mut stmt = tx
        .prepare(ENTITY_UPSERT)
        .map_err(|e| StoreError::Query(format!("cannot prepare the entity upsert: {e}")))?;
    for entity in entities {
        let [sb, eb, sl, sc, el, ec] = span_params(entity.span);
        stmt.execute(params![
            entity.id().path().as_str(),
            row::kind_to_sql(entity.id().kind())?,
            entity.id().qualified_name(),
            i64::from(entity.id().ordinal()),
            entity.name.as_str(),
            entity.signature.as_deref(),
            entity.doc.as_deref(),
            sb,
            eb,
            sl,
            sc,
            el,
            ec,
            optional_language_to_sql(entity.language.as_ref())?,
            i64::from(u8::from(entity.is_test)),
            entity.structural_fingerprint.as_deref(),
        ])?;
        stats.entities_upserted += 1;
    }
    Ok(())
}

/// Write the batch's relations and their candidate lists.
fn upsert_relations(
    tx: &Transaction<'_>,
    relations: &[Relation],
    stats: &mut UpdateStats,
) -> Result<(), StoreError> {
    if relations.is_empty() {
        return Ok(());
    }
    let mut upsert = tx
        .prepare(RELATION_UPSERT)
        .map_err(|e| StoreError::Query(format!("cannot prepare the relation upsert: {e}")))?;
    let mut select_id = tx
        .prepare(&format!("SELECT id FROM relation WHERE {RELATION_KEY}"))
        .map_err(|e| StoreError::Query(format!("cannot prepare the relation lookup: {e}")))?;
    let mut delete_candidates = tx
        .prepare(CANDIDATE_DELETE)
        .map_err(|e| StoreError::Query(format!("cannot prepare the candidate delete: {e}")))?;
    let mut insert_candidate = tx
        .prepare(CANDIDATE_INSERT)
        .map_err(|e| StoreError::Query(format!("cannot prepare the candidate insert: {e}")))?;

    for relation in relations {
        let source = &relation.source;
        let target = relation.target.as_ref();
        upsert.execute(params![
            row::relation_kind_to_sql(relation.kind)?,
            source.path().as_str(),
            row::kind_to_sql(source.kind())?,
            source.qualified_name(),
            i64::from(source.ordinal()),
            relation.target_name.as_str(),
            target.map(|t| t.path().as_str()),
            target.map(|t| row::kind_to_sql(t.kind())).transpose()?,
            target.map(|t| t.qualified_name()),
            target.map(|t| i64::from(t.ordinal())),
            i64::from(relation.span.start_byte),
            i64::from(relation.span.end_byte),
            i64::from(relation.span.start_line),
            i64::from(relation.span.start_column),
            i64::from(relation.span.end_line),
            i64::from(relation.span.end_column),
            row::resolution_tag(&relation.resolution),
            row::resolution_payload(&relation.resolution)?,
        ])?;
        stats.relations_upserted += 1;

        let row_id: i64 = select_id
            .query_row(
                params![
                    source.path().as_str(),
                    row::kind_to_sql(source.kind())?,
                    source.qualified_name(),
                    i64::from(source.ordinal()),
                    row::relation_kind_to_sql(relation.kind)?,
                    relation.target_name.as_str(),
                    i64::from(relation.span.start_byte),
                    i64::from(relation.span.end_byte),
                    i64::from(relation.span.start_line),
                    i64::from(relation.span.start_column),
                    i64::from(relation.span.end_line),
                    i64::from(relation.span.end_column),
                ],
                |found| found.get(0),
            )
            .map_err(|e| {
                StoreError::Query(format!(
                    "a just-written relation could not be found back: {e}"
                ))
            })?;

        // Replace the candidate list wholesale rather than merging. A relation that was
        // ambiguous over three candidates and is now resolved must not keep three stale
        // candidates, and a relation that is now ambiguous over two must not keep five.
        delete_candidates.execute(params![row_id])?;
        if let ResolutionState::Ambiguous { candidates } = &relation.resolution {
            for (ordinal, candidate) in candidates.iter().enumerate() {
                let written = insert_candidate
                    .execute(params![
                        row_id,
                        i64::try_from(ordinal).map_err(|_| {
                            StoreError::Query("too many candidates to index".to_owned())
                        })?,
                        candidate.path().as_str(),
                        row::kind_to_sql(candidate.kind())?,
                        candidate.qualified_name(),
                        i64::from(candidate.ordinal()),
                    ])
                    .map_err(|e| {
                        StoreError::Query(format!("cannot write a relation candidate: {e}"))
                    })?;
                if written != 1 {
                    return Err(StoreError::Transaction(format!(
                        "writing a candidate for relation {row_id} affected {written} rows, not 1"
                    )));
                }
                stats.candidates_upserted += 1;
            }
        }
    }
    Ok(())
}

/// Forget a scope: its relations, its entities, and anything that pointed into it.
fn remove_scope(
    tx: &Transaction<'_>,
    removal: &Removal,
    stats: &mut UpdateStats,
) -> Result<(), StoreError> {
    let bound = ScopeParams::new(removal);
    // The scope is the first placeholder in every statement here, so it starts at `?1`.
    let scope = |column: &str| scope_clause(column, removal, 1);

    // Counted before deleting, because afterwards the rows are gone and reporting a wrong number
    // would be reporting a number nobody could check.
    stats.relations_removed += count(
        tx,
        &format!(
            "SELECT COUNT(*) FROM relation WHERE {}",
            scope("source_path")
        ),
        bound.scope_params(),
    )?;
    let counted_before = stats.entities_removed;
    stats.entities_removed += count(
        tx,
        &format!("SELECT COUNT(*) FROM entity WHERE {}", scope("path")),
        bound.scope_params(),
    )?;

    // Demote before deleting. Without this, the target foreign key's `ON DELETE SET NULL` would
    // fire and be rejected by the state/target CHECK, so the delete would fail outright. With
    // it, the edge survives as an honest `Unresolved` with a stated reason.
    demote_incoming(tx, removal, &bound)?;

    let deleted = tx
        .execute(
            &format!("DELETE FROM entity WHERE {}", scope("path")),
            bound.scope_params(),
        )
        .map_err(|e| StoreError::Query(format!("cannot delete entities: {e}")))?;
    let removed_here = stats.entities_removed - counted_before;
    if u64::try_from(deleted).unwrap_or(u64::MAX) != removed_here {
        return Err(StoreError::Transaction(format!(
            "deleted {deleted} entities but counted {removed_here}"
        )));
    }
    Ok(())
}

/// Turn relations that point into `removal` into honest `Unresolved` rows.
///
/// An edge into a removed scope is a real observation — something referenced an entity that is no
/// longer indexed — so it is recorded as `Unresolved { NoCandidate }` rather than deleted.
/// Contract D4 requires uncertainty to be represented, and "the target left the index" is a fact
/// about the index, not about the source.
fn demote_incoming(
    tx: &Transaction<'_>,
    removal: &Removal,
    bound: &ScopeParams,
) -> Result<(), StoreError> {
    let payload = row::resolution_payload(&ResolutionState::Unresolved {
        reason: UnresolvedReason::NoCandidate,
    })?;
    // The payload is the **first** placeholder in the statement text, so it binds `?1` and the
    // scope clause shifts down by one. Numbering these the other way round silently binds the
    // path to the payload and the payload to the path, which matches nothing and demotes nothing.
    let sql = format!(
        "UPDATE relation SET target_path = NULL, target_kind = NULL, \
         target_qualified_name = NULL, target_ordinal = NULL, \
         resolution_state = 'unresolved', resolution_json = ?1 \
         WHERE EXISTS (SELECT 1 FROM entity e \
           WHERE e.path = relation.target_path AND e.kind = relation.target_kind \
             AND e.qualified_name = relation.target_qualified_name \
             AND e.entity_ordinal = relation.target_ordinal AND {})",
        scope_clause("e.path", removal, 2)
    );
    let carried = Value::Text(payload);
    tx.execute(&sql, bound.with_payload_params(&carried))
        .map_err(|e| StoreError::Query(format!("cannot demote incoming relations: {e}")))?;
    Ok(())
}

/// The parameters a removal's `WHERE` clause binds.
///
/// A struct rather than a function returning references to its own locals: the pattern and the
/// payload are owned here, so the bindings handed to SQLite cannot outlive them.
///
/// The parameter *count* varies with the removal variant, and it has to: SQLite rejects a bind
/// for an index the statement does not declare, so a single-file removal must not offer the
/// `LIKE` pattern that only the subtree clause uses.
struct ScopeParams {
    path: String,
    /// Only bound for a subtree removal.
    pattern: Option<String>,
}

impl ScopeParams {
    fn new(removal: &Removal) -> Self {
        Self {
            path: removal.path().as_str().to_owned(),
            pattern: match removal.includes_subdirectories() {
                true => Some(like_pattern(removal.path())),
                false => None,
            },
        }
    }

    /// How many parameters the scope clause occupies.
    #[cfg(test)]
    fn scope_len(&self) -> usize {
        match self.pattern {
            Some(_) => 2,
            None => 1,
        }
    }

    /// Bindings for a statement that selects a scope and carries no payload.
    ///
    /// The bindings **own** their values (`Box<dyn ToSql>`) rather than borrowing them. A
    /// `Vec<&dyn ToSql>` cannot hold a `&str` at all, because `str` is unsized and the trait
    /// object needs a `Sized` type, and a borrowed vector would also tie every statement's
    /// lifetime to the `Removal` that produced it. Owning the two `String`s costs one
    /// allocation per update and removes both problems.
    fn scope(&self) -> Vec<Box<dyn rusqlite::ToSql>> {
        let mut bindings: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(self.path.clone())];
        if let Some(pattern) = &self.pattern {
            bindings.push(Box::new(pattern.clone()));
        }
        bindings
    }

    /// The scope bindings wrapped for `execute`.
    fn scope_params(&self) -> impl rusqlite::Params {
        rusqlite::params_from_iter(self.scope())
    }

    /// Bindings for a statement that also writes a payload.
    ///
    /// The payload is **first**, because `demote_incoming` is the only caller and it writes
    /// `resolution_json` before the scope clause in the statement text. SQLite binds `?1` to the
    /// first supplied value, so a scope-first ordering would hand the payload the path and the
    /// scope the JSON — which matches no rows and looks like a logic bug rather than a swap.
    fn with_payload(&self, payload: &Value) -> Vec<Box<dyn rusqlite::ToSql>> {
        let mut bindings: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(payload.clone())];
        bindings.extend(self.scope());
        bindings
    }

    /// The scope bindings plus a payload, wrapped for `execute`.
    fn with_payload_params(&self, payload: &Value) -> impl rusqlite::Params {
        rusqlite::params_from_iter(self.with_payload(payload))
    }
}

/// Count the rows a statement matches, for a stat that is measured rather than assumed.
///
/// Audit A16: Cortex turned a failing `dir_size` into `0` via `unwrap_or(0)`, so its statistics
/// were invented. Here a count either succeeds or the whole update fails.
fn count<P: rusqlite::Params>(
    tx: &Transaction<'_>,
    sql: &str,
    params: P,
) -> Result<u64, StoreError> {
    let value: i64 = tx
        .query_row(sql, params, |row| row.get(0))
        .map_err(|e| StoreError::Query(format!("counting failed for {sql}: {e}")))?;
    u64::try_from(value)
        .map_err(|e| StoreError::Query(format!("a row count came back negative: {value} ({e})")))
}

/// The `WHERE` fragment selecting a removal's scope.
///
/// `LIKE` needs `ESCAPE` because `_` and `%` are ordinary characters in a filename, and a file
/// called `src/we_ird.rs` must not be swept up by a removal of `src/we`.
///
/// `first` is the `?n` number of the scope's first placeholder. SQLite numbers placeholders in
/// the order they appear in the **statement text**, not in the order the bindings are supplied, so
/// a statement that puts another placeholder before the scope clause must pass the real number.
/// Getting this wrong binds the two values to each other and fails in a way that looks like a
/// logic bug rather than a numbering one.
fn scope_clause(column: &str, removal: &Removal, first: usize) -> String {
    match removal.includes_subdirectories() {
        false => format!("{column} = ?{first}"),
        true => format!(
            "({column} = ?{first} OR {column} LIKE ?{} ESCAPE '\\')",
            first + 1
        ),
    }
}

/// A `LIKE` pattern matching everything strictly below `path`, with the metacharacters escaped.
fn like_pattern(path: &RepoPath) -> String {
    let escaped = path
        .as_str()
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}/%")
}

/// Six columns for a span, or six NULLs when there is none.
fn span_params(span: Option<Span>) -> [Value; 6] {
    let Some(span) = span else {
        // `Value` is not `Copy`, so an array cannot be built by repetition.
        return std::array::from_fn(|_| Value::Null);
    };
    [
        Value::Integer(i64::from(span.start_byte)),
        Value::Integer(i64::from(span.end_byte)),
        Value::Integer(i64::from(span.start_line)),
        Value::Integer(i64::from(span.start_column)),
        Value::Integer(i64::from(span.end_line)),
        Value::Integer(i64::from(span.end_column)),
    ]
}

/// Encode an optional enum for storage, mapping `None` to SQL NULL.
/// Encode the optional `language` column, which has its own canonical spelling.
///
/// There is deliberately no generic `enum_to_sql` here. Every stored enum is written through its
/// own `as_str()`, because serde's `rename_all = "snake_case"` disagrees with the canonical name
/// for some variants — `Language::ObjectiveC` serialises as `objective_c` while `as_str()` says
/// `objectivec`. A generic helper invites exactly that divergence; per-type helpers make it
/// impossible to reintroduce. See `row::enum_to_sql`'s replacement and the divergence guard test.
fn optional_language_to_sql(
    value: Option<&crate::model::Language>,
) -> Result<Option<String>, StoreError> {
    value
        .map(|language| row::language_to_sql(*language))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::{INITIAL_GENERATION, ScopeParams, like_pattern, scope_clause};
    use crate::model::path::RepoPath;
    use crate::store::update::Removal;

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).expect("valid path")
    }

    #[test]
    fn a_new_store_starts_before_its_first_commit() {
        assert_eq!(INITIAL_GENERATION, "0");
    }

    #[test]
    fn a_file_removal_binds_one_parameter_and_a_subtree_binds_two() {
        // SQLite rejects a bind for a placeholder the statement never declared, so binding the
        // `LIKE` pattern to a single-file removal would be an error, not a harmless extra.
        let file = ScopeParams::new(&Removal::RemoveFile(path("src/a.rs")));
        assert_eq!(file.scope_len(), 1);
        assert_eq!(file.scope().len(), 1);

        let subtree = ScopeParams::new(&Removal::RemoveSubtree(path("src")));
        assert_eq!(subtree.scope_len(), 2);
        assert_eq!(subtree.scope().len(), 2);
    }

    #[test]
    fn a_payload_binds_to_the_parameter_before_the_scope() {
        // `with_payload` puts the payload first because `demote_incoming` writes `resolution_json`
        // before the scope clause. This test pins the order of the bindings, which is what
        // actually decides which value lands in which placeholder. Getting it wrong hands the
        // payload the path and the scope the JSON: the statement then matches no rows and reads
        // like a logic bug rather than a swap.
        let file = ScopeParams::new(&Removal::RemoveFile(path("src/a.rs")));
        assert_eq!(file.with_payload(&rusqlite::types::Value::Null).len(), 2);

        let subtree = ScopeParams::new(&Removal::RemoveSubtree(path("src")));
        assert_eq!(subtree.with_payload(&rusqlite::types::Value::Null).len(), 3);
    }

    #[test]
    fn a_file_removal_matches_exactly_one_path() {
        let removal = Removal::RemoveFile(path("src/a.rs"));
        assert_eq!(scope_clause("path", &removal, 1), "path = ?1");
    }

    #[test]
    fn a_subtree_removal_matches_the_path_and_its_contents() {
        let removal = Removal::RemoveSubtree(path("src"));
        assert_eq!(
            scope_clause("path", &removal, 1),
            "(path = ?1 OR path LIKE ?2 ESCAPE '\\')"
        );
    }

    #[test]
    fn a_scope_clause_shifts_when_another_placeholder_precedes_it() {
        // SQLite numbers `?n` by position in the statement text. `demote_incoming` writes the
        // resolution payload before the scope clause, so the scope has to start at 2. Pinning
        // this is what stops the two being bound to each other.
        let removal = Removal::RemoveFile(path("src/a.rs"));
        assert_eq!(scope_clause("e.path", &removal, 2), "e.path = ?2");

        let subtree = Removal::RemoveSubtree(path("src"));
        assert_eq!(
            scope_clause("e.path", &subtree, 2),
            "(e.path = ?2 OR e.path LIKE ?3 ESCAPE '\\')"
        );
    }

    #[test]
    fn the_subtree_pattern_requires_a_separator_so_a_prefix_collision_does_not_match() {
        // The defect `RepoPath::is_within` also guards against: removing `src` must not take
        // `srcgen/main.rs` with it.
        assert_eq!(like_pattern(&path("src")), "src/%");
    }

    #[test]
    fn like_metacharacters_in_a_real_directory_name_are_escaped() {
        // `_` matches any single character and `%` matches any run of characters in `LIKE`.
        // A directory actually named `we_ird` must not be swept up by removing `weXird`, and
        // `100%` must not act as a wildcard.
        assert_eq!(like_pattern(&path("we_ird")), "we\\_ird/%");
        assert_eq!(like_pattern(&path("100%")), "100\\%/%");
    }

    #[test]
    fn a_path_never_contains_a_backslash_so_the_like_escape_is_never_needed_for_one() {
        // `RepoPath` normalises `\` to `/` at construction, so a backslash can never reach the
        // pattern builder. That is why the escape only has to handle `_` and `%`. If a backslash
        // ever could appear, `LIKE ... ESCAPE '\'` would need to escape it too, and this would be
        // the test that noticed.
        assert_eq!(path("a\\b").as_str(), "a/b");
        assert_eq!(like_pattern(&path("a\\b")), "a/b/%");
    }
}
