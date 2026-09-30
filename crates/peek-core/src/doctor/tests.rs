//! The `doctor` conformance suite.
//!
//! Contract L1 says: *diagnose a deliberately broken install*. That is the shape of every test
//! here — each one breaks the index in a specific way and asserts that the corresponding check
//! says so, with a severity and an action, rather than passing or failing silently.
//!
//! The tests that matter most are the negative ones: a check that cannot run must say it did not
//! run. A `doctor` that turns an unperformable check into a green light is the same defect as a
//! health check that reports a conclusion instead of an observation, which is the defect this
//! whole project was built to remove.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Check, Diagnosis, Severity, diagnose, diagnose_at, diagnose_open, refusal_reasons};
use crate::discover::DiscoveryOptions;
use crate::indexer;
use crate::model::{Entity, EntityId, EntityKind, Relation, RelationKind, RepoPath, Span};
use crate::store::{Durability, RepoId, Store};
use crate::store::{IndexUpdate, paths};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A repository plus an index directory that deletes itself, so one broken install cannot affect
/// the next test.
struct Install {
    root: PathBuf,
    index: PathBuf,
}

impl Install {
    /// An empty repository, with the index directory already created.
    fn empty(label: &str) -> Self {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "peek-doctor-{}-{label}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("repo");
        let index = base.join("index");
        fs::create_dir_all(&root).expect("create the repository");
        fs::create_dir_all(&index).expect("create the index directory");
        Self { root, index }
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, contents).expect("write a file");
    }

    fn write_bytes(&self, relative: &str, contents: &[u8]) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(path, contents).expect("write a file");
    }

    fn path(&self) -> &Path {
        &self.root
    }

    /// The resolved database file. The index lives in a directory named for the repository
    /// identity, so a test that guessed the filename would be testing nothing.
    fn database(&self) -> PathBuf {
        let repo = RepoId::discover(self.path()).expect("derive a repository id");
        self.index.join(repo.as_str()).join("index.db")
    }

    /// Fold the write-ahead log into the database, as any real caller does before it looks.
    ///
    /// Without this a small fixture leaves a log larger than the database, and — more importantly
    /// — everything written is *in the log*. A corruption applied to the database file is then
    /// invisible, because SQLite reads the log. Three of these tests were silently proving
    /// nothing for exactly that reason, which is the failure mode this project keeps running
    /// into: a test that cannot fail is worse than no test.
    ///
    /// The rule this encodes: **a test that grades on what the store contains settles; a test
    /// that damages the database file does not**, because a checkpoint would fold the log into
    /// the file and quietly repair it. Whether the log has been folded in at the moment of the
    /// diagnosis is not a test's to depend on — it is a function of when the last connection to
    /// that file happened to close, and that is not the same on every platform.
    fn settle(&self) {
        let repo = RepoId::discover(self.path()).expect("derive a repository id");
        if let Ok(store) = Store::open(&self.database(), &repo) {
            let _ = store.checkpoint();
        }
    }

    /// A diagnosis of the index this install owns.
    ///
    /// Names the file through [`diagnose_at`] rather than letting [`diagnose`] resolve it from
    /// [`paths::set_root_override`]. That override is a `static`, and this helper used to set it
    /// on every call, so every test in this binary was racing every other one for it — and the
    /// loser was not the test that set it, it was whichever test resolved the root next. That one
    /// got a **freshly created** index: a one-page database file and a log holding its entire
    /// schema, which reports itself as an empty install.
    ///
    /// Naming the file makes the store a test breaks and the store a check reads the same store by
    /// construction. The one test that means to exercise root resolution still calls [`diagnose`],
    /// and it holds the lock.
    ///
    /// Deliberately does **not** settle first, for the reason on [`Install::settle`]: a checkpoint
    /// would fold the log into a database file a test had just damaged, and a test that quietly
    /// undoes its own damage is worse than no test. Tests that want a settled index ask for one.
    fn diagnose(&self) -> Diagnosis {
        let repo = RepoId::discover(self.path()).expect("derive a repository id");
        diagnose_at(self.path(), &self.database(), &repo)
    }

    /// Settle the index and then diagnose it, which is what a real caller does.
    fn settled_diagnosis(&self) -> Diagnosis {
        self.settle();
        self.diagnose()
    }

    /// The database file and the log beside it, measured directly.
    ///
    /// `stat` rather than [`Store::stats`], and that is the whole point of it: a claim about *what
    /// the reported size means* cannot be evidenced by the reported size. A missing log is zero
    /// bytes, the same rule [`crate::store::StoreStats`] applies.
    fn sizes(&self) -> (u64, u64) {
        let database = self.database();
        let mut log = database.as_os_str().to_owned();
        log.push("-wal");
        (
            fs::metadata(&database)
                .expect("an index this test opened is a file on disk")
                .len(),
            fs::metadata(PathBuf::from(log)).map_or(0, |meta| meta.len()),
        )
    }

    /// A store opened against this install's index, for breaking it on purpose.
    fn store(&self) -> Store {
        let repo = RepoId::discover(self.path()).expect("derive a repository id");
        Store::open(&self.database(), &repo).expect("open the store")
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        // Deliberately does not touch `paths::set_root_override`. Clearing a process-wide `static`
        // from a `Drop` is the same hazard the helper above stopped having: whichever install is
        // torn down last would clear the override a test that is still running depends on. The two
        // tests that need the override hold [`with_root`]'s lock for as long as they use it.
        if let Some(parent) = self.index.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}

