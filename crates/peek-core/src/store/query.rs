//! Typed read queries over the index.
//!
//! Every method here is a bounded index seek. There is deliberately no "read everything" method
//! and no method that returns an unbounded result: a query whose cost a caller cannot see is how
//! Cortex's `dependencies` became O(V×E) (audit A8). A caller that genuinely wants a whole
//! traversal calls [`Store::outgoing`] and [`Store::incoming`] with an explicit limit per hop.

use std::collections::HashMap;

use rusqlite::params;
use rusqlite::types::Value;

use crate::model::entity::{Entity, EntityId};
use crate::model::path::RepoPath;
use crate::model::relation::{Relation, RelationKind, ResolutionState};

use super::Store;
use super::error::StoreError;
use super::row;

// The statements below are `pub(crate)` rather than inline literals so the query-plan tests
// explain the *same* SQL the methods run. A test holding its own copy of the SQL would still pass
// after someone rewrote `outgoing` into a table scan, which is exactly the regression these
// tests exist to catch.

/// The lookup of a single entity by its four identity columns.
pub(crate) const ENTITY_BY_ID: &str = "SELECT {cols} FROM entity \
WHERE path = ?1 AND kind = ?2 AND qualified_name = ?3 AND entity_ordinal = ?4";

/// Every entity in one file, ordered so `LIMIT` stops inside the index.
pub(crate) const ENTITIES_IN_FILE: &str =
    "WHERE path = ?1 ORDER BY kind, qualified_name, entity_ordinal LIMIT ?2";

/// Every entity declaring `name`, ordered so `LIMIT` stops inside `entity_by_name`.
pub(crate) const ENTITIES_NAMED: &str =
    "WHERE name = ?1 ORDER BY path, kind, qualified_name, entity_ordinal LIMIT ?2";

/// One page of a file's entities, starting after `after`.
///
/// Two spellings because a row value cannot be built from a `NULL`: binding `NULL`s for the three
/// cursor columns and comparing `(kind, qualified_name, entity_ordinal) > (NULL, NULL, NULL)` is
/// never true, so a first page read that way returns nothing at all. The cursor-free form is
/// therefore the whole of [`Store::entities_in_file_after`] with `after = None`, which is what makes
/// "page until the page comes back short" a loop over two statements rather than over one.
///
/// The row-value comparison is what keeps this an index range rather than a scan: the primary key
/// is `(path, kind, qualified_name, entity_ordinal)`, so `path = ?1` fixes the leading column and
/// the row value bounds the trailing three, and `LIMIT` stops the walk inside the index. The
/// query-plan test in `store::tests` pins that, because a page read that degrades to a scan turns
/// a linear walk into a quadratic one and nothing else in the build would notice.
pub(crate) fn entities_in_file_after_sql(cursor: bool) -> String {
    match cursor {
        false => ENTITIES_IN_FILE.to_owned(),
        true => "WHERE path = ?1 AND (kind, qualified_name, entity_ordinal) > (?2, ?3, ?4) \
                 ORDER BY kind, qualified_name, entity_ordinal LIMIT ?5"
            .to_owned(),
    }
}

/// Every entity owning `qualified_name`, ordered so `LIMIT` stops inside its index.
pub(crate) const ENTITIES_WITH_QUALIFIED_NAME: &str =
    "WHERE qualified_name = ?1 ORDER BY path, kind, entity_ordinal LIMIT ?2";

/// Relations in one resolution state, ordered so `LIMIT` stops inside `relation_by_state`.
pub(crate) fn relations_in_state_sql() -> String {
    format!(
        "SELECT {} FROM relation WHERE resolution_state = ?1 \
         ORDER BY source_path, source_kind, source_qualified_name, source_ordinal, start_byte \
         LIMIT ?2",
        row::RELATION_COLUMNS
    )
}

/// The statement [`Store::outgoing`] runs.
pub(crate) fn outgoing_sql(kind: Option<RelationKind>) -> Result<String, StoreError> {
    let (filter, limit) = match kind {
        Some(_) => (" AND kind = ?5", "?6"),
        None => ("", "?5"),
    };
    // The trailing `kind, target_name, start_byte` of the `relation_by_source` index is exactly
    // this ORDER BY, so SQLite walks the index and stops after `limit` rows with nothing to sort.
    Ok(format!(
        "SELECT {} FROM relation \
         WHERE source_path = ?1 AND source_kind = ?2 AND source_qualified_name = ?3 \
           AND source_ordinal = ?4{filter} \
         ORDER BY kind, target_name, start_byte LIMIT {limit}",
        row::RELATION_COLUMNS
    ))
}

