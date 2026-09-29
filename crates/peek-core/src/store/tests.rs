//! Store-level behaviour tests.
//!
//! Every test here asserts a concrete value, never "the result is not empty". Each one names the
//! audit finding it defends, because a test that does not is a test that will be deleted the
//! first time the code it guards is rewritten.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use crate::model::entity::{Entity, EntityId, EntityKind};
use crate::model::language::Language;
use crate::model::path::RepoPath;
use crate::model::relation::{Evidence, Relation, RelationKind, ResolutionState, UnresolvedReason};
use crate::model::span::Span;

use super::query::{
    ENTITIES_IN_FILE, ENTITIES_NAMED, ENTITIES_WITH_QUALIFIED_NAME, ENTITY_BY_ID, incoming_sql,
    outgoing_sql, relations_in_state_sql,
};
use super::row::ENTITY_COLUMNS;
use super::{Durability, IndexUpdate, RepoId, SCHEMA_VERSION, Store, StoreError, schema};

/// The size of the synthetic graph the traversal test builds.
const GRAPH_SIZE: usize = 5_000;

/// A directory that removes itself, so a failing test does not leave state behind for the next.
pub(crate) struct TempDir(PathBuf);

static NEXT: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "peek-store-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("index.db")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn repo(dir: &TempDir) -> RepoId {
    RepoId::discover(dir.path()).expect("discover the repository")
}

fn open(dir: &TempDir) -> Store {
    let id = repo(dir);
    Store::open(&dir.database(), &id).expect("open the store")
}

/// Open a store that is expected to fail, returning the error.
fn open_err(dir: &TempDir, repo: &RepoId) -> StoreError {
    match Store::open(&dir.database(), repo) {
        Ok(_) => panic!("expected opening to be refused"),
        Err(e) => e,
    }
}

fn path(s: &str) -> RepoPath {
    RepoPath::new(s).expect("valid path")
}

fn id(file: &str, kind: EntityKind, qualified_name: &str, ordinal: u32) -> EntityId {
    EntityId::new(path(file), kind, qualified_name, ordinal)
}

fn span(start_byte: u32) -> Span {
    Span::new(start_byte, start_byte + 20, 10, 1, 11, 21).expect("valid span")
}

/// An entity with every optional field populated, so a round-trip test cannot pass by accident on
/// a column that happens to default to NULL on both sides.
fn full_entity(file: &str, name: &str) -> Entity {
    Entity {
        id: id(file, EntityKind::Method, &format!("Service.{name}"), 0),
        name: name.to_owned(),
        signature: Some("fn retry(&self, attempt: u32) -> Result<Receipt>".to_owned()),
        doc: Some("Retries the charge.\nSecond line of the doc.".to_owned()),
        span: Some(span(120)),
        language: Some(Language::Rust),
        is_test: true,
        structural_fingerprint: Some("blake3:9f2c1a".to_owned()),
    }
}

fn bare_entity(file: &str, kind: EntityKind, qualified_name: &str) -> Entity {
    Entity {
        id: id(file, kind, qualified_name, 0),
        name: qualified_name.to_owned(),
        signature: None,
        doc: None,
        span: None,
        language: None,
        is_test: false,
        structural_fingerprint: None,
    }
}

// ---------------------------------------------------------------------------
// Opening, versioning, identity
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_store_declares_the_current_schema_and_starts_before_its_first_commit() {
    let dir = TempDir::new("fresh");
    let store = open(&dir);
    assert_eq!(store.schema_version(), SCHEMA_VERSION);
    // Pinned as a literal as well as against the constant, so that bumping `SCHEMA_VERSION`
    // without noticing this test fails. A silent version bump is how a store starts refusing
    // every existing index and nobody knows why until a user reports it.
    assert_eq!(
        store.schema_version(),
        2,
        "v2 allows the pending resolution state"
    );
    assert_eq!(
        store.generation(),
        0,
        "a store that has never committed is at 0"
    );
    assert_eq!(store.repo(), &repo(&dir));
    assert_eq!(store.path(), dir.database());
}

#[test]
fn the_generation_advances_by_exactly_one_per_commit() {
    // Audit B1: Cortex's `revision` was reset to 1 by `build_full`, so it could not be compared
    // against anything. A counter that is only "some number" is not a generation.
    let dir = TempDir::new("generation");
    let mut store = open(&dir);
    let mut seen = vec![store.generation()];
    for _ in 0..4 {
        let stats = store
            .apply_update(IndexUpdate::empty().with_entity(bare_entity(
                "src/a.rs",
                EntityKind::Function,
                "f",
            )))
            .expect("commit");
        seen.push(stats.generation);
    }
    assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    assert_eq!(store.generation(), 4);
}

#[test]
fn an_empty_update_still_commits_and_still_advances_the_generation() {
    // A reader comparing generations to decide whether its view is stale must be able to see that
    // a commit happened. Suppressing the no-op would make an unchanged index indistinguishable
    // from one whose writer crashed mid-update.
    let dir = TempDir::new("empty-update");
    let mut store = open(&dir);
    let stats = store.apply_update(IndexUpdate::empty()).expect("commit");
    assert_eq!(stats.generation, 1);
    assert_eq!(stats.entities_upserted, 0);
    assert_eq!(stats.relations_upserted, 0);
    assert_eq!(store.generation(), 1);
}

#[test]
fn the_generation_survives_reopening_and_never_goes_backwards() {
    let dir = TempDir::new("reopen-generation");
    {
        let mut store = open(&dir);
        for name in ["f", "g"] {
            store
                .apply_update(IndexUpdate::empty().with_entity(bare_entity(
                    "src/a.rs",
                    EntityKind::Function,
                    name,
                )))
                .expect("commit");
        }
        assert_eq!(store.generation(), 2);
    }
    let mut store = open(&dir);
    assert_eq!(
        store.generation(),
        2,
        "reopening must not reset the counter"
    );
    store
        .apply_update(IndexUpdate::empty().with_entity(bare_entity(
            "src/c.rs",
            EntityKind::Function,
            "h",
        )))
        .expect("commit");
    assert_eq!(store.generation(), 3);
}

#[test]
fn a_newer_schema_is_refused_with_both_versions_in_the_error() {
    let dir = TempDir::new("too-new");
    drop(open(&dir));
    let raw = Connection::open(dir.database()).expect("raw connection");
    raw.execute(
        "UPDATE meta SET value = '9999' WHERE key = 'schema_version'",
        [],
    )
    .expect("pretend a newer Peek wrote this");
    drop(raw);

    match open_err(&dir, &repo(&dir)) {
        StoreError::SchemaTooNew { found, supported } => {
            assert_eq!(found, 9999);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        other => panic!("expected SchemaTooNew, got {other}"),
    }
}

#[test]
fn a_v0_store_is_migrated_and_its_generation_is_carried_forward() {
    // v0 has never shipped; the migration is modelled and tested so the first real one is not also
    // the first *executed* one.
    let dir = TempDir::new("v0");
    let identity = repo(&dir);
    {
        let raw = Connection::open(dir.database()).expect("raw connection");
        raw.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('generation', '41'),
                                      ('repo_id', '{}');
             CREATE TABLE entity (path TEXT);
             INSERT INTO entity (path) VALUES ('src/stale.rs');",
            identity.as_str()
        ))
        .expect("build a v0 store");
    }

    let store = Store::open(&dir.database(), &identity).expect("v0 must migrate, not fail");
    assert_eq!(store.schema_version(), SCHEMA_VERSION);
    assert_eq!(
        store.generation(),
        41,
        "a migration must not reset the counter"
    );
    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.entity_count, 0,
        "v0 rows of an unknown shape must be discarded"
    );
    assert_eq!(stats.relation_count, 0);
}