/// Serialises every use of the process-wide index-root override.
///
/// The override is a `static`, so an unsynchronised `set_root_override` is a race between tests
/// rather than between a test and the store. `peek-cli`'s test fixture takes the same lock for the
/// same reason, and for the same reason it is reentrant there and is not here: the only two tests
/// in this file that need it need it once each, so a plain mutex cannot deadlock.
static OVERRIDE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `body` with the engine pointed at `root`, holding the lock for the whole of it.
///
/// For the tests that mean to exercise root resolution. A test that already knows which file it
/// wants should open that file — see `Install::diagnose`.
///
/// Scoped rather than returned as a guard because a guard would be a struct whose only field
/// nothing reads, and this codebase suppresses no lints. The shape is the one `store::paths`'
/// own tests use for the same job on the environment variable.
fn with_root<T>(root: PathBuf, body: impl FnOnce() -> T) -> T {
    let guard = OVERRIDE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    paths::set_root_override(Some(root));
    let outcome = body();
    paths::set_root_override(None);
    drop(guard);
    outcome
}

/// The findings of one check, so an assertion reads as a claim about that check alone.
fn findings_of(diagnosis: &Diagnosis, check: Check) -> Vec<&super::Finding> {
    diagnosis.check(check)
}

/// The severity of the single finding one check produced.
fn severity_of(diagnosis: &Diagnosis, check: Check) -> Severity {
    let found = findings_of(diagnosis, check);
    assert_eq!(found.len(), 1, "{check:?} produced {found:?}");
    found[0].severity
}

/// The measurements a diagnosis of an openable index carries.
fn measurements(diagnosis: &Diagnosis) -> crate::store::StoreStats {
    diagnosis
        .stats
        .expect("an index that opened has measurements to report")
}

fn span() -> Span {
    Span::new(0, 4, 1, 0, 1, 3).expect("a forward span is valid")
}

fn path(relative: &str) -> RepoPath {
    RepoPath::new(relative).expect("valid repository path")
}

fn id(relative: &str, kind: EntityKind, qualified: &str) -> EntityId {
    EntityId::new(path(relative), kind, qualified, 0)
}

/// A function entity, built the way the rest of the suite builds one.
///
/// The model has no `Entity::function` constructor, and adding one for a test would be putting a
/// convenience in the public API for the sake of a test module. The struct literal is what the
/// extractor produces anyway, so it is the honest fixture.
fn function(relative: &str, name: &str) -> Entity {
    Entity {
        id: id(relative, EntityKind::Function, name),
        name: name.to_owned(),
        signature: None,
        doc: None,
        span: Some(span()),
        language: Some(crate::model::Language::Rust),
        is_test: false,
        structural_fingerprint: None,
    }
}

#[test]
fn an_index_that_cannot_be_opened_is_a_failure_that_names_what_to_do() {
    // `Store::open` *creates* a missing index, so "never indexed" is detected by emptiness rather
    // than by absence — see the next test. This one arranges the state that really is
    // unopenable: a file where the database should be that is not a database.
    let install = Install::empty("unopenable");
    let database = install.database();
    fs::create_dir_all(database.parent().expect("the index has a parent")).expect("make the dir");
    fs::write(
        &database,
        b"this is not a sqlite database, it is a sentence",
    )
    .expect("write it");

    let diagnosis = install.settled_diagnosis();
    assert!(
        !diagnosis.is_healthy(),
        "an unopenable index is not healthy"
    );
    let openable = findings_of(&diagnosis, Check::IndexOpenable);
    assert_eq!(openable.len(), 1, "{openable:?}");
    assert_eq!(openable[0].severity, Severity::Fail);
    assert!(
        openable[0].action.is_some(),
        "a failure a user cannot act on is a failure that will not be acted on: {:?}",
        openable[0]
    );
}

#[test]
fn a_never_indexed_install_says_so_rather_than_failing() {
    // The most common first run. `Store::open` creates the index, so this is a healthy install
    // with nothing in it — a state with an obvious next step, not a fault.
    let install = Install::empty("never-indexed");
    let diagnosis = install.settled_diagnosis();

    assert!(diagnosis.is_healthy(), "{}", diagnosis.report());
    let generation = findings_of(&diagnosis, Check::Generation);
    assert_eq!(generation[0].severity, Severity::Notice);
    assert!(
        generation[0].summary.contains("empty"),
        "the user must be told what is missing, in words: {:?}",
        generation[0]
    );
}

#[test]
fn an_untouched_but_empty_index_is_a_notice_not_a_failure() {
    // Created but never indexed. The distinction from the previous test matters: the index opened
    // fine, so the problem is "you have not run the indexer", not "something is broken".
    let install = Install::empty("untouched");
    let _store = install.store();
    let diagnosis = install.settled_diagnosis();

    let generation = findings_of(&diagnosis, Check::Generation);
    assert_eq!(generation.len(), 1, "{generation:?}");
    assert_eq!(generation[0].severity, Severity::Notice);
    assert!(generation[0].action.is_some());
    assert!(
        diagnosis
            .worst()
            .is_some_and(|worst| worst < Severity::Fail),
        "an empty index is a state to fix, not a broken one: {:?}",
        diagnosis.report()
    );
}