/// The statement [`Store::incoming`] runs.
pub(crate) fn incoming_sql(kind: Option<RelationKind>) -> Result<String, StoreError> {
    let (filter, limit) = match kind {
        Some(_) => (" AND kind = ?5", "?6"),
        None => ("", "?5"),
    };
    Ok(format!(
        "SELECT {} FROM relation \
         WHERE target_path = ?1 AND target_kind = ?2 AND target_qualified_name = ?3 \
           AND target_ordinal = ?4{filter} \
         ORDER BY kind, source_path, start_byte LIMIT {limit}",
        row::RELATION_COLUMNS
    ))
}

/// Wrap an entity template in its column list and `FROM`.
fn entity_sql(template: &str) -> String {
    format!("SELECT {} FROM entity {template}", row::ENTITY_COLUMNS)
}

/// Read-side query surface.
impl Store {
    /// Fetch one entity by identity.
    ///
    /// `None` means no such entity, which is an ordinary answer and not an error: a query for a
    /// symbol that does not exist should not look like a broken index.
    pub fn entity(&self, id: &EntityId) -> Result<Option<Entity>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(&ENTITY_BY_ID.replace("{cols}", row::ENTITY_COLUMNS))
            .map_err(|e| StoreError::Query(format!("cannot prepare an entity lookup: {e}")))?;
        let mut rows = stmt
            .query(params![
                id.path().as_str(),
                row::kind_to_sql(id.kind())?,
                id.qualified_name(),
                i64::from(id.ordinal()),
            ])
            .map_err(|e| StoreError::Query(format!("entity lookup failed: {e}")))?;
        match rows.next() {
            Ok(Some(row)) => Ok(Some(row::entity_from_row(row)?)),
            Ok(None) => Ok(None),
            Err(e) => Err(StoreError::Query(format!("entity lookup failed: {e}"))),
        }
    }

    /// Fetch every entity declared in one file, in identity order.
    ///
    /// Serviced by the primary key's leading `path` column, so no separate index is needed; the
    /// query-plan test pins that rather than leaving it to a future reader's judgement.
    ///
    /// **This reads a prefix, not the file.** `limit` bounds one read, so a file with more entities
    /// than `limit` has its tail outside the answer, and a caller that treats this as "the file's
    /// entities" is reading a claim the result cannot support. A caller that wants the whole file
    /// pages it through [`Store::entities_in_file_after`] until a page comes back short, which is
    /// what the resolver now does. The distinction is stated here because the mistake is invisible:
    /// the returned rows are a perfectly ordinary `Vec<Entity>` either way.
    pub fn entities_in_file(
        &self,
        path: &RepoPath,
        limit: usize,
    ) -> Result<Vec<Entity>, StoreError> {
        self.entities_in_file_after(path, None, limit)
    }

    /// Fetch one page of the entities declared in one file, in identity order, strictly after
    /// `after`.
    ///
    /// `after` is the exclusive lower bound of the page, so a caller walks a file by handing back
    /// the last row of each page. `None` starts at the first entity, which makes this the same
    /// read [`Store::entities_in_file`] performs — same statement, same index, same order.
    ///
    /// This is the paging primitive, and it exists because the store has no cursor and the
    /// alternative to paging was truncation. Every other read here is bounded by one `LIMIT` and a
    /// caller who needs more is expected to loop, per the note at the top of this file; this is the
    /// one place where the loop is the *only* correct thing to do, because a partial list of a
    /// file's entities is not a smaller answer but a different one.
    ///
    /// `after` must be an entity of `path`. The two are checked rather than assumed, because a
    /// cursor from another file would silently become an offset into this file's ordering and
    /// produce a page that is neither the first nor the next.
    pub fn entities_in_file_after(
        &self,
        path: &RepoPath,
        after: Option<&EntityId>,
        limit: usize,
    ) -> Result<Vec<Entity>, StoreError> {
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(path.as_str().to_owned())];
        if let Some(after) = after {
            if after.path() != path {
                return Err(StoreError::Query(format!(
                    "entities_in_file_after: the cursor is from {} but the page is from {}",
                    after.path().as_str(),
                    path.as_str()
                )));
            }
            params.push(Box::new(row::kind_to_sql(after.kind())?));
            params.push(Box::new(after.qualified_name().to_owned()));
            params.push(Box::new(i64::from(after.ordinal())));
        }
        params.push(Box::new(limit_value(limit)));
        self.entities_where(
            &entities_in_file_after_sql(after.is_some()),
            rusqlite::params_from_iter(params),
            "entities_in_file_after",
        )
    }

    /// Fetch entities whose declared name matches exactly.
    ///
    /// This is the lookup a resolver needs *before* it knows where anything is, and it is why
    /// `entity_by_name` exists. It legitimately returns many rows: that is the ambiguity the
    /// resolver must report rather than resolve by picking the first (D-0004).
    pub fn entities_named(&self, name: &str, limit: usize) -> Result<Vec<Entity>, StoreError> {
        self.entities_where(
            ENTITIES_NAMED,
            params![name, limit_value(limit)],
            "entities_named",
        )
    }

    /// Fetch entities whose full ownership chain matches exactly.
    pub fn entities_with_qualified_name(
        &self,
        qualified_name: &str,
        limit: usize,
    ) -> Result<Vec<Entity>, StoreError> {
        self.entities_where(
            ENTITIES_WITH_QUALIFIED_NAME,
            params![qualified_name, limit_value(limit)],
            "entities_with_qualified_name",
        )
    }

    /// Relations leaving `id`, in a bounded number of index seeks.
    ///
    /// `kind` filters on the relation kind; both variants are served by `relation_by_source`, and
    /// the query-plan tests assert it. This is the operation that was a full edge scan inside
    /// Cortex's BFS loop, and it must never become one again.
    ///
    /// Rows come back in the index's own order (kind, target name, start byte) rather than an
    /// arbitrary one, so two runs over the same index produce identical output and `limit`
    /// truncates deterministically.
    pub fn outgoing(
        &self,
        id: &EntityId,
        kind: Option<RelationKind>,
        limit: usize,
    ) -> Result<Vec<Relation>, StoreError> {
        // The bindings must match the statement's shape exactly. `outgoing_sql` omits the `kind`
        // predicate and shifts the limit down a slot when there is no filter, so supplying a
        // placeholder `NULL` for the kind here would bind one value too many and SQLite would
        // reject the statement. Building both from the same `kind` keeps them in lockstep.
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(id.path().as_str().to_owned()),
            Box::new(row::kind_to_sql(id.kind())?),
            Box::new(id.qualified_name().to_owned()),
            Box::new(i64::from(id.ordinal())),
        ];
        if let Some(kind) = kind {
            params.push(Box::new(row::relation_kind_to_sql(kind)?));
        }
        params.push(Box::new(limit_value(limit)));

        let relation_rows = self.relation_rows(
            &outgoing_sql(kind)?,
            rusqlite::params_from_iter(params),
            "outgoing",
        )?;
        self.attach_candidates(relation_rows)
    }

    /// Relations arriving at `id`, served by `relation_by_target`.
    ///
    /// The mirror of [`Store::outgoing`], and what makes `dependents` cheap. It returns relations
    /// whose target could not be resolved too, because an edge naming something outside the
    /// repository is still a reference a consumer may want to see. Filter on
    /// [`Relation::is_followable`] to walk only proven edges.
    pub fn incoming(
        &self,
        id: &EntityId,
        kind: Option<RelationKind>,
        limit: usize,
    ) -> Result<Vec<Relation>, StoreError> {
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(id.path().as_str().to_owned()),
            Box::new(row::kind_to_sql(id.kind())?),
            Box::new(id.qualified_name().to_owned()),
            Box::new(i64::from(id.ordinal())),
        ];
        if let Some(kind) = kind {
            params.push(Box::new(row::relation_kind_to_sql(kind)?));
        }
        params.push(Box::new(limit_value(limit)));

        let relation_rows = self.relation_rows(
            &incoming_sql(kind)?,
            rusqlite::params_from_iter(params),
            "incoming",
        )?;
        self.attach_candidates(relation_rows)
    }

    /// The entity ids that one ambiguous reference was ambiguous between, strongest evidence
    /// first.
    ///
    /// Separate from [`Store::outgoing`] because an ambiguous relation carries *no* target, so
    /// there is nothing to look up as one. Without this method the candidate list would be
    /// written to the database and then unreachable — persisted, but useless, which is the D-0004
    /// requirement met only on paper.
    pub fn ambiguous_candidates(
        &self,
        source: &EntityId,
        kind: RelationKind,
        target_name: &str,
    ) -> Result<Vec<EntityId>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(
                "SELECT c.path, c.kind, c.qualified_name, c.entity_ordinal \
                 FROM relation_candidate c JOIN relation r ON r.id = c.relation_id \
                 WHERE r.source_path = ?1 AND r.source_kind = ?2 \
                   AND r.source_qualified_name = ?3 AND r.source_ordinal = ?4 \
                   AND r.kind = ?5 AND r.target_name = ?6 \
                 ORDER BY c.ordinal",
            )
            .map_err(|e| StoreError::Query(format!("cannot prepare a candidate lookup: {e}")))?;
        let mut rows = stmt
            .query(params![
                source.path().as_str(),
                row::kind_to_sql(source.kind())?,
                source.qualified_name(),
                i64::from(source.ordinal()),
                row::relation_kind_to_sql(kind)?,
                target_name,
            ])
            .map_err(|e| StoreError::Query(format!("candidate lookup failed: {e}")))?;
        let mut candidates = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| StoreError::Query(format!("candidate lookup failed: {e}")))?
        {
            // This query selects the four identity columns and nothing else, so it decodes from
            // column zero rather than through the `relation_id, ordinal, …` layout.
            candidates.push(row::candidate_from_bare_row(row)?);
        }
        Ok(candidates)
    }

    /// The entities the relations leaving `id` provably point at.
    ///
    /// The traversal primitive, and the reason [`Store::outgoing`] returns whole relations rather
    /// than a projection: a consumer walking a dependency graph wants identities, and discarding
    /// the relation detail here would force every caller to re-implement this filter.
    ///
    /// Unresolved and ambiguous relations are dropped rather than followed, because following them
    /// means guessing — which is D-0004. A caller that wants to *see* the ambiguity asks
    /// [`Store::outgoing`] directly.
    pub fn callees(
        &self,
        id: &EntityId,
        kind: Option<RelationKind>,
        limit: usize,
    ) -> Result<Vec<EntityId>, StoreError> {
        let mut reachable = Vec::new();
        for relation in self.outgoing(id, kind, limit)? {
            if relation.is_followable() {
                if let Some(target) = relation.target {
                    reachable.push(target);
                }
            }
        }
        Ok(reachable)
    }

    /// The entities whose relations provably arrive at `id`.
    ///
    /// The mirror of [`Store::callees`], and the primitive behind `dependents`. Contract D7
    /// reframed `impact` as reverse structural traversal; this is that traversal's primitive.
    pub fn callers(
        &self,
        id: &EntityId,
        kind: Option<RelationKind>,
        limit: usize,
    ) -> Result<Vec<EntityId>, StoreError> {
        let mut reachable = Vec::new();
        for relation in self.incoming(id, kind, limit)? {
            if relation.is_followable() {
                reachable.push(relation.source);
            }
        }
        Ok(reachable)
    }

    /// Fetch relations in one resolution state.
    ///
    /// Serviced by `relation_by_state`, and the reason that index exists: audit B11's `explain()`
    /// could not tell a resolved edge from a guessed one, so it reported a caller count that was
    /// wrong by construction. Enumerating each state separately is what lets the engine describe
    /// its own output honestly.
    pub fn relations_in_state(
        &self,
        state: &ResolutionState,
        limit: usize,
    ) -> Result<Vec<Relation>, StoreError> {
        let relation_rows = self.relation_rows(
            &relations_in_state_sql(),
            params![row::resolution_tag(state), limit_value(limit)],
            "relations_in_state",
        )?;
        self.attach_candidates(relation_rows)
    }

    /// The query plan SQLite chose for a statement, one string per step.
    ///
    /// Public so the performance property is *checkable* rather than asserted in prose: a caller
    /// can confirm a query still seeks an index after an index change, and the test suite does
    /// exactly that. A plan containing `SCAN relation` where an index was expected is the precise
    /// signature of the Cortex O(V×E) bug returning.
    ///
    /// The statement is only *planned*, never run, so the parameter **values** do not matter —
    /// but SQLite still validates the parameter **count** at prepare time, so they are bound as
    /// NULLs. Passing an empty list fails with "Wrong number of parameters passed to query", which
    /// is a confusing way to learn that planning still counts.
    pub fn query_plan(&self, sql: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .map_err(|e| StoreError::Query(format!("cannot explain {sql}: {e}")))?;
        let blanks: Vec<Option<rusqlite::types::Value>> = vec![None; stmt.parameter_count()];
        let details = stmt
            .query_map(rusqlite::params_from_iter(blanks), |row| {
                row.get::<_, String>(3)
            })
            .map_err(|e| StoreError::Query(format!("cannot explain {sql}: {e}")))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StoreError::Query(format!("cannot explain {sql}: {e}")))?;
        Ok(details)
    }

    /// Shared body for the three single-column entity lookups.
    fn entities_where<P: rusqlite::Params>(
        &self,
        template: &str,
        params: P,
        what: &str,
    ) -> Result<Vec<Entity>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(&entity_sql(template))
            .map_err(|e| StoreError::Query(format!("cannot prepare {what}: {e}")))?;
        let mut rows = stmt
            .query(params)
            .map_err(|e| StoreError::Query(format!("{what} failed: {e}")))?;
        let mut found = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| StoreError::Query(format!("{what} failed: {e}")))?
        {
            found.push(row::entity_from_row(row)?);
        }
        Ok(found)
    }

    /// Shared body for the relation-returning queries.
    fn relation_rows<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
        what: &str,
    ) -> Result<Vec<row::RelationRow>, StoreError> {
        let mut stmt = self
            .conn()
            .prepare(sql)
            .map_err(|e| StoreError::Query(format!("cannot prepare {what}: {e}")))?;
        let mut rows = stmt
            .query(params)
            .map_err(|e| StoreError::Query(format!("{what} failed: {e}")))?;
        let mut found = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|e| StoreError::Query(format!("{what} failed: {e}")))?
        {
            found.push(row::relation_from_row(row)?);
        }
        Ok(found)
    }

    /// Load the candidate list for every ambiguous relation in `rows`, in one query.
    ///
    /// A second round trip rather than a join, because only ambiguous relations have candidates
    /// and joining on every adjacency query would tax the resolved path that dominates real
    /// workloads. The `IN` list is bounded by the page size of the original query, so this is
    /// still a bounded number of seeks.
    fn attach_candidates(&self, rows: Vec<row::RelationRow>) -> Result<Vec<Relation>, StoreError> {
        let ambiguous: Vec<i64> = rows
            .iter()
            .filter(|r| row::is_ambiguous(&r.relation.resolution))
            .map(|r| r.id)
            .collect();
        if ambiguous.is_empty() {
            return Ok(rows.into_iter().map(|r| r.relation).collect());
        }

        let placeholders = std::iter::repeat_n("?", ambiguous.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT relation_id, ordinal, path, kind, qualified_name, entity_ordinal \
             FROM relation_candidate WHERE relation_id IN ({placeholders}) \
             ORDER BY relation_id, ordinal"
        );
        let mut stmt = self
            .conn()
            .prepare(&sql)
            .map_err(|e| StoreError::Query(format!("cannot prepare a candidate lookup: {e}")))?;
        let mut candidates = stmt
            .query(rusqlite::params_from_iter(ambiguous.iter()))
            .map_err(|e| StoreError::Query(format!("candidate lookup failed: {e}")))?;
        let mut by_relation: HashMap<i64, Vec<EntityId>> = HashMap::new();
        while let Some(row) = candidates
            .next()
            .map_err(|e| StoreError::Query(format!("candidate lookup failed: {e}")))?
        {
            let relation_id = row
                .get::<_, i64>(0)
                .map_err(|e| StoreError::Query(format!("candidate lookup failed: {e}")))?;
            by_relation
                .entry(relation_id)
                .or_default()
                .push(row::candidate_from_row(row)?);
        }

        Ok(rows
            .into_iter()
            .map(|r| {
                if !row::is_ambiguous(&r.relation.resolution) {
                    return r.relation;
                }
                match by_relation.get(&r.id) {
                    Some(list) => Relation {
                        resolution: ResolutionState::Ambiguous {
                            candidates: list.clone(),
                        },
                        ..r.relation
                    },
                    // An ambiguous relation with no candidate rows is a real, reportable state:
                    // the resolver found more than one name match and then failed to record
                    // them. It is not repaired here by inventing an empty answer.
                    None => r.relation,
                }
            })
            .collect())
    }
}

/// Convert a `usize` limit into a bound parameter, saturating rather than overflowing.
///
/// A caller passing `usize::MAX` means "no practical limit", and panicking on the conversion would
/// turn an intent to ask for everything into a denial of service.
fn limit_value(limit: usize) -> Value {
    match i64::try_from(limit) {
        Ok(value) => Value::Integer(value),
        Err(_) => Value::Integer(i64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::limit_value;
    use rusqlite::types::Value;

    #[test]
    fn an_unrepresentable_limit_saturates_instead_of_panicking() {
        assert_eq!(
            limit_value(usize::MAX),
            Value::Integer(i64::MAX),
            "asking for usize::MAX means 'no practical limit', not 'abort'"
        );
    }

    #[test]
    fn an_ordinary_limit_binds_unchanged() {
        assert_eq!(limit_value(0), Value::Integer(0));
        assert_eq!(limit_value(7), Value::Integer(7));
        assert_eq!(limit_value(256), Value::Integer(256));
    }
}