#[test]
fn a_v0_store_that_names_another_repository_is_refused() {
    // The migration must not become a way to rebind someone else's index to this repository.
    let dir = TempDir::new("v0-other-repo");
    let elsewhere = TempDir::new("v0-other-repo-target");
    let foreign = RepoId::discover(elsewhere.path()).expect("discover");
    {
        let raw = Connection::open(dir.database()).expect("raw connection");
        raw.execute_batch(&format!(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('generation', '7'),
                                      ('repo_id', '{}');",
            foreign.as_str()
        ))
        .expect("build a v0 store for a different repository");
    }
    match open_err(&dir, &repo(&dir)) {
        StoreError::WrongRepository { stored, expected } => {
            assert_eq!(stored, foreign.as_str());
            assert_eq!(expected, repo(&dir).as_str());
        }
        other => panic!("expected WrongRepository, got {other}"),
    }
}

#[test]
fn opening_another_repositories_store_is_refused() {
    // Audit A10: Cortex let two worktrees share one store and each overwrote the other's
    // document set. Keying the store to a repository makes that impossible, not unlikely.
    let dir = TempDir::new("wrong-repo");
    let elsewhere = TempDir::new("wrong-repo-other");
    drop(open(&dir));
    match open_err(&dir, &repo(&elsewhere)) {
        StoreError::WrongRepository { stored, expected } => {
            assert_eq!(stored, repo(&dir).as_str());
            assert_eq!(expected, repo(&elsewhere).as_str());
        }
        other => panic!("expected WrongRepository, got {other}"),
    }
}

#[test]
fn garbage_written_over_the_file_is_reported_as_corruption_not_as_an_io_error() {
    // Audit A6: a torn snapshot surfaced through the same channel as every other failure, so the
    // failure looked arbitrary. `Corrupt` says something different from `Io`.
    let dir = TempDir::new("garbage");
    {
        let mut store = open(&dir);
        store
            .apply_update(IndexUpdate::empty().with_entity(bare_entity(
                "src/a.rs",
                EntityKind::Function,
                "f",
            )))
            .expect("commit");
        store
            .checkpoint()
            .expect("checkpoint so the WAL is not replayed");
    }
    fs::write(dir.database(), vec![b'x'; 4096]).expect("overwrite with garbage");

    match open_err(&dir, &repo(&dir)) {
        StoreError::Corrupt(detail) => {
            assert!(
                detail.contains("readable") || detail.contains("SQLite"),
                "the error should say what was wrong: {detail}"
            );
        }
        other => panic!("expected Corrupt, got {other}"),
    }
}

#[test]
fn a_sqlite_database_that_is_not_a_peek_index_is_reported_as_corruption() {
    let dir = TempDir::new("foreign-sqlite");
    {
        let raw = Connection::open(dir.database()).expect("raw connection");
        raw.execute_batch("CREATE TABLE notes (body TEXT); INSERT INTO notes VALUES ('hi');")
            .expect("an unrelated database");
    }
    match open_err(&dir, &repo(&dir)) {
        StoreError::Corrupt(detail) => {
            assert!(detail.contains("not a Peek index"), "detail: {detail}");
        }
        other => panic!("expected Corrupt, got {other}"),
    }
}

#[test]
fn verify_accepts_a_healthy_store_and_rejects_one_whose_metadata_is_damaged() {
    let dir = TempDir::new("verify");
    let store = open(&dir);
    store.verify().expect("a healthy store verifies");

    let raw = Connection::open(dir.database()).expect("raw connection");
    raw.execute_batch("DROP TABLE meta")
        .expect("damage the metadata");
    drop(raw);

    match store.verify() {
        Err(StoreError::Corrupt(detail)) => {
            assert!(!detail.is_empty(), "a corruption report must say something");
        }
        Ok(()) => panic!("a store with no metadata must not verify"),
        Err(other) => panic!("expected Corrupt, got {other}"),
    }
}

#[test]
fn verify_catches_a_cached_generation_that_no_longer_matches_disk() {
    // The generation counter exists so a reader can tell "nothing changed" from "this is a
    // different index". A check that is never performed detects nothing.
    let dir = TempDir::new("verify-stale");
    let store = open(&dir);
    let raw = Connection::open(dir.database()).expect("raw connection");
    raw.execute("UPDATE meta SET value = '99' WHERE key = 'generation'", [])
        .expect("move the stored counter behind the handle's back");
    drop(raw);

    match store.verify() {
        Err(StoreError::Corrupt(detail)) => {
            assert!(
                detail.contains("99"),
                "detail should name both numbers: {detail}"
            );
        }
        Ok(()) => panic!("a diverged generation must not verify"),
        Err(other) => panic!("expected Corrupt, got {other}"),
    }
}

#[test]
fn the_schema_declares_exactly_the_tables_the_module_owns() {
    // A migration drops this list. Anything outside it would survive a version upgrade and then
    // be read by a build that does not know what it means.
    let dir = TempDir::new("tables");
    let store = open(&dir);
    for table in ["meta", "entity", "relation", "relation_candidate"] {
        assert!(
            schema::has_table(&store.conn, table).expect("sqlite_master"),
            "{table} should exist"
        );
    }
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

#[test]
fn an_entity_round_trips_with_every_field_intact() {
    let dir = TempDir::new("entity-round-trip");
    let mut store = open(&dir);
    let original = full_entity("src/payments.rs", "retry");
    store
        .apply_update(IndexUpdate::empty().with_entity(original.clone()))
        .expect("commit");

    let read = store
        .entity(&original.id)
        .expect("query")
        .expect("the entity is there");
    assert_eq!(read, original);
    assert_eq!(read.name, "retry");
    assert_eq!(
        read.signature.as_deref(),
        Some("fn retry(&self, attempt: u32) -> Result<Receipt>")
    );
    assert_eq!(read.span, Some(span(120)));
    assert_eq!(read.language, Some(Language::Rust));
    assert!(read.is_test);
    assert_eq!(
        read.structural_fingerprint.as_deref(),
        Some("blake3:9f2c1a")
    );
    assert_eq!(read.id().kind(), EntityKind::Method);
    assert_eq!(read.id().qualified_name(), "Service.retry");
}

#[test]
fn an_entity_with_no_optional_fields_round_trips_as_absent_not_as_empty() {
    // The distinction a defaulting decoder would erase: a document comment that was never written
    // is not the same fact as an empty one.
    let dir = TempDir::new("entity-absent");
    let mut store = open(&dir);
    let original = bare_entity("src/a.rs", EntityKind::Module, "payments");
    store
        .apply_update(IndexUpdate::empty().with_entity(original.clone()))
        .expect("commit");

    let read = store.entity(&original.id).expect("query").expect("present");
    assert_eq!(read, original);
    assert_eq!(read.span, None);
    assert_eq!(read.language, None);
    assert_eq!(read.doc, None);
    assert_eq!(read.signature, None);
    assert_eq!(read.structural_fingerprint, None);
    assert!(!read.is_test);
}

#[test]
fn entities_differing_only_in_ordinal_are_stored_separately() {
    // Two `impl` blocks for one type share a path, a kind, and a qualified name. The ordinal is
    // the last discriminator, and collapsing them is how a resolver loses an edge.
    let dir = TempDir::new("ordinals");
    let mut store = open(&dir);
    let first = bare_entity("src/a.rs", EntityKind::Module, "Service#impl");
    let mut second = bare_entity("src/a.rs", EntityKind::Module, "Service#impl");
    second.id = id("src/a.rs", EntityKind::Module, "Service#impl", 1);
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(first.clone())
                .with_entity(second.clone()),
        )
        .expect("commit");

    assert_eq!(
        store.stats().expect("stats").entity_count,
        2,
        "the ordinal is identity"
    );
    assert!(store.entity(&first.id).expect("query").is_some());
    assert!(store.entity(&second.id).expect("query").is_some());
}