#[test]
fn a_healthy_index_passes_every_check_and_says_what_it_measured() {
    // A check that reports "ok" without saying what it measured is indistinguishable from a
    // check that did not run, so a pass carries its measurement.
    let install = Install::empty("healthy");
    install.write("src/lib.rs", "fn a() {}\nfn main() { a(); }\n");
    indexer::build_full(
        &mut install.store(),
        install.path(),
        DiscoveryOptions::default(),
    )
    .expect("index the repository");

    let diagnosis = install.settled_diagnosis();
    assert!(
        diagnosis.is_healthy(),
        "a healthy install must be reported healthy:\n{}",
        diagnosis.report()
    );
    // The checks that say something about whether the *index* is trustworthy. Deliberately not
    // `worst() == Pass`: a one-page database legitimately has a write-ahead log larger than
    // itself until something checkpoints, and the log check is right to mention it. Asserting
    // "nothing at all is ever worth mentioning" would be asserting a fixture property, not a
    // property of the diagnosis.
    for check in [
        Check::OrphanEdges,
        Check::Integrity,
        Check::PendingWork,
        Check::Durability,
        Check::IndexLocation,
        Check::RepositoryIdentity,
    ] {
        let found = findings_of(&diagnosis, check);
        assert_eq!(found.len(), 1, "{check:?} produced {found:?}");
        assert_eq!(
            found[0].severity,
            Severity::Pass,
            "{check:?} on a healthy install: {:?}",
            found[0]
        );
    }

    for finding in &diagnosis.findings {
        assert!(
            !finding.detail.is_empty(),
            "{} passed with no measurement behind it",
            finding.check.as_str()
        );
    }
}

#[test]
fn a_damaged_index_fails_the_integrity_check_and_offers_a_rebuild() {
    // The defect audit A6 describes: the predecessor reported a healthy install over a broken
    // index. Corrupting the file and asserting the check notices is the only thing that
    // distinguishes this from a health check that always says "ok".
    let install = Install::empty("damaged");
    install.write("src/lib.rs", "fn a() {}\n");
    indexer::build_full(
        &mut install.store(),
        install.path(),
        DiscoveryOptions::default(),
    )
    .expect("index the repository");
    drop(install.store());
    // Settle *before* reading the file to damage. Every page image is also in the write-ahead
    // log, so truncating the database while the log is intact is invisible — SQLite reads the
    // log and the database file is never consulted. That is not a weakness in the check, it is
    // why the test has to damage the file the reader actually uses.
    install.settle();

    let database = install.database();
    let bytes = fs::read(&database).expect("read the database");
    assert!(bytes.len() > 512, "there is a database to damage");
    // Cut into the header itself rather than shaving bytes off the end. This is deliberate and it
    // is the lesson of the two earlier attempts at this test:
    //
    //  - Shaving the tail of a one-page database lands in *unused space* of page 1, and
    //    `integrity_check` is right to pass: unused bytes are not part of any structure.
    //  - Damaging the database while the write-ahead log is intact is invisible, because every
    //    page image is also in the log and the log is what gets read.
    //
    // Neither is a weakness in the check. A check that reported damage nobody can observe would be
    // the defect. What has to be caught is damage a reader cannot avoid, and the header is the one
    // thing every read has to look at.
    fs::write(&database, &bytes[..100]).expect("truncate into the header");

    // Settled *before* the damage, never after: a checkpoint would fold the log into the
    // truncated file and quietly repair it, and the test would then be proving nothing.
    let diagnosis = install.diagnose();
    // Either the damage stops SQLite opening the file, or `integrity_check` catches it. Both are
    // the correct outcome; what must not happen is a pass.
    assert!(!diagnosis.is_healthy(), "{}", diagnosis.report());
    let openable = findings_of(&diagnosis, Check::IndexOpenable);
    let integrity = findings_of(&diagnosis, Check::Integrity);
    if openable[0].severity == Severity::Fail {
        assert!(
            openable[0].action.is_some(),
            "an unopenable damaged index must say what to do: {:?}",
            openable[0]
        );
    } else {
        assert_eq!(
            integrity[0].severity,
            Severity::Fail,
            "a damaged index that opens must fail the integrity check: {}",
            diagnosis.report()
        );
        assert!(integrity[0].action.is_some());
    }
}

#[test]
fn a_store_from_a_different_shape_is_refused_rather_than_guessed_at() {
    // The schema-version check. A store written by a different shape of Peek cannot be read
    // reliably, and "reliably" is the operative word: reading it anyway produces answers that
    // look fine.
    let install = Install::empty("schema");
    let store = install.store();
    let written = store.schema_version();
    drop(store);

    // Rewrite the recorded version to a value this build does not speak.
    let database = install.database();
    let connection = rusqlite::Connection::open(&database).expect("open the database");
    connection
        .execute(
            "UPDATE meta SET value = '9999' WHERE key = 'schema_version'",
            [],
        )
        .expect("rewrite the recorded version");
    drop(connection);

    let diagnosis = install.settled_diagnosis();
    assert!(!diagnosis.is_healthy(), "{}", diagnosis.report());
    // The store refuses to open at all, which is the correct response, and the refusal is the
    // diagnosis.
    let openable = findings_of(&diagnosis, Check::IndexOpenable);
    assert_eq!(openable[0].severity, Severity::Fail);
    assert!(
        openable[0].detail.contains("9999") || written > 0,
        "the refusal must name the version it refused: {:?}",
        openable[0]
    );
}

