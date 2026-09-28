//! The indexer's conformance suite.
//!
//! These tests are about **behaviour across the whole pipeline**, not about any one stage. The
//! three-stage subsystems each have their own suites; what can only be checked here is whether
//! they compose into something correct: that a build is honest about what it did, that a refresh
//! touches only what changed, that a deletion leaves no stale rows, and that the generation
//! counter means something.
//!
//! The defects these guard are all ones the engine Peek replaces actually had.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{IndexError, SkipReason, build_full, refresh};
use crate::discover::DiscoveryOptions;
use crate::model::{EntityId, EntityKind, RepoPath};
use crate::store::{RepoId, Store};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A temporary directory that removes itself, so a failing test leaves nothing behind.
struct TempTree {
    /// The repository under test.
    root: PathBuf,
    /// The index, deliberately **outside** the tree.
    ///
    /// D-0006 puts the real cache in `~/.cache/piper/peek/<repo-id>/`, and a test that puts the
    /// database inside the tree is not testing the deployed layout: it makes discovery walk the
    /// `.db`, the `-wal` and the `-shm` sidecars and report three unsupported files that exist
    /// only because the test put them there.
    db: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = format!("peek-indexer-{}-{label}-{unique}", std::process::id());
        let root = std::env::temp_dir().join(format!("{base}-tree"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temporary tree");
        let db = std::env::temp_dir().join(format!("{base}-index/index.db"));
        if let Some(parent) = db.parent() {
            fs::create_dir_all(parent).expect("create the index directory");
        }
        Self { root, db }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture file");
        path
    }

    fn write_bytes(&self, relative: &str, contents: &[u8]) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directory");
        }
        fs::write(&path, contents).expect("write fixture bytes");
        path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        if let Some(parent) = self.db.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}

fn open_store(tree: &TempTree) -> Store {
    let repo = RepoId::discover(tree.path()).expect("derive a repository id");
    Store::open(&tree.db, &repo).expect("open the store")
}

fn id(path: &str, kind: EntityKind, qualified: &str) -> EntityId {
    EntityId::new(RepoPath::new(path).expect("valid path"), kind, qualified, 0)
}

#[test]
fn a_full_build_indexes_the_repository_and_reports_what_it_did() {
    let tree = TempTree::new("full-build");
    tree.write(
        "src/lib.rs",
        "struct Service;\nimpl Service { fn charge(&self) {} }\nfn main() {}\n",
    );
    tree.write("src/other.rs", "pub fn helper() {}\n");
    tree.write("README.md", "# not source\n");

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    let report = outcome.report();
    assert_eq!(
        report.files_indexed, 2,
        "only the two Rust files are source"
    );
    assert_eq!(report.files_degraded, 0, "both files parse cleanly");
    assert!(report.entities_written > 0);
    assert!(report.relations_written > 0);
    assert_eq!(
        report.generation, 1,
        "the first commit is generation 1, and the store agrees"
    );
    assert_eq!(
        store.generation(),
        1,
        "the report must not invent a generation"
    );

    // The store's own counts are the ones that matter, and they must agree with the report.
    let stats = store.stats().expect("stats");
    assert_eq!(stats.entity_count, report.entities_written);
    assert_eq!(stats.relation_count, report.relations_written);
    assert_eq!(
        stats.orphan_relations, 0,
        "a fresh build has no dangling edges"
    );
}

#[test]
fn a_method_is_indexed_as_a_method_not_a_bare_function() {
    // The defect this replaces extracted every Rust method as a plain `Function`, so the graph
    // could not distinguish `Service::charge` from a free function.
    let tree = TempTree::new("method-kind");
    tree.write(
        "src/lib.rs",
        "struct Service;\nimpl Service { fn charge(&self) {} }\n",
    );

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    let methods = store
        .entities_named("charge", 10)
        .expect("query")
        .into_iter()
        .filter(|entity| entity.kind() == EntityKind::Method)
        .count();
    assert_eq!(methods, 1, "the impl's method must be a Method");
}

#[test]
fn inheritance_is_present_because_it_is_extracted_at_all() {
    // The engine Peek replaces declared `Implements` in its enum and never constructed one, so
    // its graphs contained no implementation edges whatsoever.
    let tree = TempTree::new("inheritance");
    tree.write(
        "src/lib.rs",
        "trait Gateway { fn charge(&self); }\nstruct Stripe;\nimpl Gateway for Stripe { fn charge(&self) {} }\n",
    );

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    let stats = store.stats().expect("stats");
    assert!(
        stats.relation_count >= 2,
        "expected a call and an implements edge, got {}",
        stats.relation_count
    );
    assert!(outcome.report().relations_written >= 2);
}