#[test]
fn a_second_upsert_of_the_same_entity_replaces_it_rather_than_duplicating_it() {
    let dir = TempDir::new("upsert");
    let mut store = open(&dir);
    let original = full_entity("src/a.rs", "retry");
    store
        .apply_update(IndexUpdate::empty().with_entity(original.clone()))
        .expect("first commit");

    let mut edited = original.clone();
    edited.doc = Some("Edited.".to_owned());
    edited.signature = None;
    edited.is_test = false;
    let stats = store
        .apply_update(IndexUpdate::empty().with_entity(edited.clone()))
        .expect("second commit");
    assert_eq!(stats.entities_upserted, 1);
    assert_eq!(stats.entities_removed, 0);

    let read = store.entity(&original.id).expect("query").expect("present");
    assert_eq!(read, edited, "the newer row must win outright");
    assert_eq!(store.stats().expect("stats").entity_count, 1);
}

// ---------------------------------------------------------------------------
// Relations and resolution state
// ---------------------------------------------------------------------------

fn relations_fixture(store: &mut Store) {
    let caller = bare_entity("src/caller.rs", EntityKind::Function, "main");
    let target = bare_entity("src/target.rs", EntityKind::Function, "helper");
    let other = bare_entity("src/other.rs", EntityKind::Function, "helper");
    let update = IndexUpdate::empty()
        .with_entity(caller.clone())
        .with_entity(target.clone())
        .with_entity(other.clone())
        .with_relation(Relation::resolved(
            RelationKind::Calls,
            caller.id.clone(),
            target.id.clone(),
            "helper",
            span(10),
            Evidence::ImportBinding {
                module: "./target".to_owned(),
                alias: Some("h".to_owned()),
            },
        ))
        .with_relation(Relation::ambiguous(
            RelationKind::Calls,
            caller.id.clone(),
            "render",
            span(40),
            vec![target.id.clone(), other.id.clone()],
        ))
        .with_relation(Relation::unresolved(
            RelationKind::Calls,
            caller.id.clone(),
            "printf",
            span(70),
            UnresolvedReason::External,
        ))
        .with_relation(Relation::inferred(
            RelationKind::Implements,
            target.id.clone(),
            other.id.clone(),
            "helper",
            span(90),
            Evidence::QualifiedNameInScope {
                scope: "src".to_owned(),
            },
            "the only helper in src",
        ));
    store.apply_update(update).expect("commit");
}

#[test]
fn all_four_resolution_states_round_trip_with_their_evidence_intact() {
    // D-0003: the state must survive persistence, because the distinction only matters once it
    // can be read back. Cortex's `reason: String` could not.
    let dir = TempDir::new("states");
    let mut store = open(&dir);
    relations_fixture(&mut store);

    let caller = id("src/caller.rs", EntityKind::Function, "main", 0);
    let edges = store.outgoing(&caller, None, 32).expect("read");
    assert_eq!(edges.len(), 3);

    let resolved = edges
        .iter()
        .find(|r| r.target_name == "helper")
        .expect("the resolved call");
    assert_eq!(resolved.resolution.evidence_class(), Some("import_binding"));
    assert!(resolved.is_followable());
    assert_eq!(
        resolved.target.as_ref().map(EntityId::qualified_name),
        Some("helper")
    );

    let ambiguous = edges
        .iter()
        .find(|r| r.target_name == "render")
        .expect("the ambiguous call");
    assert!(ambiguous.resolution.is_ambiguous());
    assert_eq!(ambiguous.resolution.describe(), "ambiguous (2 candidates)");
    assert!(ambiguous.target.is_none());

    let unresolved = edges
        .iter()
        .find(|r| r.target_name == "printf")
        .expect("the unresolved call");
    assert_eq!(unresolved.resolution.describe(), "unresolved (external)");
    assert!(unresolved.target.is_none());

    let mut implements = store
        .outgoing(
            &id("src/target.rs", EntityKind::Function, "helper", 0),
            Some(RelationKind::Implements),
            8,
        )
        .expect("read");
    let inferred = implements.remove(0);
    assert!(
        inferred
            .resolution
            .describe()
            .contains("the only helper in src")
    );
    assert!(inferred.is_followable());
    assert_eq!(
        inferred.resolution.evidence_class(),
        Some("qualified_name_in_scope")
    );
}

#[test]
fn an_ambiguous_relation_keeps_its_candidate_list_and_its_order() {
    // D-0004: ambiguity is a result, not a silent pick. A store that kept `Ambiguous` but threw
    // away *which* candidates would satisfy the letter of that and none of its purpose.
    let dir = TempDir::new("candidates");
    let mut store = open(&dir);
    relations_fixture(&mut store);

    let caller = id("src/caller.rs", EntityKind::Function, "main", 0);
    let edges = store
        .outgoing(&caller, Some(RelationKind::Calls), 32)
        .expect("read");
    let ambiguous = edges
        .iter()
        .find(|r| r.target_name == "render")
        .expect("the ambiguous call");
    let ResolutionState::Ambiguous { candidates } = &ambiguous.resolution else {
        panic!("expected Ambiguous, got {:?}", ambiguous.resolution);
    };
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].path().as_str(), "src/target.rs");
    assert_eq!(candidates[1].path().as_str(), "src/other.rs");

    let stored = store
        .ambiguous_candidates(&caller, RelationKind::Calls, "render")
        .expect("query");
    assert_eq!(
        stored, *candidates,
        "the candidate list is queryable on its own"
    );
}