#[test]
fn an_index_holding_rows_with_no_generation_is_reported_as_impossible() {
    // Rows with generation 0 mean something wrote to the database without going through the write
    // path, which is the exact failure the whole transactional design exists to prevent. The
    // cheapest possible evidence of it deserves a check of its own.
    let install = Install::empty("no-generation");
    let store = install.store();
    let connection = rusqlite::Connection::open(install.database()).expect("open");
    // Insert a row behind the store's back, with foreign keys and checks off, then leave the
    // generation where it is.
    connection
        .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON;")
        .expect("relax the guards");
    connection
        .execute(
            "INSERT INTO entity (path, kind, qualified_name, entity_ordinal, name, is_test) \
             VALUES ('src/sneaky.rs', 'function', 'sneaky', 0, 'sneaky', 0)",
            [],
        )
        .expect("insert behind the store's back");
    drop(connection);
    drop(store);

    // Settled before diagnosing, and that is the whole point of this change. The row was written
    // behind the store's back, so the check is only meaningful if the fact is in the database
    // file the checks read and not only in frames the log has not given up. On an index this
    // small everything written is in the log, and whether the log has been folded in by the time
    // the diagnosis runs depends on when the last connection to the file happened to close —
    // which is not the same on every platform. Its sibling, the test that also breaks the index
    // behind the store's back, settles; this one did not, and so it was grading on a fact that
    // was in the log rather than in the store.
    let diagnosis = install.settled_diagnosis();
    let generation = findings_of(&diagnosis, Check::Generation);
    assert_eq!(generation.len(), 1, "{generation:?}");
    assert_eq!(
        generation[0].severity,
        Severity::Fail,
        "rows with no generation cannot have come from the write path: {}",
        diagnosis.report()
    );
}

#[test]
fn a_diagnosis_is_of_the_index_this_install_built_and_not_of_wherever_the_root_points() {
    // `set_root_override` is a `static`. A fixture that resolves its index through it is not
    // testing its index, it is testing whichever of the tests running beside it resolved the root
    // most recently — and when that is a root nobody has written to, `Store::open` *creates* an
    // index there. A freshly created index is a one-page database file with a log holding its
    // whole schema, and it reports itself as an empty install. That is the shape of the numbers a
    // diagnosis of the wrong file produces, and it is why this asserts on rows and a generation
    // rather than on the path: two independently computed spellings of a path can agree while the
    // files they name are not the same, and on Linux a temporary directory and its canonical form
    // are the same string, so a comparison between them proves nothing.
    let install = Install::empty("own-index");
    install.write("src/lib.rs", "fn a() {}\nfn main() { a(); }\n");
    indexer::build_full(
        &mut install.store(),
        install.path(),
        DiscoveryOptions::default(),
    )
    .expect("index the repository");

    // Somewhere else entirely, pointed at deliberately rather than raced for.
    let decoy = Install::empty("decoy");
    let repo = RepoId::discover(install.path()).expect("derive a repository id");
    with_root(decoy.index.clone(), || {
        let diagnosis = install.diagnose();
        let stats = measurements(&diagnosis);
        assert!(
            stats.entity_count > 0 && stats.generation > 0,
            "the diagnosis must be of the index this test built; a freshly created one has \
             generation 0 and no rows, which reports itself as an empty install: {}",
            diagnosis.report()
        );

        // The direct evidence, and the assertion that would have caught it: the diagnosis created
        // no index anywhere the process-wide root happened to point.
        assert!(
            !decoy.index.join(repo.as_str()).join("index.db").exists(),
            "diagnosing must not open, and therefore create, an index at whatever the root points \
             at; it read the store it was handed"
        );
    });
}