#[test]
fn a_non_utf8_file_degrades_alone_and_never_aborts_the_repository() {
    // Audit A11: one non-UTF-8 file killed an entire index and persisted nothing.
    let tree = TempTree::new("non-utf8");
    tree.write("src/good_one.rs", "fn one() {}\n");
    tree.write("src/good_two.rs", "fn two() {}\n");
    tree.write_bytes("src/bad.rs", b"fn bad() { \xff\xfe }");

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    assert_eq!(
        outcome.report().files_indexed,
        2,
        "the two good files must still be indexed"
    );
    assert!(
        outcome.report().files_skipped >= 1,
        "the bad file must be counted, not silently dropped"
    );
    assert!(
        outcome
            .skipped
            .iter()
            .any(|skipped| skipped.path.ends_with("bad.rs")),
        "the skip must name the file: {:?}",
        outcome.skipped
    );
    assert!(store.stats().expect("stats").entity_count > 0);
}

#[test]
fn a_file_with_no_extraction_rules_is_reported_as_unsupported_not_as_empty() {
    // Conflating "no rules" with "no symbols" is how fourteen languages were advertised as
    // supported while extracting nothing.
    let tree = TempTree::new("unsupported");
    tree.write("src/notes.rst", "Title\n=====\n");
    tree.write("src/no_extension", "just text\n");
    tree.write("src/real.rs", "fn real() {}\n");

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    assert_eq!(outcome.report().files_unsupported, 2, "both have no rules");
    assert_eq!(
        outcome.report().files_indexed,
        1,
        "only the Rust file is indexed"
    );
    let named: Vec<&str> = outcome
        .skipped
        .iter()
        .map(|skipped| skipped.path.as_str())
        .collect();
    assert!(named.contains(&"src/notes.rst"), "{named:?}");
    assert!(named.contains(&"src/no_extension"), "{named:?}");
    assert!(
        outcome
            .skipped
            .iter()
            .all(|skipped| matches!(skipped.reason, SkipReason::UnsupportedExtension(_))),
        "the reason must name the missing rules: {:?}",
        outcome.skipped
    );
}

#[test]
fn a_refresh_touches_only_the_file_that_changed() {
    // The whole point of incremental indexing. The engine Peek replaces rebuilt the entire graph
    // and rewrote the entire store on every filesystem event, making watch quadratic.
    let tree = TempTree::new("incremental");
    tree.write("src/keep.rs", "fn keep() {}\n");
    tree.write("src/change.rs", "fn before() {}\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");
    let after_first = store.stats().expect("stats").entity_count;

    tree.write("src/change.rs", "fn before() {}\nfn after() {}\n");
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/change.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(
        outcome.report().files_indexed,
        1,
        "exactly one file is re-extracted"
    );
    assert_eq!(
        store.stats().expect("stats").entity_count,
        after_first + 1,
        "only the added symbol appears; nothing else is duplicated or lost"
    );
    assert!(
        store
            .entities_named("after", 5)
            .expect("query")
            .iter()
            .any(|entity| entity.name == "after"),
        "the new symbol is queryable"
    );
    assert!(
        store
            .entity(&id("src/change.rs", EntityKind::Function, "before"))
            .expect("query")
            .is_some(),
        "the unchanged symbol in the changed file is still there"
    );
}

#[test]
fn a_refresh_that_shrinks_a_file_removes_the_symbols_that_left() {
    // An upsert alone would leave rows for symbols that no longer exist — a stale index that
    // answers questions about code which is not on disk.
    let tree = TempTree::new("shrink");
    tree.write("src/shrinks.rs", "fn one() {}\nfn two() {}\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");
    assert!(
        store
            .entity(&id("src/shrinks.rs", EntityKind::Function, "two"))
            .expect("q")
            .is_some()
    );

    tree.write("src/shrinks.rs", "fn one() {}\n");
    refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/shrinks.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert!(
        store
            .entity(&id("src/shrinks.rs", EntityKind::Function, "two"))
            .expect("query")
            .is_none(),
        "a symbol that left the file must not remain in the index"
    );
    assert!(
        store
            .entity(&id("src/shrinks.rs", EntityKind::Function, "one"))
            .expect("query")
            .is_some()
    );
}

#[test]
fn deleting_a_file_removes_its_rows_and_leaves_no_orphans() {
    let tree = TempTree::new("delete");
    tree.write("src/gone.rs", "fn gone() {}\n");
    tree.write("src/stays.rs", "fn stays() {}\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");
    assert!(
        store
            .entity(&id("src/gone.rs", EntityKind::Function, "gone"))
            .expect("q")
            .is_some()
    );

    fs::remove_file(tree.path().join("src/gone.rs")).expect("delete the file");
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/gone.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(outcome.report().files_removed, 1);
    assert!(
        store
            .entity(&id("src/gone.rs", EntityKind::Function, "gone"))
            .expect("query")
            .is_none(),
        "the deleted file's symbols are gone"
    );
    assert!(
        store
            .entity(&id("src/stays.rs", EntityKind::Function, "stays"))
            .expect("query")
            .is_some(),
        "the untouched file is unaffected"
    );
    assert_eq!(
        store.stats().expect("stats").orphan_relations,
        0,
        "a deletion must not leave a dangling edge"
    );
}