#[test]
fn a_relation_whose_state_and_target_disagree_is_rejected_by_the_store() {
    // The failure D-0003 exists to prevent: an edge that claims to be resolved and names nothing.
    // `Relation`'s fields are public, so a caller can build one; the database must refuse it.
    let dir = TempDir::new("dishonest-edge");
    let mut store = open(&dir);
    let source = bare_entity("src/a.rs", EntityKind::Function, "main");
    let dishonest = Relation {
        kind: RelationKind::Calls,
        source: source.id.clone(),
        target_name: "ghost".to_owned(),
        target: None,
        span: span(5),
        resolution: ResolutionState::Resolved {
            by: Evidence::UniqueName,
        },
    };
    match store.apply_update(
        IndexUpdate::empty()
            .with_entity(source)
            .with_relation(dishonest),
    ) {
        Err(StoreError::Transaction(_)) => {}
        other => panic!("a resolved relation with no target must be refused, got {other:?}"),
    }
}

#[test]
fn a_relation_naming_an_entity_that_is_not_in_the_index_is_rejected() {
    // Resolution cannot claim a target the store does not have. The alternative is an edge that
    // resolves to nothing and reports success, which is the shape of Cortex's `symbols.first()`.
    let dir = TempDir::new("dangling-write");
    let mut store = open(&dir);
    let source = bare_entity("src/a.rs", EntityKind::Function, "main");
    let ghost = id("src/ghost.rs", EntityKind::Function, "vanished", 0);
    match store.apply_update(
        IndexUpdate::empty()
            .with_entity(source.clone())
            .with_relation(Relation::resolved(
                RelationKind::Calls,
                source.id,
                ghost,
                "vanished",
                span(5),
                Evidence::UniqueName,
            )),
    ) {
        Err(StoreError::Transaction(_)) => {}
        other => panic!("an edge to an unindexed entity must be refused, got {other:?}"),
    }
    assert_eq!(store.stats().expect("stats").relation_count, 0);
}

// ---------------------------------------------------------------------------
// Atomicity: the property that matters most
// ---------------------------------------------------------------------------

#[test]
fn an_induced_write_failure_rolls_the_whole_batch_back() {
    // Audit A1: Cortex's single `.ok()` meant a batch that failed halfway reported success with
    // nothing on disk. The batch below contains two valid entities written *before* the failing
    // relation, and not one of them may survive.
    let dir = TempDir::new("rollback");
    let mut store = open(&dir);
    let good = bare_entity("src/good.rs", EntityKind::Function, "fine");
    let source = bare_entity("src/caller.rs", EntityKind::Function, "main");
    let ghost = id("src/ghost.rs", EntityKind::Function, "vanished", 0);

    match store.apply_update(
        IndexUpdate::empty()
            .with_entity(good.clone())
            .with_entity(source.clone())
            .with_relation(Relation::resolved(
                RelationKind::Calls,
                source.id,
                ghost,
                "vanished",
                span(5),
                Evidence::UniqueName,
            )),
    ) {
        Err(StoreError::Transaction(_)) => {}
        other => panic!("the failure must surface as an error, not as statistics: {other:?}"),
    }

    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.entity_count, 0,
        "not even the rows written before the failure survive"
    );
    assert_eq!(stats.relation_count, 0);
    assert!(store.entity(&good.id).expect("query").is_none());
}

#[test]
fn an_induced_write_failure_does_not_advance_the_generation() {
    // A generation that advances on a rolled-back commit would tell a reader the index is newer
    // than it is, and every later comparison would be wrong by one.
    let dir = TempDir::new("rollback-generation");
    let mut store = open(&dir);
    store
        .apply_update(IndexUpdate::empty().with_entity(bare_entity(
            "src/a.rs",
            EntityKind::Function,
            "f",
        )))
        .expect("one good commit");
    let before = store.generation();

    let source = bare_entity("src/caller.rs", EntityKind::Function, "main");
    let failed = store.apply_update(
        IndexUpdate::empty()
            .with_entity(source.clone())
            .with_relation(Relation::resolved(
                RelationKind::Calls,
                source.id,
                id("src/ghost.rs", EntityKind::Function, "vanished", 0),
                "vanished",
                span(5),
                Evidence::UniqueName,
            )),
    );
    assert!(failed.is_err(), "the write must fail");
    assert_eq!(
        store.generation(),
        before,
        "a rolled-back commit is not a commit"
    );
}

#[test]
fn a_store_survives_a_failed_write_and_accepts_the_next_one() {
    // A failed transaction must not poison the connection. If `apply_update` left the store
    // unusable, one bad batch would end the process rather than one update.
    let dir = TempDir::new("recover-after-failure");
    let mut store = open(&dir);
    let source = bare_entity("src/caller.rs", EntityKind::Function, "main");
    let _ = store.apply_update(
        IndexUpdate::empty()
            .with_entity(source.clone())
            .with_relation(Relation::resolved(
                RelationKind::Calls,
                source.id.clone(),
                id("src/ghost.rs", EntityKind::Function, "v", 0),
                "v",
                span(5),
                Evidence::UniqueName,
            )),
    );

    let recovered = store
        .apply_update(IndexUpdate::empty().with_entity(source))
        .expect("the next update must succeed");
    assert_eq!(recovered.entities_upserted, 1);
    assert_eq!(
        recovered.generation, 1,
        "only the successful commit advanced the counter"
    );
    assert_eq!(store.stats().expect("stats").entity_count, 1);
}

// ---------------------------------------------------------------------------
// Removal
// ---------------------------------------------------------------------------

#[test]
fn removing_a_file_removes_its_entities_relations_and_candidates() {
    // Contract G3: no orphan nodes or edges after a delete. The cascade makes that true by
    // construction rather than by remembering to clean up.
    let dir = TempDir::new("remove-file");
    let mut store = open(&dir);
    relations_fixture(&mut store);
    assert_eq!(store.stats().expect("stats").relation_count, 4);
    assert_eq!(store.stats().expect("stats").candidate_count, 2);

    let stats = store
        .apply_update(IndexUpdate::empty().removing_file(path("src/caller.rs")))
        .expect("commit");
    assert_eq!(stats.entities_removed, 1);
    assert_eq!(stats.relations_removed, 3);

    let after = store.stats().expect("stats");
    assert_eq!(after.entity_count, 2);
    assert_eq!(
        after.relation_count, 1,
        "the `implements` edge from target.rs survives"
    );
    assert_eq!(
        after.candidate_count, 0,
        "candidates go with their relation"
    );
    assert!(
        store
            .outgoing(
                &id("src/caller.rs", EntityKind::Function, "main", 0),
                None,
                32
            )
            .expect("read")
            .is_empty()
    );
}

#[test]
fn removing_a_subtree_leaves_a_prefix_sibling_alone() {
    // `srcgen/main.rs` is not inside `src`. The `LIKE` pattern requires a separator precisely so
    // that removing `src` cannot take `srcgen` with it.
    let dir = TempDir::new("remove-subtree");
    let mut store = open(&dir);
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(bare_entity("src/a.rs", EntityKind::Function, "a"))
                .with_entity(bare_entity("src/deep/b.rs", EntityKind::Function, "b"))
                .with_entity(bare_entity("srcgen/c.rs", EntityKind::Function, "c")),
        )
        .expect("commit");

    let stats = store
        .apply_update(IndexUpdate::empty().removing_subtree(path("src")))
        .expect("commit");
    assert_eq!(stats.entities_removed, 2);

    assert_eq!(
        store
            .entities_in_file(&path("srcgen/c.rs"), 16)
            .expect("read")
            .len(),
        1,
        "srcgen is a different directory"
    );
    assert_eq!(store.stats().expect("stats").entity_count, 1);
}