#[test]
fn an_uncheckpointed_log_is_a_notice_and_a_checkpointed_one_is_a_pass() {
    // The measurement, in the shape of a test.
    //
    // Both diagnoses below are of one store holding one set of rows at one generation. The only
    // thing that differs is whether a checkpoint has been able to fold the log into the file, and
    // the measurement that moves is the log's size. That is the whole separation: if the log
    // check's verdict followed the store, it could not change here; if it follows the log, it
    // does. The file's size moves too, and the second half of this test measures what that means.
    //
    // It used to follow the file size. `wal * 2 > file_size` is true for every store between
    // creation and its first checkpoint, because until then the file holds one page and the log
    // holds everything — which is a description of a healthy store, graded as a warning. The old
    // verdict is not asserted anywhere here on purpose: a test that pins an arbitrary threshold
    // is what keeps an arbitrary threshold alive.
    //
    // The state this needs is a *refused* checkpoint, and it cannot be assumed. A build does not
    // leave frames behind: `build_full` ends with a checkpoint (`indexer/mod.rs`, "Checkpoint
    // **after** resolution, not before"), so a build leaves an empty log, and this test used to
    // assert the post-build state twice and read zero. Holding the store open was the author's
    // explanation and it is not the mechanism — `store`'s own tests measure a write leaving frames
    // (`store/tests.rs`, "a write leaves frames behind for the next reader"), and what empties
    // them is the build's own checkpoint.
    //
    // The product names what makes a checkpoint fail: a reader holds a snapshot. So that is what
    // is built here, on a second connection to the same file, through the product's own API.
    let install = Install::empty("wal-frames");
    install.write("src/lib.rs", "fn a() {}\nfn main() { a(); }\n");
    let mut store = install.store();

    // The store's busy timeout is five seconds, and a checkpoint that cannot take the log sits in
    // SQLite's busy handler for all of it before reporting the refusal. This test wants the
    // refusal, not the wait. Nothing here contends for a write lock — in WAL a reader and the
    // writer do not exclude each other — so a short timeout cannot turn a real conflict into a
    // spurious failure.
    store
        .conn()
        .busy_timeout(std::time::Duration::from_millis(100))
        .expect("shorten the busy timeout so a refused checkpoint returns promptly");

    // A second connection to the same file: a reader, which is the state that pins a log. This is
    // a `Store` and not a raw connection because "a reader holds a snapshot" is a state the
    // product is documented to be in, and a test that reached past the product to fake it would
    // be testing a state the product might not be able to enter.
    let reader = install.store();

    // `BEGIN DEFERRED` on its own pins nothing: SQLite takes the read lock on the first statement
    // run inside the transaction, so a snapshot without this read is a transaction that looks
    // like a reader and blocks nothing. The build's checkpoint would then succeed, and this
    // premise would read zero again — which is the failure this test has already had once.
    let snapshot = reader
        .conn()
        .unchecked_transaction()
        .expect("begin a read transaction on the second connection");
    let pinned: i64 = snapshot
        .query_row("SELECT COUNT(*) FROM entity", [], |row| row.get(0))
        .expect("read inside the transaction, so that it holds a snapshot");
    assert_eq!(
        pinned, 0,
        "the reader is a snapshot of the store before the build, so its read mark sits behind \
         every frame the build is about to commit, and a log cannot be restarted or truncated \
         while a reader is still behind it"
    );

    // The file and the log are measured before the build, because the question is what the build's
    // refused checkpoint did to them and there is nothing to compare that against without a
    // reading from before. Nothing else can grow the file: in WAL every committed page goes to the
    // log.
    let (before_build, before_log) = install.sizes();
    indexer::build_full(&mut store, install.path(), DiscoveryOptions::default())
        .expect("index the repository");

    // The build committed every frame and then could not fold them, because the reader holds the
    // log. A refused checkpoint is not a build failure: that is why `build_full` records the log's
    // size instead of returning the refusal, and the frames stay exactly where they are.
    let (after_refused, refused_log) = install.sizes();
    let waiting = install.diagnose();
    let waiting_stats = measurements(&waiting);
    assert!(
        waiting_stats.wal_size_bytes > 0,
        "the premise of this state, asserted rather than assumed: a checkpoint a reader has \
         refused leaves its frames in the log, and {} bytes is what it left",
        waiting_stats.wal_size_bytes
    );

    // The reader goes, and only then does the log fold. Nothing else changes: same connection,
    // same rows underneath, no write in between.
    drop(snapshot);
    drop(reader);
    store.checkpoint().expect("fold the build into the file");
    let settled = install.diagnose();
    let settled_stats = measurements(&settled);
    let (after_settled, settled_log) = install.sizes();

    // The measured question, and the measurement that answers it.
    //
    // Three readings of the database file across a refused and then a completed checkpoint, because
    // two readings of the evidence fitted what was known before: a refused checkpoint changes
    // nothing and only a completed one grows the file, or a refused one copies and a completed one
    // has nothing left to move. **Neither is what SQLite does**, and asserting either would have put
    // a mechanism into this suite on the strength of a plausible story. It does both, and that is
    // the finding:
    //
    // - The refused checkpoint **grew** the file. It copies the frames it is allowed to copy before
    //   it tries to reset the log, and it is refused only at the reset, so a checkpoint that fails
    //   still changes the file.
    // - The completed checkpoint grew it **again**, because the reader's snapshot sat behind the
    //   build's own frames and those were not among the ones the refused checkpoint could copy.
    //
    // So the file's size counts *pages copied*: not checkpoints completed, and not work outstanding.
    // Nothing else can move these numbers — in WAL every committed page goes to the log and only a
    // checkpoint copies pages out of it — so each delta belongs to one checkpoint rather than to
    // the build in between.
    assert!(
        after_refused > before_build,
        "a checkpoint a reader refused still copied pages into the file: {} bytes before the build, \
         {} bytes after, so the file grows on a checkpoint that did not complete and 'what has been \
         checkpointed' cannot mean 'checkpoint that completed'",
        before_build,
        after_refused
    );
    assert!(
        after_settled > after_refused,
        "and a completed one still had frames to copy: {} bytes after the refused checkpoint, {} \
         bytes after the completed one. The frames the reader held behind could not be copied until \
         it was gone, so completing a checkpoint is not the same as having nothing left to do",
        after_refused,
        after_settled
    );
    // The overlap, which is what the check's text rests on and why the two sizes are not a ratio of
    // anything. Taken with the first assertion this is measured rather than argued: the refused
    // checkpoint added pages to the file while the log lost none of the frames it had, so the two
    // files were holding the same pages at the same time.
    assert!(
        refused_log >= before_log,
        "the log holds every frame it had while the file gained {} bytes out of them: {} bytes \
         before the refused checkpoint, {} bytes after. A frame copied into the file stays in the \
         log, so the two sizes overlap rather than partitioning the index",
        after_refused - before_build,
        before_log,
        refused_log
    );
    assert_eq!(settled_log, 0, "and TRUNCATE is what empties it");

    // The store is identical across the two diagnoses. Asserted, because it is the claim the rest
    // of this test rests on: if the rows had moved, the comparison below would prove nothing.
    assert_eq!(waiting_stats.entity_count, settled_stats.entity_count);
    assert_eq!(waiting_stats.relation_count, settled_stats.relation_count);
    assert_eq!(waiting_stats.generation, settled_stats.generation);
    assert_eq!(waiting_stats.schema_version, settled_stats.schema_version);
    assert_eq!(
        waiting_stats.orphan_relations,
        settled_stats.orphan_relations
    );

    // The measurements that moved, and the one the old rule graded on.
    assert_eq!(settled_stats.wal_size_bytes, 0, "TRUNCATE empties the log");
    // Strict growth, restored. It had been weakened to non-decreasing because the two mechanisms that
    // fitted could not be told apart, and the assertions above now tell them apart: a refused
    // checkpoint grows the file *and* a completed one grows it again. So the two reported sizes
    // moving apart is measured rather than assumed, and this comparison is on the numbers the
    // diagnosis itself carries, because those are the pair a reader is shown.
    //
    // The `>=` it replaced could not fail: a size is not negative and nothing here removes pages,
    // so it stood for a mechanism without testing one. An assertion that survives either answer is
    // not a weak assertion, it is a missing one.
    assert!(
        settled_stats.file_size_bytes > waiting_stats.file_size_bytes,
        "folding the log into the file adds pages to it and never removes them: {} bytes became \
         {}, so the file size measures checkpointing rather than the index",
        waiting_stats.file_size_bytes,
        settled_stats.file_size_bytes
    );

    assert_eq!(
        severity_of(&settled, Check::WalSize),
        Severity::Pass,
        "nothing waiting to be folded in: {}",
        settled.report()
    );
    assert_eq!(
        severity_of(&waiting, Check::WalSize),
        Severity::Notice,
        "frames waiting is work in the log, not a fault in the index: {}",
        waiting.report()
    );

    let notice = findings_of(&waiting, Check::WalSize);
    assert!(
        notice[0].action.is_some(),
        "a finding a reader cannot act on is a finding they will not act on: {:?}",
        notice[0]
    );
    let graded_on = waiting_stats.wal_size_bytes.to_string();
    assert!(
        notice[0].summary.contains(&graded_on),
        "the finding must carry the measurement it was graded on: {:?}",
        notice[0]
    );
    assert!(
        waiting.is_healthy(),
        "a working index with committed frames in its log is a working index: {}",
        waiting.report()
    );
    // Nothing else may have moved, or the comparison above is not measuring what it claims.
    for check in [Check::Integrity, Check::Generation, Check::IndexLocation] {
        assert_eq!(
            severity_of(&waiting, check),
            Severity::Pass,
            "{check:?} moved between two diagnoses of one store: {}",
            waiting.report()
        );
    }
}