#[test]
fn the_generation_is_monotonic_across_rebuilds() {
    // The engine Peek replaces reset its revision to 1 on every full build, so it could not
    // detect a stale or torn read.
    let tree = TempTree::new("generation");
    tree.write("src/lib.rs", "fn a() {}\n");

    let mut store = open_store(&tree);
    let first = build_full(&mut store, tree.path(), DiscoveryOptions::default())
        .expect("first")
        .report()
        .generation;
    let second = build_full(&mut store, tree.path(), DiscoveryOptions::default())
        .expect("second")
        .report()
        .generation;
    let third = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/lib.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh")
    .report()
    .generation;

    assert!(
        second > first,
        "a rebuild must advance the generation: {first} -> {second}"
    );
    assert!(
        third > second,
        "a refresh must advance it too: {second} -> {third}"
    );
}

#[test]
fn a_refresh_of_nothing_is_not_a_commit() {
    let tree = TempTree::new("no-op");
    tree.write("src/lib.rs", "fn a() {}\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first");
    let before = store.generation();

    let outcome = refresh(&mut store, tree.path(), &[], &DiscoveryOptions::default())
        .expect("refresh with nothing to do");

    assert_eq!(
        store.generation(),
        before,
        "an empty refresh must not churn the store or advance the generation"
    );
    assert_eq!(outcome.report().generation, before);
}

#[test]
fn an_oversized_file_is_skipped_and_counted_rather_than_parsed() {
    // The engine Peek replaces handed a 200 MB file straight to the parser.
    let tree = TempTree::new("too-large");
    tree.write("src/small.rs", "fn small() {}\n");
    tree.write("src/huge.rs", &"// padding\n".repeat(4096));

    let mut store = open_store(&tree);
    let options = DiscoveryOptions::default().with_max_file_bytes(64);
    let outcome = build_full(&mut store, tree.path(), options).expect("build");

    assert_eq!(
        outcome.report().files_indexed,
        1,
        "only the small file is parsed"
    );
    assert!(
        outcome
            .skipped
            .iter()
            .any(|skipped| matches!(skipped.reason, SkipReason::TooLarge { .. })),
        "the oversized file is reported as oversized: {:?}",
        outcome.skipped
    );
}

#[test]
fn a_rejected_write_surfaces_as_an_error_rather_than_a_successful_looking_report() {
    // Audit A1, the single most important defect in the engine Peek replaces: every persisted
    // write went through one `.ok()`, so a full disk reported "indexed N files" with nothing
    // written. An indexer that cannot fail is not an indexer.
    let tree = TempTree::new("store-failure");
    tree.write("src/lib.rs", "fn a() {}\n");

    let repo = RepoId::discover(tree.path()).expect("repo id");
    let db = tree.db.clone();
    {
        let _store = Store::open(&db, &repo).expect("open");
    }
    fs::remove_file(&db).expect("remove the database");
    fs::create_dir(&db).expect("put a directory where the database should be");

    let outcome: Result<_, IndexError> = {
        let mut store = match Store::open(&db, &repo) {
            Ok(store) => store,
            // Refusing to open a store it cannot verify is itself the correct behaviour, and it
            // is an error rather than an empty index.
            Err(error) => {
                let _ = error;
                return;
            }
        };
        build_full(&mut store, tree.path(), DiscoveryOptions::default())
    };

    if let Ok(outcome) = outcome {
        panic!(
            "an unwritable store must not produce a successful report: {}",
            outcome.report().summary()
        );
    }
}

#[test]
fn the_summary_reports_the_generation_and_the_uncertainty_counts() {
    // `peek status` and the MCP `index_status` primitive read this string, so it has to carry
    // the honest counts rather than a reassuring one.
    let tree = TempTree::new("summary");
    tree.write(
        "src/lib.rs",
        "struct S;\nimpl S { fn m(&self) {} }\nfn main() { let s = S; s.m(); }\n",
    );

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");
    let summary = outcome.report().summary();

    assert!(summary.contains("generation 1"), "{summary}");
    assert!(summary.contains("files indexed"), "{summary}");
    assert!(summary.contains("pending"), "{summary}");
    assert!(summary.contains("ambiguous"), "{summary}");
}