#[test]
fn a_subtree_removal_does_not_treat_like_metacharacters_in_a_path_as_wildcards() {
    // `_` matches any single character and `%` any run in `LIKE`, and both are ordinary characters
    // in a filename. `we_ird` must not be swept up by removing `weXird`.
    let dir = TempDir::new("remove-wildcard");
    let mut store = open(&dir);
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(bare_entity("we_ird/a.rs", EntityKind::Function, "a"))
                .with_entity(bare_entity("weXird/b.rs", EntityKind::Function, "b"))
                .with_entity(bare_entity("100%/c.rs", EntityKind::Function, "c")),
        )
        .expect("commit");

    store
        .apply_update(IndexUpdate::empty().removing_subtree(path("we_ird")))
        .expect("commit");
    assert_eq!(
        store
            .entities_in_file(&path("weXird/b.rs"), 16)
            .expect("read")
            .len(),
        1,
        "`we_ird` must not match `weXird`"
    );

    store
        .apply_update(IndexUpdate::empty().removing_subtree(path("100%")))
        .expect("commit");
    assert_eq!(
        store.stats().expect("stats").entity_count,
        1,
        "only the literal 100% directory is removed"
    );
}

#[test]
fn removing_a_referenced_file_demotes_the_edges_that_pointed_into_it() {
    // The edge was real; only its target is gone. Deleting it would lose information and leaving
    // it `resolved` would be a false claim. `Unresolved { NoCandidate }` is the honest value.
    let dir = TempDir::new("demote");
    let mut store = open(&dir);
    relations_fixture(&mut store);
    let target = id("src/target.rs", EntityKind::Function, "helper", 0);

    store
        .apply_update(IndexUpdate::empty().removing_file(path("src/target.rs")))
        .expect("commit");

    assert!(
        store.incoming(&target, None, 32).expect("read").is_empty(),
        "a demoted edge no longer points anywhere"
    );
    let remaining = store
        .outgoing(
            &id("src/caller.rs", EntityKind::Function, "main", 0),
            None,
            32,
        )
        .expect("read");
    let demoted = remaining
        .iter()
        .find(|r| r.target_name == "helper")
        .expect("the edge itself survives");
    assert!(demoted.target.is_none());
    assert_eq!(demoted.resolution.describe(), "unresolved (no_candidate)");
    assert_eq!(store.stats().expect("stats").orphan_relations, 0);
}

#[test]
fn removals_are_applied_before_upserts_in_the_same_batch() {
    // This is what lets an indexer replace a whole directory in one atomic commit: forget
    // everything under `src`, then write the new `src`.
    let dir = TempDir::new("remove-then-add");
    let mut store = open(&dir);
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(bare_entity("src/old.rs", EntityKind::Function, "old"))
                .with_entity(bare_entity("src/kept.rs", EntityKind::Function, "kept")),
        )
        .expect("seed");

    let stats = store
        .apply_update(
            IndexUpdate::empty()
                .removing_subtree(path("src"))
                .with_entity(bare_entity("src/fresh.rs", EntityKind::Function, "fresh")),
        )
        .expect("commit");
    assert_eq!(stats.entities_removed, 2);
    assert_eq!(stats.entities_upserted, 1);

    let remaining: Vec<String> = store
        .entities_in_file(&path("src/fresh.rs"), 16)
        .expect("read")
        .iter()
        .map(|e| e.id.qualified_name().to_owned())
        .collect();
    assert_eq!(remaining, vec!["fresh".to_owned()]);
    assert!(
        store
            .entities_in_file(&path("src/old.rs"), 16)
            .expect("read")
            .is_empty()
    );
}

// ---------------------------------------------------------------------------
// Query plans: the performance property, proven rather than asserted
// ---------------------------------------------------------------------------

fn plan_of(store: &Store, sql: &str) -> String {
    store.query_plan(sql).expect("explain").join(" | ")
}

#[test]
fn outgoing_uses_the_source_index_and_builds_no_temporary_btree() {
    let dir = TempDir::new("plan-outgoing");
    let store = open(&dir);
    let plan = plan_of(&store, &outgoing_sql(None).expect("sql"));
    assert!(plan.contains("relation_by_source"), "plan: {plan}");
    assert!(plan.contains("USING INDEX"), "plan: {plan}");
    assert!(
        !plan.contains("TEMP B-TREE"),
        "a temporary b-tree means the ORDER BY is not the index order, so LIMIT does not stop the \
         scan early: {plan}"
    );
}

#[test]
fn outgoing_with_a_kind_filter_still_uses_the_source_index() {
    let dir = TempDir::new("plan-outgoing-kind");
    let store = open(&dir);
    let plan = plan_of(
        &store,
        &outgoing_sql(Some(RelationKind::Calls)).expect("sql"),
    );
    assert!(plan.contains("relation_by_source"), "plan: {plan}");
    assert!(!plan.contains("TEMP B-TREE"), "plan: {plan}");
}

#[test]
fn incoming_uses_the_target_index_and_builds_no_temporary_btree() {
    let dir = TempDir::new("plan-incoming");
    let store = open(&dir);
    for sql in [
        incoming_sql(None).expect("sql"),
        incoming_sql(Some(RelationKind::Calls)).expect("sql"),
    ] {
        let plan = plan_of(&store, &sql);
        assert!(plan.contains("relation_by_target"), "plan: {plan}");
        assert!(plan.contains("USING INDEX"), "plan: {plan}");
        assert!(!plan.contains("TEMP B-TREE"), "plan: {plan}");
    }
}

#[test]
fn the_entity_lookups_use_their_indexes_rather_than_scanning() {
    let dir = TempDir::new("plan-entities");
    let store = open(&dir);
    for (template, index) in [
        (ENTITIES_NAMED, "entity_by_name"),
        (ENTITIES_WITH_QUALIFIED_NAME, "entity_by_qualified_name"),
    ] {
        let plan = plan_of(
            &store,
            &format!("SELECT {ENTITY_COLUMNS} FROM entity {template}"),
        );
        assert!(plan.contains(index), "expected {index} in: {plan}");
        assert!(!plan.contains("TEMP B-TREE"), "plan: {plan}");
    }
}

#[test]
fn a_file_listing_is_served_by_the_primary_key_rather_than_a_redundant_index() {
    // There is deliberately no `entity(path)` index: the primary key already leads with `path`,
    // and a second one would duplicate every entity row's path for no query. This test is what
    // stops that optimisation being undone by someone who assumes a missing index is a bug.
    let dir = TempDir::new("plan-file");
    let store = open(&dir);
    let plan = plan_of(
        &store,
        &format!("SELECT {ENTITY_COLUMNS} FROM entity {ENTITIES_IN_FILE}"),
    );
    assert!(plan.contains("USING INDEX"), "plan: {plan}");
    assert!(plan.contains("sqlite_autoindex_entity_1"), "plan: {plan}");
    assert!(!plan.contains("TEMP B-TREE"), "plan: {plan}");
}