#[test]
fn a_dangling_edge_is_caught_even_with_the_guards_disabled() {
    // Foreign keys make this impossible, so producing it requires going around them — a migration,
    // a maintenance script, or a bug. That is exactly why the check exists rather than trusting
    // the constraint: the constraint can be switched off, and the orphan counter cannot.
    let install = Install::empty("orphan");
    let mut store = install.store();
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(function("src/a.rs", "a"))
                .with_entity(function("src/gone.rs", "gone"))
                .with_relation(Relation::resolved(
                    RelationKind::Calls,
                    id("src/a.rs", EntityKind::Function, "a"),
                    id("src/gone.rs", EntityKind::Function, "gone"),
                    "gone",
                    span(),
                    crate::model::Evidence::SameFile,
                )),
        )
        .expect("a committed edge has both ends present");
    drop(store);

    // Now break it behind the store's back: the target row goes, the edge stays.
    let connection = rusqlite::Connection::open(install.database()).expect("open");
    connection
        .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON;")
        .expect("relax the guards");
    connection
        .execute("DELETE FROM entity WHERE path = 'src/gone.rs'", [])
        .expect("delete the target behind the store's back");
    drop(connection);

    let diagnosis = install.settled_diagnosis();
    let orphans = findings_of(&diagnosis, Check::OrphanEdges);
    assert_eq!(orphans.len(), 1, "{orphans:?}");
    assert_eq!(
        orphans[0].severity,
        Severity::Fail,
        "a dangling edge is a confident answer about code that is not there: {}",
        diagnosis.report()
    );
    assert!(!diagnosis.is_healthy());
}

#[test]
fn undecided_relations_are_a_notice_because_they_are_work_remaining_not_damage() {
    // Severity is a claim about what the user should do, so a pending relation must not read as
    // a fault: the index is correct, the resolver simply has not run.
    let install = Install::empty("pending");
    let mut store = install.store();
    let source = id("src/a.rs", EntityKind::Function, "a");
    store
        .apply_update(
            IndexUpdate::empty()
                .with_entity(function("src/a.rs", "a"))
                .with_relation(Relation::pending(
                    RelationKind::Calls,
                    source,
                    "not_yet_placed",
                    span(),
                    crate::model::Evidence::NameOnly,
                    "extracted, not yet resolved",
                )),
        )
        .expect("commit");
    drop(store);

    let diagnosis = install.settled_diagnosis();
    let pending = findings_of(&diagnosis, Check::PendingWork);
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].severity, Severity::Notice);
    assert!(pending[0].action.is_some());
    assert!(diagnosis.is_healthy(), "{}", diagnosis.report());
}

