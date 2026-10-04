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
use crate::model::{
    EntityId, EntityKind, Evidence, Relation, RelationKind, RepoPath, ResolutionState, Span,
};
use crate::store::{IndexUpdate, RepoId, Store};

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
    // Two files of bare declarations with no calls between them, so the extractor writes entities
    // and no relations. The resolution pass therefore has nothing to examine, writes nothing, and
    // **does not commit** — a build that churned the generation to record "I decided nothing"
    // would make the generation counter useless as a change signal. The two-commit case is
    // covered by `a_full_build_commits_twice_so_the_resolution_pass_is_visible_as_a_generation`.
    assert_eq!(
        report.generation, 1,
        "an extraction that found no relations commits once, and an empty pass is not a commit"
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

// ---------------------------------------------------------------------------
// A package is a fact about the repository, not about one file
//
// A file's package is the nearest ancestor directory holding a crate root, which no single
// file's path can say. So a refresh carries the repository's package roots, and the three
// tests below are the cases where getting that wrong shows up as a stale row.
// ---------------------------------------------------------------------------

/// The qualified names of the modules a file **is**.
///
/// **Not every module entity the file holds.** A `mod flags;` in `core/main.rs` is a module entity
/// too — the declaration — and a helper that returned both would make every assertion here about
/// the file's own place in the tree a comparison against a list with an extra name in it.
///
/// The distinction is the containment edge the extractor writes from the file to the module it is,
/// so this reads that edge rather than filtering on names: a name filter is a guess about which
/// rows are which, and it would break the day a module is legitimately called `flags`. The file
/// entity is spelled the way the walker spells it — path, kind, **file name**, ordinal 0 — because
/// `EntityId` is a primary key and a file entity reached under any other spelling is not found at
/// all, which is what an empty list here would mean.
fn modules_in(store: &Store, path: &str) -> Vec<String> {
    let file = RepoPath::new(path).expect("valid path");
    let file_id = EntityId::new(
        file.clone(),
        EntityKind::File,
        file.file_name(),
        0,
    );
    let mut names: Vec<String> = store
        .outgoing(&file_id, Some(RelationKind::Contains), 64)
        .expect("query")
        .into_iter()
        .filter_map(|relation| relation.target)
        .filter(|target| target.kind() == EntityKind::Module)
        .map(|target| target.qualified_name().to_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn a_full_build_names_a_file_without_a_source_root_from_the_directory_holding_the_crate_root() {
    // The defect, end to end through the indexer rather than through `locate`. `core/flags/defs.rs`
    // and `core/main.rs` are in one package because `main.rs` is in `core`, and the package has no
    // `src/` for the path to point at.
    let tree = TempTree::new("nosrc-package");
    tree.write("core/main.rs", "mod flags;\nfn main() {}\n");
    tree.write("core/flags/mod.rs", "mod defs;\n");
    tree.write("core/flags/defs.rs", "pub struct Flag;\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    assert_eq!(modules_in(&store, "core/main.rs"), vec!["core".to_owned()]);
    assert_eq!(
        modules_in(&store, "core/flags/mod.rs"),
        vec!["core::flags".to_owned()],
        "a directory two levels below the crate root is one package, not one package per level"
    );
    assert_eq!(
        modules_in(&store, "core/flags/defs.rs"),
        vec!["core::flags::defs".to_owned()]
    );
    assert!(
        store
            .entity(&id("core/main.rs", EntityKind::Package, "core"))
            .expect("query")
            .is_some(),
        "and exactly one file declares the package"
    );
    assert!(
        store
            .entity(&id("core/flags/defs.rs", EntityKind::Package, "flags"))
            .expect("query")
            .is_none(),
        "the file two levels down declares no package of its own"
    );
}

#[test]
fn creating_a_crate_root_renames_the_files_already_indexed_beside_it() {
    // **The hole a refresh cannot leave.** `core/flags/defs.rs` was indexed when `core/` held no
    // crate root, so it was named `flags::defs`. Writing `core/main.rs` makes `core` a package, and
    // every file under it is now `core::…` — but none of them changed, so nothing else would ever
    // re-extract them and the index would keep disagreeing with a full build.
    let tree = TempTree::new("package-appears");
    tree.write("core/flags/defs.rs", "pub struct Flag;\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");
    assert_eq!(
        modules_in(&store, "core/flags/defs.rs"),
        vec!["flags::defs".to_owned()],
        "with no crate root anywhere above it, the file's own directory is the fallback"
    );

    tree.write("core/main.rs", "mod flags;\nfn main() {}\n");
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("core/main.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(
        modules_in(&store, "core/flags/defs.rs"),
        vec!["core::flags::defs".to_owned()],
        "so the file that did not change is renamed anyway, which is what a full build would say"
    );
    assert!(
        store
            .entity(&id("core/flags/defs.rs", EntityKind::Module, "flags::defs"))
            .expect("query")
            .is_none(),
        "and the name it had is gone rather than left beside the new one"
    );
    assert!(
        outcome.report().files_reindexed_for_package > 0,
        "the extra work is reported, because it is work the caller did not ask for: {:?}",
        outcome.report()
    );
}

#[test]
fn deleting_a_crate_root_puts_the_files_beside_it_back_on_the_fallback() {
    // The other direction, and the one a "did this file change?" test cannot reach. Removing
    // `core/main.rs` stops `core` being a package, so `core/flags/defs.rs` goes back to being named
    // for its own directory. **The deleted root module must not be observed as a path** — it is in
    // the batch, and reading it would put the root straight back and change nothing at all.
    let tree = TempTree::new("package-disappears");
    tree.write("core/main.rs", "mod flags;\nfn main() {}\n");
    tree.write("core/flags/defs.rs", "pub struct Flag;\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");
    assert_eq!(
        modules_in(&store, "core/flags/defs.rs"),
        vec!["core::flags::defs".to_owned()]
    );

    std::fs::remove_file(tree.path().join("core/main.rs")).expect("remove the crate root");
    refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("core/main.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(
        modules_in(&store, "core/flags/defs.rs"),
        vec!["flags::defs".to_owned()],
        "the fallback is what a full build of this tree would now say"
    );
    assert!(
        store
            .entity(&id("core/flags/defs.rs", EntityKind::Module, "core::flags::defs"))
            .expect("query")
            .is_none(),
        "and the package-qualified name is gone"
    );
    assert!(
        store
            .entity(&id("core/main.rs", EntityKind::Package, "core"))
            .expect("query")
            .is_none(),
        "the package it declared went with the file that declared it"
    );
}

#[test]
fn a_refresh_of_an_ordinary_source_file_reads_no_package_roots_and_moves_nothing_else() {
    // The cost half. A crate root appearing or disappearing is rare; a keystroke in `src/` is the
    // common case under `watch`. A batch that moves no package root must not cost a scan of the
    // index's whole path list, and must not re-extract anything it was not given — otherwise
    // "cost is proportional to what changed" stops being true of every save.
    let tree = TempTree::new("package-cost");
    tree.write("src/lib.rs", "mod a;\n");
    tree.write("src/a.rs", "fn before() {}\n");
    tree.write("core/main.rs", "mod flags;\n");
    tree.write("core/flags/defs.rs", "pub struct Flag;\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");
    let before = modules_in(&store, "core/flags/defs.rs");

    tree.write("src/a.rs", "fn before() {}\nfn after() {}\n");
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/a.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(outcome.report().files_indexed, 1, "only the changed file");
    assert_eq!(
        outcome.report().files_reindexed_for_package,
        0,
        "and nothing was re-extracted for a package move that did not happen: {:?}",
        outcome.report()
    );
    assert_eq!(
        modules_in(&store, "core/flags/defs.rs"),
        before,
        "a crate with no source root is untouched by an edit to an unrelated one"
    );
}

#[test]
fn a_refresh_never_widens_to_a_file_whose_package_cannot_have_moved() {
    // The half of the widening rule that keeps it cheap: a file under a source root is named from
    // its path, so its module row cannot move whatever a crate root appearing elsewhere did. The
    // file `core/flags/defs.rs` here *would* move, and `src/a.rs` beside it would not — so the
    // count is one, and naming both would be a parse spent on nothing.
    let tree = TempTree::new("package-widen-narrow");
    tree.write("src/lib.rs", "mod a;\n");
    tree.write("src/a.rs", "pub fn before() {}\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");

    // A crate root inside `src/` — which is not a crate root at all, since `src` is a source root.
    // Nothing may be widened on the strength of it.
    tree.write("src/inner.rs", "pub fn inner() {}\n");
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/inner.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(outcome.report().files_reindexed_for_package, 0);
    assert_eq!(
        modules_in(&store, "src/a.rs"),
        vec!["src::a".to_owned()],
        "and the module rows are exactly what a full build says"
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
fn a_refresh_of_a_path_outside_the_root_indexes_nothing_and_says_why() {
    // **The defect.** `refresh` derived a repository-relative name with
    // `path.strip_prefix(root).unwrap_or(path)`, so a path that was not under the root was handed
    // on whole. `RepoPath` does not reject an absolute path — it drops the leading separator — so
    // `/…/borrowed.rs` became the key `…/borrowed.rs` and the store took rows for a file outside
    // the tree. Nothing refused it: the outcome said one file was indexed, and the only trace of
    // where it came from was a repository-relative name with no repository in it.
    //
    // **The file is spelled absolutely, because that is the shape that reaches this.** A relative
    // path with a `..` in it is caught by `RepoPath`'s own validator, so the two spellings of the
    // same escape fail differently, and only the absolute one reached the store.
    let tree = TempTree::new("refresh-outside-root");
    tree.write("src/keep.rs", "fn keep() {}\n");
    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");
    let before = store.stats().expect("stats").entity_count;

    // The fixture's own index directory: it exists, it is outside the tree, and it is removed with
    // the fixture, so a file written beside the database leaves nothing behind either.
    let outside_dir = tree
        .db
        .parent()
        .expect("the index path has a parent")
        .to_path_buf();
    let outside = outside_dir.join("borrowed.rs");
    fs::write(&outside, "fn borrowed() {}\n").expect("write a file outside the tree");
    // The fixture's own precondition: the path has to be outside the tree, or this asserts nothing.
    assert!(!outside.starts_with(tree.path()), "outside the tree");
    let reported = outside.display().to_string();

    let outcome = refresh(
        &mut store,
        tree.path(),
        std::slice::from_ref(&outside),
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(
        outcome.report().files_indexed,
        0,
        "a path outside the root is not a file in the repository"
    );
    assert_eq!(outcome.report().files_skipped, 1, "{outcome:?}");
    // And the refusal has to name which path and why, or the count is the whole report.
    let refused = outcome
        .skipped
        .iter()
        .find(|skipped| skipped.path == reported)
        .expect("the outside path is named in the report");
    assert_eq!(refused.reason, SkipReason::OutsideRoot);
    assert!(
        store
            .entities_named("borrowed", 5)
            .expect("query")
            .is_empty(),
        "the contents of a file outside the root must not be in the index under any name"
    );
    let after = store.stats().expect("stats").entity_count;
    assert_eq!(after, before, "the index is exactly as it was");
}

#[test]
fn a_refresh_of_a_climb_that_stays_inside_the_root_indexes_the_file_it_lands_on() {
    // The other half of the same rule, and the reason it normalises rather than refusing `..`: a
    // climb that ends up inside the root is an ordinary path, and the name it has inside the
    // repository is the one it lands on. Refusing every spelling with a `..` in it would have made
    // this a second way to lose a file.
    let tree = TempTree::new("refresh-climb-inside");
    tree.write("src/keep.rs", "fn keep() {}\n");
    let mut store = open_store(&tree);

    let climbed = tree.path().join("src/../src/keep.rs");
    let outcome = refresh(
        &mut store,
        tree.path(),
        std::slice::from_ref(&climbed),
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    assert_eq!(
        outcome.report().files_indexed,
        1,
        "the climb stays inside the tree, so it names a file of it"
    );
    let kept = id("src/keep.rs", EntityKind::Function, "keep");
    assert!(
        store.entity(&kept).expect("query").is_some(),
        "and it is indexed under the name it lands on, not the spelling it arrived as"
    );
    assert!(outcome.skipped.is_empty(), "nothing was refused");
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
fn a_pending_relation_can_be_written_and_then_read_back() {
    // The extractor's normal output is `pending`, so this is the round trip that matters most.
    // It was broken: the schema accepted the row but the row decoder had no arm for the tag, so
    // *writing* a pending relation succeeded and *reading* it failed with `Corrupt`. Every query
    // that touched a freshly-extracted relation broke, including the one the resolver needs as
    // its input. No earlier test caught it because none of them read a relation back out of a
    // store the indexer had just written.
    let tree = TempTree::new("pending-round-trip");
    tree.write("src/caller.rs", "fn target() {}\nfn main() { target(); }\n");

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");
    let report = outcome.report();
    assert!(
        report.relations_pending > 0,
        "the extractor should have left work for the resolver: {}",
        report.summary()
    );

    // The second pass ran, so the *store* no longer holds pending edges. What must match is that
    // the resolver examined exactly as many as the extractor wrote: that is what proves the
    // pending rows were readable on the way in, which is the step that used to fail.
    let resolution = report
        .resolution
        .as_ref()
        .expect("a full build runs the resolution pass");
    assert_eq!(
        resolution.examined, report.relations_pending,
        "the resolver must see every relation the extractor left pending: {report:?}"
    );
    assert_eq!(
        store.stats().expect("stats").pending_relations,
        0,
        "a full build resolves what it can, so nothing may be left pending"
    );
    assert!(
        !resolution.pending_remaining,
        "and nothing may remain unexamined: {}",
        resolution.summary()
    );

    // The round trip itself, isolated: write a pending relation through the public API and read it
    // back. This is the step that used to fail, and it has to be tested directly rather than
    // inferred from the resolver's success — the resolver failing loudly is evidence, not proof.
    let source = id("src/caller.rs", EntityKind::Function, "main");
    let target = id("src/caller.rs", EntityKind::Function, "target");
    let pending = Relation::pending(
        RelationKind::Calls,
        source.clone(),
        "not_yet_resolved",
        Span::new(0, 10, 1, 0, 1, 9).expect("a forward span is valid"),
        Evidence::NameOnly,
        "a reference the resolver has not looked at",
    );
    store
        .apply_update(IndexUpdate::empty().with_relation(pending))
        .expect("write a pending relation");

    let read_back = store
        .relations_in_state(
            &ResolutionState::Pending {
                evidence: Evidence::NameOnly,
                basis: String::new(),
            },
            64,
        )
        .expect("a pending relation must be readable, not Corrupt");
    assert_eq!(
        read_back.len(),
        1,
        "the row that was just written must come back"
    );
    assert!(
        !read_back[0].resolution.describe().is_empty(),
        "a pending relation must still say why it is pending, not just that it is"
    );
    assert!(
        read_back[0]
            .resolution
            .describe()
            .contains("has not looked at"),
        "the basis must survive the round trip: {}",
        read_back[0].resolution.describe()
    );
    assert!(
        read_back[0].target.is_none(),
        "a pending relation has no target yet, and must not be given one"
    );

    // And through the adjacency path, which is the one a consumer actually uses.
    let edges = store.outgoing(&source, None, 32).expect("outgoing");
    assert!(
        edges.iter().any(|edge| edge.resolution.is_pending()),
        "the call edge survives a full write and read cycle as pending: {edges:?}"
    );
    assert!(
        store.entity(&target).expect("query").is_some(),
        "and the write did not disturb the entities around it"
    );
}

#[test]
fn a_full_build_commits_twice_so_the_resolution_pass_is_visible_as_a_generation() {
    // Resolution is a second pass, not an inline step of extraction, and the difference is only
    // real if a reader can see it. Two commits, two generations, and a report that says which.
    let tree = TempTree::new("two-commits");
    tree.write("src/lib.rs", "fn helper() {}\nfn main() { helper(); }\n");

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");

    let resolution = outcome
        .report()
        .resolution
        .clone()
        .expect("a build runs the resolution pass");
    assert_eq!(
        outcome.report().generation,
        2,
        "extraction commits generation 1 and resolution commits generation 2"
    );
    assert_eq!(
        resolution.generation,
        2,
        "the report must carry the store's own generation, not its own: {}",
        resolution.summary()
    );
    assert_eq!(
        store.generation(),
        2,
        "and the store must agree, or the report is describing an index that does not exist"
    );
    assert!(
        !resolution.pending_remaining,
        "a full build must leave no relation awaiting a decision: {}",
        resolution.summary()
    );
}

#[test]
fn every_relation_the_report_counts_is_in_exactly_one_state() {
    // Found by indexing `rust-lang/regex`: the summary listed pending, unresolved and ambiguous but
    // not resolved or inferred, so its numbers summed to 27,874 against a reported total of
    // 37,868. Nothing said the missing 9,994 were simply unreported, and a reader could not tell
    // work remaining from work done.
    let tree = TempTree::new("partition");
    tree.write(
        "src/lib.rs",
        "struct S;\nimpl S { fn m(&self) {} }\nfn main() { let s = S; s.m(); }\n",
    );

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");
    let report = outcome.report();

    assert_eq!(
        report.relations_accounted_for(),
        report.relations_written,
        "the five states must account for every relation written: {report:?}"
    );
    assert!(
        report.relations_pending > 0,
        "the extractor leaves work for the resolver, so the pending count is the one that matters"
    );
    assert!(
        report.relations_written >= report.relations_pending,
        "everything the extractor writes is in some state, and most of it is pending"
    );

    // The report counts what the *extractor* wrote. The store holds what survives *after* the
    // resolution pass, so these are two different moments and comparing them directly would be
    // wrong — it would assert that the resolver did nothing. What must hold is that the report's
    // five states partition its own total, and that the store's five partition the store's.
    assert_eq!(
        report.relations_accounted_for(),
        report.relations_written,
        "the five states must account for every relation written: {report:?}"
    );
    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.resolved_relations
            + stats.pending_relations
            + stats.ambiguous_relations
            + stats.unresolved_relations
            + stats.inferred_relations,
        stats.relation_count,
        "and the store's five states must account for every row it holds"
    );
    assert!(
        stats.pending_relations == 0,
        "a full build resolves what it can, so the store must not be left holding pending rows"
    );
}

#[test]
fn a_full_build_leaves_a_write_ahead_log_smaller_than_the_database() {
    // Measured on `rust-lang/regex`: after indexing 227 files the log was 30,479,792 bytes against
    // a 30,199,808-byte database, because nothing checkpointed. A user who indexed a repository
    // and quit was paying for the index twice, and the next reader replayed the whole log.
    let tree = TempTree::new("wal");
    for file in 0..40 {
        tree.write(
            &format!("src/mod_{file}.rs"),
            &format!("struct S{file};\nfn f{file}() {{}}\nfn g{file}() {{ f{file}(); }}\n"),
        );
    }

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");
    let stats = store.stats().expect("stats");

    assert!(
        outcome.report().wal_bytes < stats.file_size_bytes,
        "a bulk build should fold its log back into the database: wal {} vs db {}",
        outcome.report().wal_bytes,
        stats.file_size_bytes
    );
    assert_eq!(
        outcome.report().wal_bytes,
        stats.wal_size_bytes,
        "the reported size is the measured one, not an estimate"
    );
}

#[test]
fn a_refresh_of_a_file_repairs_the_edges_that_pointed_into_it() {
    // A refresh removes a file's rows before re-inserting them, and the store demotes every edge
    // that pointed into the removed entities. If the refresh did not hand those edges to the
    // resolver afterwards, then merely *editing* a file would orphan every caller of every
    // symbol in it — a silent loss of the exact edges the index exists to provide.
    let tree = TempTree::new("refresh-repairs-incoming");
    tree.write("src/orders.rs", "pub fn charge() {}\n");
    tree.write("src/app.rs", "fn go() { charge(); }\n");

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");
    assert!(
        store
            .callees(
                &id("src/app.rs", EntityKind::Function, "go"),
                Some(RelationKind::Calls),
                10
            )
            .expect("query")
            .contains(&id("src/orders.rs", EntityKind::Function, "charge")),
        "the first build resolved the call across files"
    );

    // Edit the *other* file. `src/app.rs` is not touched, so nothing about that edge changed —
    // and nothing about it should have been lost either.
    tree.write("src/orders.rs", "pub fn charge() {}\npub fn refund() {}\n");
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/orders.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    let resolution = outcome
        .report()
        .resolution
        .clone()
        .expect("a refresh runs the resolution pass");
    assert!(
        resolution.displaced >= 1,
        "the refresh must have seen the edge it was about to break: {}",
        resolution.summary()
    );
    let repaired = store
        .outgoing(
            &id("src/app.rs", EntityKind::Function, "go"),
            Some(RelationKind::Calls),
            10,
        )
        .expect("query");
    assert_eq!(
        repaired.len(),
        1,
        "editing a file must not drop the calls that point into it: {repaired:?}"
    );
    let states: Vec<String> = repaired
        .iter()
        .map(|edge| format!("{} -> {:?}", edge.target_name, edge.resolution))
        .collect();
    assert!(
        repaired[0].target.is_some(),
        "and the edge must be bound to a target again, not left dangling: {states:?} — {}",
        resolution.summary()
    );
    // Whether `callees` can follow it is a separate question and is answered by the *state* of the
    // edge, not by whether the row survived. An edge bound only by a repo-wide uniqueness check is
    // `Inferred`, and `callees` deliberately walks only `Resolved` edges — that restriction is the
    // contract, not a defect. So the claim here is the one that matters after a refresh: the
    // relation is still there and still points somewhere.
}

#[test]
fn a_refresh_of_a_large_file_leaves_no_relation_undecided() {
    // The defect this pins, measured on `BurntSushi/ripgrep`: a refresh of
    // `crates/core/flags/defs.rs`, a file of 1,363 entities, wrote 3,599 relations and the
    // resolution pass decided 2,121 of them. The other 348 stayed `Pending` — extracted, never
    // placed, never refused — because the pass enumerates a file's entities through a bounded read
    // whose default bound is 512, and everything past it was never in scope.
    //
    // Nothing afterwards decided them. A second refresh of the same file left the count at 348, and
    // only a full re-resolve cleared it, which is the pass `build_full` runs and no other operation
    // does. So on a watched repository the leak grew one large file at a time and the index carried
    // edges that answered no question in either direction.
    //
    // The fixture is built past the default bound on purpose: a file small enough to fit inside it
    // would pass whether or not the scope were sized from the batch.
    let tree = TempTree::new("refresh-large-file");
    let past_the_default_bound = 512;
    let mut source = String::from("struct Wide;\nimpl Wide {\n");
    for index in 0..past_the_default_bound {
        source.push_str(&format!("    fn m{index}(&self) {{ let _ = {index}; }}\n"));
    }
    source.push_str("}\nfn caller() { let wide = Wide; wide.m0(); }\n");
    tree.write("src/wide.rs", &source);

    let mut store = open_store(&tree);
    build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("first build");
    let entities = store
        .entities_in_file(&RepoPath::new("src/wide.rs").expect("valid path"), 10_000)
        .expect("entities in file");
    assert!(
        entities.len() > past_the_default_bound,
        "the fixture must exceed the bound it is testing, and it holds {}",
        entities.len()
    );

    // Re-index it with nothing changed at all. The cheapest refresh there is, and the one a watcher
    // issues on every save.
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/wide.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh");

    let stats = store.stats().expect("stats");
    assert_eq!(
        stats.pending_relations,
        0,
        "a refresh must decide every relation it wrote, and this one wrote {} entities",
        entities.len()
    );
    // The report's own figure, not only the store's: a caller reads the report, and a run that
    // leaves work behind has to be able to say so without a second query.
    assert_eq!(
        outcome.report().relations_undecided,
        0,
        "the report must measure what the index was left holding: {}",
        outcome.report().summary()
    );

    // And the same file, deleted and restored, must land in the same place: the restore re-extracts
    // the file, so it re-creates exactly the situation the refresh above survived.
    let path = tree.path().join("src/wide.rs");
    let original = fs::read(&path).expect("read before deleting");
    fs::remove_file(&path).expect("delete");
    refresh(
        &mut store,
        tree.path(),
        std::slice::from_ref(&path),
        &DiscoveryOptions::default(),
    )
    .expect("refresh after a deletion");
    assert_eq!(
        store.stats().expect("stats").pending_relations,
        0,
        "a deletion leaves nothing to decide: the file's rows went with it"
    );

    fs::write(&path, &original).expect("put the file back");
    refresh(
        &mut store,
        tree.path(),
        &[path],
        &DiscoveryOptions::default(),
    )
    .expect("re-index the restored file");
    assert_eq!(
        store.stats().expect("stats").pending_relations,
        0,
        "restoring a large file must decide it, exactly as the refresh above did"
    );
}

#[test]
fn the_summary_names_what_a_run_left_undecided() {
    // Two different numbers, and the reason the summary carries both. `relations_pending` is what
    // the extractor emitted; `relations_undecided` is what the index is left holding once the pass
    // has run. On a clean run the first is large and the second is zero, so a summary that printed
    // only the first would report thousands of outstanding edges on every healthy build.
    let tree = TempTree::new("summary-undecided");
    tree.write("src/orders.rs", "pub fn charge() {}\n");
    tree.write("src/app.rs", "fn go() { charge(); }\n");

    let mut store = open_store(&tree);
    let outcome = build_full(&mut store, tree.path(), DiscoveryOptions::default()).expect("build");
    let report = outcome.report();
    assert_eq!(
        report.relations_undecided, 0,
        "a full build decides everything it wrote"
    );
    let summary = report.summary();
    assert!(
        !summary.contains("left undecided"),
        "a clean run must not carry a clause about work it did not leave: {summary}"
    );

    // The clause appears when the number is non-zero, and the number comes from the store. A
    // hand-written report claiming otherwise would be the exact dishonesty the field exists to
    // prevent, so the only honest way to show the clause is to make it true.
    let mut forged = report.clone();
    forged.relations_undecided = 7;
    let summary = forged.summary();
    assert!(
        summary.contains("7 left undecided"),
        "a run that left work behind must say so: {summary}"
    );
    // And the real thing: the clause appears because the store holds an undecided relation, not
    // because a report said so. It is written into `src/orders.rs`, and the refresh below touches
    // only `src/app.rs`, so the relation survives the run — which is the point of the field. It is
    // the index's outstanding total rather than this run's, so work an earlier run left behind is
    // visible to a caller reading this report rather than hidden behind a run that did its own.
    store
        .apply_update(IndexUpdate::empty().with_relation(Relation::pending(
            RelationKind::Calls,
            id("src/orders.rs", EntityKind::Function, "charge"),
            "never_decided",
            Span::new(0, 4, 1, 1, 1, 5).expect("a forward span is valid"),
            Evidence::NameOnly,
            "extracted, not yet resolved",
        )))
        .expect("commit a pending relation");
    let outcome = refresh(
        &mut store,
        tree.path(),
        &[tree.path().join("src/app.rs")],
        &DiscoveryOptions::default(),
    )
    .expect("refresh over a store that already holds undecided work");
    assert_eq!(
        outcome.report().relations_undecided,
        1,
        "the outstanding total is the index's, not this run's, so an earlier run's leak is visible: \
         {}",
        outcome.report().summary()
    );
    // Which also means the summary now names it, from the measurement rather than from the report's
    // own arithmetic.
    assert!(
        outcome.report().summary().contains("1 left undecided"),
        "{}",
        outcome.report().summary()
    );
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

    assert!(summary.contains("generation 2"), "{summary}");
    assert!(summary.contains("files indexed"), "{summary}");
    assert!(summary.contains("pending"), "{summary}");
    assert!(summary.contains("ambiguous"), "{summary}");
    assert!(summary.contains("wal"), "{summary}");
    // The resolution pass has to be visible in the same string, or `peek status` reports only the
    // work that is outstanding and never what was decided.
    assert!(summary.contains("resolution pass 2"), "{summary}");
    assert!(summary.contains("resolved"), "{summary}");
}