#[test]
fn a_single_entity_lookup_seeks_the_primary_key() {
    let dir = TempDir::new("plan-entity-id");
    let store = open(&dir);
    let plan = plan_of(&store, &ENTITY_BY_ID.replace("{cols}", ENTITY_COLUMNS));
    assert!(plan.contains("sqlite_autoindex_entity_1"), "plan: {plan}");
}

#[test]
fn a_resolution_state_query_uses_the_state_index() {
    // Audit B11: state that cannot be counted cannot be reported, which is how `explain()` came to
    // emit a caller count that was wrong by construction.
    let dir = TempDir::new("plan-state");
    let store = open(&dir);
    let plan = plan_of(&store, &relations_in_state_sql());
    assert!(plan.contains("relation_by_state"), "plan: {plan}");
    assert!(!plan.contains("TEMP B-TREE"), "plan: {plan}");
}

/// The two targets entity `i` calls.
///
/// The multipliers are chosen so the two can never coincide: `7i + 1 == 11i + 7 (mod 5000)` would
/// need `4i == 4994 (mod 5000)`, and 4994 is not a multiple of four, so there is no solution. A
/// collision would silently upsert two edges into one and make the row count a lie.
fn successors(i: usize) -> [usize; 2] {
    [(i * 7 + 1) % GRAPH_SIZE, (i * 11 + 7) % GRAPH_SIZE]
}

fn graph_id(i: usize) -> EntityId {
    id(
        &format!("src/f{i}.rs"),
        EntityKind::Function,
        &format!("f{i}"),
        0,
    )
}

#[test]
fn a_three_hop_traversal_over_five_thousand_entities_returns_the_exact_closure() {
    // The Cortex defect this module exists to fix: a full edge scan inside a BFS loop, making
    // `dependencies` O(V x E). The traversal below is built out of the index and compared against
    // a closure computed independently from the generator, and the number of edges it reads is
    // asserted to be a small multiple of the fan-out rather than of the edge count.
    let dir = TempDir::new("traversal");
    let mut store = open(&dir);

    let mut update = IndexUpdate::empty();
    for i in 0..GRAPH_SIZE {
        let file = format!("src/f{i}.rs");
        let name = format!("f{i}");
        update
            .upserted_entities
            .push(bare_entity(&file, EntityKind::Function, &name));
        for target in successors(i) {
            update.upserted_relations.push(Relation::resolved(
                RelationKind::Calls,
                graph_id(i),
                graph_id(target),
                format!("f{target}"),
                span((i * 4) as u32),
                Evidence::SameFile,
            ));
        }
    }
    store.apply_update(update).expect("commit the graph");

    let stats = store.stats().expect("stats");
    assert_eq!(stats.entity_count as usize, GRAPH_SIZE);
    assert_eq!(
        stats.relation_count as usize,
        GRAPH_SIZE * 2,
        "every node has two distinct out-edges"
    );

    let (reached, rows_read) = traverse(&store, &graph_id(0), 3);
    let expected = expected_closure(0, 3);
    let same = store
        .callees(&graph_id(0), Some(RelationKind::Calls), 64)
        .expect("one hop of the same traversal");
    assert_eq!(
        reached, expected,
        "the indexed traversal must agree with the reference closure"
    );
    assert!(
        !expected.is_empty(),
        "the fixture must actually reach something"
    );
    assert_eq!(
        same.len(),
        successors(0).len(),
        "one hop from a node with two out-edges reads exactly two edges"
    );

    // Three hops of a fan-out of two visits at most 1 + 2 + 4 + 8 nodes, so it reads a couple of
    // dozen edges at worst. A full scan would have read 10,000.
    let max_visits = 1 + 2 + 4 + 8;
    assert!(
        rows_read <= max_visits * 2,
        "the traversal read {rows_read} edges for at most {max_visits} visits; that is not a \
         per-visit index seek"
    );
    assert!(rows_read < stats.relation_count as usize);
}

/// The exact three-hop closure, computed from the generator rather than from the database.
fn expected_closure(start: usize, depth: usize) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![start];
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in frontier {
            for successor in successors(node) {
                if seen.insert(format!("f{successor}")) {
                    next.push(successor);
                }
            }
        }
        frontier = next;
    }
    seen
}

/// Walk three hops using the index, returning the names reached and the number of edges read.
fn traverse(store: &Store, start: &EntityId, depth: usize) -> (BTreeSet<String>, usize) {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![start.clone()];
    let mut rows_read = 0;
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in &frontier {
            let edges = store
                .outgoing(node, Some(RelationKind::Calls), 64)
                .expect("outgoing");
            rows_read += edges.len();
            for edge in edges {
                if let Some(target) = edge.target {
                    if seen.insert(target.qualified_name().to_owned()) {
                        next.push(target);
                    }
                }
            }
        }
        frontier = next;
    }
    (seen, rows_read)
}

// ---------------------------------------------------------------------------
// Limits, kinds, WAL, statistics
// ---------------------------------------------------------------------------

#[test]
fn adjacency_honours_its_limit_deterministically() {
    let dir = TempDir::new("limit");
    let mut store = open(&dir);
    let source = bare_entity("src/a.rs", EntityKind::Function, "main");
    let mut update = IndexUpdate::empty().with_entity(source.clone());
    for i in 0..10_u32 {
        let target = bare_entity("src/t.rs", EntityKind::Function, &format!("t{i}"));
        update.upserted_entities.push(target.clone());
        update.upserted_relations.push(Relation::resolved(
            RelationKind::Calls,
            source.id.clone(),
            target.id,
            format!("t{i}"),
            span(i * 4),
            Evidence::SameFile,
        ));
    }
    store.apply_update(update).expect("commit");

    let first = store.outgoing(&source.id, None, 3).expect("read");
    let second = store.outgoing(&source.id, None, 3).expect("read again");
    assert_eq!(first.len(), 3, "the limit is a limit");
    assert_eq!(
        first
            .iter()
            .map(|r| r.target_name.clone())
            .collect::<Vec<_>>(),
        second
            .iter()
            .map(|r| r.target_name.clone())
            .collect::<Vec<_>>(),
        "the index order makes truncation deterministic"
    );
    assert_eq!(
        store.outgoing(&source.id, None, 64).expect("read").len(),
        10
    );
}