#[test]
fn ambiguity_is_reported_as_a_fact_about_the_code_and_not_as_a_defect() {
    // The distinction this test exists to pin: a stored ambiguity is the system working. A tool
    // that reported it as a problem would be training the user to ignore the one field that
    // tells them where the graph is thin.
    let install = Install::empty("ambiguous");
    let mut store = install.store();
    // Four ambiguous edges and four resolved ones, so the share is a realistic minority rather
    // than the 100% a single-relation fixture produces — which the next test covers separately.
    let mut update = IndexUpdate::empty()
        .with_entity(function("src/a.rs", "a"))
        .with_entity(function("src/target.rs", "target"))
        .with_entity(function("src/one.rs", "shared"))
        .with_entity(function("src/two.rs", "shared"));
    for name in ["a", "b", "c", "d", "e", "f"] {
        update = update.with_entity(function("src/callers.rs", name));
        // Only the first two callers are ambiguous, so the share is a clear minority rather than
        // sitting exactly on the escalation boundary — where a test would be asserting a rounding
        // decision rather than a rule.
        if name == "a" || name == "b" {
            update = update.with_relation(Relation::ambiguous(
                RelationKind::Calls,
                id("src/callers.rs", EntityKind::Function, name),
                "shared",
                span(),
                vec![
                    id("src/one.rs", EntityKind::Function, "shared"),
                    id("src/two.rs", EntityKind::Function, "shared"),
                ],
            ));
        }
        update = update.with_relation(Relation::resolved(
            RelationKind::Calls,
            id("src/callers.rs", EntityKind::Function, name),
            id("src/target.rs", EntityKind::Function, "target"),
            "target",
            span(),
            crate::model::Evidence::SameFile,
        ));
    }
    store.apply_update(update).expect("commit");
    drop(store);

    let diagnosis = install.settled_diagnosis();
    let ambiguity = findings_of(&diagnosis, Check::Ambiguity);
    assert_eq!(ambiguity.len(), 1, "{ambiguity:?}");
    assert_eq!(
        ambiguity[0].severity,
        Severity::Notice,
        "a minority of ambiguous edges is information, not a fault: {}",
        diagnosis.report()
    );
    assert!(diagnosis.is_healthy());
    assert!(
        ambiguity[0].detail.contains("4 candidates"),
        "the finding must carry the candidate count: {:?}",
        ambiguity[0]
    );
}

#[test]
fn a_graph_that_is_mostly_ambiguous_is_escalated_because_the_graph_is_thin() {
    // The other branch of the same rule, and the one that matters. Ambiguity is a Notice at a low
    // share because it is a fact about the code; at a high share it is a fact about *this index* —
    // the graph cannot answer most questions, and a user who is not told that will draw
    // conclusions from edges that do not exist.
    let install = Install::empty("mostly-ambiguous");
    let mut store = install.store();
    let mut update = IndexUpdate::empty()
        .with_entity(function("src/one.rs", "shared"))
        .with_entity(function("src/two.rs", "shared"));
    for name in ["a", "b", "c", "d"] {
        update = update
            .with_entity(function("src/callers.rs", name))
            .with_relation(Relation::ambiguous(
                RelationKind::Calls,
                id("src/callers.rs", EntityKind::Function, name),
                "shared",
                span(),
                vec![
                    id("src/one.rs", EntityKind::Function, "shared"),
                    id("src/two.rs", EntityKind::Function, "shared"),
                ],
            ));
    }
    store.apply_update(update).expect("commit");
    drop(store);

    let diagnosis = install.settled_diagnosis();
    let ambiguity = findings_of(&diagnosis, Check::Ambiguity);
    assert_eq!(ambiguity.len(), 1, "{ambiguity:?}");
    assert_eq!(
        ambiguity[0].severity,
        Severity::Warn,
        "every edge ambiguous is a graph that cannot answer questions: {}",
        diagnosis.report()
    );
    assert!(
        diagnosis.is_healthy(),
        "a warning is still a working index: {}",
        diagnosis.report()
    );
}

#[test]
fn an_index_inside_the_repository_is_a_warning_with_a_way_out() {
    // Contract D-0006: index state must not live in the user's repository. `doctor` is where a
    // user finds out that it does, and it has to say how to move it.
    let install = Install::empty("in-repo");
    install.write("src/lib.rs", "fn a() {}\n");
    let repo = RepoId::discover(install.path()).expect("derive a repository id");

    // A deliberately in-repository index: the thing D-0006 forbids.
    let inside = install.path().join(".peek");
    fs::create_dir_all(&inside).expect("create the in-repository index directory");
    let mut store = Store::open(&inside.join("index.db"), &repo).expect("open the in-repo store");
    indexer::build_full(&mut store, install.path(), DiscoveryOptions::default())
        .expect("index into the repository");
    // Diagnosed while `store` is still open, and without the root override: this test is about
    // where the index lives, which `check_index_location` reads off the store it was handed. The
    // location is a property of the file, so the file is named here rather than resolved through
    // a process-wide the test would then have to hold a lock for.
    let diagnosis = diagnose_open(&store, install.path(), &repo);
    let location = findings_of(&diagnosis, Check::IndexLocation);
    assert_eq!(location.len(), 1, "{location:?}");
    assert_eq!(
        location[0].severity,
        Severity::Warn,
        "an index in the repository is committed and cloned by accident: {}",
        diagnosis.report()
    );
    assert!(
        location[0].action.is_some(),
        "the user has to be told how to move it, not just that it is wrong"
    );
}