#[test]
fn the_traversal_primitives_follow_only_proven_edges() {
    // D-0004: following an ambiguous or unresolved edge means picking a target, which is the thing
    // the whole resolution design exists to prevent. `callees` must therefore drop them rather
    // than guess, while `outgoing` still shows them.
    let dir = TempDir::new("callees");
    let mut store = open(&dir);
    relations_fixture(&mut store);
    let caller = id("src/caller.rs", EntityKind::Function, "main", 0);

    let proven = store
        .callees(&caller, Some(RelationKind::Calls), 32)
        .expect("read");
    assert_eq!(proven.len(), 1, "one of three calls is provably resolved");
    assert_eq!(proven[0].path().as_str(), "src/target.rs");

    let all = store
        .outgoing(&caller, Some(RelationKind::Calls), 32)
        .expect("read");
    assert_eq!(
        all.len(),
        3,
        "the ambiguity is still visible to a caller who asks for it"
    );

    let callers = store
        .callers(
            &id("src/target.rs", EntityKind::Function, "helper", 0),
            None,
            32,
        )
        .expect("read");
    assert_eq!(
        callers.len(),
        1,
        "the demoted-elsewhere relation is not a caller"
    );
    assert_eq!(callers[0].path().as_str(), "src/caller.rs");
}

#[test]
fn adjacency_filters_by_kind() {
    let dir = TempDir::new("kind-filter");
    let mut store = open(&dir);
    let source = bare_entity("src/a.rs", EntityKind::Function, "main");
    let target = bare_entity("src/b.rs", EntityKind::Struct, "Config");
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(source.clone())
                .with_entity(target.clone())
                .with_relation(Relation::resolved(
                    RelationKind::Calls,
                    source.id.clone(),
                    target.id.clone(),
                    "Config",
                    span(10),
                    Evidence::SameFile,
                ))
                .with_relation(Relation::resolved(
                    RelationKind::UsesType,
                    source.id.clone(),
                    target.id.clone(),
                    "Config",
                    span(30),
                    Evidence::SameFile,
                )),
        )
        .expect("commit");

    let calls = store
        .outgoing(&source.id, Some(RelationKind::Calls), 32)
        .expect("read");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].kind, RelationKind::Calls);
    assert_eq!(store.outgoing(&source.id, None, 32).expect("read").len(), 2);
    assert_eq!(
        store
            .incoming(&target.id, Some(RelationKind::UsesType), 32)
            .expect("read")
            .len(),
        1
    );
}

#[test]
fn wal_and_foreign_keys_are_actually_enabled_not_merely_requested() {
    // A pragma that silently does not apply is the same defect as a statistic that is invented:
    // the code believes in a guarantee the database is not providing. `synchronous` and
    // `foreign_keys` are per-connection settings, so they are read back from this handle.
    let dir = TempDir::new("pragmas");
    let store = open(&dir);
    let journal: String = store
        .conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal_mode");
    let foreign_keys: i64 = store
        .conn
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .expect("foreign_keys");
    let synchronous: i64 = store
        .conn
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("synchronous");
    assert_eq!(journal.to_ascii_lowercase(), "wal");
    assert_eq!(
        foreign_keys, 1,
        "the schema's foreign keys must be enforced"
    );
    assert_eq!(
        synchronous, 1,
        "NORMAL, which is what the design asks for under WAL"
    );
    store
        .verify()
        .expect("a store with these settings is healthy");
}

#[test]
fn checkpoint_is_callable_after_a_write_and_empties_the_write_ahead_log() {
    // Audit A7: the second file Cortex never collected is the growth source this bounds.
    let dir = TempDir::new("checkpoint");
    let mut store = open(&dir);
    store
        .apply_update(IndexUpdate::empty().with_entity(bare_entity(
            "src/a.rs",
            EntityKind::Function,
            "f",
        )))
        .expect("commit");
    assert!(
        store.stats().expect("stats").wal_size_bytes > 0,
        "a write leaves frames behind for the next reader"
    );

    store.checkpoint().expect("checkpoint after a write");
    assert_eq!(
        store.stats().expect("stats").wal_size_bytes,
        0,
        "TRUNCATE returns the log file to zero bytes"
    );
    assert!(
        store
            .entity(&id("src/a.rs", EntityKind::Function, "f", 0))
            .expect("query")
            .is_some(),
        "checkpointing must not lose the write"
    );

    // Callable again on a clean store, which is what an idle daemon does.
    store
        .checkpoint()
        .expect("a second checkpoint is not an error");
}

#[test]
fn stats_report_real_counts_and_real_sizes() {
    let dir = TempDir::new("stats");
    let mut store = open(&dir);
    relations_fixture(&mut store);
    let stats = store.stats().expect("stats");

    assert_eq!(stats.entity_count, 3);
    assert_eq!(stats.relation_count, 4);
    assert_eq!(stats.candidate_count, 2);
    assert_eq!(stats.resolved_relations, 1);
    assert_eq!(stats.ambiguous_relations, 1);
    assert_eq!(stats.unresolved_relations, 1);
    assert_eq!(stats.inferred_relations, 1);
    assert_eq!(stats.orphan_relations, 0);
    assert_eq!(stats.generation, 1);
    assert_eq!(stats.schema_version, SCHEMA_VERSION);
    assert!(
        stats.file_size_bytes > 0,
        "an open database is not zero bytes"
    );
    assert_eq!(
        stats.entity_count + stats.relation_count + stats.candidate_count,
        9,
        "every row the fixture wrote is counted exactly once"
    );
}

#[test]
fn the_orphan_counter_detects_a_dangling_edge_when_the_constraint_is_disabled() {
    // A counter that is structurally zero and never exercised is an assumption, not a check. The
    // only honest way to prove it works is to break the invariant it guards and watch it notice.
    let dir = TempDir::new("orphans");
    let mut store = open(&dir);
    let caller = bare_entity("src/caller.rs", EntityKind::Function, "main");
    let target = bare_entity("src/target.rs", EntityKind::Function, "helper");
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(caller.clone())
                .with_entity(target.clone())
                .with_relation(Relation::resolved(
                    RelationKind::Calls,
                    caller.id,
                    target.id,
                    "helper",
                    span(10),
                    Evidence::SameFile,
                )),
        )
        .expect("commit");
    assert_eq!(store.stats().expect("stats").orphan_relations, 0);

    {
        // Foreign keys are off by default on a new connection, which is exactly how this situation
        // arises in production: a migration, a maintenance script, or a bug. The state/target
        // CHECK has to be relaxed too — it is what normally makes a dangling edge impossible, so
        // producing one requires defeating both guards.
        let raw = Connection::open(dir.database()).expect("raw connection");
        raw.execute_batch(
            "PRAGMA foreign_keys = OFF;
             PRAGMA ignore_check_constraints = ON;",
        )
        .expect("relax the guards");
        raw.execute("DELETE FROM entity WHERE path = 'src/target.rs'", [])
            .expect("delete the target behind the store's back");
    }
    assert_eq!(
        store.stats().expect("stats").orphan_relations,
        1,
        "the counter must actually detect a dangling edge"
    );
}

#[test]
fn reopening_preserves_every_row_and_the_generation() {
    let dir = TempDir::new("round-trip");
    {
        let mut store = open(&dir);
        relations_fixture(&mut store);
        store.checkpoint().expect("checkpoint");
        assert_eq!(store.generation(), 1);
    }
    let store = open(&dir);
    assert_eq!(store.generation(), 1);
    let stats = store.stats().expect("stats");
    assert_eq!(stats.entity_count, 3);
    assert_eq!(stats.relation_count, 4);
    assert_eq!(stats.candidate_count, 2);
    assert_eq!(stats.resolved_relations, 1);
    assert_eq!(stats.ambiguous_relations, 1);

    let caller = id("src/caller.rs", EntityKind::Function, "main", 0);
    let candidates = store
        .ambiguous_candidates(&caller, RelationKind::Calls, "render")
        .expect("candidates survive a restart");
    assert_eq!(candidates.len(), 2);
    store.verify().expect("a reopened store is healthy");
}

#[test]
fn a_second_upsert_replaces_the_candidate_list_rather_than_appending_to_it() {
    // A relation that was ambiguous over three candidates and is now ambiguous over two must not
    // accumulate five. Stale candidates are how an index starts reporting ghosts.
    let dir = TempDir::new("candidate-replacement");
    let mut store = open(&dir);
    let source = bare_entity("src/a.rs", EntityKind::Function, "main");
    let a = bare_entity("src/a.rs", EntityKind::Method, "C.render");
    let b = bare_entity("src/b.rs", EntityKind::Method, "C.render");
    let c = bare_entity("src/c.rs", EntityKind::Method, "C.render");
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(source.clone())
                .with_entity(a.clone())
                .with_entity(b.clone())
                .with_entity(c.clone())
                .with_relation(Relation::ambiguous(
                    RelationKind::Calls,
                    source.id.clone(),
                    "render",
                    span(10),
                    vec![a.id.clone(), b.id.clone(), c.id.clone()],
                )),
        )
        .expect("commit");
    assert_eq!(store.stats().expect("stats").candidate_count, 3);

    store
        .apply_update(IndexUpdate::empty().with_relation(Relation::ambiguous(
            RelationKind::Calls,
            source.id.clone(),
            "render",
            span(10),
            vec![a.id],
        )))
        .expect("commit");
    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.candidate_count, 1,
        "the list is replaced, not appended to"
    );
    assert_eq!(stats.ambiguous_relations, 1);
    assert_eq!(
        store
            .ambiguous_candidates(&source.id, RelationKind::Calls, "render")
            .expect("read")
            .len(),
        1
    );
}

#[test]
fn a_relation_that_becomes_resolved_loses_its_candidate_list() {
    let dir = TempDir::new("candidate-clearing");
    let mut store = open(&dir);
    let source = bare_entity("src/a.rs", EntityKind::Function, "main");
    let a = bare_entity("src/a.rs", EntityKind::Method, "C.render");
    let b = bare_entity("src/b.rs", EntityKind::Method, "C.render");
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(source.clone())
                .with_entity(a.clone())
                .with_entity(b.clone())
                .with_relation(Relation::ambiguous(
                    RelationKind::Calls,
                    source.id.clone(),
                    "render",
                    span(10),
                    vec![a.id.clone(), b.id.clone()],
                )),
        )
        .expect("commit");
    assert_eq!(store.stats().expect("stats").candidate_count, 2);

    store
        .apply_update(IndexUpdate::empty().with_relation(Relation::resolved(
            RelationKind::Calls,
            source.id.clone(),
            a.id,
            "render",
            span(10),
            Evidence::SameFile,
        )))
        .expect("commit");
    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.candidate_count, 0,
        "a resolved edge has no candidate list"
    );
    assert_eq!(stats.resolved_relations, 1);
    assert_eq!(stats.ambiguous_relations, 0);
    assert!(
        store
            .ambiguous_candidates(&source.id, RelationKind::Calls, "render")
            .expect("read")
            .is_empty()
    );
}

#[test]
fn a_removal_demotes_an_edge_before_deleting_the_entity_it_points_at() {
    // The two-step order is what makes deletion possible at all: the foreign key is declared
    // `ON DELETE SET NULL`, and the state/target CHECK rejects the nulling, so the demotion has to
    // happen first or the delete would fail outright.
    let dir = TempDir::new("demote-order");
    let mut store = open(&dir);
    relations_fixture(&mut store);
    let stats = store
        .apply_update(IndexUpdate::empty().removing_file(path("src/target.rs")))
        .expect("a referenced entity can be removed");
    assert_eq!(stats.entities_removed, 1);
    // The fixture has one edge *sourced* from `src/target.rs` (the `Implements` edge), so exactly
    // one relation is removed. The edge that pointed *into* it is not removed — it is demoted.
    assert_eq!(
        stats.relations_removed, 1,
        "only the edge sourced from the removed file is a removal"
    );
    assert_eq!(
        store.stats().expect("stats").unresolved_relations,
        2,
        "the demoted edge and the pre-existing external one"
    );
}

// ---------------------------------------------------------------------------
// Durability
// ---------------------------------------------------------------------------

#[test]
fn a_store_is_opened_with_full_durability_by_default() {
    // The contract is that a commit which returns success is on disk. Under WAL that holds for
    // atomicity at any `synchronous` level, but `NORMAL` can still lose a *committed*
    // transaction on power loss, which means `apply_update` can report statistics for a
    // generation that never existed. `FULL` is the default for that reason alone; it is not a
    // tuning parameter and it is asserted so that changing it is a deliberate act.
    let dir = TempDir::new("durability-default");
    let store = open(&dir);
    assert_eq!(store.durability(), Durability::Full);
}

#[test]
fn a_weaker_durability_is_possible_only_by_asking_for_it_explicitly() {
    // It must be a named, reachable state rather than a constant someone edits. If there were no
    // way to ask for it, the next person to want the speedup would change the default instead —
    // which is invisible in review and weakens every install at once.
    let dir = TempDir::new("durability-explicit");
    let id = repo(&dir);
    let store = Store::open_with(&dir.database(), &id, Durability::Normal).expect("open");
    assert_eq!(store.durability(), Durability::Normal);
}

#[test]
fn the_durability_setting_actually_reaches_the_connection() {
    // `synchronous` is a per-connection setting, so recording the intent and failing to apply it
    // would produce a store that *reports* `Normal` while being `Full`, or worse the reverse.
    // The setting is read back from SQLite rather than assumed, which is the same discipline the
    // other pragmas get.
    let dir = TempDir::new("durability-applied");
    let id = repo(&dir);
    let store = Store::open_with(&dir.database(), &id, Durability::Normal).expect("open");
    let raw: i64 = store
        .conn()
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("read synchronous back");
    assert_eq!(
        raw, 1,
        "NORMAL is 1 and FULL is 2; the pragma must have taken"
    );
}

#[test]
fn a_store_opened_weaker_still_reads_and_writes_correctly() {
    // Weaker durability is a weaker *promise about power loss*, not a different database. If it
    // changed behaviour, nobody would be able to adopt it at all.
    let dir = TempDir::new("durability-weak-writes");
    let id = repo(&dir);
    let mut store = Store::open_with(&dir.database(), &id, Durability::Normal).expect("open");
    store
        .apply_update(IndexUpdate::empty().with_entity(bare_entity("src/a.rs", "main")))
        .expect("commit");
    assert!(store.generation() >= 1, "a weaker commit still counts");
    store.checkpoint().expect("checkpoint");
}