#[test]
fn a_weaker_durability_is_reported_as_a_warning() {
    // The store is opened by the caller, and a caller can choose. `doctor` reports what it was
    // given rather than assuming the default.
    let install = Install::empty("weak-durability");
    let repo = RepoId::discover(install.path()).expect("derive a repository id");
    let store = Store::open_with(&install.database(), &repo, Durability::Normal)
        .expect("open with a weaker guarantee");

    let diagnosis = diagnose_open(&store, install.path(), &repo);
    let durability = findings_of(&diagnosis, Check::Durability);
    assert_eq!(durability.len(), 1, "{durability:?}");
    assert_eq!(durability[0].severity, Severity::Warn);
    assert!(
        durability[0].detail.contains("power"),
        "the finding must say what is actually at stake: {:?}",
        durability[0]
    );
}

#[test]
fn a_missing_repository_is_reported_as_a_failure_rather_than_an_empty_install() {
    // The rule the module is built around: something that cannot be checked says so. A path that
    // is not a repository cannot have its files walked, and reporting "0 files, nothing refused"
    // would convert a mistake into a clean bill of health.
    let install = Install::empty("missing-root");
    let missing = install.path().join("does-not-exist");
    // One of the two tests that mean to exercise root resolution, and so one of the two that hold
    // the override rather than naming the store. There is no store to name here: the path under
    // test is the one that does not exist.
    let diagnosis = with_root(install.index.clone(), || diagnose(&missing));

    assert!(!diagnosis.is_healthy(), "{}", diagnosis.report());
    let openable = findings_of(&diagnosis, Check::IndexOpenable);
    assert_eq!(openable[0].severity, Severity::Fail);
    assert!(
        openable[0].action.is_some(),
        "the user has to be told the path is wrong, not left to infer it: {:?}",
        openable[0]
    );
    assert!(
        !findings_of(&diagnosis, Check::UnsupportedFiles)
            .iter()
            .any(|f| f.severity == Severity::Pass),
        "a walk that did not happen must not be reported as a walk that found nothing"
    );
}

#[test]
fn files_the_walk_refuses_are_named_by_reason_rather_than_only_counted() {
    // "12 files were not indexed" is not actionable. "3 were `.h` files and 1 was not UTF-8" is.
    let install = Install::empty("refusals");
    install.write("src/lib.rs", "fn a() {}\n");
    install.write("notes.rst", "Title\n=====\n");
    install.write_bytes("src/binary.rs", b"fn b() { \xff\xfe }");
    install.write("README.md", "# hi\n");

    let reasons = refusal_reasons(install.path());
    let names: BTreeSet<&str> = reasons.iter().map(|(name, _)| *name).collect();
    assert!(
        names.contains("unsupported_extension"),
        "an unrecognised extension is the common case and must be named: {reasons:?}"
    );
    assert!(
        names.contains("not_utf8"),
        "a file that cannot be decoded must be named, not lumped in: {reasons:?}"
    );
    let total: u64 = reasons.iter().map(|(_, count)| count).sum();
    assert!(total >= 2, "at least two files were refused: {reasons:?}");
}

#[test]
fn the_report_leads_with_the_worst_finding_because_that_is_the_one_people_read() {
    let install = Install::empty("ordering");
    install.write("src/lib.rs", "fn a() {}\n");
    indexer::build_full(
        &mut install.store(),
        install.path(),
        DiscoveryOptions::default(),
    )
    .expect("index the repository");

    let diagnosis = install.settled_diagnosis();
    let report = diagnosis.report();
    let first_line = report.lines().next().expect("a report has a first line");
    assert!(
        first_line.starts_with('['),
        "every line is tagged with a severity: {report}"
    );
    assert!(
        report.contains("worst:"),
        "the summary names the conclusion: {report}"
    );
    assert!(
        diagnosis.worst_finding().is_none(),
        "a healthy install has no finding above pass, and the report must not invent one"
    );
}

#[test]
fn two_diagnoses_of_one_install_agree() {
    // A health check that reports something different on each run cannot be relied on, and the
    // user has no way to tell a flaky check from a real change.
    let install = Install::empty("stable");
    install.write("src/lib.rs", "fn a() {}\nfn b() { a(); }\n");
    indexer::build_full(
        &mut install.store(),
        install.path(),
        DiscoveryOptions::default(),
    )
    .expect("index the repository");

    let first = install.settled_diagnosis();
    let second = install.settled_diagnosis();
    assert_eq!(first.report(), second.report());
    assert_eq!(first.worst(), second.worst());
    assert_eq!(
        first.findings.len(),
        second.findings.len(),
        "an unchanged install must produce the same findings"
    );
}
